use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    JoinHandle {
        _marker: std::marker::PhantomData,
    }
}

pub struct JoinHandle<T> {
    _marker: std::marker::PhantomData<T>,
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}

#[derive(Debug)]
pub struct JoinError;

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "task join error")
    }
}

impl std::error::Error for JoinError {}

pub mod time {
    use super::*;

    pub fn sleep(duration: Duration) -> Sleep {
        Sleep { _duration: duration }
    }

    pub struct Sleep {
        _duration: Duration,
    }

    impl Future for Sleep {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(())
        }
    }
}

pub mod sync {
    pub use std::sync::Mutex;
    
    pub mod mpsc {
        use std::sync::mpsc as std_mpsc;
        
        pub fn channel<T>(_buffer: usize) -> (Sender<T>, Receiver<T>) {
            let (tx, rx) = std_mpsc::channel();
            (Sender(tx), Receiver(rx))
        }
        
        pub struct Sender<T>(std_mpsc::Sender<T>);
        pub struct Receiver<T>(std_mpsc::Receiver<T>);
        
        impl<T> Sender<T> {
            pub fn send(&self, value: T) -> Result<(), SendError<T>> {
                self.0.send(value).map_err(|e| SendError(e.0))
            }
        }
        
        pub struct SendError<T>(pub T);
    }
}

pub mod task {
    pub async fn yield_now() {}
}

pub use time::sleep;

#[doc(hidden)]
pub fn _taukio_init() {}
