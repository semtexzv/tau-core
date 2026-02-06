//! Asynchronous I/O traits and utilities.
//!
//! Provides [`AsyncRead`], [`AsyncWrite`], [`ReadBuf`], and extension traits
//! mirroring `tokio::io`.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

pub use std::io::{Error, Result};

// =============================================================================
// Core traits
// =============================================================================

/// Reads bytes asynchronously.
pub trait AsyncRead {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>>;
}

/// Writes bytes asynchronously.
pub trait AsyncWrite {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>>;

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>>;

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>>;

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        for buf in bufs {
            if !buf.is_empty() {
                return self.poll_write(cx, buf);
            }
        }
        self.poll_write(cx, &[])
    }

    fn is_write_vectored(&self) -> bool {
        false
    }
}

/// Reads bytes asynchronously with an internal buffer.
pub trait AsyncBufRead: AsyncRead {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>>;
    fn consume(self: Pin<&mut Self>, amt: usize);
}

// =============================================================================
// Blanket impls for references
// =============================================================================

impl<T: AsyncRead + Unpin + ?Sized> AsyncRead for &mut T {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut **self).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin + ?Sized> AsyncWrite for &mut T {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut **self).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut **self).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut **self).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut **self).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        (**self).is_write_vectored()
    }
}

impl<T: AsyncBufRead + Unpin + ?Sized> AsyncBufRead for &mut T {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        Pin::new(&mut **self.get_mut()).poll_fill_buf(cx)
    }

    fn consume(mut self: Pin<&mut Self>, amt: usize) {
        Pin::new(&mut **self).consume(amt)
    }
}

impl<T: AsyncRead + Unpin + ?Sized> AsyncRead for Box<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut **self).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin + ?Sized> AsyncWrite for Box<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut **self).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut **self).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut **self).poll_shutdown(cx)
    }
}

/// Seeks to a position asynchronously.
pub trait AsyncSeek {
    fn start_seek(self: Pin<&mut Self>, position: io::SeekFrom) -> io::Result<()>;
    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>>;
}

// =============================================================================
// ReadBuf
// =============================================================================

/// A wrapper around a byte buffer for reading.
pub struct ReadBuf<'a> {
    buf: &'a mut [u8],
    filled: usize,
    initialized: usize,
}

impl<'a> ReadBuf<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        let len = buf.len();
        Self {
            buf,
            filled: 0,
            initialized: len,
        }
    }

    pub fn uninit(buf: &'a mut [std::mem::MaybeUninit<u8>]) -> Self {
        Self {
            buf: unsafe {
                std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, buf.len())
            },
            filled: 0,
            initialized: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub fn filled(&self) -> &[u8] {
        &self.buf[..self.filled]
    }

    pub fn filled_mut(&mut self) -> &mut [u8] {
        &mut self.buf[..self.filled]
    }

    pub fn initialized(&self) -> &[u8] {
        &self.buf[..self.initialized]
    }

    pub fn initialized_mut(&mut self) -> &mut [u8] {
        &mut self.buf[..self.initialized]
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.filled
    }

    pub fn unfilled_mut(&mut self) -> &mut [std::mem::MaybeUninit<u8>] {
        unsafe {
            let ptr = self.buf.as_mut_ptr().add(self.filled) as *mut std::mem::MaybeUninit<u8>;
            let len = self.buf.len() - self.filled;
            std::slice::from_raw_parts_mut(ptr, len)
        }
    }

    pub fn initialize_unfilled(&mut self) -> &mut [u8] {
        self.initialize_unfilled_to(self.remaining())
    }

    pub fn initialize_unfilled_to(&mut self, n: usize) -> &mut [u8] {
        let end = self.filled + n;
        assert!(end <= self.buf.len());
        if self.initialized < end {
            self.buf[self.initialized..end].fill(0);
            self.initialized = end;
        }
        &mut self.buf[self.filled..end]
    }

    pub fn inner_mut(&mut self) -> &mut [std::mem::MaybeUninit<u8>] {
        unsafe {
            let ptr = self.buf.as_mut_ptr() as *mut std::mem::MaybeUninit<u8>;
            std::slice::from_raw_parts_mut(ptr, self.buf.len())
        }
    }

    pub fn set_filled(&mut self, n: usize) {
        assert!(n <= self.initialized);
        self.filled = n;
    }

    pub fn advance(&mut self, n: usize) {
        let new = self.filled + n;
        assert!(new <= self.initialized);
        self.filled = new;
    }

    pub fn assume_init(&mut self, n: usize) {
        let new_init = self.filled + n;
        if new_init > self.initialized {
            self.initialized = new_init;
        }
    }

    pub fn put_slice(&mut self, src: &[u8]) {
        let end = self.filled + src.len();
        assert!(end <= self.buf.len());
        self.buf[self.filled..end].copy_from_slice(src);
        if end > self.initialized {
            self.initialized = end;
        }
        self.filled = end;
    }

    pub fn clear(&mut self) {
        self.filled = 0;
    }

    pub fn take(&mut self, n: usize) -> ReadBuf<'_> {
        let max = std::cmp::min(self.remaining(), n);
        ReadBuf {
            buf: &mut self.buf[self.filled..self.filled + max],
            filled: 0,
            initialized: std::cmp::min(self.initialized.saturating_sub(self.filled), max),
        }
    }
}

