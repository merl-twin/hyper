//! HTTP Server
//!
//! A `Server` is created to listen on a port, parse HTTP requests, and hand
//! them off to a `Service`.

#[cfg(feature = "compat")]
pub mod compat;
pub mod conn;
mod service;

use std::cell::RefCell;
use std::fmt;
use std::io;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use futures::task::{self, Task};
use futures::future::{self};
use futures::{Future, Stream, Poll, Async};
use net2;

#[cfg(feature = "compat")]
use http;

use tokio_io::{AsyncRead, AsyncWrite};
use tokio::reactor::{Core, Handle, Interval, Timeout};
use tokio::net::TcpListener;
pub use tokio_service::{NewService, Service};

use proto;
#[cfg(feature = "compat")]
use proto::Body;
use self::addr_stream::AddrStream;
use self::hyper_service::HyperService;

pub use proto::response::Response;
pub use proto::request::Request;

feat_server_proto! {
    mod server_proto;
    pub use self::server_proto::{
        __ProtoRequest,
        __ProtoResponse,
        __ProtoTransport,
        __ProtoBindTransport,
    };
}

pub use self::conn::Connection;
pub use self::service::{const_service, service_fn};

/// A configuration of the HTTP protocol.
///
/// This structure is used to create instances of `Server` or to spawn off tasks
/// which handle a connection to an HTTP server. Each instance of `Http` can be
/// configured with various protocol-level options such as keepalive.
pub struct Http<B = ::Chunk> {
    max_buf_size: Option<usize>,
    keep_alive: bool,
    pipeline: bool,
    sleep_on_errors: bool,
    _marker: PhantomData<fn() -> B>,
}

/// An instance of a server created through `Http::bind`.
///
/// This server is intended as a convenience for creating a TCP listener on an
/// address and then serving TCP connections accepted with the service provided.
pub struct Server<S, B>
where B: Stream<Error=::Error>,
      B::Item: AsRef<[u8]>,
{
    protocol: Http<B::Item>,
    new_service: S,
    reactor: Core,
    listener: TcpListener,
    shutdown_timeout: Duration,
}

/// A stream mapping incoming IOs to new services.
///
/// Yields `Connection`s that are futures that should be put on a reactor.
#[must_use = "streams do nothing unless polled"]
#[derive(Debug)]
pub struct Serve<I, S> {
    incoming: I,
    new_service: S,
    protocol: Http,
}

/*
#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub struct SpawnAll<I, S, E> {
    executor: E,
    serve: Serve<I, S>,
}
*/

/// A stream of connections from binding to an address.
#[must_use = "streams do nothing unless polled"]
#[derive(Debug)]
pub struct AddrIncoming {
    addr: SocketAddr,
    keep_alive_timeout: Option<Duration>,
    listener: TcpListener,
    handle: Handle,
    sleep_on_errors: bool,
    timeout: Option<Timeout>,
}


// ===== impl Http =====

