//! Lower-level Server connection API.
//!
//! The types in this module are to provide a lower-level API based around a
//! single connection. Accepting a connection and binding it with a service
//! are not handled at this level. This module provides the building blocks to
//! customize those things externally.
//!
//! If you don't have need to manage connections yourself, consider using the
//! higher-level [Server](super) API.

use std::error::Error as StdError;
use std::fmt;
use std::mem;
#[cfg(feature = "runtime")] use std::net::SocketAddr;
use std::sync::Arc;
#[cfg(feature = "runtime")] use std::time::Duration;

use bytes::Bytes;
use futures_core::Stream;
use h2;
use pin_utils::{unsafe_pinned, unsafe_unpinned};
use tokio_io::{AsyncRead, AsyncWrite};
#[cfg(feature = "runtime")] use tokio_reactor::Handle;

use crate::body::{Body, Payload};
use crate::common::exec::{Exec, H2Exec, NewSvcExec};
use crate::common::io::Rewind;
use crate::common::{Future, Pin, Poll, Unpin, task};
use crate::error::{Kind, Parse};
use crate::proto;
use crate::service::{MakeServiceRef, Service};
use crate::upgrade::Upgraded;

pub(super) use self::spawn_all::NoopWatcher;
use self::spawn_all::NewSvcTask;
pub(super) use self::spawn_all::Watcher;
pub(super) use self::upgrades::UpgradeableConnection;

#[cfg(feature = "runtime")] pub use super::tcp::{AddrIncoming, AddrStream};

/// A lower-level configuration of the HTTP protocol.
///
/// This structure is used to configure options for an HTTP server connection.
///
/// If you don't have need to manage connections yourself, consider using the
/// higher-level [Server](super) API.
#[derive(Clone, Debug)]
pub struct Http<E = Exec> {
    exec: E,
    h1_half_close: bool,
    h1_writev: bool,
    h2_builder: h2::server::Builder,
    mode: ConnectionMode,
    keep_alive: bool,
    max_buf_size: Option<usize>,
    pipeline_flush: bool,
}

/// The internal mode of HTTP protocol which indicates the behavior when a parse error occurs.
#[derive(Clone, Debug, PartialEq)]
enum ConnectionMode {
    /// Always use HTTP/1 and do not upgrade when a parse error occurs.
    H1Only,
    /// Always use HTTP/2.
    H2Only,
    /// Use HTTP/1 and try to upgrade to h2 when a parse error occurs.
    Fallback,
}

/// A stream mapping incoming IOs to new services.
///
/// Yields `Connecting`s that are futures that should be put on a reactor.
#[must_use = "streams do nothing unless polled"]
#[derive(Debug)]
pub struct Serve<I, S, E = Exec> {
    incoming: I,
    make_service: S,
    protocol: Http<E>,
}

/// A future building a new `Service` to a `Connection`.
///
/// Wraps the future returned from `MakeService` into one that returns
/// a `Connection`.
#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub struct Connecting<I, F, E = Exec> {
    future: F,
    io: Option<I>,
    protocol: Http<E>,
}

#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub(super) struct SpawnAll<I, S, E> {
    pub(super) serve: Serve<I, S, E>,
}

/// A future binding a connection with a Service.
///
/// Polling this future will drive HTTP forward.
#[must_use = "futures do nothing unless polled"]
pub struct Connection<T, S, E = Exec>
where
    S: Service,
{
    pub(super) conn: Option<Either<
        proto::h1::Dispatcher<
            proto::h1::dispatch::Server<S>,
            S::ResBody,
            T,
            proto::ServerTransaction,
        >,
        proto::h2::Server<
            Rewind<T>,
            S,
            S::ResBody,
            E,
        >,
    >>,
    fallback: Fallback<E>,
}

pub(super) enum Either<A, B> {
    A(A),
    B(B),
}

#[derive(Clone, Debug)]
enum Fallback<E> {
    ToHttp2(h2::server::Builder, E),
    Http1Only,
}

impl<E> Fallback<E> {
    fn to_h2(&self) -> bool {
        match *self {
            Fallback::ToHttp2(..) => true,
            Fallback::Http1Only => false,
        }
    }
}

