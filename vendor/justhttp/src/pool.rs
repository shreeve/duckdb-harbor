//! Cross-thread scheduling: the accept-side task pool and the message
//! queue behind `recv()`.

pub use messages_queue::MessagesQueue;
pub use task_pool::TaskPool;

mod messages_queue {
    use std::collections::VecDeque;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};
    
    enum Control<T> {
        Elem(T),
        Unblock,
    }
    
    pub struct MessagesQueue<T>
    where
        T: Send,
    {
        queue: Mutex<VecDeque<Control<T>>>,
        condvar: Condvar,
    }
    
    impl<T> MessagesQueue<T>
    where
        T: Send,
    {
        pub fn with_capacity(capacity: usize) -> Arc<MessagesQueue<T>> {
            Arc::new(MessagesQueue {
                queue: Mutex::new(VecDeque::with_capacity(capacity)),
                condvar: Condvar::new(),
            })
        }
    
        /// Pushes an element to the queue.
        pub fn push(&self, value: T) {
            let mut queue = self.queue.lock().unwrap();
            queue.push_back(Control::Elem(value));
            self.condvar.notify_one();
        }
    
        /// Unblock one thread stuck in pop loop.
        pub fn unblock(&self) {
            let mut queue = self.queue.lock().unwrap();
            queue.push_back(Control::Unblock);
            self.condvar.notify_one();
        }
    
        /// Pops an element. Blocks until one is available.
        /// Returns None in case unblock() was issued.
        pub fn pop(&self) -> Option<T> {
            let mut queue = self.queue.lock().unwrap();
    
            loop {
                match queue.pop_front() {
                    Some(Control::Elem(value)) => return Some(value),
                    Some(Control::Unblock) => return None,
                    None => (),
                }
    
                queue = self.condvar.wait(queue).unwrap();
            }
        }
    
        /// Tries to pop an element without blocking.
        pub fn try_pop(&self) -> Option<T> {
            let mut queue = self.queue.lock().unwrap();
            match queue.pop_front() {
                Some(Control::Elem(value)) => Some(value),
                Some(Control::Unblock) | None => None,
            }
        }
    
        /// Tries to pop an element without blocking
        /// more than the specified timeout duration
        /// or unblock() was issued
        pub fn pop_timeout(&self, timeout: Duration) -> Option<T> {
            let mut queue = self.queue.lock().unwrap();
            let mut duration = timeout;
            loop {
                match queue.pop_front() {
                    Some(Control::Elem(value)) => return Some(value),
                    Some(Control::Unblock) => return None,
                    None => (),
                }
                let now = Instant::now();
                let (_queue, result) = self.condvar.wait_timeout(queue, timeout).unwrap();
                queue = _queue;
                let sleep_time = now.elapsed();
                duration = if duration > sleep_time {
                    duration - sleep_time
                } else {
                    Duration::from_millis(0)
                };
                if result.timed_out()
                    || (duration.as_secs() == 0 && duration.subsec_nanos() < 1_000_000)
                {
                    return None;
                }
            }
        }
    }
}

mod task_pool {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use std::time::Duration;
    
    /// Manages a collection of threads.
    ///
    /// A new thread is created every time all the existing threads are full.
    /// Any idle thread will automatically die after a few seconds.
    pub struct TaskPool {
        sharing: Arc<Sharing>,
    }
    
    struct Sharing {
        // list of the tasks to be done by worker threads
        todo: Mutex<VecDeque<Box<dyn FnMut() + Send>>>,
    
        // condvar that will be notified whenever a task is added to `todo`
        condvar: Condvar,
    
        // number of total worker threads running
        active_tasks: AtomicUsize,
    
        // number of idle worker threads
        waiting_tasks: AtomicUsize,
    }
    
    /// Minimum number of active threads.
    const MIN_THREADS: usize = 4;
    
    struct Registration<'a> {
        nb: &'a AtomicUsize,
    }
    
    impl<'a> Registration<'a> {
        fn new(nb: &'a AtomicUsize) -> Registration<'a> {
            nb.fetch_add(1, Ordering::Release);
            Registration { nb }
        }
    }
    
    impl<'a> Drop for Registration<'a> {
        fn drop(&mut self) {
            self.nb.fetch_sub(1, Ordering::Release);
        }
    }
    
    impl TaskPool {
        pub fn new() -> TaskPool {
            let pool = TaskPool {
                sharing: Arc::new(Sharing {
                    todo: Mutex::new(VecDeque::new()),
                    condvar: Condvar::new(),
                    active_tasks: AtomicUsize::new(0),
                    waiting_tasks: AtomicUsize::new(0),
                }),
            };
    
            for _ in 0..MIN_THREADS {
                pool.add_thread(None)
            }
    
            pool
        }
    
        /// Executes a function in a thread.
        /// If no thread is available, spawns a new one.
        pub fn spawn(&self, code: Box<dyn FnMut() + Send>) {
            let mut queue = self.sharing.todo.lock().unwrap();
    
            if self.sharing.waiting_tasks.load(Ordering::Acquire) == 0 {
                self.add_thread(Some(code));
            } else {
                queue.push_back(code);
                self.sharing.condvar.notify_one();
            }
        }
    
        fn add_thread(&self, initial_fn: Option<Box<dyn FnMut() + Send>>) {
            let sharing = self.sharing.clone();
    
            thread::spawn(move || {
                let sharing = sharing;
                let _active_guard = Registration::new(&sharing.active_tasks);
    
                if let Some(mut f) = initial_fn {
                    f();
                }
    
                loop {
                    let mut task: Box<dyn FnMut() + Send> = {
                        let mut todo = sharing.todo.lock().unwrap();
    
                        let task;
                        loop {
                            if let Some(poped_task) = todo.pop_front() {
                                task = poped_task;
                                break;
                            }
                            let _waiting_guard = Registration::new(&sharing.waiting_tasks);
    
                            let received =
                                if sharing.active_tasks.load(Ordering::Acquire) <= MIN_THREADS {
                                    todo = sharing.condvar.wait(todo).unwrap();
                                    true
                                } else {
                                    let (new_lock, waitres) = sharing
                                        .condvar
                                        .wait_timeout(todo, Duration::from_secs(5))
                                        .unwrap();
                                    todo = new_lock;
                                    !waitres.timed_out()
                                };
    
                            if !received && todo.is_empty() {
                                return;
                            }
                        }
    
                        task
                    };
    
                    task();
                }
            });
        }
    }
    
    impl Drop for TaskPool {
        fn drop(&mut self) {
            self.sharing
                .active_tasks
                .store(999_999_999, Ordering::Release);
            self.sharing.condvar.notify_all();
        }
    }
}