impl<B: AsRef<[u8]> + 'static> Http<B> {
    /// Creates a new instance of the HTTP protocol, ready to spawn a server or
    /// start accepting connections.
    pub fn new() -> Http<B> {
        Http {
            keep_alive: true,
            max_buf_size: None,
            pipeline: false,
            sleep_on_errors: false,
            _marker: PhantomData,
        }
    }

    /// Enables or disables HTTP keep-alive.
    ///
    /// Default is true.
    pub fn keep_alive(&mut self, val: bool) -> &mut Self {
        self.keep_alive = val;
        self
    }

    /// Set the maximum buffer size for the connection.
    pub fn max_buf_size(&mut self, max: usize) -> &mut Self {
        self.max_buf_size = Some(max);
        self
    }

    /// Aggregates flushes to better support pipelined responses.
    ///
    /// Experimental, may be have bugs.
    ///
    /// Default is false.
    pub fn pipeline(&mut self, enabled: bool) -> &mut Self {
        self.pipeline = enabled;
        self
    }

    /// Swallow connection accept errors. Instead of passing up IO errors when
    /// the server is under heavy load the errors will be ignored. Some
    /// connection accept errors (like "connection reset") can be ignored, some
    /// (like "too many files open") may consume 100% CPU and a timout of 10ms
    /// is used in that case.
    ///
    /// Default is false.
    pub fn sleep_on_errors(&mut self, enabled: bool) -> &mut Self {
        self.sleep_on_errors = enabled;
        self
    }

    /// Bind the provided `addr` and return a server ready to handle
    /// connections.
    ///
    /// This method will bind the `addr` provided with a new TCP listener ready
    /// to accept connections. Each connection will be processed with the
    /// `new_service` object provided as well, creating a new service per
    /// connection.
    ///
    /// The returned `Server` contains one method, `run`, which is used to
    /// actually run the server.
    pub fn bind<S, Bd>(&self, addr: &SocketAddr, new_service: S) -> ::Result<Server<S, Bd>>
        where S: NewService<Request = Request, Response = Response<Bd>, Error = ::Error> + 'static,
              Bd: Stream<Item=B, Error=::Error>,
    {
        let core = try!(Core::new());
        let handle = core.handle();
        let listener = try!(thread_listener(addr, &handle));

        Ok(Server {
            new_service: new_service,
            reactor: core,
            listener: listener,
            protocol: self.clone(),
            shutdown_timeout: Duration::new(1, 0),
        })
    }


    /// Bind a `NewService` using types from the `http` crate.
    ///
    /// See `Http::bind`.
    #[cfg(feature = "compat")]
    pub fn bind_compat<S, Bd>(&self, addr: &SocketAddr, new_service: S) -> ::Result<Server<compat::NewCompatService<S>, Bd>>
        where S: NewService<Request = http::Request<Body>, Response = http::Response<Bd>, Error = ::Error> +
                    Send + Sync + 'static,
              Bd: Stream<Item=B, Error=::Error>,
    {
        self.bind(addr, self::compat::new_service(new_service))
    }

    /// Bind the provided `addr` and return a server with a shared `Core`.
    ///
    /// This method allows the ability to share a `Core` with multiple servers.
    ///
    /// This is method will bind the `addr` provided with a new TCP listener ready
    /// to accept connections. Each connection will be processed with the
    /// `new_service` object provided as well, creating a new service per
    /// connection.
    pub fn serve_addr_handle<S, Bd>(&self, addr: &SocketAddr, handle: &Handle, new_service: S) -> ::Result<Serve<AddrIncoming, S>>
        where S: NewService<Request = Request, Response = Response<Bd>, Error = ::Error>,
              Bd: Stream<Item=B, Error=::Error>,
    {
        let listener = TcpListener::bind(addr, &handle)?;
        let mut incoming = AddrIncoming::new(listener, handle.clone(), self.sleep_on_errors)?;
        if self.keep_alive {
            incoming.set_keepalive(Some(Duration::from_secs(90)));
        }
        Ok(self.serve_incoming(incoming, new_service))
    }

    /// Bind the provided stream of incoming IO objects with a `NewService`.
    ///
    /// This method allows the ability to share a `Core` with multiple servers.
    pub fn serve_incoming<I, S, Bd>(&self, incoming: I, new_service: S) -> Serve<I, S>
        where I: Stream<Error=::std::io::Error>,
              I::Item: AsyncRead + AsyncWrite,
              S: NewService<Request = Request, Response = Response<Bd>, Error = ::Error>,
              Bd: Stream<Item=B, Error=::Error>,
    {
        Serve {
            incoming: incoming,
            new_service: new_service,
            protocol: Http {
                keep_alive: self.keep_alive,
                max_buf_size: self.max_buf_size,
                pipeline: self.pipeline,
                sleep_on_errors: self.sleep_on_errors,
                _marker: PhantomData,
            },
        }
    }

    /// Bind a connection together with a Service.
    ///
    /// This returns a Future that must be polled in order for HTTP to be
    /// driven on the connection.
    ///
    /// # Example
    ///
    /// ```
    /// # extern crate futures;
    /// # extern crate hyper;
    /// # extern crate tokio_core;
    /// # extern crate tokio_io;
    /// # use futures::Future;
    /// # use hyper::server::{Http, Request, Response, Service};
    /// # use tokio_io::{AsyncRead, AsyncWrite};
    /// # use tokio_core::reactor::Handle;
    /// # fn run<I, S>(some_io: I, some_service: S, some_handle: &Handle)
    /// # where
    /// #     I: AsyncRead + AsyncWrite + 'static,
    /// #     S: Service<Request=Request, Response=Response, Error=hyper::Error> + 'static,
    /// # {
    /// let http = Http::<hyper::Chunk>::new();
    /// let conn = http.serve_connection(some_io, some_service);
    ///
    /// let fut = conn
    ///     .map(|_| ())
    ///     .map_err(|e| eprintln!("server connection error: {}", e));
    ///
    /// some_handle.spawn(fut);
    /// # }
    /// # fn main() {}
    /// ```
    pub fn serve_connection<S, I, Bd>(&self, io: I, service: S) -> Connection<I, S>
        where S: Service<Request = Request, Response = Response<Bd>, Error = ::Error>,
              Bd: Stream<Error=::Error>,
              Bd::Item: AsRef<[u8]>,
              I: AsyncRead + AsyncWrite + RemoteAddr,

    {
        let addr = io.remote();
        let mut conn = proto::Conn::new(io);
        if !self.keep_alive {
            conn.disable_keep_alive();
        }
        conn.set_flush_pipeline(self.pipeline);
        if let Some(max) = self.max_buf_size {
            conn.set_max_buf_size(max);
        }
        Connection {
            conn: proto::dispatch::Dispatcher::new(proto::dispatch::Server::new(service), conn),
            remote_addr: addr,
        }
    }
}



