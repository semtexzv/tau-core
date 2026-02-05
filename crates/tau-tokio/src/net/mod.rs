//! TCP/UDP/Unix networking types.
//!
//! Uses the tau IO reactor for proper epoll/kqueue-based async IO.

use crate::io::{AsyncRead, AsyncWrite, ReadBuf};
use std::io;
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::pin::Pin;
use std::task::{Context, Poll};
use tau::io as tio;
use tau::types::FfiWaker;

fn debug() -> bool {
    crate::debug_enabled()
}

// =============================================================================
// TcpStream
// =============================================================================

/// An async TCP stream backed by a non-blocking `std::net::TcpStream`.
/// Registered with the tau IO reactor for proper async wakeup.
pub struct TcpStream {
    inner: std::net::TcpStream,
    reactor_handle: u64,  // Handle from tau_io_register
}

impl TcpStream {
    pub async fn connect<A: std::net::ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let addrs: Vec<SocketAddr> = addr.to_socket_addrs()?.collect();
        if debug() {
            eprintln!("[tcp] TcpStream::connect addrs={:?}", addrs);
        }
        for addr in &addrs {
            match std::net::TcpStream::connect_timeout(addr, std::time::Duration::from_secs(30)) {
                Ok(stream) => {
                    stream.set_nonblocking(true)?;
                    if debug() {
                        eprintln!("[tcp] TcpStream::connect → Ok({:?})", addr);
                    }
                    return Self::from_std(stream);
                }
                Err(e) => {
                    if debug() {
                        eprintln!("[tcp] TcpStream::connect → Err({}) for {:?}", e, addr);
                    }
                    if addrs.len() == 1 {
                        return Err(e);
                    }
                    continue;
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "could not connect to any address",
        ))
    }

    pub fn from_std(stream: std::net::TcpStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        let fd = stream.as_raw_fd();
        
        // Register with the reactor for both read and write events
        let handle = tio::register(fd, tio::READABLE | tio::WRITABLE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "failed to register with reactor"))?;
        
        if debug() {
            eprintln!("[tcp] from_std fd={} reactor_handle={}", fd, handle);
        }
        
        Ok(TcpStream { 
            inner: stream,
            reactor_handle: handle,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.inner.peer_addr()
    }

    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        self.inner.set_nodelay(nodelay)
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        self.inner.nodelay()
    }

    pub fn into_split(self) -> (OwnedReadHalf, OwnedWriteHalf) {
        // Use ManuallyDrop to prevent Drop from running
        let this = std::mem::ManuallyDrop::new(self);
        // Safety: we're taking ownership and preventing the drop
        let inner = unsafe { std::ptr::read(&this.inner) };
        let handle = this.reactor_handle;
        let inner = std::sync::Arc::new(inner);
        (
            OwnedReadHalf {
                inner: inner.clone(),
                reactor_handle: handle,
                owns_registration: true,
            },
            OwnedWriteHalf { 
                inner,
                reactor_handle: handle,
                owns_registration: false,  // ReadHalf owns it
            },
        )
    }
    
    /// Convert context waker to FfiWaker for reactor
    fn make_ffi_waker(cx: &Context<'_>) -> FfiWaker {
        // We need to convert std::task::Waker to FfiWaker
        // The waker's wake function will be called by the reactor
        let waker = cx.waker().clone();
        let boxed = Box::new(waker);
        let data = Box::into_raw(boxed) as *mut ();
        
        extern "C" fn wake_fn(data: *mut ()) {
            let waker = unsafe { Box::from_raw(data as *mut std::task::Waker) };
            waker.wake();
        }
        
        FfiWaker {
            data,
            wake_fn: Some(wake_fn),
        }
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        if debug() {
            eprintln!("[tcp] drop TcpStream reactor_handle={}", self.reactor_handle);
        }
        tio::deregister(self.reactor_handle);
    }
}