impl std::fmt::Debug for ReadBuf<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadBuf")
            .field("filled", &self.filled)
            .field("initialized", &self.initialized)
            .field("capacity", &self.buf.len())
            .finish()
    }
}

// =============================================================================
// Extension traits
// =============================================================================

pub trait AsyncReadExt: AsyncRead {
    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> Read<'a, Self>
    where
        Self: Unpin,
    {
        Read { reader: self, buf }
    }

    fn read_buf<'a, B: BufMut>(&'a mut self, buf: &'a mut B) -> ReadBufFut<'a, Self, B>
    where
        Self: Unpin,
    {
        ReadBufFut { reader: self, buf }
    }

    fn read_to_end<'a>(&'a mut self, buf: &'a mut Vec<u8>) -> ReadToEnd<'a, Self>
    where
        Self: Unpin,
    {
        ReadToEnd { reader: self, buf }
    }
}

impl<T: AsyncRead + ?Sized> AsyncReadExt for T {}

pub trait AsyncWriteExt: AsyncWrite {
    fn write<'a>(&'a mut self, buf: &'a [u8]) -> Write<'a, Self>
    where
        Self: Unpin,
    {
        Write { writer: self, buf }
    }

    fn write_all<'a>(&'a mut self, buf: &'a [u8]) -> WriteAll<'a, Self>
    where
        Self: Unpin,
    {
        WriteAll {
            writer: self,
            buf,
            pos: 0,
        }
    }

    fn flush(&mut self) -> Flush<'_, Self>
    where
        Self: Unpin,
    {
        Flush { writer: self }
    }

    fn shutdown(&mut self) -> Shutdown<'_, Self>
    where
        Self: Unpin,
    {
        Shutdown { writer: self }
    }
}

impl<T: AsyncWrite + ?Sized> AsyncWriteExt for T {}

// =============================================================================
// Future types for extension methods
// =============================================================================

pub struct Read<'a, R: ?Sized> {
    reader: &'a mut R,
    buf: &'a mut [u8],
}

impl<R: AsyncRead + Unpin + ?Sized> std::future::Future for Read<'_, R> {
    type Output = io::Result<usize>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let this = unsafe { self.get_unchecked_mut() };
        let mut read_buf = ReadBuf::new(this.buf);
        match Pin::new(&mut *this.reader).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

pub struct ReadBufFut<'a, R: ?Sized, B> {
    #[allow(dead_code)]
    reader: &'a mut R,
    #[allow(dead_code)]
    buf: &'a mut B,
}

/// Minimal BufMut trait (enough for bytes::BytesMut compatibility).
pub trait BufMut {
    fn remaining_mut(&self) -> usize;
    fn chunk_mut(&mut self) -> &mut [std::mem::MaybeUninit<u8>];
    unsafe fn advance_mut(&mut self, cnt: usize);
}

