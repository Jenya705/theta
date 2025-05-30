use bevy::log;

#[derive(Debug)]
pub(crate) struct PacketOrderer {
    vec: Vec<Option<Vec<u8>>>,
    order_start: usize,
    deque_start: usize,
    capacity: usize,
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum PacketOrdererError {
    #[error("Reached the maximum capacity")]
    CapacityReached,
}

impl PacketOrderer {
    pub fn new(capacity: usize) -> Self {
        Self {
            vec: vec![],
            order_start: 0,
            deque_start: 0,
            capacity,
        }
    }

    pub fn push(&mut self, order: usize, packet: Vec<u8>) -> Result<(), PacketOrdererError> {
        // order_start = O
        // deque_start = D

        // O+4 O+5 O+6 O+7 O(D) O+1 O+2 O+3
        let order_dist_minus_1 = order.wrapping_sub(self.order_start);

        if order_dist_minus_1 >= self.capacity {
            return Err(PacketOrdererError::CapacityReached);
        }

        if order_dist_minus_1 >= self.vec.len() {
            self.flatten(order_dist_minus_1 + 1);
            for _ in self.vec.len()..=order_dist_minus_1 {
                self.vec.push(None);
            }
        }

        let l = self.vec.len();

        if self.vec[(self.deque_start.wrapping_add(order_dist_minus_1)) % l]
            .replace(packet)
            .is_some()
        {
            log::warn!("Got a duplicate packet with order {:?} (ignoring it)", order);
        }

        Ok(())
    }

    pub fn flatten(&mut self, new_capacity: usize) {
        // arleady flatten
        if self.deque_start == 0 {
            return;
        }
        // O+4 O+5 O+6 O+7 O(D) O+1 O+2 O+3
        // O(D) O+1 O+2 ... O+7
        let mut new_vec = Vec::with_capacity(new_capacity);
        new_vec.extend_from_slice(&self.vec[self.deque_start..]);
        new_vec.extend_from_slice(&self.vec[0..self.deque_start]);
        self.deque_start = 0;
        self.vec = new_vec;
    }

    pub fn retain<E>(&mut self, mut func: impl FnMut(Vec<u8>) -> Result<(), E>) -> Result<(), E> {
        while let Some(element) = self.vec[self.deque_start].take() {
            func(element)?;
            self.order_start = self.order_start.wrapping_add(1);
            self.deque_start = (self.deque_start + 1) % self.vec.len();
        }
        Ok(())
    }
}