impl<E> Unpin for Fallback<E> {}

/// Deconstructed parts of a `Connection`.
///
/// This allows taking apart a `Connection` at a later time, in order to
/// reclaim the IO object, and additional related pieces.
#[derive(Debug)]
pub struct Parts<T, S>  {
    /// The original IO object used in the handshake.
    pub io: T,
    /// A buffer of bytes that have been read but not processed as HTTP.
    ///
    /// If the client sent additional bytes after its last request, and
    /// this connection "ended" with an upgrade, the read buffer will contain
    /// those bytes.
    ///
    /// You will want to check for any existing bytes if you plan to continue
    /// communicating on the IO object.
    pub read_buf: Bytes,
    /// The `Service` used to serve this connection.
    pub service: S,
    _inner: (),
}

// ===== impl Http =====

impl Http {
    /// Creates a new instance of the HTTP protocol, ready to spawn a server or
    /// start accepting connections.
    pub fn new() -> Http {
        Http {
            exec: Exec::Default,
            h1_half_close: true,
            h1_writev: true,
            h2_builder: h2::server::Builder::default(),
            mode: ConnectionMode::Fallback,
            keep_alive: true,
            max_buf_size: None,
            pipeline_flush: false,
        }
    }
}

impl<E> Http<E> {
    /// Sets whether HTTP1 is required.
    ///
    /// Default is false
    pub fn http1_only(&mut self, val: bool) -> &mut Self {
        if val {
            self.mode = ConnectionMode::H1Only;
        } else {
            self.mode = ConnectionMode::Fallback;
        }
        self
    }

    /// Set whether HTTP/1 connections should support half-closures.
    ///
    /// Clients can chose to shutdown their write-side while waiting
    /// for the server to respond. Setting this to `false` will
    /// automatically close any connection immediately if `read`
    /// detects an EOF.
    ///
    /// Default is `true`.
    #[inline]
    pub fn http1_half_close(&mut self, val: bool) -> &mut Self {
        self.h1_half_close = val;
        self
    }

    /// Set whether HTTP/1 connections should try to use vectored writes,
    /// or always flatten into a single buffer.
    ///
    /// Note that setting this to false may mean more copies of body data,
    /// but may also improve performance when an IO transport doesn't
    /// support vectored writes well, such as most TLS implementations.
    ///
    /// Default is `true`.
    #[inline]
    pub fn http1_writev(&mut self, val: bool) -> &mut Self {
        self.h1_writev = val;
        self
    }

    /// Sets whether HTTP2 is required.
    ///
    /// Default is false
    pub fn http2_only(&mut self, val: bool) -> &mut Self {
        if val {
            self.mode = ConnectionMode::H2Only;
        } else {
            self.mode = ConnectionMode::Fallback;
        }
        self
    }

    /// Sets the [`SETTINGS_INITIAL_WINDOW_SIZE`][spec] option for HTTP2
    /// stream-level flow control.
    ///
    /// Default is 65,535
    ///
    /// [spec]: https://http2.github.io/http2-spec/#SETTINGS_INITIAL_WINDOW_SIZE
    pub fn http2_initial_stream_window_size(&mut self, sz: impl Into<Option<u32>>) -> &mut Self {
        if let Some(sz) = sz.into() {
            self.h2_builder.initial_window_size(sz);
        }
        self
    }

    /// Sets the max connection-level flow control for HTTP2
    ///
    /// Default is 65,535
    pub fn http2_initial_connection_window_size(&mut self, sz: impl Into<Option<u32>>) -> &mut Self {
        if let Some(sz) = sz.into() {
            self.h2_builder.initial_connection_window_size(sz);
        }
        self
    }

    /// Sets the [`SETTINGS_MAX_CONCURRENT_STREAMS`][spec] option for HTTP2
    /// connections.
    ///
    /// Default is no limit (`None`).
    ///
    /// [spec]: https://http2.github.io/http2-spec/#SETTINGS_MAX_CONCURRENT_STREAMS
    pub fn http2_max_concurrent_streams(&mut self, max: impl Into<Option<u32>>) -> &mut Self {
        if let Some(max) = max.into() {
            self.h2_builder.max_concurrent_streams(max);
        }
        self
    }

