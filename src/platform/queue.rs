//! Priority queue shared by the main-thread and background executors.
//! (GPUI's own queue is only exported on Linux, Windows and wasm.)

use std::{
    collections::VecDeque,
    sync::{Condvar, Mutex},
};

use gpui::Priority;

pub(crate) struct PriorityQueue<T> {
    /// High, medium and low priority items, popped in that order.
    queues: Mutex<[VecDeque<T>; 3]>,
    condvar: Condvar,
}

impl<T> Default for PriorityQueue<T> {
    fn default() -> Self {
        Self {
            queues: Mutex::new([VecDeque::new(), VecDeque::new(), VecDeque::new()]),
            condvar: Condvar::new(),
        }
    }
}

fn index(priority: Priority) -> usize {
    match priority {
        Priority::Medium => 1,
        Priority::Low => 2,
        // Realtime work gets its own thread (`spawn_realtime`); anything
        // queued with it is at least high priority.
        _ => 0,
    }
}

impl<T> PriorityQueue<T> {
    pub(crate) fn push(&self, priority: Priority, item: T) {
        self.queues.lock().unwrap()[index(priority)].push_back(item);
        self.condvar.notify_one();
    }

    pub(crate) fn try_pop(&self) -> Option<T> {
        let mut queues = self.queues.lock().unwrap();
        queues.iter_mut().find_map(VecDeque::pop_front)
    }

    /// Blocks until an item is available.
    pub(crate) fn pop(&self) -> T {
        let mut queues = self.queues.lock().unwrap();
        loop {
            if let Some(item) = queues.iter_mut().find_map(VecDeque::pop_front) {
                return item;
            }
            queues = self.condvar.wait(queues).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pops_by_priority_then_fifo() {
        let queue = PriorityQueue::default();
        queue.push(Priority::Low, "low");
        queue.push(Priority::Medium, "medium-1");
        queue.push(Priority::High, "high");
        queue.push(Priority::Medium, "medium-2");
        let order: Vec<_> = std::iter::from_fn(|| queue.try_pop()).collect();
        assert_eq!(order, ["high", "medium-1", "medium-2", "low"]);
    }

    #[test]
    fn blocking_pop_wakes_on_push() {
        let queue = std::sync::Arc::new(PriorityQueue::default());
        let popper = std::thread::spawn({
            let queue = queue.clone();
            move || queue.pop()
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        queue.push(Priority::Medium, 7);
        assert_eq!(popper.join().unwrap(), 7);
    }
}