impl AsyncRead for TcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        use std::io::Read;
        let this = unsafe { self.get_unchecked_mut() };
        
        // Check reactor readiness first
        let ffi_waker = Self::make_ffi_waker(cx);
        if !tio::poll_ready(this.reactor_handle, tio::DIR_READ, ffi_waker) {
            // Not ready, waker stored in reactor
            return Poll::Pending;
        }
        
        // Reactor says we're ready, try the read
        let dst = buf.initialize_unfilled();
        match (&this.inner).read(dst) {
            Ok(n) => {
                buf.advance(n);
                if debug() {
                    eprintln!("[tcp] poll_read → Ok({})", n);
                }
                Poll::Ready(Ok(()))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Clear readiness so reactor will wake us when actually ready
                tio::clear_ready(this.reactor_handle, tio::DIR_READ);
                if debug() {
                    eprintln!("[tcp] poll_read → WouldBlock, cleared readiness");
                }
                // Re-register waker
                let ffi_waker = Self::make_ffi_waker(cx);
                tio::poll_ready(this.reactor_handle, tio::DIR_READ, ffi_waker);
                Poll::Pending
            }
            Err(e) => {
                if debug() {
                    eprintln!("[tcp] poll_read → Err({})", e);
                }
                Poll::Ready(Err(e))
            }
        }
    }
}