    /// Enables or disables HTTP keep-alive.
    ///
    /// Default is true.
    pub fn keep_alive(&mut self, val: bool) -> &mut Self {
        self.keep_alive = val;
        self
    }

    /// Set the maximum buffer size for the connection.
    ///
    /// Default is ~400kb.
    ///
    /// # Panics
    ///
    /// The minimum value allowed is 8192. This method panics if the passed `max` is less than the minimum.
    pub fn max_buf_size(&mut self, max: usize) -> &mut Self {
        assert!(
            max >= proto::h1::MINIMUM_MAX_BUFFER_SIZE,
            "the max_buf_size cannot be smaller than the minimum that h1 specifies."
        );
        self.max_buf_size = Some(max);
        self
    }

    /// Aggregates flushes to better support pipelined responses.
    ///
    /// Experimental, may have bugs.
    ///
    /// Default is false.
    pub fn pipeline_flush(&mut self, enabled: bool) -> &mut Self {
        self.pipeline_flush = enabled;
        self
    }

    /// Set the executor used to spawn background tasks.
    ///
    /// Default uses implicit default (like `tokio::spawn`).
    pub fn with_executor<E2>(self, exec: E2) -> Http<E2> {
        Http {
            exec,
            h1_half_close: self.h1_half_close,
            h1_writev: self.h1_writev,
            h2_builder: self.h2_builder,
            mode: self.mode,
            keep_alive: self.keep_alive,
            max_buf_size: self.max_buf_size,
            pipeline_flush: self.pipeline_flush,
        }
    }

    /// Bind a connection together with a [`Service`](::service::Service).
    ///
    /// This returns a Future that must be polled in order for HTTP to be
    /// driven on the connection.
    ///
    /// # Example
    ///
    /// ```
    /// # #![feature(async_await)]
    /// # use hyper::{Body, Request, Response};
    /// # use hyper::service::Service;
    /// # use hyper::server::conn::Http;
    /// # #[cfg(feature = "runtime")]
    /// # use tokio_io::{AsyncRead, AsyncWrite};
    /// # #[cfg(feature = "runtime")]
    /// # async fn run<I, S>(some_io: I, some_service: S)
    /// # where
    /// #     I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    /// #     S: Service<ReqBody=Body, ResBody=Body> + Send + 'static,
    /// #     S::Future: Send
    /// # {
    /// let http = Http::new();
    /// let conn = http.serve_connection(some_io, some_service);
    ///
    /// if let Err(e) = conn.await {
    ///     eprintln!("server connection error: {}", e);
    /// }
    /// # }
    /// # fn main() {}
    /// ```
    pub fn serve_connection<S, I, Bd>(&self, io: I, service: S) -> Connection<I, S, E>
    where
        S: Service<ReqBody=Body, ResBody=Bd>,
        S::Error: Into<Box<dyn StdError + Send + Sync>>,
        Bd: Payload,
        I: AsyncRead + AsyncWrite + Unpin,
        E: H2Exec<S::Future, Bd>,
    {
        let either = match self.mode {
            ConnectionMode::H1Only | ConnectionMode::Fallback => {
                let mut conn = proto::Conn::new(io);
                if !self.keep_alive {
                    conn.disable_keep_alive();
                }
                if !self.h1_half_close {
                    conn.set_disable_half_close();
                }
                if !self.h1_writev {
                    conn.set_write_strategy_flatten();
                }
                conn.set_flush_pipeline(self.pipeline_flush);
                if let Some(max) = self.max_buf_size {
                    conn.set_max_buf_size(max);
                }
                let sd = proto::h1::dispatch::Server::new(service);
                Either::A(proto::h1::Dispatcher::new(sd, conn))
            }
            ConnectionMode::H2Only => {
                let rewind_io = Rewind::new(io);
                let h2 = proto::h2::Server::new(rewind_io, service, &self.h2_builder, self.exec.clone());
                Either::B(h2)
            }
        };

        Connection {
            conn: Some(either),
            fallback: if self.mode == ConnectionMode::Fallback {
                Fallback::ToHttp2(self.h2_builder.clone(), self.exec.clone())
            } else {
                Fallback::Http1Only
            },
        }
    }