impl<B> Clone for Http<B> {
    fn clone(&self) -> Http<B> {
        Http {
            ..*self
        }
    }
}

impl<B> fmt::Debug for Http<B> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Http")
            .field("keep_alive", &self.keep_alive)
            .field("pipeline", &self.pipeline)
            .finish()
    }
}



// ===== impl Server =====

impl<S, B> Server<S, B>
    where S: NewService<Request = Request, Response = Response<B>, Error = ::Error> + 'static,
          B: Stream<Error=::Error> + 'static,
          B::Item: AsRef<[u8]>,
{
    /// Returns the local address that this server is bound to.
    pub fn local_addr(&self) -> ::Result<SocketAddr> {
        Ok(try!(self.listener.local_addr()))
    }

    /// Returns a handle to the underlying event loop that this server will be
    /// running on.
    pub fn handle(&self) -> Handle {
        self.reactor.handle()
    }

    /// Configure the amount of time this server will wait for a "graceful
    /// shutdown".
    ///
    /// This is the amount of time after the shutdown signal is received the
    /// server will wait for all pending connections to finish. If the timeout
    /// elapses then the server will be forcibly shut down.
    ///
    /// This defaults to 1s.
    pub fn shutdown_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.shutdown_timeout = timeout;
        self
    }

    #[doc(hidden)]
    #[deprecated(since="0.11.11", note="no_proto is always enabled")]
    pub fn no_proto(&mut self) -> &mut Self {
        self
    }

    /// Execute this server infinitely.
    ///
    /// This method does not currently return, but it will return an error if
    /// one occurs.
    pub fn run(self) -> ::Result<()> {
        self.run_until(future::empty())
    }

    /// Execute this server until the given future, `shutdown_signal`, resolves.
    ///
    /// This method, like `run` above, is used to execute this HTTP server. The
    /// difference with `run`, however, is that this method allows for shutdown
    /// in a graceful fashion. The future provided is interpreted as a signal to
    /// shut down the server when it resolves.
    ///
    /// This method will block the current thread executing the HTTP server.
    /// When the `shutdown_signal` has resolved then the TCP listener will be
    /// unbound (dropped). The thread will continue to block for a maximum of
    /// `shutdown_timeout` time waiting for active connections to shut down.
    /// Once the `shutdown_timeout` elapses or all active connections are
    /// cleaned out then this method will return.
    pub fn run_until<F>(self, shutdown_signal: F) -> ::Result<()>
        where F: Future<Item = (), Error = ()>,
    {
        let Server { protocol, new_service, mut reactor, listener, shutdown_timeout } = self;

        let handle = reactor.handle();

        let mut incoming = AddrIncoming::new(listener, handle.clone(), protocol.sleep_on_errors)?;

        if protocol.keep_alive {
            incoming.set_keepalive(Some(Duration::from_secs(90)));
        }

        date_render_interval(&handle);

        // Mini future to track the number of active services
        let info = Rc::new(RefCell::new(Info {
            active: 0,
            blocker: None,
        }));

        // Future for our server's execution
        let srv = incoming.for_each(|socket| {
            let addr = socket.remote_addr;
            debug!("accepted new connection ({})", addr);

            let addr_service = SocketAddrService::new(addr, new_service.new_service()?);
            let s = NotifyService {
                inner: addr_service,
                info: Rc::downgrade(&info),
            };
            info.borrow_mut().active += 1;
            let fut = protocol.serve_connection(socket, s)
                .map(|_| ())
                .map_err(move |err| error!("server connection error: ({}) {}", addr, err));
            handle.spawn(fut);
            Ok(())
        });

        // for now, we don't care if the shutdown signal succeeds or errors
        // as long as it resolves, we will shutdown.
        let shutdown_signal = shutdown_signal.then(|_| Ok(()));

        // Main execution of the server. Here we use `select` to wait for either
        // `incoming` or `f` to resolve. We know that `incoming` will never
        // resolve with a success (it's infinite) so we're actually just waiting
        // for an error or for `f`, our shutdown signal.
        //
        // When we get a shutdown signal (`Ok`) then we drop the TCP listener to
        // stop accepting incoming connections.
        match reactor.run(shutdown_signal.select(srv)) {
            Ok(((), _incoming)) => {}
            Err((e, _other)) => return Err(e.into()),
        }

        // Ok we've stopped accepting new connections at this point, but we want
        // to give existing connections a chance to clear themselves out. Wait
        // at most `shutdown_timeout` time before we just return clearing
        // everything out.
        //
        // Our custom `WaitUntilZero` will resolve once all services constructed
        // here have been destroyed.
        let timeout = try!(Timeout::new(shutdown_timeout, &handle));
        let wait = WaitUntilZero { info: info.clone() };
        match reactor.run(wait.select(timeout)) {
            Ok(_) => Ok(()),
            Err((e, _)) => Err(e.into())
        }
    }
}


