//! Single-threaded executor implementation

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
// use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use tau::types::{FfiPoll, FfiWaker, RawDropFn, RawPollFn, TaskId};

/// A spawned task
struct Task {
    id: TaskId,
    future_ptr: *mut (),
    poll_fn: RawPollFn,
    drop_fn: RawDropFn,
    completed: bool,
}

impl Drop for Task {
    fn drop(&mut self) {
        if !self.future_ptr.is_null() {
            unsafe { (self.drop_fn)(self.future_ptr) };
        }
    }
}

/// The single-threaded runtime
pub struct Runtime {
    /// All tasks by ID
    tasks: HashMap<TaskId, Task>,
    /// Queue of tasks ready to be polled
    ready_queue: VecDeque<TaskId>,
    /// Whether runtime is initialized
    initialized: bool,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            tasks: HashMap::new(),
            ready_queue: VecDeque::new(),
            initialized: false,
        }
    }
}

impl Runtime {
    pub fn init(&mut self) {
        self.tasks.clear();
        self.ready_queue.clear();
        self.initialized = true;
    }

    pub fn spawn(
        &mut self,
        task_id: TaskId,
        future_ptr: *mut (),
        poll_fn: RawPollFn,
        drop_fn: RawDropFn,
    ) {
        let task = Task {
            id: task_id,
            future_ptr,
            poll_fn,
            drop_fn,
            completed: false,
        };

        self.tasks.insert(task_id, task);
        self.ready_queue.push_back(task_id);
    }

    pub fn poll_task(&mut self, task_id: TaskId, waker: FfiWaker) -> FfiPoll {
        if let Some(task) = self.tasks.get_mut(&task_id) {
            if task.completed {
                return FfiPoll::Ready;
            }

            let result = unsafe { (task.poll_fn)(task.future_ptr, waker) };

            if result == FfiPoll::Ready {
                task.completed = true;
            }

            result
        } else {
            FfiPoll::Ready // Task not found, consider it complete
        }
    }

    pub fn block_on(&mut self, task_id: TaskId) {
        // Create a simple waker that adds task back to ready queue
        let waker = self.create_waker(task_id);

        loop {
            // Poll the target task
            let result = self.poll_task(task_id, waker);

            if result == FfiPoll::Ready {
                // Task complete, remove it
                self.tasks.remove(&task_id);
                return;
            }

            // Drive other tasks
            self.drive();
        }
    }

    pub fn drive(&mut self) -> u32 {
        let mut completed = 0;

        // Process ready queue
        let to_poll: Vec<TaskId> = self.ready_queue.drain(..).collect();

        for task_id in to_poll {
            if let Some(task) = self.tasks.get(&task_id) {
                if task.completed {
                    continue;
                }

                let waker = self.create_waker(task_id);
                let result = unsafe { (task.poll_fn)(task.future_ptr, waker) };

                if result == FfiPoll::Ready {
                    if let Some(task) = self.tasks.get_mut(&task_id) {
                        task.completed = true;
                        completed += 1;
                    }
                }
            }
        }

        completed
    }

    fn create_waker(&self, task_id: TaskId) -> FfiWaker {
        // For simplicity, create a waker that does nothing
        // A real implementation would wake the runtime
        FfiWaker {
            data: task_id.0 as *mut (),
            wake_fn: Some(noop_wake),
        }
    }
}

extern "C" fn noop_wake(_: *mut ()) {
    // In a real impl, this would signal the runtime to poll this task
}

thread_local! {
    pub static RUNTIME: RefCell<Runtime> = RefCell::new(Runtime::default());
}