    /// Bind the provided `addr` with the default `Handle` and return [`Serve`](Serve).
    ///
    /// This method will bind the `addr` provided with a new TCP listener ready
    /// to accept connections. Each connection will be processed with the
    /// `make_service` object provided, creating a new service per
    /// connection.
    #[cfg(feature = "runtime")]
    pub fn serve_addr<S, Bd>(&self, addr: &SocketAddr, make_service: S) -> crate::Result<Serve<AddrIncoming, S, E>>
    where
        S: MakeServiceRef<
            AddrStream,
            ReqBody=Body,
            ResBody=Bd,
        >,
        S::Error: Into<Box<dyn StdError + Send + Sync>>,
        Bd: Payload,
        E: H2Exec<<S::Service as Service>::Future, Bd>,
    {
        let mut incoming = AddrIncoming::new(addr, None)?;
        if self.keep_alive {
            incoming.set_keepalive(Some(Duration::from_secs(90)));
        }
        Ok(self.serve_incoming(incoming, make_service))
    }

    /// Bind the provided `addr` with the `Handle` and return a [`Serve`](Serve)
    ///
    /// This method will bind the `addr` provided with a new TCP listener ready
    /// to accept connections. Each connection will be processed with the
    /// `make_service` object provided, creating a new service per
    /// connection.
    #[cfg(feature = "runtime")]
    pub fn serve_addr_handle<S, Bd>(&self, addr: &SocketAddr, handle: &Handle, make_service: S) -> crate::Result<Serve<AddrIncoming, S, E>>
    where
        S: MakeServiceRef<
            AddrStream,
            ReqBody=Body,
            ResBody=Bd,
        >,
        S::Error: Into<Box<dyn StdError + Send + Sync>>,
        Bd: Payload,
        E: H2Exec<<S::Service as Service>::Future, Bd>,
    {
        let mut incoming = AddrIncoming::new(addr, Some(handle))?;
        if self.keep_alive {
            incoming.set_keepalive(Some(Duration::from_secs(90)));
        }
        Ok(self.serve_incoming(incoming, make_service))
    }

    /// Bind the provided stream of incoming IO objects with a `MakeService`.
    pub fn serve_incoming<I, IO, IE, S, Bd>(&self, incoming: I, make_service: S) -> Serve<I, S, E>
    where
        I: Stream<Item = Result<IO, IE>>,
        IE: Into<Box<dyn StdError + Send + Sync>>,
        IO: AsyncRead + AsyncWrite + Unpin,
        S: MakeServiceRef<
            IO,
            ReqBody=Body,
            ResBody=Bd,
        >,
        S::Error: Into<Box<dyn StdError + Send + Sync>>,
        Bd: Payload,
        E: H2Exec<<S::Service as Service>::Future, Bd>,
    {
        Serve {
            incoming,
            make_service,
            protocol: self.clone(),
        }
    }
}


// ===== impl Connection =====