impl AsyncWrite for TcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        use std::io::Write;
        let this = unsafe { self.get_unchecked_mut() };
        
        // Check reactor readiness first
        let ffi_waker = Self::make_ffi_waker(cx);
        if !tio::poll_ready(this.reactor_handle, tio::DIR_WRITE, ffi_waker) {
            return Poll::Pending;
        }
        
        match (&this.inner).write(buf) {
            Ok(n) => {
                if debug() {
                    eprintln!("[tcp] poll_write → Ok({})", n);
                }
                Poll::Ready(Ok(n))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                tio::clear_ready(this.reactor_handle, tio::DIR_WRITE);
                if debug() {
                    eprintln!("[tcp] poll_write → WouldBlock, cleared readiness");
                }
                let ffi_waker = Self::make_ffi_waker(cx);
                tio::poll_ready(this.reactor_handle, tio::DIR_WRITE, ffi_waker);
                Poll::Pending
            }
            Err(e) => {
                if debug() {
                    eprintln!("[tcp] poll_write → Err({})", e);
                }
                Poll::Ready(Err(e))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        use std::io::Write;
        let this = unsafe { self.get_unchecked_mut() };
        match (&this.inner).flush() {
            Ok(()) => Poll::Ready(Ok(())),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                let ffi_waker = Self::make_ffi_waker(cx);
                tio::poll_ready(this.reactor_handle, tio::DIR_WRITE, ffi_waker);
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = unsafe { self.get_unchecked_mut() };
        this.inner.shutdown(std::net::Shutdown::Write)?;
        Poll::Ready(Ok(()))
    }
}

// =============================================================================
// Split halves
// =============================================================================

pub struct OwnedReadHalf {
    inner: std::sync::Arc<std::net::TcpStream>,
    reactor_handle: u64,
    owns_registration: bool,
}

impl Drop for OwnedReadHalf {
    fn drop(&mut self) {
        if self.owns_registration {
            if debug() {
                eprintln!("[tcp] drop OwnedReadHalf reactor_handle={}", self.reactor_handle);
            }
            tio::deregister(self.reactor_handle);
        }
    }
}

impl OwnedReadHalf {
    fn make_ffi_waker(cx: &Context<'_>) -> FfiWaker {
        let waker = cx.waker().clone();
        let boxed = Box::new(waker);
        let data = Box::into_raw(boxed) as *mut ();
        
        extern "C" fn wake_fn(data: *mut ()) {
            let waker = unsafe { Box::from_raw(data as *mut std::task::Waker) };
            waker.wake();
        }
        
        FfiWaker {
            data,
            wake_fn: Some(wake_fn),
        }
    }
}

impl AsyncRead for OwnedReadHalf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        use std::io::Read;
        let this = self.get_mut();
        
        let ffi_waker = Self::make_ffi_waker(cx);
        if !tio::poll_ready(this.reactor_handle, tio::DIR_READ, ffi_waker) {
            return Poll::Pending;
        }
        
        let dst = buf.initialize_unfilled();
        match (&*this.inner).read(dst) {
            Ok(n) => {
                buf.advance(n);
                Poll::Ready(Ok(()))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                tio::clear_ready(this.reactor_handle, tio::DIR_READ);
                let ffi_waker = Self::make_ffi_waker(cx);
                tio::poll_ready(this.reactor_handle, tio::DIR_READ, ffi_waker);
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

pub struct OwnedWriteHalf {
    inner: std::sync::Arc<std::net::TcpStream>,
    reactor_handle: u64,
    owns_registration: bool,
}

impl Drop for OwnedWriteHalf {
    fn drop(&mut self) {
        if self.owns_registration {
            if debug() {
                eprintln!("[tcp] drop OwnedWriteHalf reactor_handle={}", self.reactor_handle);
            }
            tio::deregister(self.reactor_handle);
        }
    }
}

impl OwnedWriteHalf {
    fn make_ffi_waker(cx: &Context<'_>) -> FfiWaker {
        let waker = cx.waker().clone();
        let boxed = Box::new(waker);
        let data = Box::into_raw(boxed) as *mut ();
        
        extern "C" fn wake_fn(data: *mut ()) {
            let waker = unsafe { Box::from_raw(data as *mut std::task::Waker) };
            waker.wake();
        }
        
        FfiWaker {
            data,
            wake_fn: Some(wake_fn),
        }
    }
}

impl AsyncWrite for OwnedWriteHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        use std::io::Write;
        let this = self.get_mut();
        
        let ffi_waker = Self::make_ffi_waker(cx);
        if !tio::poll_ready(this.reactor_handle, tio::DIR_WRITE, ffi_waker) {
            return Poll::Pending;
        }
        
        match (&*this.inner).write(buf) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                tio::clear_ready(this.reactor_handle, tio::DIR_WRITE);
                let ffi_waker = Self::make_ffi_waker(cx);
                tio::poll_ready(this.reactor_handle, tio::DIR_WRITE, ffi_waker);
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        use std::io::Write;
        let this = self.get_mut();
        match (&*this.inner).flush() {
            Ok(()) => Poll::Ready(Ok(())),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                let ffi_waker = Self::make_ffi_waker(cx);
                tio::poll_ready(this.reactor_handle, tio::DIR_WRITE, ffi_waker);
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.shutdown(std::net::Shutdown::Write)?;
        Poll::Ready(Ok(()))
    }
}

// =============================================================================
// TcpSocket
// =============================================================================

pub struct TcpSocket {
    inner: socket2::Socket,
}

impl TcpSocket {
    pub fn from_std_stream(stream: std::net::TcpStream) -> Self {
        use std::os::fd::FromRawFd;
        let raw_fd = stream.as_raw_fd();
        // Prevent the TcpStream from closing the fd when dropped
        std::mem::forget(stream);
        let socket = unsafe { socket2::Socket::from_raw_fd(raw_fd) };
        Self { inner: socket }
    }

    pub fn new_v4() -> io::Result<Self> {
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )?;
        Ok(Self { inner: socket })
    }

    pub fn new_v6() -> io::Result<Self> {
        let socket = socket2::Socket::new(
            socket2::Domain::IPV6,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )?;
        Ok(Self { inner: socket })
    }

    pub fn set_reuseaddr(&self, reuseaddr: bool) -> io::Result<()> {
        self.inner.set_reuse_address(reuseaddr)
    }

    pub fn set_reuseport(&self, reuseport: bool) -> io::Result<()> {
        self.inner.set_reuse_port(reuseport)
    }

    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        self.inner.set_nodelay(nodelay)
    }

    pub fn set_recv_buffer_size(&self, size: u32) -> io::Result<()> {
        self.inner.set_recv_buffer_size(size as usize)
    }

    pub fn set_send_buffer_size(&self, size: u32) -> io::Result<()> {
        self.inner.set_send_buffer_size(size as usize)
    }

    pub fn bind(&self, addr: SocketAddr) -> io::Result<()> {
        self.inner.bind(&addr.into())
    }

    pub async fn connect(self, addr: SocketAddr) -> io::Result<TcpStream> {
        self.inner.set_nonblocking(true)?;

        if debug() {
            eprintln!("[tcp] TcpSocket::connect → initiating non-blocking connect to {:?}", addr);
        }

        // Initiate non-blocking connect
        match self.inner.connect(&addr.into()) {
            Ok(()) => {
                // Connected immediately (unlikely but possible on localhost)
                if debug() {
                    eprintln!("[tcp] TcpSocket::connect → immediate success");
                }
            }
            Err(ref e) if e.raw_os_error() == Some(libc::EINPROGRESS) => {
                // Expected for non-blocking connect — poll until ready
                if debug() {
                    eprintln!("[tcp] TcpSocket::connect → EINPROGRESS, polling");
                }
                ConnectFuture::new(&self.inner).await?;
            }
            Err(e) => {
                if debug() {
                    eprintln!("[tcp] TcpSocket::connect → Err({})", e);
                }
                return Err(e);
            }
        }

        let std_stream: std::net::TcpStream = self.inner.into();
        TcpStream::from_std(std_stream)
    }

    pub fn listen(self, backlog: u32) -> io::Result<TcpListener> {
        self.inner.listen(backlog as i32)?;
        self.inner.set_nonblocking(true)?;
        let std_listener: std::net::TcpListener = self.inner.into();
        TcpListener::from_std(std_listener)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner
            .local_addr()?
            .as_socket()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a socket address"))
    }
}

