//! IO integration tests using pipes.
//!
//! These tests verify the IO reactor and AsyncFd functionality by using
//! Unix pipes for controlled async IO.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use tau_host::runtime::{self, executor};

/// Create a pipe and return (read_fd, write_fd) as OwnedFds.
fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Set non-blocking
    unsafe {
        libc::fcntl(fds[0], libc::F_SETFL, libc::O_NONBLOCK);
        libc::fcntl(fds[1], libc::F_SETFL, libc::O_NONBLOCK);
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Initialize the runtime for tests
fn init_runtime() {
    runtime::tau_runtime_init();
}

#[test]
fn test_pipe_read_write() {
    init_runtime();
    
    let (read_fd, write_fd) = pipe().expect("failed to create pipe");
    
    // Write to the pipe
    let mut writer = unsafe { std::fs::File::from_raw_fd(write_fd.as_raw_fd()) };
    std::mem::forget(write_fd); // Don't double-close
    writer.write_all(b"hello").expect("write failed");
    drop(writer);
    
    // Read from the pipe
    let mut reader = unsafe { std::fs::File::from_raw_fd(read_fd.as_raw_fd()) };
    std::mem::forget(read_fd); // Don't double-close
    let mut buf = [0u8; 5];
    reader.read_exact(&mut buf).expect("read failed");
    assert_eq!(&buf, b"hello");
}

#[test]
fn test_reactor_register_deregister() {
    init_runtime();
    
    let (read_fd, _write_fd) = pipe().expect("failed to create pipe");
    
    // Register the fd
    let handle = runtime::tau_io_register(read_fd.as_raw_fd(), 0b01); // READABLE
    assert_ne!(handle, 0, "register should return non-zero handle");
    
    // Deregister
    runtime::tau_io_deregister(handle);
}

#[test]
fn test_reactor_poll_ready_immediate() {
    init_runtime();
    
    let (read_fd, write_fd) = pipe().expect("failed to create pipe");
    
    // Register for read
    let handle = runtime::tau_io_register(read_fd.as_raw_fd(), 0b01);
    assert_ne!(handle, 0);
    
    // Write some data so it's immediately readable
    let mut writer = unsafe { std::fs::File::from_raw_fd(write_fd.as_raw_fd()) };
    std::mem::forget(write_fd);
    writer.write_all(b"test").expect("write failed");
    drop(writer);
    
    // Poll the reactor to pick up events
    runtime::poll_reactor(Some(Duration::from_millis(10)));
    
    // Now poll_ready should return true
    let waker = tau_rt::types::FfiWaker::null();
    let ready = runtime::tau_io_poll_ready(handle, 0, waker); // direction 0 = read
    assert_eq!(ready, 1, "should be ready after data written");
    
    runtime::tau_io_deregister(handle);
}

#[test]
fn test_executor_spawn_complete() {
    init_runtime();
    
    // Allocate a slot
    let (id, _gen, storage, executor_idx) = executor::alloc_raw();
    assert!(storage != std::ptr::null_mut());
    
    // Create a simple completed future
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};
    
    struct Ready(i32);
    impl Future for Ready {
        type Output = i32;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<i32> {
            Poll::Ready(self.0)
        }
    }
    
    // Type-erased functions
    use std::mem::ManuallyDrop;
    
    union Payload {
        future: ManuallyDrop<Ready>,
        result: ManuallyDrop<i32>,
    }
    
    unsafe fn poll_fn(ptr: *mut u8, waker: &Waker) -> Poll<()> {
        let payload = &mut *(ptr as *mut Payload);
        let mut cx = Context::from_waker(waker);
        match Pin::new_unchecked(&mut *payload.future).poll(&mut cx) {
            Poll::Ready(v) => {
                ManuallyDrop::drop(&mut payload.future);
                std::ptr::write(&mut payload.result, ManuallyDrop::new(v));
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }
    
    unsafe fn drop_result(ptr: *mut u8) {
        let payload = &mut *(ptr as *mut Payload);
        ManuallyDrop::drop(&mut payload.result);
    }
    
    unsafe fn drop_future(ptr: *mut u8) {
        let payload = &mut *(ptr as *mut Payload);
        ManuallyDrop::drop(&mut payload.future);
    }
    
    unsafe fn read_fn(ptr: *mut u8) -> *mut u8 {
        let payload = &mut *(ptr as *mut Payload);
        &mut *payload.result as *mut i32 as *mut u8
    }
    
    // Write the future to storage
    unsafe {
        let ptr = storage as *mut Payload;
        std::ptr::write(ptr, Payload { future: ManuallyDrop::new(Ready(42)) });
    }
    
    // Commit the spawn
    executor::commit_spawn(
        id,
        poll_fn,
        Some(drop_result),
        Some(drop_future),
        Some(read_fn),
        None,
        0,
    );
    
    // Drive the executor
    let did_work = executor::drive_cycle();
    assert!(did_work, "should have done work");
    
    // Check state
    let (state, _) = executor::tau_exec_slot_state(id, executor_idx);
    assert_eq!(state, 3, "should be STATE_COMPLETE");
    
    // Take result
    let (ptr, _) = executor::tau_exec_take_result(id, executor_idx);
    let result = unsafe { std::ptr::read(ptr as *const i32) };
    assert_eq!(result, 42);
    
    // Cleanup
    executor::tau_exec_dealloc_storage(id, executor_idx);
}

#[test]
fn test_executor_timer() {
    init_runtime();
    
    use std::task::{RawWaker, RawWakerVTable, Waker};
    use std::time::Instant;

    
    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |d| RawWaker::new(d, &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    
    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
    
    // Register a timer 10ms in the future
    let deadline = Instant::now() + Duration::from_millis(10);
    let handle = executor::timer_register(deadline, waker, 0);
    assert_ne!(handle, 0);
    
    // Check next deadline
    let next = executor::next_timer_deadline();
    assert!(next.is_some());
    assert!(next.unwrap() <= Duration::from_millis(15));
    
    // Cancel the timer
    executor::timer_cancel(handle);
}

#[test]
fn test_plugin_id_allocation() {
    init_runtime();
    
    let id1 = executor::allocate_plugin_id();
    let id2 = executor::allocate_plugin_id();
    let id3 = executor::allocate_plugin_id();
    
    assert_ne!(id1, id2);
    assert_ne!(id2, id3);
    assert_ne!(id1, id3);
    
    // IDs should be sequential
    assert!(id2 == id1 + 1 || id2 > id1);
    assert!(id3 == id2 + 1 || id3 > id2);
}

#[test]
fn test_current_plugin() {
    init_runtime();
    
    // Initially no plugin
    assert_eq!(executor::current_plugin_id(), 0);
    
    executor::set_current_plugin(42);
    assert_eq!(executor::current_plugin_id(), 42);
    
    executor::clear_current_plugin();
    assert_eq!(executor::current_plugin_id(), 0);
}