impl<I, B, S, E> Connection<I, S, E>
where
    S: Service<ReqBody=Body, ResBody=B>,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
    I: AsyncRead + AsyncWrite + Unpin,
    B: Payload + 'static,
    E: H2Exec<S::Future, B>,
{
    /// Start a graceful shutdown process for this connection.
    ///
    /// This `Connection` should continue to be polled until shutdown
    /// can finish.
    pub fn graceful_shutdown(self: Pin<&mut Self>) {
        // Safety: neither h1 nor h2 poll any of the generic futures
        // in these methods.
        match unsafe { self.get_unchecked_mut() }.conn.as_mut().unwrap() {
            Either::A(ref mut h1) => {
                h1.disable_keep_alive();
            },
            Either::B(ref mut h2) => {
                h2.graceful_shutdown();
            }
        }
    }

    /// Return the inner IO object, and additional information.
    ///
    /// If the IO object has been "rewound" the io will not contain those bytes rewound.
    /// This should only be called after `poll_without_shutdown` signals
    /// that the connection is "done". Otherwise, it may not have finished
    /// flushing all necessary HTTP bytes.
    ///
    /// # Panics
    /// This method will panic if this connection is using an h2 protocol.
    pub fn into_parts(self) -> Parts<I, S> {
        self.try_into_parts().unwrap_or_else(|| panic!("h2 cannot into_inner"))
    }

    /// Return the inner IO object, and additional information, if available.
    ///
    /// This method will return a `None` if this connection is using an h2 protocol.
    pub fn try_into_parts(self) -> Option<Parts<I, S>> {
        match self.conn.unwrap() {
            Either::A(h1) => {
                let (io, read_buf, dispatch) = h1.into_inner();
                Some(Parts {
                    io: io,
                    read_buf: read_buf,
                    service: dispatch.into_service(),
                    _inner: (),
                })
            },
            Either::B(_h2) => None,
        }
    }

    /// Poll the connection for completion, but without calling `shutdown`
    /// on the underlying IO.
    ///
    /// This is useful to allow running a connection while doing an HTTP
    /// upgrade. Once the upgrade is completed, the connection would be "done",
    /// but it is not desired to actally shutdown the IO object. Instead you
    /// would take it back using `into_parts`.
    ///
    /// Use [`poll_fn`](https://docs.rs/futures/0.1.25/futures/future/fn.poll_fn.html)
    /// and [`try_ready!`](https://docs.rs/futures/0.1.25/futures/macro.try_ready.html)
    /// to work with this function; or use the `without_shutdown` wrapper.
    pub fn poll_without_shutdown(&mut self, cx: &mut task::Context<'_>) -> Poll<crate::Result<()>>
    where
        S: Unpin,
        S::Future: Unpin,
        B: Unpin,
    {
        loop {
            let polled = match *self.conn.as_mut().unwrap() {
                Either::A(ref mut h1) => h1.poll_without_shutdown(cx),
                Either::B(ref mut h2) => unimplemented!("Connection::poll_without_shutdown h2"),//return h2.poll().map(|x| x.map(|_| ())),
            };
            match ready!(polled) {
                Ok(x) => return Poll::Ready(Ok(x)),
                Err(e) => {
                    match *e.kind() {
                        Kind::Parse(Parse::VersionH2) if self.fallback.to_h2() => {
                            self.upgrade_h2();
                            continue;
                        }
                        _ => return Poll::Ready(Err(e)),
                    }
                }
            }
        }
    }

    /// Prevent shutdown of the underlying IO object at the end of service the request,
    /// instead run `into_parts`. This is a convenience wrapper over `poll_without_shutdown`.
    pub fn without_shutdown(self) -> impl Future<Output=crate::Result<Parts<I, S>>>
    where
        S: Unpin,
        S::Future: Unpin,
        B: Unpin,
    {
        let mut conn = Some(self);
        futures_util::future::poll_fn(move |cx| {
            ready!(conn.as_mut().unwrap().poll_without_shutdown(cx))?;
            Poll::Ready(Ok(conn.take().unwrap().into_parts()))
        })
    }

    fn upgrade_h2(&mut self) {
        trace!("Trying to upgrade connection to h2");
        let conn = self.conn.take();

        let (io, read_buf, dispatch) = match conn.unwrap() {
            Either::A(h1) => {
                h1.into_inner()
            },
            Either::B(_h2) => {
                panic!("h2 cannot into_inner");
            }
        };
        let mut rewind_io = Rewind::new(io);
        rewind_io.rewind(read_buf);
        let (builder, exec) = match self.fallback {
            Fallback::ToHttp2(ref builder, ref exec) => (builder, exec),
            Fallback::Http1Only => unreachable!("upgrade_h2 with Fallback::Http1Only"),
        };
        let h2 = proto::h2::Server::new(
            rewind_io,
            dispatch.into_service(),
            builder,
            exec.clone(),
        );

        debug_assert!(self.conn.is_none());
        self.conn = Some(Either::B(h2));
    }

    /// Enable this connection to support higher-level HTTP upgrades.
    ///
    /// See [the `upgrade` module](::upgrade) for more.
    pub fn with_upgrades(self) -> UpgradeableConnection<I, S, E>
    where
        I: Send,
    {
        UpgradeableConnection {
            inner: self,
        }
    }
}

impl<I, B, S, E> Future for Connection<I, S, E>
where
    S: Service<ReqBody=Body, ResBody=B> + 'static,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
    I: AsyncRead + AsyncWrite + Unpin + 'static,
    B: Payload + 'static,
    E: H2Exec<S::Future, B>,
{
    type Output = crate::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        loop {
            match ready!(Pin::new(self.conn.as_mut().unwrap()).poll(cx)) {
                Ok(done) => {
                    if let proto::Dispatched::Upgrade(pending) = done {
                        // With no `Send` bound on `I`, we can't try to do
                        // upgrades here. In case a user was trying to use
                        // `Body::on_upgrade` with this API, send a special
                        // error letting them know about that.
                        pending.manual();
                    }
                    return Poll::Ready(Ok(()));
                },
                Err(e) => {
                    match *e.kind() {
                        Kind::Parse(Parse::VersionH2) if self.fallback.to_h2() => {
                            self.upgrade_h2();
                            continue;
                        }
                        _ => return Poll::Ready(Err(e)),
                    }
                }
            }
        }
    }
}