pub struct ReadToEnd<'a, R: ?Sized> {
    reader: &'a mut R,
    buf: &'a mut Vec<u8>,
}

impl<R: AsyncRead + Unpin + ?Sized> std::future::Future for ReadToEnd<'_, R> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let this = unsafe { self.get_unchecked_mut() };
        let start_len = this.buf.len();
        loop {
            if this.buf.len() == this.buf.capacity() {
                this.buf.reserve(32);
            }
            let dst = this.buf.spare_capacity_mut();
            let mut read_buf = ReadBuf::uninit(dst);
            match Pin::new(&mut *this.reader).poll_read(cx, &mut read_buf) {
                Poll::Ready(Ok(())) => {
                    let filled_len = read_buf.filled().len();
                    if filled_len == 0 {
                        return Poll::Ready(Ok(this.buf.len() - start_len));
                    }
                    unsafe {
                        let new_len = this.buf.len() + filled_len;
                        this.buf.set_len(new_len);
                    }
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

pub struct Write<'a, W: ?Sized> {
    writer: &'a mut W,
    buf: &'a [u8],
}

impl<W: AsyncWrite + Unpin + ?Sized> std::future::Future for Write<'_, W> {
    type Output = io::Result<usize>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let this = unsafe { self.get_unchecked_mut() };
        Pin::new(&mut *this.writer).poll_write(cx, this.buf)
    }
}

pub struct WriteAll<'a, W: ?Sized> {
    writer: &'a mut W,
    buf: &'a [u8],
    pos: usize,
}

impl<W: AsyncWrite + Unpin + ?Sized> std::future::Future for WriteAll<'_, W> {
    type Output = io::Result<()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = unsafe { self.get_unchecked_mut() };
        while this.pos < this.buf.len() {
            match Pin::new(&mut *this.writer).poll_write(cx, &this.buf[this.pos..]) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "write zero",
                    )));
                }
                Poll::Ready(Ok(n)) => this.pos += n,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        Poll::Ready(Ok(()))
    }
}

pub struct Flush<'a, W: ?Sized> {
    writer: &'a mut W,
}

impl<W: AsyncWrite + Unpin + ?Sized> std::future::Future for Flush<'_, W> {
    type Output = io::Result<()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = unsafe { self.get_unchecked_mut() };
        Pin::new(&mut *this.writer).poll_flush(cx)
    }
}

pub struct Shutdown<'a, W: ?Sized> {
    writer: &'a mut W,
}

impl<W: AsyncWrite + Unpin + ?Sized> std::future::Future for Shutdown<'_, W> {
    type Output = io::Result<()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = unsafe { self.get_unchecked_mut() };
        Pin::new(&mut *this.writer).poll_shutdown(cx)
    }
}

// =============================================================================
// BufReader
// =============================================================================

const DEFAULT_BUF_SIZE: usize = 8 * 1024;

/// An async buffered reader.
pub struct BufReader<R> {
    inner: R,
    buf: Box<[u8]>,
    pos: usize,
    cap: usize,
}

impl<R: AsyncRead> BufReader<R> {
    /// Creates a new buffered reader with default buffer size.
    pub fn new(inner: R) -> Self {
        Self::with_capacity(DEFAULT_BUF_SIZE, inner)
    }

    /// Creates a new buffered reader with specified buffer size.
    pub fn with_capacity(capacity: usize, inner: R) -> Self {
        Self {
            inner,
            buf: vec![0; capacity].into_boxed_slice(),
            pos: 0,
            cap: 0,
        }
    }

    /// Gets a reference to the underlying reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Gets a mutable reference to the underlying reader.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Returns the internal buffer.
    pub fn buffer(&self) -> &[u8] {
        &self.buf[self.pos..self.cap]
    }

    /// Unwraps this BufReader, returning the underlying reader.
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for BufReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = unsafe { self.get_unchecked_mut() };
        
        // If we have buffered data, use it
        if this.pos < this.cap {
            let amt = std::cmp::min(this.cap - this.pos, buf.remaining());
            buf.put_slice(&this.buf[this.pos..this.pos + amt]);
            this.pos += amt;
            return Poll::Ready(Ok(()));
        }