impl<S, B> Server<S, B>
    where S: NewService<Request = Request, Response = Response<B>, Error = ::Error> + Send + Sync + 'static,
          B: Stream<Error=::Error> + 'static,
          B::Item: AsRef<[u8]>,
{
    /// Run the server on multiple threads.
    #[cfg(unix)]
    pub fn run_threads(self, threads: usize) {
        assert!(threads > 0, "threads must be more than 0");

        let Server {
            protocol,
            new_service,
            reactor,
            listener,
            shutdown_timeout,
        } = self;

        let new_service = Arc::new(new_service);
        let addr = listener.local_addr().unwrap();

        let threads = (1..threads).map(|i| {
            let protocol = protocol.clone();
            let new_service = new_service.clone();
            thread::Builder::new()
                .name(format!("hyper-server-thread-{}", i))
                .spawn(move || {
                    let reactor = Core::new().unwrap();
                    let listener = thread_listener(&addr, &reactor.handle()).unwrap();
                    let srv = Server {
                        protocol,
                        new_service,
                        reactor,
                        listener,
                        shutdown_timeout,
                    };
                    srv.run().unwrap();
                })
                .unwrap()
        }).collect::<Vec<_>>();

        let srv = Server {
            protocol,
            new_service,
            reactor,
            listener,
            shutdown_timeout,
        };
        srv.run().unwrap();

        for thread in threads {
            thread.join().unwrap();
        }
    }
}