impl<I, S> fmt::Debug for Connection<I, S>
where
    S: Service,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Connection")
            .finish()
    }
}
// ===== impl Serve =====

impl<I, S, E> Serve<I, S, E> {
    unsafe_pinned!(incoming: I);
    unsafe_unpinned!(make_service: S);

    /// Spawn all incoming connections onto the executor in `Http`.
    pub(super) fn spawn_all(self) -> SpawnAll<I, S, E> {
        SpawnAll {
            serve: self,
        }
    }

    /// Get a reference to the incoming stream.
    #[inline]
    pub fn incoming_ref(&self) -> &I {
        &self.incoming
    }

    /// Get a mutable reference to the incoming stream.
    #[inline]
    pub fn incoming_mut(&mut self) -> &mut I {
        &mut self.incoming
    }
}

impl<I, IO, IE, S, B, E> Stream for Serve<I, S, E>
where
    I: Stream<Item = Result<IO, IE>>,
    IO: AsyncRead + AsyncWrite + Unpin,
    IE: Into<Box<dyn StdError + Send + Sync>>,
    S: MakeServiceRef<IO, ReqBody=Body, ResBody=B>,
    //S::Error2: Into<Box<StdError + Send + Sync>>,
    //SME: Into<Box<StdError + Send + Sync>>,
    B: Payload,
    E: H2Exec<<S::Service as Service>::Future, B>,
{
    type Item = crate::Result<Connecting<IO, S::Future, E>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        match ready!(self.as_mut().make_service().poll_ready_ref(cx)) {
            Ok(()) => (),
            Err(e) => {
                trace!("make_service closed");
                return Poll::Ready(Some(Err(crate::Error::new_user_make_service(e))));
            }
        }

        if let Some(item) = ready!(self.as_mut().incoming().poll_next(cx)) {
            let io = item.map_err(crate::Error::new_accept)?;
            let new_fut = self.as_mut().make_service().make_service_ref(&io);
            Poll::Ready(Some(Ok(Connecting {
                future: new_fut,
                io: Some(io),
                protocol: self.protocol.clone(),
            })))
        } else {
            Poll::Ready(None)
        }
    }
}

// ===== impl Connecting =====

impl<I, F, E> Connecting<I, F, E> {
    unsafe_pinned!(future: F);
    unsafe_unpinned!(io: Option<I>);
}

impl<I, F, S, FE, E, B> Future for Connecting<I, F, E>
where
    I: AsyncRead + AsyncWrite + Unpin,
    F: Future<Output=Result<S, FE>>,
    S: Service<ReqBody=Body, ResBody=B>,
    B: Payload,
    E: H2Exec<S::Future, B>,
{
    type Output = Result<Connection<I, S, E>, FE>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let service = ready!(self.as_mut().future().poll(cx))?;
        let io = self.as_mut().io().take().expect("polled after complete");
        Poll::Ready(Ok(self.protocol.serve_connection(io, service)))
    }
}

// ===== impl SpawnAll =====

#[cfg(feature = "runtime")]
impl<S, E> SpawnAll<AddrIncoming, S, E> {
    pub(super) fn local_addr(&self) -> SocketAddr {
        self.serve.incoming.local_addr()
    }
}

