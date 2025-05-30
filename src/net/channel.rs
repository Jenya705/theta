use std::num::NonZero;

use bevy::platform_support::collections::HashMap;

pub struct ChannelRegistry {
    pub channels: HashMap<String, Channel>,
}

#[derive(Debug)]
pub struct Channel {
    pub reliability: Option<ReliableChannel>,
    pub orderability: Option<OrderedChannel>,
    pub frequency: ChannelFrequency,
}

impl Channel {
    pub fn new(frequency: ChannelFrequency) -> Self {
        Self {
            reliability: None,
            orderability: None,
            frequency,
        }
    }

    pub fn with_reliability(self, reliable_channel: ReliableChannel) -> Self {
        assert!(self.reliability.is_none());
        Self {
            reliability: Some(reliable_channel),
            ..self
        }
    }

    pub fn with_orderability(self, ordered_channel: OrderedChannel) -> Self {
        assert!(self.orderability.is_none());
        Self {
            orderability: Some(ordered_channel),
            ..self
        }
    }

    pub fn is_ordered(&self) -> bool {
        self.orderability.is_some()
    }

    pub fn is_reliable(&self) -> bool {
        self.reliability.is_some()
    }
}

#[derive(Debug)]
pub struct ReliableChannel {
    pub resend_interval: NonZero<u32>,
    /// when resend_attempts is 0, then packets of this channel will get a unique
    /// resend key, which could be used by the other client to detect duplicates of this packet
    /// but the packet itself won't be resent, thus making it kind of packet that is unreliable
    /// but will only be handled once
    pub resend_attempts: u32,
    pub additional_capacity: usize,
}

#[derive(Debug)]
pub struct OrderedChannel {
    pub capacity: NonZero<usize>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct ChannelFrequency(pub u32);

impl ChannelFrequency {
    pub const HIGH: Self = Self(1 << 20);
    pub const MID: Self = Self(1 << 16);
    pub const LOW: Self = Self(1 << 12);
    pub const NEVER: Self = Self(0);
}

impl ChannelRegistry {}