        // Buffer empty, read directly if buf is large enough
        if buf.remaining() >= this.buf.len() {
            return Pin::new(&mut this.inner).poll_read(cx, buf);
        }

        // Fill our buffer - need to split the borrow
        let inner = &mut this.inner;
        let internal_buf = &mut this.buf;
        let mut read_buf = ReadBuf::new(internal_buf);
        match Pin::new(inner).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => {
                this.pos = 0;
                this.cap = read_buf.filled().len();
                if this.cap == 0 {
                    return Poll::Ready(Ok(()));
                }
                let amt = std::cmp::min(this.cap, buf.remaining());
                buf.put_slice(&this.buf[..amt]);
                this.pos = amt;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncBufRead for BufReader<R> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = unsafe { self.get_unchecked_mut() };
        if this.pos >= this.cap {
            let mut read_buf = ReadBuf::new(&mut this.buf);
            match Pin::new(&mut this.inner).poll_read(cx, &mut read_buf) {
                Poll::Ready(Ok(())) => {
                    this.pos = 0;
                    this.cap = read_buf.filled().len();
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        Poll::Ready(Ok(&this.buf[this.pos..this.cap]))
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        let this = unsafe { self.get_unchecked_mut() };
        this.pos = std::cmp::min(this.pos + amt, this.cap);
    }
}

impl<R: std::fmt::Debug> std::fmt::Debug for BufReader<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufReader")
            .field("reader", &self.inner)
            .field("buffer", &format!("{}/{}", self.cap - self.pos, self.buf.len()))
            .finish()
    }
}

// =============================================================================
// BufWriter
// =============================================================================

/// An async buffered writer.
pub struct BufWriter<W> {
    inner: W,
    buf: Vec<u8>,
}

impl<W: AsyncWrite> BufWriter<W> {
    /// Creates a new buffered writer with default buffer size.
    pub fn new(inner: W) -> Self {
        Self::with_capacity(DEFAULT_BUF_SIZE, inner)
    }

    /// Creates a new buffered writer with specified buffer size.
    pub fn with_capacity(capacity: usize, inner: W) -> Self {
        Self {
            inner,
            buf: Vec::with_capacity(capacity),
        }
    }

    /// Gets a reference to the underlying writer.
    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    /// Gets a mutable reference to the underlying writer.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// Returns the internal buffer.
    pub fn buffer(&self) -> &[u8] {
        &self.buf
    }

    /// Unwraps this BufWriter, returning the underlying writer.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for BufWriter<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = unsafe { self.get_unchecked_mut() };
        
        // If the buffer is full, flush first
        if this.buf.len() >= this.buf.capacity() {
            while !this.buf.is_empty() {
                let write_buf: &[u8] = &this.buf;
                match Pin::new(&mut this.inner).poll_write(cx, write_buf) {
                    Poll::Ready(Ok(0)) => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "failed to write buffered data",
                        )));
                    }
                    Poll::Ready(Ok(n)) => {
                        this.buf.drain(..n);
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => return Poll::Pending,
                }
            }
        }

        // Buffer the write
        let amt = std::cmp::min(buf.len(), this.buf.capacity() - this.buf.len());
        this.buf.extend_from_slice(&buf[..amt]);
        Poll::Ready(Ok(amt))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = unsafe { self.get_unchecked_mut() };
        while !this.buf.is_empty() {
            let write_buf: &[u8] = &this.buf;
            match Pin::new(&mut this.inner).poll_write(cx, write_buf) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write buffered data",
                    )));
                }
                Poll::Ready(Ok(n)) => {
                    this.buf.drain(..n);
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = unsafe { self.get_unchecked_mut() };
        // Flush first
        while !this.buf.is_empty() {
            let write_buf: &[u8] = &this.buf;
            match Pin::new(&mut this.inner).poll_write(cx, write_buf) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write buffered data",
                    )));
                }
                Poll::Ready(Ok(n)) => {
                    this.buf.drain(..n);
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

impl<W: std::fmt::Debug> std::fmt::Debug for BufWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufWriter")
            .field("writer", &self.inner)
            .field("buffer", &format!("{}/{}", self.buf.len(), self.buf.capacity()))
            .finish()
    }
}

