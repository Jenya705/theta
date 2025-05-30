use std::{collections::VecDeque, time::Duration};

use parking_lot::{Condvar, Mutex};

pub struct ManyToSingleChannel<T> {
    values: Mutex<VecDeque<T>>,
    condvar: Condvar,
}

impl<T> ManyToSingleChannel<T> {
    pub fn new() -> Self {
        Self {
            values: Mutex::new(VecDeque::new()),
            condvar: Condvar::new(),
        }
    }

    pub fn push(&self, value: T) {
        self.values.lock().push_back(value);
        self.condvar.notify_all();
    }

    pub fn push_many(&self, values: impl Iterator<Item = T>) {
        let mut guard = self.values.lock();
        for value in values {
            guard.push_back(value)
        }
        self.condvar.notify_all();
    }

    pub fn wait_and_take_bunch(&self, bunch: &mut Vec<T>, timeout: Duration) {
        let mut guard = self.values.lock();

        if !timeout.is_zero() {
            self.condvar
                .wait_while_for(&mut guard, |values| values.is_empty(), timeout);
        }

        loop {
            if bunch.capacity() == bunch.len() {
                break;
            }

            match guard.pop_front() {
                Some(value) => bunch.push(value),
                None => {
                    break;
                }
            }
        }
    }
}