// =============================================================================
// ConnectFuture — polls a non-blocking socket for connect completion
// =============================================================================

struct ConnectFuture<'a> {
    socket: &'a socket2::Socket,
    reactor_handle: Option<u64>,
}

impl<'a> ConnectFuture<'a> {
    fn new(socket: &'a socket2::Socket) -> Self {
        let fd = socket.as_raw_fd();
        // Register with reactor for write events (connect completion triggers POLLOUT)
        let handle = tio::register(fd, tio::WRITABLE);
        if debug() {
            eprintln!("[tcp] ConnectFuture::new fd={} reactor_handle={:?}", fd, handle);
        }
        Self { socket, reactor_handle: handle }
    }
    
    fn make_ffi_waker(cx: &Context<'_>) -> FfiWaker {
        let waker = cx.waker().clone();
        let boxed = Box::new(waker);
        let data = Box::into_raw(boxed) as *mut ();
        
        extern "C" fn wake_fn(data: *mut ()) {
            let waker = unsafe { Box::from_raw(data as *mut std::task::Waker) };
            waker.wake();
        }
        
        FfiWaker {
            data,
            wake_fn: Some(wake_fn),
        }
    }
}

impl<'a> Drop for ConnectFuture<'a> {
    fn drop(&mut self) {
        if let Some(handle) = self.reactor_handle {
            if debug() {
                eprintln!("[tcp] ConnectFuture::drop deregistering reactor_handle={}", handle);
            }
            tio::deregister(handle);
        }
    }
}

impl<'a> std::future::Future for ConnectFuture<'a> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Check reactor for writability (connect completion)
        if let Some(handle) = self.reactor_handle {
            let ffi_waker = Self::make_ffi_waker(cx);
            if !tio::poll_ready(handle, tio::DIR_WRITE, ffi_waker) {
                // Not ready yet, waker registered with reactor
                return Poll::Pending;
            }
        }

        // Socket should be writable — check SO_ERROR to see if connect succeeded
        match self.socket.take_error()? {
            None => {
                if debug() {
                    eprintln!("[tcp] ConnectFuture → connected!");
                }
                Poll::Ready(Ok(()))
            }
            Some(e) if e.raw_os_error() == Some(0) => {
                if debug() {
                    eprintln!("[tcp] ConnectFuture → connected (SO_ERROR=0)!");
                }
                Poll::Ready(Ok(()))
            }
            Some(e) => {
                if debug() {
                    eprintln!("[tcp] ConnectFuture → connect error: {}", e);
                }
                // Clear readiness and retry
                if let Some(handle) = self.reactor_handle {
                    tio::clear_ready(handle, tio::DIR_WRITE);
                }
                Poll::Ready(Err(e))
            }
        }
    }
}

// =============================================================================
// TcpListener
// =============================================================================

pub struct TcpListener {
    inner: std::net::TcpListener,
    reactor_handle: u64,
}

