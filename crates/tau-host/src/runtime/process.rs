use std::sync::{OnceLock, Mutex};
use std::task::Waker;
use std::os::fd::RawFd;
use tau_rt::types::FfiWaker;
use crate::runtime::reactor::with_reactor;

static SIGCHLD_STATE: OnceLock<SigchldState> = OnceLock::new();
static mut SIGCHLD_WRITE_FD: RawFd = -1;

struct SigchldState {
    subscribers: Mutex<Vec<Waker>>,
    pipe_read_handle: usize, // reactor token
    pipe_read_fd: RawFd,
    #[allow(dead_code)]
    pipe_write_fd: RawFd,
}

#[no_mangle]
pub extern "C" fn tau_host_sigchld_subscribe(waker: FfiWaker) {
    let waker = waker.into_waker();
    
    let state = SIGCHLD_STATE.get_or_init(init_sigchld);
    
    {
        let mut subs = state.subscribers.lock().unwrap();
        subs.push(waker);
    } // unlock before ensuring polling

    ensure_polling(state);
}

fn init_sigchld() -> SigchldState {
    let mut fds = [0i32; 2];
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) < 0 {
            panic!("failed to create sigchld pipe");
        }
        libc::fcntl(fds[0], libc::F_SETFL, libc::O_NONBLOCK);
        libc::fcntl(fds[1], libc::F_SETFL, libc::O_NONBLOCK);
    }
    let read_fd = fds[0];
    let write_fd = fds[1];

    unsafe { SIGCHLD_WRITE_FD = write_fd };

    // Register with reactor
    // interest: 1 = READABLE (matches tau-rt definition)
    let handle = with_reactor(|reactor| {
        reactor.register(read_fd, 1).expect("failed to register sigchld pipe")
    });

    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = sigchld_handler as *const () as usize;
        sa.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGCHLD, &sa, std::ptr::null_mut());
    }

    SigchldState {
        subscribers: Mutex::new(Vec::new()),
        pipe_read_handle: handle,
        pipe_read_fd: read_fd,
        pipe_write_fd: write_fd,
    }
}

extern "C" fn sigchld_handler(_sig: i32) {
    unsafe {
        if SIGCHLD_WRITE_FD != -1 {
            let buf = b"\0";
            libc::write(SIGCHLD_WRITE_FD, buf.as_ptr() as *const _, 1);
        }
    }
}

fn ensure_polling(state: &SigchldState) {
    // Create special waker for the reactor call.
    // It calls wake_sigchld_callback.
    // We construct a Waker manually using FfiWaker utility or implementing RawWaker.
    // Since we are in the host, we can just use FfiWaker trick or standard Waker::from_raw.
    // To minimize boilerplate, let's use FfiWaker since we have it.
    
    let waker = FfiWaker {
        data: std::ptr::null_mut(),
        wake_fn: Some(wake_sigchld_callback),
        clone_fn: None, // non-owned
        drop_fn: None,
    }.into_waker();

    // Poll ready.
    // direction: 0 = read
    let ready = with_reactor(|reactor| {
        reactor.poll_ready(state.pipe_read_handle, 0, &waker)
    });
    
    if ready {
        unsafe { wake_sigchld_callback(std::ptr::null_mut()) };
    }
}

unsafe extern "C" fn wake_sigchld_callback(_: *mut ()) {
    if let Some(state) = SIGCHLD_STATE.get() {
        // 1. Drain pipe until WouldBlock
        let mut buf = [0u8; 16];
        loop {
            let ret = libc::read(state.pipe_read_fd, buf.as_mut_ptr() as *mut _, buf.len());
            if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    break;
                }
                // Ignore other errors
                break;
            }
            if ret == 0 { break; } // EOF?
        }

        // 2. Clear readiness so next poll works
        // direction: 0 = read
        with_reactor(|reactor| {
            reactor.clear_ready(state.pipe_read_handle, 0);
        });

        // 3. Notify all subscribers
        let mut subs = state.subscribers.lock().unwrap();
        for waker in subs.drain(..) {
            waker.wake();
        }
        drop(subs);

        // 4. Re-arm the poll
        ensure_polling(state);
    }
}
