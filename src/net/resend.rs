use std::num::NonZero;

use crate::util::slot_map::{SlotMap, SlotMapKey};

#[derive(Default)]
pub(crate) struct PacketResender {
    packets: SlotMap<ReliableBufferedPacket>,
    capacity: usize,
}

#[derive(Debug)]
pub(crate) struct ReliableBufferedPacket {
    pub payload: Vec<u8>,
    resend_interval: u32,
    resend_attempts: u32,
    current_time: u32,
}

impl ReliableBufferedPacket {
    pub fn new(payload: Vec<u8>, resend_interval: NonZero<u32>, resend_attempts: u32) -> Self {
        Self {
            payload,
            resend_interval: resend_interval.get(),
            resend_attempts: resend_attempts,
            current_time: 0,
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum PacketResenderError {
    #[error("Reached the maximum capacity")]
    CapacityReached,
}

impl PacketResender {
    pub fn mut_capacity(&mut self) -> &mut usize {
        &mut self.capacity
    }

    pub fn add(
        &mut self,
        func: impl FnOnce(SlotMapKey) -> ReliableBufferedPacket,
    ) -> Result<SlotMapKey, PacketResenderError> {
        if self.packets.len() == self.capacity {
            Err(PacketResenderError::CapacityReached)
        } else {
            Ok(self.packets.add(|key| Some(func(key))))
        }
    }

    pub fn update(&mut self, delta: u32, mut func: impl FnMut(&ReliableBufferedPacket)) {
        self.packets.retain_if(|packet| {
            packet.current_time += delta;
            if packet.current_time >= packet.resend_interval {
                packet.current_time = 0;
                if packet.resend_attempts != 0 {
                    packet.resend_attempts -= 1;
                    func(packet);
                }
                packet.resend_attempts == 0
            } else {
                false
            }
        });
    }

    pub fn remove(&mut self, id: SlotMapKey) -> Option<ReliableBufferedPacket> {
        self.packets.remove(id)
    }

    pub fn map(&self) -> &SlotMap<ReliableBufferedPacket> {
        &self.packets
    }

    pub fn map_mut(&mut self) -> &mut SlotMap<ReliableBufferedPacket> {
        &mut self.packets
    }
}