impl TcpListener {
    pub async fn bind<A: std::net::ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let listener = std::net::TcpListener::bind(addr)?;
        Self::from_std(listener)
    }
    
    pub fn from_std(listener: std::net::TcpListener) -> io::Result<Self> {
        listener.set_nonblocking(true)?;
        let fd = listener.as_raw_fd();
        let handle = tio::register(fd, tio::READABLE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "failed to register listener"))?;
        Ok(Self { inner: listener, reactor_handle: handle })
    }

    pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        loop {
            match self.inner.accept() {
                Ok((stream, addr)) => {
                    return Ok((TcpStream::from_std(stream)?, addr));
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Wait for reactor to notify us
                    tio::clear_ready(self.reactor_handle, tio::DIR_READ);
                    tau::drive();
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        tio::deregister(self.reactor_handle);
    }
}

// =============================================================================
// UnixStream (unix only)
// =============================================================================

#[cfg(unix)]
pub struct UnixStream {
    inner: std::os::unix::net::UnixStream,
    reactor_handle: u64,
}

#[cfg(unix)]
impl UnixStream {
    pub async fn connect<P: AsRef<std::path::Path>>(path: P) -> io::Result<Self> {
        let stream = std::os::unix::net::UnixStream::connect(path)?;
        stream.set_nonblocking(true)?;
        let fd = stream.as_raw_fd();
        let handle = tio::register(fd, tio::READABLE | tio::WRITABLE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "failed to register unix stream"))?;
        Ok(Self { inner: stream, reactor_handle: handle })
    }
    
    fn make_ffi_waker(cx: &Context<'_>) -> FfiWaker {
        let waker = cx.waker().clone();
        let boxed = Box::new(waker);
        let data = Box::into_raw(boxed) as *mut ();
        
        extern "C" fn wake_fn(data: *mut ()) {
            let waker = unsafe { Box::from_raw(data as *mut std::task::Waker) };
            waker.wake();
        }
        
        FfiWaker {
            data,
            wake_fn: Some(wake_fn),
        }
    }
}

#[cfg(unix)]
impl Drop for UnixStream {
    fn drop(&mut self) {
        tio::deregister(self.reactor_handle);
    }
}

#[cfg(unix)]
impl AsyncRead for UnixStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        use std::io::Read;
        let this = unsafe { self.get_unchecked_mut() };
        
        let ffi_waker = Self::make_ffi_waker(cx);
        if !tio::poll_ready(this.reactor_handle, tio::DIR_READ, ffi_waker) {
            return Poll::Pending;
        }
        
        let dst = buf.initialize_unfilled();
        match (&this.inner).read(dst) {
            Ok(n) => {
                buf.advance(n);
                Poll::Ready(Ok(()))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                tio::clear_ready(this.reactor_handle, tio::DIR_READ);
                let ffi_waker = Self::make_ffi_waker(cx);
                tio::poll_ready(this.reactor_handle, tio::DIR_READ, ffi_waker);
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

#[cfg(unix)]
impl AsyncWrite for UnixStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        use std::io::Write;
        let this = unsafe { self.get_unchecked_mut() };
        
        let ffi_waker = Self::make_ffi_waker(cx);
        if !tio::poll_ready(this.reactor_handle, tio::DIR_WRITE, ffi_waker) {
            return Poll::Pending;
        }
        
        match (&this.inner).write(buf) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                tio::clear_ready(this.reactor_handle, tio::DIR_WRITE);
                let ffi_waker = Self::make_ffi_waker(cx);
                tio::poll_ready(this.reactor_handle, tio::DIR_WRITE, ffi_waker);
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        use std::io::Write;
        let this = unsafe { self.get_unchecked_mut() };
        match (&this.inner).flush() {
            Ok(()) => Poll::Ready(Ok(())),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                let ffi_waker = Self::make_ffi_waker(cx);
                tio::poll_ready(this.reactor_handle, tio::DIR_WRITE, ffi_waker);
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = unsafe { self.get_unchecked_mut() };
        this.inner.shutdown(std::net::Shutdown::Write)?;
        Poll::Ready(Ok(()))
    }
}

#[cfg(unix)]
impl std::os::unix::io::AsRawFd for UnixStream {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.inner.as_raw_fd()
    }
}

// =============================================================================
// UdpSocket
// =============================================================================

/// An async UDP socket backed by a non-blocking `std::net::UdpSocket`.
pub struct UdpSocket {
    inner: std::net::UdpSocket,
    reactor_handle: u64,
}