fn date_render_interval(handle: &Handle) {
    // Since we own the executor, we can spawn an interval to update the
    // thread_local rendered date, instead of checking the clock on every
    // single response.
    let mut date_interval = match Interval::new(Duration::from_secs(1), &handle) {
        Ok(i) => i,
        Err(e) => {
            trace!("error spawning date rendering interval: {}", e);
            // It'd be quite weird to error, but if it does, we
            // don't actually need it, so just back out.
            return;
        }
    };

    let on_drop = IntervalDrop;

    let fut =
        future::poll_fn(move || {
            try_ready!(date_interval.poll().map_err(|_| ()));
            // If here, we were ready!
            proto::date::update_interval();
            // However, to prevent Interval from needing to clone its Task
            // and check Instant::now() *again*, we just return NotReady
            // always.
            //
            // The interval has already rescheduled itself, so it's a waste
            // to poll the interval until it reports NotReady...
            Ok(Async::NotReady)
        })
        .then(move |_: Result<(), ()>| {
            // if this interval is ever dropped, the thread_local should be
            // updated to know about that. Otherwise, starting a server on a
            // thread, and then later closing it and then serving connections
            // without a Server would mean the date would never be updated
            // again.
            //
            // I know, I know, that'd be a super odd thing to do. But, just
            // being careful...
            drop(on_drop);
            Ok(())
        });

    handle.spawn(fut);

    struct IntervalDrop;

    impl Drop for IntervalDrop {
        fn drop(&mut self) {
            proto::date::interval_off();
        }
    }
}

fn thread_listener(addr: &SocketAddr, handle: &Handle) -> io::Result<TcpListener> {
    let listener = match *addr {
        SocketAddr::V4(_) => net2::TcpBuilder::new_v4()?,
        SocketAddr::V6(_) => net2::TcpBuilder::new_v6()?,
    };
    reuse_port(&listener);
    listener.reuse_address(true)?;
    listener.bind(addr)?;
    listener.listen(1024).and_then(|l| {
        TcpListener::from_listener(l, addr, handle)
    })
}

#[cfg(unix)]
fn reuse_port(tcp: &net2::TcpBuilder) {
    use net2::unix::*;
    if let Err(e) = tcp.reuse_port(true) {
        debug!("error setting SO_REUSEPORT: {}", e);
    }
}

#[cfg(not(unix))]
fn reuse_port(_tcp: &net2::TcpBuilder) {
}

impl<S: fmt::Debug, B: Stream<Error=::Error>> fmt::Debug for Server<S, B>
where B::Item: AsRef<[u8]>
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Server")
         .field("reactor", &"...")
         .field("listener", &self.listener)
         .field("new_service", &self.new_service)
         .field("protocol", &self.protocol)
         .finish()
    }
}

// ===== impl Serve =====

pub trait RemoteAddr {
    fn remote(&self) -> SocketAddr;
}
pub trait HasRemoteAddr {
    fn remote_addr(&mut self, addr: SocketAddr);
}

impl<I, S> Serve<I, S> {
    /*
    /// Spawn all incoming connections onto the provide executor.
    pub fn spawn_all<E>(self, executor: E) -> SpawnAll<I, S, E> {
        SpawnAll {
            executor: executor,
            serve: self,
        }
    }
    */

    /// Get a reference to the incoming stream.
    #[inline]
    pub fn incoming_ref(&self) -> &I {
        &self.incoming
    }
}

impl<I, S, B, SI> Stream for Serve<I, S>
where
    I: Stream<Error=io::Error>,
    I::Item: AsyncRead + AsyncWrite + RemoteAddr,
    S: NewService<Request=Request, Response=Response<B>, Error=::Error, Instance=SI>,
    SI: HasRemoteAddr + Service<Request=Request, Response=Response<B>, Error=::Error>,
    B: Stream<Error=::Error>,
    B::Item: AsRef<[u8]>,
{
    type Item = Connection<I::Item, S::Instance>;
    type Error = ::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if let Some(io) = try_ready!(self.incoming.poll()) {
            let mut service = self.new_service.new_service()?;
            service.remote_addr(io.remote());
            Ok(Async::Ready(Some(self.protocol.serve_connection(io, service))))
        } else {
            Ok(Async::Ready(None))
        }
    }
}

// ===== impl SpawnAll =====

