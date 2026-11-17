// The work pool: a few threads that run heavy jobs (`Reply::Later`) so that an I/O
// thread never has to. Its size is the limit on how many jobs run at the same time.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crate::reactor::{Mail, Mailbox};
use crate::session::Job;

/// One job, and where its result goes: to connection `slot` (if `id` still lives
/// there) on the I/O thread that owns `mailbox`.
pub struct Task {
    pub mailbox: Arc<Mailbox>,
    pub slot: usize,
    pub id: u64,
    pub job: Job,
}

pub struct Pool {
    queue: Mutex<VecDeque<Task>>,
    waiting: Condvar,
}

impl Pool {
    /// Starts `threads` workers. They live as long as the program does.
    pub fn start(threads: usize) -> std::io::Result<Arc<Pool>> {
        let pool = Arc::new(Pool { queue: Mutex::new(VecDeque::new()), waiting: Condvar::new() });
        for n in 0..threads {
            let pool = pool.clone();
            thread::Builder::new().name(format!("work-{n}")).spawn(move || pool.run())?;
        }
        Ok(pool)
    }

    /// Queues a job. Jobs start in the order they arrive, as threads come free.
    pub fn submit(&self, task: Task) {
        self.queue.lock().unwrap().push_back(task);
        self.waiting.notify_one();
    }

    fn run(&self) {
        loop {
            let task = {
                let mut queue = self.queue.lock().unwrap();
                loop {
                    match queue.pop_front() {
                        Some(task) => break task,
                        None => queue = self.waiting.wait(queue).unwrap(),
                    }
                }
            };
            // The lock is released: the job runs alone on this thread, for as long as it takes.
            let mut body = Vec::new();
            let (status, kind) = (task.job)(&mut body);
            task.mailbox.post(Mail::Done { slot: task.slot, id: task.id, status, kind, body });
        }
    }
}