impl UdpSocket {
    /// Creates a UDP socket from the given address.
    pub async fn bind<A: std::net::ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let socket = std::net::UdpSocket::bind(addr)?;
        socket.set_nonblocking(true)?;
        let fd = socket.as_raw_fd();
        let handle = tio::register(fd, tio::READABLE | tio::WRITABLE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "failed to register with reactor"))?;
        if debug() {
            eprintln!("[udp] UdpSocket::bind fd={} handle={}", fd, handle);
        }
        Ok(Self { inner: socket, reactor_handle: handle })
    }

    /// Creates a new UdpSocket from a std::net::UdpSocket.
    pub fn from_std(socket: std::net::UdpSocket) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        let fd = socket.as_raw_fd();
        let handle = tio::register(fd, tio::READABLE | tio::WRITABLE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "failed to register with reactor"))?;
        if debug() {
            eprintln!("[udp] UdpSocket::from_std fd={} handle={}", fd, handle);
        }
        Ok(Self { inner: socket, reactor_handle: handle })
    }

    /// Returns the local address that this socket is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// Returns the socket address of the remote peer this socket was connected to.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.inner.peer_addr()
    }

    /// Connects the UDP socket to a remote address.
    pub async fn connect<A: std::net::ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        self.inner.connect(addr)
    }

    /// Sends data on the socket to the remote address.
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        SendFut { socket: self, buf }.await
    }

    /// Receives data from the socket.
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        RecvFut { socket: self, buf }.await
    }

    /// Sends data on the socket to the given address.
    pub async fn send_to<A: std::net::ToSocketAddrs>(&self, buf: &[u8], target: A) -> io::Result<usize> {
        let addr = target.to_socket_addrs()?.next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses to send to"))?;
        SendToFut { socket: self, buf, addr }.await
    }

    /// Receives data from the socket, returning the number of bytes read and the address from whence the data came.
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        RecvFromFut { socket: self, buf }.await
    }

    /// Receives data from the socket, without removing it from the queue.
    pub async fn peek_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        PeekFromFut { socket: self, buf }.await
    }

    /// Gets the value of the SO_BROADCAST option for this socket.
    pub fn broadcast(&self) -> io::Result<bool> {
        self.inner.broadcast()
    }

    /// Sets the value of the SO_BROADCAST option for this socket.
    pub fn set_broadcast(&self, on: bool) -> io::Result<()> {
        self.inner.set_broadcast(on)
    }

    /// Gets the value of the IP_TTL option for this socket.
    pub fn ttl(&self) -> io::Result<u32> {
        self.inner.ttl()
    }

    /// Sets the value of the IP_TTL option for this socket.
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        self.inner.set_ttl(ttl)
    }

    /// Executes an operation of the IP_ADD_MEMBERSHIP type.
    pub fn join_multicast_v4(&self, multiaddr: std::net::Ipv4Addr, interface: std::net::Ipv4Addr) -> io::Result<()> {
        self.inner.join_multicast_v4(&multiaddr, &interface)
    }

    /// Executes an operation of the IPV6_ADD_MEMBERSHIP type.
    pub fn join_multicast_v6(&self, multiaddr: &std::net::Ipv6Addr, interface: u32) -> io::Result<()> {
        self.inner.join_multicast_v6(multiaddr, interface)
    }

    /// Executes an operation of the IP_DROP_MEMBERSHIP type.
    pub fn leave_multicast_v4(&self, multiaddr: std::net::Ipv4Addr, interface: std::net::Ipv4Addr) -> io::Result<()> {
        self.inner.leave_multicast_v4(&multiaddr, &interface)
    }

    /// Executes an operation of the IPV6_DROP_MEMBERSHIP type.
    pub fn leave_multicast_v6(&self, multiaddr: &std::net::Ipv6Addr, interface: u32) -> io::Result<()> {
        self.inner.leave_multicast_v6(multiaddr, interface)
    }

    fn make_ffi_waker(cx: &Context<'_>) -> FfiWaker {
        let waker = cx.waker().clone();
        let data = Box::into_raw(Box::new(waker)) as *mut ();
        
        extern "C" fn wake_fn(data: *mut ()) {
            let waker = unsafe { Box::from_raw(data as *mut std::task::Waker) };
            waker.wake();
        }
        
        FfiWaker {
            data,
            wake_fn: Some(wake_fn),
        }
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        if debug() {
            eprintln!("[udp] drop UdpSocket handle={}", self.reactor_handle);
        }
        tio::deregister(self.reactor_handle);
    }
}