/*
impl<I, S, E> Future for SpawnAll<I, S, E>
where
    I: Stream<Error=io::Error>,
    I::Item: AsyncRead + AsyncWrite,
    S: NewService<Request=Request, Response=Response<B>, Error=::Error>,
    B: Stream<Error=::Error>,
    B::Item: AsRef<[u8]>,
    //E: Executor<Connection<I::Item, S::Instance>>,
{
    type Item = ();
    type Error = ::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            if let Some(conn) = try_ready!(self.serve.poll()) {
                let fut = conn
                    .map(|_| ())
                    .map_err(|err| debug!("conn error: {}", err));
                match self.executor.execute(fut) {
                    Ok(()) => (),
                    Err(err) => match err.kind() {
                        ExecuteErrorKind::NoCapacity => {
                            debug!("SpawnAll::poll; executor no capacity");
                            // continue loop
                        },
                        ExecuteErrorKind::Shutdown | _ => {
                            debug!("SpawnAll::poll; executor shutdown");
                            return Ok(Async::Ready(()))
                        }
                    }
                }
            } else {
                return Ok(Async::Ready(()))
            }
        }
    }
}
*/

// ===== impl AddrIncoming =====

impl AddrIncoming {
    fn new(listener: TcpListener, handle: Handle, sleep_on_errors: bool) -> io::Result<AddrIncoming> {
         Ok(AddrIncoming {
            addr: listener.local_addr()?,
            keep_alive_timeout: None,
            listener: listener,
            handle: handle,
            sleep_on_errors: sleep_on_errors,
            timeout: None,
        })
    }

    /// Get the local address bound to this listener.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    fn set_keepalive(&mut self, dur: Option<Duration>) {
        self.keep_alive_timeout = dur;
    }
}

impl Stream for AddrIncoming {
    // currently unnameable...
    type Item = AddrStream;
    type Error = ::std::io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        // Check if a previous timeout is active that was set by IO errors.
        if let Some(ref mut to) = self.timeout {
            match to.poll().expect("timeout never fails") {
                Async::Ready(_) => {}
                Async::NotReady => return Ok(Async::NotReady),
            }
        }
        self.timeout = None;
        loop {
            match self.listener.accept() {
                Ok((socket, addr)) => {
                    if let Some(dur) = self.keep_alive_timeout {
                        if let Err(e) = socket.set_keepalive(Some(dur)) {
                            trace!("error trying to set TCP keepalive: {}", e);
                        }
                    }
                    return Ok(Async::Ready(Some(AddrStream::new(socket, addr))));
                },
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(Async::NotReady),
                Err(ref e) if self.sleep_on_errors => {
                    // Connection errors can be ignored directly, continue by
                    // accepting the next request.
                    if connection_error(e) {
                        continue;
                    }
                    // Sleep 10ms.
                    let delay = ::std::time::Duration::from_millis(10);
                    debug!("accept error: {}; sleeping {:?}",
                        e, delay);
                    let mut timeout = Timeout::new(delay, &self.handle)
                        .expect("can always set a timeout");
                    let result = timeout.poll()
                        .expect("timeout never fails");
                    match result {
                        Async::Ready(()) => continue,
                        Async::NotReady => {
                            self.timeout = Some(timeout);
                            return Ok(Async::NotReady);
                        }
                    }
                },
                Err(e) => return Err(e),
            }
        }
    }
}

/// This function defines errors that are per-connection. Which basically
/// means that if we get this error from `accept()` system call it means
/// next connection might be ready to be accepted.
///
/// All other errors will incur a timeout before next `accept()` is performed.
/// The timeout is useful to handle resource exhaustion errors like ENFILE
/// and EMFILE. Otherwise, could enter into tight loop.
fn connection_error(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::ConnectionRefused ||
    e.kind() == io::ErrorKind::ConnectionAborted ||
    e.kind() == io::ErrorKind::ConnectionReset
}

mod addr_stream {
    use std::io::{self, Read, Write};
    use std::net::SocketAddr;
    use bytes::{Buf, BufMut};
    use futures::Poll;
    use tokio::net::TcpStream;
    use tokio_io::{AsyncRead, AsyncWrite};
    use super::RemoteAddr;

    #[derive(Debug)]
    pub struct AddrStream {
        inner: TcpStream,
        pub(super) remote_addr: SocketAddr,
    }