// =============================================================================
// copy
// =============================================================================

/// Copies all bytes from a reader to a writer.
pub async fn copy<R, W>(reader: &mut R, writer: &mut W) -> io::Result<u64>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut buf = [0u8; 8192];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n]).await?;
        total += n as u64;
    }
    Ok(total)
}

/// Copies all bytes from a buffered reader to a writer.
pub async fn copy_buf<R, W>(reader: &mut R, writer: &mut W) -> io::Result<u64>
where
    R: AsyncBufRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    // Simplified: delegate to copy
    copy(reader, writer).await
}

// =============================================================================
// split
// =============================================================================

/// Splits a single value implementing AsyncRead + AsyncWrite into separate reader and writer halves.
pub fn split<T: AsyncRead + AsyncWrite>(stream: T) -> (ReadHalf<T>, WriteHalf<T>)
where
    T: Unpin,
{
    use std::sync::Arc;
    use std::sync::Mutex;
    
    let inner = Arc::new(Mutex::new(stream));
    (
        ReadHalf { inner: inner.clone() },
        WriteHalf { inner },
    )
}

/// The read half of a split AsyncRead + AsyncWrite.
pub struct ReadHalf<T> {
    inner: std::sync::Arc<std::sync::Mutex<T>>,
}

impl<T: AsyncRead + Unpin> AsyncRead for ReadHalf<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut guard = self.inner.lock().unwrap();
        Pin::new(&mut *guard).poll_read(cx, buf)
    }
}

/// The write half of a split AsyncRead + AsyncWrite.
pub struct WriteHalf<T> {
    inner: std::sync::Arc<std::sync::Mutex<T>>,
}

impl<T: AsyncWrite + Unpin> AsyncWrite for WriteHalf<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut guard = self.inner.lock().unwrap();
        Pin::new(&mut *guard).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut guard = self.inner.lock().unwrap();
        Pin::new(&mut *guard).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut guard = self.inner.lock().unwrap();
        Pin::new(&mut *guard).poll_shutdown(cx)
    }
}

// =============================================================================
// duplex
// =============================================================================

/// A bidirectional in-memory stream.
pub struct DuplexStream {
    read_buf: std::sync::Arc<std::sync::Mutex<DuplexBuffer>>,
    write_buf: std::sync::Arc<std::sync::Mutex<DuplexBuffer>>,
    max_buf_size: usize,
}

#[derive(Default)]
struct DuplexBuffer {
    data: std::collections::VecDeque<u8>,
    closed: bool,
    read_waker: Option<std::task::Waker>,
    write_waker: Option<std::task::Waker>,
}

/// Creates a pair of connected in-memory streams.
pub fn duplex(max_buf_size: usize) -> (DuplexStream, DuplexStream) {
    use std::sync::{Arc, Mutex};

    let a_to_b = Arc::new(Mutex::new(DuplexBuffer::default()));
    let b_to_a = Arc::new(Mutex::new(DuplexBuffer::default()));

    (
        DuplexStream {
            read_buf: b_to_a.clone(),
            write_buf: a_to_b.clone(),
            max_buf_size,
        },
        DuplexStream {
            read_buf: a_to_b,
            write_buf: b_to_a,
            max_buf_size,
        },
    )
}

impl AsyncRead for DuplexStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut inner = self.read_buf.lock().unwrap();
        if !inner.data.is_empty() {
            let amt = std::cmp::min(inner.data.len(), buf.remaining());
            for _ in 0..amt {
                if let Some(byte) = inner.data.pop_front() {
                    buf.put_slice(&[byte]);
                }
            }
            if let Some(waker) = inner.write_waker.take() {
                waker.wake();
            }
            Poll::Ready(Ok(()))
        } else if inner.closed {
            Poll::Ready(Ok(()))
        } else {
            inner.read_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl AsyncWrite for DuplexStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut inner = self.write_buf.lock().unwrap();
        if inner.closed {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed")));
        }
        if inner.data.len() >= self.max_buf_size {
            inner.write_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let amt = std::cmp::min(buf.len(), self.max_buf_size - inner.data.len());
        inner.data.extend(&buf[..amt]);
        if let Some(waker) = inner.read_waker.take() {
            waker.wake();
        }
        Poll::Ready(Ok(amt))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = self.write_buf.lock().unwrap();
        inner.closed = true;
        if let Some(waker) = inner.read_waker.take() {
            waker.wake();
        }
        Poll::Ready(Ok(()))
    }
}