impl AsRawFd for UdpSocket {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.inner.as_raw_fd()
    }
}

impl std::fmt::Debug for UdpSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(f)
    }
}

// UDP Future types
struct SendFut<'a> {
    socket: &'a UdpSocket,
    buf: &'a [u8],
}

impl std::future::Future for SendFut<'_> {
    type Output = io::Result<usize>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let ffi_waker = UdpSocket::make_ffi_waker(cx);
        if !tio::poll_ready(self.socket.reactor_handle, tio::DIR_WRITE, ffi_waker) {
            return Poll::Pending;
        }
        match self.socket.inner.send(self.buf) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                tio::clear_ready(self.socket.reactor_handle, tio::DIR_WRITE);
                let ffi_waker = UdpSocket::make_ffi_waker(cx);
                tio::poll_ready(self.socket.reactor_handle, tio::DIR_WRITE, ffi_waker);
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

struct RecvFut<'a> {
    socket: &'a UdpSocket,
    buf: &'a mut [u8],
}

impl std::future::Future for RecvFut<'_> {
    type Output = io::Result<usize>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let ffi_waker = UdpSocket::make_ffi_waker(cx);
        if !tio::poll_ready(self.socket.reactor_handle, tio::DIR_READ, ffi_waker) {
            return Poll::Pending;
        }
        match self.socket.inner.recv(self.buf) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                tio::clear_ready(self.socket.reactor_handle, tio::DIR_READ);
                let ffi_waker = UdpSocket::make_ffi_waker(cx);
                tio::poll_ready(self.socket.reactor_handle, tio::DIR_READ, ffi_waker);
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

struct SendToFut<'a> {
    socket: &'a UdpSocket,
    buf: &'a [u8],
    addr: SocketAddr,
}

impl std::future::Future for SendToFut<'_> {
    type Output = io::Result<usize>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let ffi_waker = UdpSocket::make_ffi_waker(cx);
        if !tio::poll_ready(self.socket.reactor_handle, tio::DIR_WRITE, ffi_waker) {
            return Poll::Pending;
        }
        match self.socket.inner.send_to(self.buf, self.addr) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                tio::clear_ready(self.socket.reactor_handle, tio::DIR_WRITE);
                let ffi_waker = UdpSocket::make_ffi_waker(cx);
                tio::poll_ready(self.socket.reactor_handle, tio::DIR_WRITE, ffi_waker);
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

struct RecvFromFut<'a> {
    socket: &'a UdpSocket,
    buf: &'a mut [u8],
}

impl std::future::Future for RecvFromFut<'_> {
    type Output = io::Result<(usize, SocketAddr)>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let ffi_waker = UdpSocket::make_ffi_waker(cx);
        if !tio::poll_ready(self.socket.reactor_handle, tio::DIR_READ, ffi_waker) {
            return Poll::Pending;
        }
        match self.socket.inner.recv_from(self.buf) {
            Ok((n, addr)) => Poll::Ready(Ok((n, addr))),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                tio::clear_ready(self.socket.reactor_handle, tio::DIR_READ);
                let ffi_waker = UdpSocket::make_ffi_waker(cx);
                tio::poll_ready(self.socket.reactor_handle, tio::DIR_READ, ffi_waker);
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

struct PeekFromFut<'a> {
    socket: &'a UdpSocket,
    buf: &'a mut [u8],
}

impl std::future::Future for PeekFromFut<'_> {
    type Output = io::Result<(usize, SocketAddr)>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let ffi_waker = UdpSocket::make_ffi_waker(cx);
        if !tio::poll_ready(self.socket.reactor_handle, tio::DIR_READ, ffi_waker) {
            return Poll::Pending;
        }
        match self.socket.inner.peek_from(self.buf) {
            Ok((n, addr)) => Poll::Ready(Ok((n, addr))),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                tio::clear_ready(self.socket.reactor_handle, tio::DIR_READ);
                let ffi_waker = UdpSocket::make_ffi_waker(cx);
                tio::poll_ready(self.socket.reactor_handle, tio::DIR_READ, ffi_waker);
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}