impl<I, S, E> SpawnAll<I, S, E> {
    pub(super) fn incoming_ref(&self) -> &I {
        self.serve.incoming_ref()
    }
}

impl<I, IO, IE, S, B, E> SpawnAll<I, S, E>
where
    I: Stream<Item=Result<IO, IE>>,
    IE: Into<Box<dyn StdError + Send + Sync>>,
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: MakeServiceRef<
        IO,
        ReqBody=Body,
        ResBody=B,
    >,
    B: Payload,
    E: H2Exec<<S::Service as Service>::Future, B>,
{
    pub(super) fn poll_watch<W>(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>, watcher: &W) -> Poll<crate::Result<()>>
    where
        E: NewSvcExec<IO, S::Future, S::Service, E, W>,
        W: Watcher<IO, S::Service, E>,
    {
        // Safety: futures are never moved... lolwtf
        let me = unsafe { self.get_unchecked_mut() };
        loop {
            if let Some(connecting) = ready!(unsafe { Pin::new_unchecked(&mut me.serve) }.poll_next(cx)?) {
                let fut = NewSvcTask::new(connecting, watcher.clone());
                me.serve.protocol.exec.execute_new_svc(fut)?;
            } else {
                return Poll::Ready(Ok(()));
            }
        }
    }
}


impl<A, B> Future for Either<A, B>
where
    A: Future,
    B: Future<Output=A::Output>,
{
    type Output = A::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        // Just simple pin projection to the inner variants
        unsafe {
            match self.get_unchecked_mut() {
                Either::A(a) => Pin::new_unchecked(a).poll(cx),
                Either::B(b) => Pin::new_unchecked(b).poll(cx),
            }
        }
    }
}

pub(crate) mod spawn_all {
    use std::error::Error as StdError;
    use tokio_io::{AsyncRead, AsyncWrite};

    use crate::body::{Body, Payload};
    use crate::common::exec::H2Exec;
    use crate::common::{Future, Pin, Poll, Unpin, task};
    use crate::service::Service;
    use super::{Connecting, UpgradeableConnection};

    // Used by `SpawnAll` to optionally watch a `Connection` future.
    //
    // The regular `hyper::Server` just uses a `NoopWatcher`, which does
    // not need to watch anything, and so returns the `Connection` untouched.
    //
    // The `Server::with_graceful_shutdown` needs to keep track of all active
    // connections, and signal that they start to shutdown when prompted, so
    // it has a `GracefulWatcher` implementation to do that.
    pub trait Watcher<I, S: Service, E>: Clone {
        type Future: Future<Output = crate::Result<()>>;

        fn watch(&self, conn: UpgradeableConnection<I, S, E>) -> Self::Future;
    }

    #[allow(missing_debug_implementations)]
    #[derive(Copy, Clone)]
    pub struct NoopWatcher;

    impl<I, S, E> Watcher<I, S, E> for NoopWatcher
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        S: Service<ReqBody=Body> + 'static,
        E: H2Exec<S::Future, S::ResBody>,
    {
        type Future = UpgradeableConnection<I, S, E>;

        fn watch(&self, conn: UpgradeableConnection<I, S, E>) -> Self::Future {
            conn
        }
    }

    // This is a `Future<Item=(), Error=()>` spawned to an `Executor` inside
    // the `SpawnAll`. By being a nameable type, we can be generic over the
    // user's `Service::Future`, and thus an `Executor` can execute it.
    //
    // Doing this allows for the server to conditionally require `Send` futures,
    // depending on the `Executor` configured.
    //
    // Users cannot import this type, nor the associated `NewSvcExec`. Instead,
    // a blanket implementation for `Executor<impl Future>` is sufficient.
    #[allow(missing_debug_implementations)]
    pub struct NewSvcTask<I, N, S: Service, E, W: Watcher<I, S, E>> {
        state: State<I, N, S, E, W>,
    }

    enum State<I, N, S: Service, E, W: Watcher<I, S, E>> {
        Connecting(Connecting<I, N, E>, W),
        Connected(W::Future),
    }