    impl AddrStream {
        pub(super) fn new(tcp: TcpStream, addr: SocketAddr) -> AddrStream {
            AddrStream {
                inner: tcp,
                remote_addr: addr,
            }
        }
    }

    impl RemoteAddr for AddrStream {
        fn remote(&self) -> SocketAddr {
            self.remote_addr
        }
    }

    impl Read for AddrStream {
        #[inline]
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.inner.read(buf)
        }
    }

    impl Write for AddrStream {
        #[inline]
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inner.write(buf)
        }

        #[inline]
        fn flush(&mut self ) -> io::Result<()> {
            self.inner.flush()
        }
    }

    impl AsyncRead for AddrStream {
        #[inline]
        unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
            self.inner.prepare_uninitialized_buffer(buf)
        }

        #[inline]
        fn read_buf<B: BufMut>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
            self.inner.read_buf(buf)
        }
    }

    impl AsyncWrite for AddrStream {
        #[inline]
        fn shutdown(&mut self) -> Poll<(), io::Error> {
            AsyncWrite::shutdown(&mut self.inner)
        }

        #[inline]
        fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
            self.inner.write_buf(buf)
        }
    }
}

// ===== SocketAddrService

// This is used from `Server::run`, which captures the remote address
// in this service, and then injects it into each `Request`.
struct SocketAddrService<S> {
    addr: SocketAddr,
    inner: S,
}

impl<S> SocketAddrService<S> {
    fn new(addr: SocketAddr, service: S) -> SocketAddrService<S> {
        SocketAddrService {
            addr: addr,
            inner: service,
        }
    }
}

impl<S> Service for SocketAddrService<S>
where
    S: Service<Request=Request>,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn call(&self, mut req: Self::Request) -> Self::Future {
        proto::request::addr(&mut req, self.addr);
        self.inner.call(req)
    }
}

// ===== NotifyService =====

struct NotifyService<S> {
    inner: S,
    info: Weak<RefCell<Info>>,
}

struct WaitUntilZero {
    info: Rc<RefCell<Info>>,
}

struct Info {
    active: usize,
    blocker: Option<Task>,
}

impl<S: Service> Service for NotifyService<S> {
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn call(&self, message: Self::Request) -> Self::Future {
        self.inner.call(message)
    }
}

impl<S> Drop for NotifyService<S> {
    fn drop(&mut self) {
        let info = match self.info.upgrade() {
            Some(info) => info,
            None => return,
        };
        let mut info = info.borrow_mut();
        info.active -= 1;
        if info.active == 0 {
            if let Some(task) = info.blocker.take() {
                task.notify();
            }
        }
    }
}

impl Future for WaitUntilZero {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        let mut info = self.info.borrow_mut();
        if info.active == 0 {
            Ok(().into())
        } else {
            info.blocker = Some(task::current());
            Ok(Async::NotReady)
        }
    }
}

mod hyper_service {
    use super::{Request, Response, Service, Stream};
    /// A "trait alias" for any type that implements `Service` with hyper's
    /// Request, Response, and Error types, and a streaming body.
    ///
    /// There is an auto implementation inside hyper, so no one can actually
    /// implement this trait. It simply exists to reduce the amount of generics
    /// needed.
    pub trait HyperService: Service + Sealed {
        #[doc(hidden)]
        type ResponseBody;
        #[doc(hidden)]
        type Sealed: Sealed2;
    }

    pub trait Sealed {}
    pub trait Sealed2 {}

    #[allow(missing_debug_implementations)]
    pub struct Opaque {
        _inner: (),
    }

    impl Sealed2 for Opaque {}

    impl<S, B> Sealed for S
    where
        S: Service<
            Request=Request,
            Response=Response<B>,
            Error=::Error,
        >,
        B: Stream<Error=::Error>,
        B::Item: AsRef<[u8]>,
    {}

    impl<S, B> HyperService for S
    where
        S: Service<
            Request=Request,
            Response=Response<B>,
            Error=::Error,
        >,
        S: Sealed,
        B: Stream<Error=::Error>,
        B::Item: AsRef<[u8]>,
    {
        type ResponseBody = B;
        type Sealed = Opaque;
    }
}