impl std::fmt::Debug for DuplexStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DuplexStream").finish()
    }
}

// =============================================================================
// Empty and Sink
// =============================================================================

/// Creates a reader that is always at EOF.
pub fn empty() -> Empty {
    Empty(())
}

/// An async reader that yields nothing.
pub struct Empty(());

impl AsyncRead for Empty {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl std::fmt::Debug for Empty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Empty").finish()
    }
}

/// Creates a writer that discards all data.
pub fn sink() -> Sink {
    Sink(())
}

/// An async writer that discards all data.
pub struct Sink(());

impl AsyncWrite for Sink {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl std::fmt::Debug for Sink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sink").finish()
    }
}

// =============================================================================
// AsyncBufReadExt
// =============================================================================

/// Extension methods for AsyncBufRead.
pub trait AsyncBufReadExt: AsyncBufRead {
    /// Reads all bytes until a newline is found.
    fn read_line<'a>(&'a mut self, buf: &'a mut String) -> ReadLine<'a, Self>
    where
        Self: Unpin,
    {
        ReadLine { reader: self, buf }
    }

    /// Reads all bytes until the delimiter is found.
    fn read_until<'a>(&'a mut self, byte: u8, buf: &'a mut Vec<u8>) -> ReadUntil<'a, Self>
    where
        Self: Unpin,
    {
        ReadUntil { reader: self, byte, buf }
    }
}

impl<T: AsyncBufRead + ?Sized> AsyncBufReadExt for T {}

pub struct ReadLine<'a, R: ?Sized> {
    reader: &'a mut R,
    buf: &'a mut String,
}

impl<R: AsyncBufRead + Unpin + ?Sized> std::future::Future for ReadLine<'_, R> {
    type Output = io::Result<usize>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let this = unsafe { self.get_unchecked_mut() };
        let mut bytes = Vec::new();
        loop {
            let available = match Pin::new(&mut *this.reader).poll_fill_buf(cx) {
                Poll::Ready(Ok(buf)) => buf,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            if available.is_empty() {
                let s = String::from_utf8(bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let len = s.len();
                this.buf.push_str(&s);
                return Poll::Ready(Ok(len));
            }
            if let Some(pos) = available.iter().position(|&b| b == b'\n') {
                bytes.extend_from_slice(&available[..=pos]);
                Pin::new(&mut *this.reader).consume(pos + 1);
                let s = String::from_utf8(bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let len = s.len();
                this.buf.push_str(&s);
                return Poll::Ready(Ok(len));
            }
            bytes.extend_from_slice(available);
            let len = available.len();
            Pin::new(&mut *this.reader).consume(len);
        }
    }
}

pub struct ReadUntil<'a, R: ?Sized> {
    reader: &'a mut R,
    byte: u8,
    buf: &'a mut Vec<u8>,
}

impl<R: AsyncBufRead + Unpin + ?Sized> std::future::Future for ReadUntil<'_, R> {
    type Output = io::Result<usize>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let this = unsafe { self.get_unchecked_mut() };
        let start_len = this.buf.len();
        loop {
            let available = match Pin::new(&mut *this.reader).poll_fill_buf(cx) {
                Poll::Ready(Ok(buf)) => buf,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            if available.is_empty() {
                return Poll::Ready(Ok(this.buf.len() - start_len));
            }
            if let Some(pos) = available.iter().position(|&b| b == this.byte) {
                this.buf.extend_from_slice(&available[..=pos]);
                Pin::new(&mut *this.reader).consume(pos + 1);
                return Poll::Ready(Ok(this.buf.len() - start_len));
            }
            this.buf.extend_from_slice(available);
            let len = available.len();
            Pin::new(&mut *this.reader).consume(len);
        }
    }
}