    impl<I, N, S: Service, E, W: Watcher<I, S, E>> NewSvcTask<I, N, S, E, W> {
        pub(super) fn new(connecting: Connecting<I, N, E>, watcher: W) -> Self {
            NewSvcTask {
                state: State::Connecting(connecting, watcher),
            }
        }
    }

    impl<I, N, S, NE, B, E, W> Future for NewSvcTask<I, N, S, E, W>
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        N: Future<Output=Result<S, NE>>,
        NE: Into<Box<dyn StdError + Send + Sync>>,
        S: Service<ReqBody=Body, ResBody=B>,
        B: Payload,
        E: H2Exec<S::Future, B>,
        W: Watcher<I, S, E>,
    {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
            // If it weren't for needing to name this type so the `Send` bounds
            // could be projected to the `Serve` executor, this could just be
            // an `async fn`, and much safer. Woe is me.

            let me = unsafe { self.get_unchecked_mut() };
            loop {
                let next = match me.state {
                    State::Connecting(ref mut connecting, ref watcher) => {
                        let res = ready!(unsafe { Pin::new_unchecked(connecting).poll(cx) });
                        let conn = match res {
                            Ok(conn) => conn,
                            Err(err) => {
                                let err = crate::Error::new_user_make_service(err);
                                debug!("connecting error: {}", err);
                                return Poll::Ready(());
                            }
                        };
                        let connected = watcher.watch(conn.with_upgrades());
                        State::Connected(connected)
                    },
                    State::Connected(ref mut future) => {
                        return unsafe { Pin::new_unchecked(future) }
                            .poll(cx)
                            .map(|res| {
                                if let Err(err) = res {
                                    debug!("connection error: {}", err);
                                }
                            });
                    }
                };

                me.state = next;
            }
        }
    }
}

mod upgrades {
    use super::*;

    // A future binding a connection with a Service with Upgrade support.
    //
    // This type is unnameable outside the crate, and so basically just an
    // `impl Future`, without requiring Rust 1.26.
    #[must_use = "futures do nothing unless polled"]
    #[allow(missing_debug_implementations)]
    pub struct UpgradeableConnection<T, S, E>
    where
        S: Service,
    {
        pub(super) inner: Connection<T, S, E>,
    }

    impl<I, B, S, E> UpgradeableConnection<I, S, E>
    where
        S: Service<ReqBody=Body, ResBody=B>,// + 'static,
        S::Error: Into<Box<dyn StdError + Send + Sync>>,
        I: AsyncRead + AsyncWrite + Unpin,
        B: Payload + 'static,
        E: H2Exec<S::Future, B>,
    {
        /// Start a graceful shutdown process for this connection.
        ///
        /// This `Connection` should continue to be polled until shutdown
        /// can finish.
        pub fn graceful_shutdown(mut self: Pin<&mut Self>) {
            Pin::new(&mut self.inner).graceful_shutdown()
        }
    }

    impl<I, B, S, E> Future for UpgradeableConnection<I, S, E>
    where
        S: Service<ReqBody=Body, ResBody=B> + 'static,
        S::Error: Into<Box<dyn StdError + Send + Sync>>,
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        B: Payload + 'static,
        E: super::H2Exec<S::Future, B>,
    {
        type Output = crate::Result<()>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
            loop {
                match ready!(Pin::new(self.inner.conn.as_mut().unwrap()).poll(cx)) {
                    Ok(proto::Dispatched::Shutdown) => return Poll::Ready(Ok(())),
                    Ok(proto::Dispatched::Upgrade(pending)) => {
                        let h1 = match mem::replace(&mut self.inner.conn, None) {
                            Some(Either::A(h1)) => h1,
                            _ => unreachable!("Upgrade expects h1"),
                        };

                        let (io, buf, _) = h1.into_inner();
                        pending.fulfill(Upgraded::new(Box::new(io), buf));
                        return Poll::Ready(Ok(()));
                    },
                    Err(e) => {
                        match *e.kind() {
                            Kind::Parse(Parse::VersionH2) if self.inner.fallback.to_h2() => {
                                self.inner.upgrade_h2();
                                continue;
                            }
                            _ => return Poll::Ready(Err(e)),
                        }
                    }
                }
            }
        }
    }
}

