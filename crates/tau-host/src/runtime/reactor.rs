//! IO Reactor using the `polling` crate.
//!
//! This provides epoll/kqueue-based IO multiplexing for the runtime.
//! Instead of spin-polling on WouldBlock, tasks register interest with the reactor
//! and get woken when the socket becomes ready.

use polling::{Event, Events, Poller, PollMode};
use std::collections::HashMap;
use std::io;
use std::os::fd::{BorrowedFd, RawFd};
use std::task::Waker;
use std::time::Duration;

use super::executor::debug_enabled;

// =============================================================================
// Interest and Direction flags (match tokio conventions)
// =============================================================================

pub const READABLE: u8 = 0b01;
pub const WRITABLE: u8 = 0b10;

pub const DIR_READ: u8 = 0;

// =============================================================================
// Per-socket IO state
// =============================================================================

struct IoState {
    fd: RawFd,
    interest: u8,           // What we're registered for (READABLE | WRITABLE)
    readiness: u8,          // What's currently ready
    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
}

// =============================================================================
// Reactor
// =============================================================================

pub struct Reactor {
    poller: Poller,
    io_states: HashMap<usize, IoState>,  // token -> IoState
    next_token: usize,
    events: Events,
}

impl Reactor {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            poller: Poller::new()?,
            io_states: HashMap::new(),
            next_token: 1,  // Start at 1, reserve 0 for special use
            events: Events::new(),
        })
    }

    /// Wake the reactor from another thread.
    /// This interrupts any blocking poll() call.
    pub fn notify(&self) -> io::Result<()> {
        self.poller.notify()
    }

    /// Register a file descriptor with the reactor.
    /// Returns a handle (token) that can be used for subsequent operations.
    pub fn register(&mut self, fd: RawFd, interest: u8) -> io::Result<usize> {
        let token = self.next_token;
        self.next_token += 1;

        // Determine polling mode based on interest
        let readable = (interest & READABLE) != 0;
        let writable = (interest & WRITABLE) != 0;
        
        // Use level-triggered mode (like tokio)
        // Safety: fd must be valid and open
        unsafe {
            self.poller.add_with_mode(
                fd,  // RawFd implements AsRawSource
                Event::new(token, readable, writable),
                PollMode::Level,
            )?;
        }

        self.io_states.insert(token, IoState {
            fd,
            interest,
            readiness: 0,
            read_waker: None,
            write_waker: None,
        });

        if debug_enabled() {
            eprintln!("[reactor] register fd={} token={} interest={:02b}", fd, token, interest);
        }

        Ok(token)
    }

    /// Deregister a file descriptor.
    pub fn deregister(&mut self, token: usize) -> io::Result<()> {
        if let Some(state) = self.io_states.remove(&token) {
            // Ignore errors on deregister (fd might already be closed)
            unsafe {
                let borrowed = BorrowedFd::borrow_raw(state.fd);
                let _ = self.poller.delete(&borrowed);
            }
            
            if debug_enabled() {
                eprintln!("[reactor] deregister token={} fd={}", token, state.fd);
            }
        }
        Ok(())
    }

    /// Poll for readiness in a specific direction.
    /// Returns true if ready, false if pending (waker stored).
    pub fn poll_ready(&mut self, token: usize, direction: u8, waker: &Waker) -> bool {
        let state = match self.io_states.get_mut(&token) {
            Some(s) => s,
            None => return true,  // Already deregistered, treat as ready
        };

        let mask = if direction == DIR_READ { READABLE } else { WRITABLE };

        // Check if already ready
        if (state.readiness & mask) != 0 {
            if debug_enabled() {
                eprintln!("[reactor] poll_ready token={} dir={} -> already ready", token, direction);
            }
            return true;
        }

        // Not ready - store waker
        let waker_slot = if direction == DIR_READ {
            &mut state.read_waker
        } else {
            &mut state.write_waker
        };

        match waker_slot {
            Some(existing) => existing.clone_from(waker),
            None => *waker_slot = Some(waker.clone()),
        }

        if debug_enabled() {
            eprintln!("[reactor] poll_ready token={} dir={} -> pending (waker stored)", token, direction);
        }

        false
    }

    /// Clear readiness for a direction (called after WouldBlock).
    pub fn clear_ready(&mut self, token: usize, direction: u8) {
        if let Some(state) = self.io_states.get_mut(&token) {
            let mask = if direction == DIR_READ { READABLE } else { WRITABLE };
            state.readiness &= !mask;
            
            if debug_enabled() {
                eprintln!("[reactor] clear_ready token={} dir={} -> readiness={:02b}", 
                         token, direction, state.readiness);
            }
        }
    }

    /// Poll the OS for IO events and wake tasks.
    /// Called from tau_drive().
    pub fn poll(&mut self, timeout: Option<Duration>) -> io::Result<usize> {
        self.events.clear();
        
        // Wait for events (or timeout)
        self.poller.wait(&mut self.events, timeout)?;
        
        let mut woken = 0usize;

        for ev in self.events.iter() {
            let token = ev.key;
            
            if let Some(state) = self.io_states.get_mut(&token) {
                // Update readiness
                if ev.readable {
                    state.readiness |= READABLE;
                }
                if ev.writable {
                    state.readiness |= WRITABLE;
                }

                if debug_enabled() && (ev.readable || ev.writable) {
                    eprintln!("[reactor] event token={} readable={} writable={}", 
                             token, ev.readable, ev.writable);
                }

                // Wake tasks
                if ev.readable {
                    if let Some(waker) = state.read_waker.take() {
                        waker.wake();
                        woken += 1;
                    }
                }
                if ev.writable {
                    if let Some(waker) = state.write_waker.take() {
                        waker.wake();
                        woken += 1;
                    }
                }

                // Re-arm the fd for level-triggered mode
                let readable = (state.interest & READABLE) != 0;
                let writable = (state.interest & WRITABLE) != 0;
                unsafe {
                    let borrowed = BorrowedFd::borrow_raw(state.fd);
                    let _ = self.poller.modify_with_mode(
                        &borrowed,
                        Event::new(token, readable, writable),
                        PollMode::Level,
                    );
                }
            }
        }

        Ok(woken)
    }
}

// =============================================================================
// Global reactor instance (static, survives TLS destruction)
// =============================================================================

use std::sync::{Mutex, OnceLock};

static REACTOR: OnceLock<Mutex<Reactor>> = OnceLock::new();

pub fn init_reactor() -> io::Result<()> {
    REACTOR.get_or_init(|| {
        Mutex::new(Reactor::new().expect("failed to create reactor"))
    });
    Ok(())
}

pub fn with_reactor<F, R>(f: F) -> R
where
    F: FnOnce(&mut Reactor) -> R,
{
    let reactor = REACTOR.get().expect("reactor not initialized");
    let mut guard = reactor.lock().unwrap();
    f(&mut guard)
}
