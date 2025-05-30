use std::{
    borrow::Cow, fmt::Debug, marker::PhantomData, num::NonZero, ops::DerefMut, sync::Arc,
    time::Instant, u32,
};

use bevy::{log, platform_support::collections::HashMap};
use parking_lot::{Mutex, MutexGuard, RwLock};

use crate::{
    net::resend::ReliableBufferedPacket,
    proto::{
        StrWithMaxLen, VarInt,
        decode::{Decode, Decoder},
        encode::{Encode, Encoder},
    },
    util::slot_map::{SlotMapKey, SlotSet, SlotSetInsertError},
};

use super::{
    channel::{Channel, ChannelFrequency, OrderedChannel, ReliableChannel},
    ordering::{PacketOrderer, PacketOrdererError},
    resend::{PacketResender, PacketResenderError},
    segment::{
        PacketSegmentJoiner, PacketSegmentJoinerError, PacketSegmentMetadata,
        PacketSegmentMetadataIntType,
    },
};

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct ChannelId<T>(pub(crate) u32, pub(crate) PhantomData<T>);

#[derive(PartialEq, Eq, Clone, Copy, Debug, Default)]
pub struct UniqueChannelId<const LOCAL: bool>;

#[derive(PartialEq, Eq, Clone, Copy, Debug, Default)]
pub struct GlobalChannelId;

impl<T: PartialEq + Eq + Clone + Copy + Debug> Default for ChannelId<T> {
    fn default() -> Self {
        Self::NULL
    }
}

impl<T: PartialEq + Eq + Clone + Copy + Debug> ChannelId<T> {
    const NULL: Self = Self::new(NULL_CHANNEL_ID);

    pub(crate) const fn new(val: u32) -> Self {
        Self(val, PhantomData)
    }

    pub fn is_null(&self) -> bool {
        *self == Self::NULL
    }
}

pub trait ChannelProvider {
    fn by_id(&self, id: ChannelId<GlobalChannelId>) -> Option<&Channel>;

    fn by_name(&self, name: &str) -> Option<ChannelId<GlobalChannelId>>;

    fn all(&self) -> impl Iterator<Item = (&str, ChannelId<GlobalChannelId>)>;
}

pub struct DefaultChannelProvider {
    pub channels: Vec<Channel>,
    pub name_to_id: HashMap<String, ChannelId<GlobalChannelId>>,
}

impl DefaultChannelProvider {
    pub fn new() -> Self {
        let mut this = Self {
            channels: vec![],
            name_to_id: HashMap::default(),
        };

        this.add(ACK_CHANNEL_NAME.into(), ACK_CHANNEL);
        this.add(
            CHANNEL_MANAGEMENT_CHANNEL_NAME.into(),
            CHANNEL_MANAGEMENT_CHANNEL,
        );

        this
    }

    pub fn add(&mut self, name: String, channel: Channel) -> ChannelId<GlobalChannelId> {
        let id = ChannelId::new(self.channels.len() as u32);
        self.channels.push(channel);
        self.name_to_id.insert(name, id);
        id
    }
}

impl ChannelProvider for DefaultChannelProvider {
    fn by_id(&self, id: ChannelId<GlobalChannelId>) -> Option<&Channel> {
        self.channels.get(id.0 as usize)
    }

    fn by_name(&self, name: &str) -> Option<ChannelId<GlobalChannelId>> {
        self.name_to_id.get(name).cloned()
    }

    fn all(&self) -> impl Iterator<Item = (&str, ChannelId<GlobalChannelId>)> {
        self.name_to_id
            .iter()
            .map(|(name, id)| (name.as_str(), *id))
    }
}

pub(crate) trait PacketSender {
    fn send_packet(&mut self, packet: Cow<[u8]>) -> Result<(), ()>;
}

#[derive(Clone, Copy, Debug)]
pub enum ConnectionState {
    Registration {
        received_last_packet: bool,
        last_packet_resend_key: Option<SlotMapKey>,
    },
    Transfer,
}

impl ConnectionState {
    pub fn convert_to_transfer_if_possible(&mut self) -> bool {
        if matches!(
            self,
            ConnectionState::Registration {
                received_last_packet: true,
                last_packet_resend_key: None
            }
        ) {
            *self = Self::Transfer;
            true
        } else {
            false
        }
    }
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self::Registration {
            received_last_packet: false,
            last_packet_resend_key: None,
        }
    }
}

#[derive(Default)]
pub(crate) struct ConnectionIdMapper {
    pub local_inner: ConnectionIdMapperInner<true>,
    pub not_local_inner: ConnectionIdMapperInner<false>,
}

#[derive(Default)]
pub struct ConnectionIdMapperInner<const LOCAL: bool> {
    global_id_to_local_id: Vec<ChannelId<UniqueChannelId<LOCAL>>>,
    local_id_to_global_id: Vec<ChannelId<GlobalChannelId>>,
    ack_channel_id: ChannelId<UniqueChannelId<LOCAL>>,
    channel_management_channel_id: ChannelId<UniqueChannelId<LOCAL>>,
}

impl<const LOCAL: bool> ConnectionIdMapperInner<LOCAL> {
    pub fn local_id(
        &self,
        state: ConnectionState,
        global_id: ChannelId<GlobalChannelId>,
    ) -> Option<ChannelId<UniqueChannelId<LOCAL>>> {
        match state {
            ConnectionState::Registration { .. } => match global_id.0 {
                ACK_CHANNEL_ID_REGISTRATION_STAGE
                | CHANNEL_MANAGEMENT_CHANNEL_ID_REGISTRATION_STAGE => {
                    Some(ChannelId::new(global_id.0))
                }
                _ => None,
            },
            ConnectionState::Transfer => self
                .global_id_to_local_id
                .get(global_id.0 as usize)
                .filter(|id| !id.is_null())
                .cloned(),
        }
    }

    pub fn global_id(
        &self,
        state: ConnectionState,
        local_id: ChannelId<UniqueChannelId<LOCAL>>,
    ) -> Option<ChannelId<GlobalChannelId>> {
        match state {
            ConnectionState::Registration { .. } => match local_id.0 {
                ACK_CHANNEL_ID_REGISTRATION_STAGE
                | CHANNEL_MANAGEMENT_CHANNEL_ID_REGISTRATION_STAGE => {
                    Some(ChannelId::new(local_id.0))
                }
                _ => None,
            },
            ConnectionState::Transfer => {
                self.local_id_to_global_id.get(local_id.0 as usize).cloned()
            }
        }
    }

    /// Maps the given local id to the given global id.
    /// It shifts all other ids in case of the given local_id being already occupied.
    /// If the given global id repeats, then it removes it from the map and adds according to the rules.
    ///
    /// Returns whether the local id lies somewhere where it could be (i.e. local_id <= len of the vec), thus
    /// if false is returned, nothing is changed.
    pub fn insert(
        &mut self,
        local_id: ChannelId<UniqueChannelId<LOCAL>>,
        global_id: ChannelId<GlobalChannelId>,
        name: &str,
    ) -> bool {
        if local_id.0 as usize > self.local_id_to_global_id.len() {
            return false;
        }

        self.local_id_to_global_id
            .insert(local_id.0 as usize, global_id);

        for id in [
            &mut self.ack_channel_id,
            &mut self.channel_management_channel_id,
        ] {
            if !id.is_null() && id.0 > local_id.0 {
                id.0 += 1;
            }
        }

        if self.global_id_to_local_id.len() <= global_id.0 as usize {
            self.global_id_to_local_id
                .resize(global_id.0 as usize + 1, ChannelId::NULL);
        }

        self.global_id_to_local_id[global_id.0 as usize] = local_id;

        match name {
            ACK_CHANNEL_NAME => {
                self.ack_channel_id = local_id;
            }
            CHANNEL_MANAGEMENT_CHANNEL_NAME => {
                self.channel_management_channel_id = local_id;
            }
            _ => {}
        }

        true
    }

    pub fn ack_channel_id(&self) -> Option<ChannelId<UniqueChannelId<LOCAL>>> {
        Some(self.ack_channel_id).filter(|v| !v.is_null())
    }

    pub fn channel_management_channel_id(&self) -> Option<ChannelId<UniqueChannelId<LOCAL>>> {
        Some(self.channel_management_channel_id).filter(|v| !v.is_null())
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ConnectionChannel<'a, const LOCAL: bool> {
    pub local_id: ChannelId<UniqueChannelId<LOCAL>>,
    pub channel: &'a Channel,
}

impl<'a, const LOCAL: bool> ConnectionChannel<'a, LOCAL> {
    pub fn new(
        state: ConnectionState,
        local_id: ChannelId<UniqueChannelId<LOCAL>>,
        global_id: ChannelId<GlobalChannelId>,
        provider: &'a impl ChannelProvider,
    ) -> Option<ConnectionChannel<'a, LOCAL>> {
        match state {
            ConnectionState::Registration { .. } => match local_id.0 {
                ACK_CHANNEL_ID_REGISTRATION_STAGE => Some(&ACK_CHANNEL),
                CHANNEL_MANAGEMENT_CHANNEL_ID_REGISTRATION_STAGE => {
                    Some(&CHANNEL_MANAGEMENT_CHANNEL)
                }
                _ => None,
            },
            ConnectionState::Transfer => provider.by_id(global_id),
        }
        .map(|channel| Self { local_id, channel })
    }
}

#[derive(Default)]
pub(crate) struct ConnectionAcknowledgeBuffer {
    buf: Vec<u8>,
}

impl ConnectionAcknowledgeBuffer {
    pub fn flush(
        &mut self,
        state: ConnectionState,
        ack_channel_id: ChannelId<UniqueChannelId<true>>,
        resender: &mut PacketResender,
        segment_keys_counter: &mut u32,
        orders: &mut ChannelData<u32, UniqueChannelId<true>>,
        sender: &mut impl PacketSender,
    ) -> Result<(), ConnectionError> {
        // nothing to flush
        if self.buf.is_empty() {
            // still do nothing, because we want to have a keep alive packet
        }

        let local_id = match state {
            ConnectionState::Registration { .. } => {
                ChannelId::new(ACK_CHANNEL_ID_REGISTRATION_STAGE)
            }
            ConnectionState::Transfer => ack_channel_id,
        };

        let channel = ConnectionChannel {
            local_id,
            // we could get it from the mapper, but at the end of a day the channel must be always ACK_CHANNEL
            channel: &ACK_CHANNEL,
        };

        send_packet(
            channel,
            &self.buf,
            state,
            resender,
            segment_keys_counter,
            orders,
            sender,
            None,
        )?;

        self.buf.clear();

        Ok(())
    }

    pub fn acknowledge(
        &mut self,
        key: SlotMapKey,
        state: ConnectionState,
        ack_channel_id: ChannelId<UniqueChannelId<true>>,
        resender: &mut PacketResender,
        segment_keys_counter: &mut u32,
        orders: &mut ChannelData<u32, UniqueChannelId<true>>,
        sender: &mut impl PacketSender,
    ) -> Result<(), ConnectionError> {
        let buf_len = self.buf.len();

        key.encode(&mut self.buf)
            .map_err(|_| ConnectionError::EncodeError)?;

        if self.buf.len() > MAX_NON_SEGMENTED_PAYLOAD_LEN {
            self.buf.resize(buf_len, 0);
            self.flush(
                state,
                ack_channel_id,
                resender,
                segment_keys_counter,
                orders,
                sender,
            )?;
            key.encode(&mut self.buf)
                .map_err(|_| ConnectionError::EncodeError)?;
        }

        Ok(())
    }

    fn clear(&mut self) {
        self.buf.clear();
    }
}

pub(crate) fn send_packet(
    channel: ConnectionChannel<true>,
    payload: &[u8],
    state: ConnectionState,
    resender: &mut PacketResender,
    segment_keys_counter: &mut u32,
    orders: &mut ChannelData<u32, UniqueChannelId<true>>,
    sender: &mut impl PacketSender,
    mut resend_keys: Option<&mut Vec<SlotMapKey>>,
) -> Result<(), ConnectionError> {
    let order = if channel.channel.is_ordered() {
        let order = orders.get_mut(state, channel.local_id);
        let v = *order;
        *order = order.wrapping_add(1);
        Some(v)
    } else {
        None
    };

    let mut send_real_packet =
        |segment: Option<PacketSegmentMetadata>, payload: &[u8]| -> Result<(), ConnectionError> {
            debug_assert!(payload.len() <= MAX_PACKET_LEN);

            let mut to_send = vec![];

            PrimaryPacketMetadata {
                channel_id: channel.local_id,
                segment: segment.is_some(),
            }
            .encode(&mut to_send)
            .map_err(|_| ConnectionError::EncodeError)?;

            if let Some(segment) = segment {
                segment
                    .encode(&mut to_send)
                    .map_err(|_| ConnectionError::EncodeError)?;
            }

            if let Some(order) = order {
                VarInt(order)
                    .encode(&mut to_send)
                    .map_err(|_| ConnectionError::EncodeError)?;
            }

            if let Some(ref reliable_channel) = channel.channel.reliability {
                let key = resender.add(|key| {
                    if let Some(resend_keys) = &mut resend_keys {
                        resend_keys.push(key);
                    }
                    // TODO: can we actually handle it the right way (i.e. no panicing)?
                    // but VarInt doesn't panic (does it make difference to handle it the right way)
                    key.encode(&mut to_send).unwrap();
                    to_send.extend_from_slice(payload);
                    ReliableBufferedPacket::new(
                        to_send,
                        reliable_channel.resend_interval,
                        reliable_channel.resend_attempts,
                    )
                })?;
                sender.send_packet(Cow::Borrowed(&resender.map().get(key).unwrap().payload))
            } else {
                to_send.extend_from_slice(payload);
                sender.send_packet(Cow::Owned(to_send))
            }
            .map_err(|_| ConnectionError::PacketSendingError)
        };

    if MAX_NON_SEGMENTED_PAYLOAD_LEN < payload.len() {
        let segments = payload.chunks(MAX_PACKET_LEN - MAX_PACKET_METADATA_LEN);

        let len = segments.len();

        let segment_key = *segment_keys_counter;
        *segment_keys_counter = segment_keys_counter.wrapping_add(1);

        for (i, segment) in segments.enumerate() {
            send_real_packet(
                Some(PacketSegmentMetadata {
                    segment_key,
                    segment: i,
                    segment_count: len,
                }),
                segment,
            )?;
        }

        Ok(())
    } else {
        send_real_packet(None, &payload)
    }
}

#[derive(Default)]
pub struct ConnectionChannelMessageSwapBuffer {
    effective: Mutex<Vec<Vec<u8>>>,
}

impl ConnectionChannelMessageSwapBuffer {
    fn effective_lock(&self) -> MutexGuard<Vec<Vec<u8>>> {
        self.effective.lock()
    }

    pub fn take(&self, swap: &mut Vec<Vec<u8>>) {
        let mut effective_lock = self.effective.lock();

        std::mem::swap(effective_lock.deref_mut(), swap);
    }
}

pub(crate) struct Connection {
    pub state: ConnectionState,
    pub id_mapper: ConnectionIdMapper,
    pub acknowledge_buffer: ConnectionAcknowledgeBuffer,
    pub resend_key_duplicate_checker: SlotSet,
    pub resender: PacketResender,
    pub segment_keys_counter: u32,
    pub orders: ChannelData<u32, UniqueChannelId<true>>,
    pub orderers: ChannelData<Option<Box<PacketOrderer>>, UniqueChannelId<false>>,
    pub joiner: PacketSegmentJoiner,
    pub buffers: Arc<RwLock<GlobalChannelData<ConnectionChannelMessageSwapBuffer>>>,
    pub last_time_received_packet: Instant,
}

impl Default for Connection {
    fn default() -> Self {
        Self {
            state: Default::default(),
            id_mapper: Default::default(),
            acknowledge_buffer: Default::default(),
            resend_key_duplicate_checker: Default::default(),
            resender: Default::default(),
            segment_keys_counter: Default::default(),
            orders: Default::default(),
            orderers: {
                let mut orderer = ChannelData::default();
                orderer.set(
                    ConnectionState::Registration {
                        received_last_packet: false,
                        last_packet_resend_key: None,
                    },
                    ChannelId::new(CHANNEL_MANAGEMENT_CHANNEL_ID_REGISTRATION_STAGE),
                    Some(Box::new(PacketOrderer::new(
                        CHANNEL_MANAGEMENT_CHANNEL
                            .orderability
                            .unwrap()
                            .capacity
                            .get(),
                    ))),
                );
                orderer
            },
            joiner: Default::default(),
            buffers: Default::default(),
            last_time_received_packet: Instant::now(),
        }
    }
}

pub(crate) fn handle_message(
    mut payload: &[u8],
    state: &mut ConnectionState,
    id_mapper: &mut ConnectionIdMapper,
    provider: &impl ChannelProvider,
    acknowledge_buffer: &mut ConnectionAcknowledgeBuffer,
    resend_key_duplicate_checker: &mut SlotSet,
    resender: &mut PacketResender,
    segment_keys_counter: &mut u32,
    orders: &mut ChannelData<u32, UniqueChannelId<true>>,
    sender: &mut impl PacketSender,
    orderers: &mut ChannelData<Option<Box<PacketOrderer>>, UniqueChannelId<false>>,
    joiner: &mut PacketSegmentJoiner,
    last_time_received_packet: &mut Instant,
    buffers: &Arc<RwLock<GlobalChannelData<ConnectionChannelMessageSwapBuffer>>>,
) -> Result<(), ConnectionError> {
    let PrimaryPacketMetadata {
        channel_id,
        segment,
    } = PrimaryPacketMetadata::<false>::decode(&mut payload)
        .map_err(|_| ConnectionError::DecodeError)?;

    // assume, the local state is Registration
    // if we get a transfer packet from the other client and we had received the last registration packet from it
    // it implies that it received our acknowledgement of receiving its last packet
    // but we didn't receive its acknowledgement of our acknowledgement
    // so we can safely change our state to Transfer without receiving any acknowledgment from the other client
    let speculatively_transfer = matches!(
        state,
        ConnectionState::Registration {
            received_last_packet: true,
            ..
        }
    );

    let transfer_state_convertion_cleanup =
        |acknowledge_buffer: &mut ConnectionAcknowledgeBuffer,
         resend_key_duplicate_checker: &mut SlotSet,
         resender: &mut PacketResender| {
            log::debug!("Registration -> Transfer");
            acknowledge_buffer.clear();
            resend_key_duplicate_checker.clear();
            resender.map_mut().clear();
        };

    if speculatively_transfer
        && id_mapper
            .not_local_inner
            .global_id(*state, channel_id)
            .is_none()
    {
        *state = ConnectionState::Transfer;
        transfer_state_convertion_cleanup(
            acknowledge_buffer,
            resend_key_duplicate_checker,
            resender,
        );
    }

    let global_channel_id = id_mapper
        .not_local_inner
        .global_id(*state, channel_id)
        .ok_or(ConnectionError::UnknownChannel)?;

    let channel = ConnectionChannel::new(*state, channel_id, global_channel_id, provider)
        .ok_or(ConnectionError::UnknownChannel)?;

    let read_order_and_resend_key =
        |payload: &mut &[u8]| -> Result<(Option<u32>, Option<SlotMapKey>), ConnectionError> {
            let order = if channel.channel.is_ordered() {
                Some(
                    VarInt::<u32>::decode(payload)
                        .map_err(|_| ConnectionError::DecodeError)?
                        .0,
                )
            } else {
                None
            };

            let resend_key = if channel.channel.is_reliable() {
                Some(SlotMapKey::decode(payload).map_err(|_| ConnectionError::DecodeError)?)
            } else {
                None
            };

            Ok((order, resend_key))
        };

    let mut acknowledge_if_needed = |resend_key: Option<SlotMapKey>| {
        if let Some(resend_key) = resend_key {
            match resend_key_duplicate_checker.insert(resend_key) {
                Ok(_) => {}
                Err(SlotSetInsertError::InsertedError) => {
                    log::warn!(
                        "Received a packet that was received before (key: {:?})",
                        resend_key
                    );
                    return Err(ConnectionError::PacketDuplicateDetected);
                }
            }

            acknowledge_buffer.acknowledge(
                resend_key,
                *state,
                id_mapper.local_inner.ack_channel_id().unwrap(),
                resender,
                segment_keys_counter,
                orders,
                sender,
            )?;
        }

        Ok(())
    };

    let (order, _resend_key, payload) = if segment {
        let segment_metadata = PacketSegmentMetadata::decode(&mut payload)
            .map_err(|_| ConnectionError::DecodeError)?;

        let (_, resend_key) = read_order_and_resend_key(&mut payload)?;
        acknowledge_if_needed(resend_key)?;

        match joiner.add(segment_metadata, payload.to_vec(), true)? {
            Some(full_packet) => {
                let mut full_payload = vec![];
                let mut first_payload = full_packet[0].as_slice();
                let (order, resend_key) = read_order_and_resend_key(&mut first_payload)?;
                full_payload.extend_from_slice(first_payload);
                for packet in full_packet.iter().skip(1) {
                    let mut packet = packet.as_slice();
                    let _ = read_order_and_resend_key(&mut packet);
                    full_payload.extend_from_slice(packet);
                }
                (order, resend_key, full_payload)
            }
            // no message to handle, it was consumed by the joiner
            None => return Ok(()),
        }
    } else {
        let (order, resend_key) = read_order_and_resend_key(&mut payload)?;
        let payload = payload.to_vec();

        acknowledge_if_needed(resend_key)?;

        (order, resend_key, payload)
    };

    {
        let acknowledge_locally =
            |resender: &mut PacketResender, mut packet: &[u8], state: &mut ConnectionState| {
                while let Ok(key) = SlotMapKey::decode(&mut packet) {
                    if let ConnectionState::Registration {
                        last_packet_resend_key,
                        ..
                    } = state
                    {
                        if *last_packet_resend_key == Some(key) {
                            *last_packet_resend_key = None;
                        }
                    }

                    if resender.remove(key).is_none() {
                        log::warn!(
                            "A packet that doesn't exist was acknowledged (key: {:?})",
                            key
                        );
                    }
                }
            };

        let mut handle_message = |orderers: &mut ChannelData<
            Option<Box<PacketOrderer>>,
            UniqueChannelId<false>,
        >,
                                  state: &mut ConnectionState,
                                  packet: Vec<u8>|
         -> Result<(), ConnectionError> {
            match state {
                ConnectionState::Registration {
                    received_last_packet,
                    ..
                } => match channel.local_id.0 {
                    ACK_CHANNEL_ID_REGISTRATION_STAGE => {
                        acknowledge_locally(resender, &packet, state);
                        if state.convert_to_transfer_if_possible() {
                            transfer_state_convertion_cleanup(
                                acknowledge_buffer,
                                resend_key_duplicate_checker,
                                resender,
                            );
                        }
                    }
                    CHANNEL_MANAGEMENT_CHANNEL_ID_REGISTRATION_STAGE => {
                        let mut packet = packet.as_slice();
                        let channel_name = ChannelName::decode(&mut packet)
                            .map_err(|_| ConnectionError::DecodeError)?
                            .0;

                        if channel_name.is_empty() {
                            *received_last_packet = true;
                            if state.convert_to_transfer_if_possible() {
                                transfer_state_convertion_cleanup(
                                    acknowledge_buffer,
                                    resend_key_duplicate_checker,
                                    resender,
                                );
                            }
                        } else {
                            let channel_id = ChannelId::new(
                                VarInt::<u32>::decode(&mut packet)
                                    .map_err(|_| ConnectionError::DecodeError)?
                                    .0,
                            );

                            let global_id = provider
                                .by_name(channel_name)
                                .ok_or(ConnectionError::UnknownChannel)
                                .inspect_err(|_| {
                                    log::error!(
                                        r#"
                                        An unknown channel was tried to be 
                                        registered, thus it is ignored! (id: {:?}, name: {:?})"#,
                                        channel_id,
                                        channel_name
                                    );
                                })?;

                            if !id_mapper.not_local_inner.insert(
                                channel_id,
                                global_id,
                                channel_name,
                            ) {
                                log::error!(
                                    r#"
                                    An invalid local channel id was given, 
                                    it must lie between 0 and the count of the registered local channel ids 
                                    (id: {:?}, name: {:?})"#,
                                    channel_id,
                                    channel_name
                                );
                                return Err(ConnectionError::ChannelRegistrationStageInvalidValue);
                            };

                            let channel = provider.by_id(global_id).expect(
                                "ChannelProvider must have returned a valid global channel id",
                            );

                            if let Some(ref ordered_channel) = channel.orderability {
                                orderers.set(
                                    ConnectionState::Transfer,
                                    channel_id,
                                    Some(Box::new(PacketOrderer::new(
                                        ordered_channel.capacity.get(),
                                    ))),
                                );
                            }
                        }
                    }
                    _ => unreachable!(),
                },
                ConnectionState::Transfer => {
                    if matches!(id_mapper.not_local_inner.ack_channel_id(), Some(id) if id == channel.local_id)
                    {
                        acknowledge_locally(resender, &packet, state);
                    } else if matches!(id_mapper.not_local_inner.channel_management_channel_id(), Some(id) if id == channel.local_id)
                    {
                        todo!()
                    } else {
                        let lock = buffers.read();
                        let buffer = lock.get(global_channel_id).unwrap();
                        let mut lock = buffer.effective_lock();
                        lock.push(packet);
                    }
                }
            }
            Ok(())
        };

        match order {
            Some(order) => {
                // avoiding the problem: when can the local id of the channel change itself?
                // only when the channel is the channel management channel and the state of the connection is registration
                // but the local id of the channel is not to be changed in no circumstances
                // so using PacketOrderer::insert after that won't break anything
                let state_copy = *state;
                let mut orderer = orderers
                    .get_mut(state_copy, channel.local_id)
                    .take()
                    .unwrap();
                orderer.push(order as usize, payload)?;
                orderer.retain(|it| handle_message(orderers, state, it))?;
                *orderers.get_mut(state_copy, channel.local_id) = Some(orderer);
            }
            None => {
                handle_message(orderers, state, payload)?;
            }
        }
    }

    // at this point the packet uniqueness was checked.
    // if any error has occured then the packet would be ignored.
    *last_time_received_packet = Instant::now();

    Ok(())
}

pub(crate) fn update_connection(
    delta: u32,
    state: &mut ConnectionState,
    id_mapper: &mut ConnectionIdMapper,
    acknowledge_buffer: &mut ConnectionAcknowledgeBuffer,
    resender: &mut PacketResender,
    segment_keys_counter: &mut u32,
    orders: &mut ChannelData<u32, UniqueChannelId<true>>,
    sender: &mut impl PacketSender,
) -> Result<(), ConnectionError> {
    resender.update(delta, |packet| {
        if let Err(_) = sender.send_packet(Cow::Borrowed(&packet.payload)) {
            log::error!("Failed to resend a packet");
        };
    });

    acknowledge_buffer.flush(
        *state,
        id_mapper.local_inner.ack_channel_id().unwrap(),
        resender,
        segment_keys_counter,
        orders,
        sender,
    )?;

    Ok(())
}

pub(crate) fn initialize_connection(
    state: &mut ConnectionState,
    id_mapper: &mut ConnectionIdMapper,
    acknowledge_buffer: &mut ConnectionAcknowledgeBuffer,
    resender: &mut PacketResender,
    segment_keys_counter: &mut u32,
    orders: &mut ChannelData<u32, UniqueChannelId<true>>,
    sender: &mut impl PacketSender,
    provider: &impl ChannelProvider,
    buffers: &Arc<RwLock<GlobalChannelData<ConnectionChannelMessageSwapBuffer>>>,
) -> Result<(), ConnectionError> {
    assert!(
        matches!(state, ConnectionState::Registration { .. }),
        "initialize_connection may only be called when the state is registration"
    );

    log::info!("Initializing connection");

    let channel = ConnectionChannel {
        local_id: ChannelId::new(CHANNEL_MANAGEMENT_CHANNEL_ID_REGISTRATION_STAGE),
        channel: &CHANNEL_MANAGEMENT_CHANNEL,
    };

    let mut all = provider.all().collect::<Vec<_>>();
    all.sort_by_key(|(_, id)| !provider.by_id(*id).unwrap().frequency.0);

    let mut buffer = vec![];

    let mut lock = buffers.write();

    // todo: add new channel`` function
    for (_, id) in all.iter() {
        if let Some(ref reliable_channel) = provider.by_id(*id).unwrap().reliability {
            *resender.mut_capacity() += reliable_channel.additional_capacity;
        }
    }

    for (i, (name, id)) in all.into_iter().enumerate() {
        let channel_id = ChannelId::new(i as u32);

        let success = id_mapper.local_inner.insert(channel_id, id, name);
        debug_assert!(success);

        ChannelName::new(name)
            .encode(&mut buffer)
            .inspect_err(|_| {
                log::error!(
                    "Channel with name {} is exceeding the limit of length",
                    name
                );
            })
            .map_err(|_| ConnectionError::EncodeError)?;

        VarInt::<u32>(channel_id.0)
            .encode(&mut buffer)
            .map_err(|_| ConnectionError::EncodeError)?;

        let _ = send_packet(
            channel,
            &buffer,
            *state,
            resender,
            segment_keys_counter,
            orders,
            sender,
            None,
        );

        buffer.clear();

        orders.set(ConnectionState::Transfer, channel_id, 0);
        lock.insert(id, ConnectionChannelMessageSwapBuffer::default());
    }

    drop(lock);

    ChannelName::new("")
        .encode(&mut buffer)
        .map_err(|_| ConnectionError::EncodeError)?;

    let mut resend_keys = vec![];

    let _ = send_packet(
        channel,
        &buffer,
        *state,
        resender,
        segment_keys_counter,
        orders,
        sender,
        Some(&mut resend_keys),
    );

    if let ConnectionState::Registration {
        last_packet_resend_key,
        ..
    } = state
    {
        *last_packet_resend_key = Some(resend_keys[0]);
    } else {
        unreachable!()
    }

    Ok(())
}

#[derive(Default, Debug)]
pub struct ChannelData<T, C> {
    basic_stage: Vec<T>,
    ack_channel_registration_stage: T,
    channel_management_registration_stage: T,
    _marker: PhantomData<C>,
}

impl<T, C> ChannelData<T, C> {
    pub fn get(&self, state: ConnectionState, id: ChannelId<C>) -> &T {
        match state {
            ConnectionState::Registration { .. } => match id.0 {
                ACK_CHANNEL_ID_REGISTRATION_STAGE => &self.ack_channel_registration_stage,
                CHANNEL_MANAGEMENT_CHANNEL_ID_REGISTRATION_STAGE => {
                    &self.channel_management_registration_stage
                }
                _ => unreachable!(),
            },
            ConnectionState::Transfer => &self.basic_stage[id.0 as usize],
        }
    }

    pub fn get_mut(&mut self, state: ConnectionState, id: ChannelId<C>) -> &mut T {
        match state {
            ConnectionState::Registration { .. } => match id.0 {
                ACK_CHANNEL_ID_REGISTRATION_STAGE => &mut self.ack_channel_registration_stage,
                CHANNEL_MANAGEMENT_CHANNEL_ID_REGISTRATION_STAGE => {
                    &mut self.channel_management_registration_stage
                }
                _ => unreachable!(),
            },
            ConnectionState::Transfer => &mut self.basic_stage[id.0 as usize],
        }
    }

    pub fn set(&mut self, state: ConnectionState, id: ChannelId<C>, value: T)
    where
        T: Default,
    {
        match state {
            ConnectionState::Registration { .. } => match id.0 {
                ACK_CHANNEL_ID_REGISTRATION_STAGE => self.ack_channel_registration_stage = value,
                CHANNEL_MANAGEMENT_CHANNEL_ID_REGISTRATION_STAGE => {
                    self.channel_management_registration_stage = value;
                }
                _ => unreachable!(),
            },
            ConnectionState::Transfer => {
                if self.basic_stage.len() <= id.0 as usize {
                    self.basic_stage
                        .resize_with(id.0 as usize + 1, Default::default);
                }
                self.basic_stage[id.0 as usize] = value;
            }
        }
    }
}

#[derive(Default)]
pub struct GlobalChannelData<T> {
    values: Vec<Option<T>>,
}

impl<T> GlobalChannelData<T> {
    pub fn get(&self, id: ChannelId<GlobalChannelId>) -> Option<&T> {
        self.values.get(id.0 as usize).map(Option::as_ref).flatten()
    }

    pub fn get_mut(&mut self, id: ChannelId<GlobalChannelId>) -> Option<&mut T> {
        self.values
            .get_mut(id.0 as usize)
            .map(Option::as_mut)
            .flatten()
    }

    pub fn insert(&mut self, id: ChannelId<GlobalChannelId>, value: T) -> Option<T> {
        if self.values.len() <= id.0 as usize {
            self.values.resize_with((id.0 + 1) as usize, || None);
        }

        self.values[id.0 as usize].replace(value)
    }
}

/// The protocol for packets with channel id 1 << 31 - 3 is following:
/// 1. the channel name, which is a [String]
/// 2. the channel id, which is decoded as [VarInt<u32>]
/// If the channel name is empty, than the state will be swapped to [ConnectionState::Transfer]
///
/// While the stage is active, the ack channel gets the id of 1 << 31 - 2.
///
/// If the channel id lies somewhere in between other ids, then it shifts all other ids
/// greater or **equal** to it to the right by 1
///
/// (De facto, the stage ignores all registered channels and handles the channel's ids by it's own schema written above)
const __CHANNEL_REGISTRATION_STAGE: () = ();

type ChannelName<'a> = StrWithMaxLen<'a, 500>;

/// The packet contains metadata at the start of the payload and the real payload after the metadata:
/// # Metadata:
/// 1. primary packet metadata, which consists of a single [VarInt<u32>] number, that decodes 2 numbers:
///     - 1 bit number, which determines whether the given packet is a segment of a bigger one
///     - 31 bit number (rest), which determines the channel id the packet is sent as
/// 2. segment metadata (when the given packet is a segment), which is a single [PacketSegmentMetadata]
/// 3. order (when the channel is an ordered one), which is a single [VarInt<u32>] number
/// 4. resend metadata (when the channel is a reliable one), which is a single [SlotMapKey]
const __PACKET_STRUCTURE: () = ();

const MAX_NON_SEGMENTED_PACKET_METADATA_LEN: usize = 0
    + (std::mem::size_of::<u32>() + 1)
    + (std::mem::size_of::<u32>() + 1)
    + (std::mem::size_of::<u32>() + 1) * 2;

const MAX_PACKET_METADATA_LEN: usize = MAX_NON_SEGMENTED_PACKET_METADATA_LEN
    + (std::mem::size_of::<u32>() + 1)
    + (std::mem::size_of::<PacketSegmentMetadataIntType>() + 1) * 2;

pub const MAX_PACKET_LEN: usize = 2000;

pub const MAX_NON_SEGMENTED_PAYLOAD_LEN: usize =
    MAX_PACKET_LEN - MAX_NON_SEGMENTED_PACKET_METADATA_LEN;

#[derive(thiserror::Error, Debug)]
pub(crate) enum ConnectionError {
    #[error("Error during decoding")]
    DecodeError,
    #[error("Error during encoding")]
    EncodeError,
    #[error("The message's metadata refers to an unknown channel")]
    UnknownChannel,
    #[error("The channel registration stage check value is invalid")]
    ChannelRegistrationStageInvalidValue,
    #[error("Failed to send a packet")]
    PacketSendingError,
    #[error("Packet was already handled")]
    PacketDuplicateDetected,
    #[error("Packet resender error: {0:?}")]
    PacketResenderError(#[from] PacketResenderError),
    #[error("Packet orderer error: {0:?}")]
    PacketOrdererError(#[from] PacketOrdererError),
    #[error("Packet segment joiner error: {0:?}")]
    PacketSegmentJoinerError(#[from] PacketSegmentJoinerError),
}

pub const ACK_CHANNEL_NAME: &str = "__ack";
pub const CHANNEL_MANAGEMENT_CHANNEL_NAME: &str = "__channel";

const NULL_CHANNEL_ID: u32 = 1 << 31 - 1;
const ACK_CHANNEL_ID_REGISTRATION_STAGE: u32 = NULL_CHANNEL_ID - 1;
const CHANNEL_MANAGEMENT_CHANNEL_ID_REGISTRATION_STAGE: u32 = ACK_CHANNEL_ID_REGISTRATION_STAGE - 1;

pub const ACK_CHANNEL: Channel = Channel {
    reliability: Some(ReliableChannel {
        resend_interval: NonZero::new(750).unwrap(),
        resend_attempts: u32::MAX,
        additional_capacity: 300,
    }),
    orderability: None,
    frequency: ChannelFrequency::HIGH,
};

pub const CHANNEL_MANAGEMENT_CHANNEL: Channel = Channel {
    reliability: Some(ReliableChannel {
        resend_interval: NonZero::new(100).unwrap(),
        resend_attempts: u32::MAX,
        additional_capacity: 300,
    }),
    orderability: Some(OrderedChannel {
        capacity: NonZero::new(1000).unwrap(),
    }),
    frequency: ChannelFrequency::NEVER,
};

#[derive(Debug)]
struct PrimaryPacketMetadata<const LOCAL: bool> {
    pub channel_id: ChannelId<UniqueChannelId<LOCAL>>,
    pub segment: bool,
}

impl<'a, const LOCAL: bool> Decode<'a> for PrimaryPacketMetadata<LOCAL> {
    fn decode(decoder: &mut impl Decoder<'a>) -> Result<Self, ()> {
        let num: u32 = VarInt::decode(decoder)?.0;
        Ok(PrimaryPacketMetadata {
            channel_id: ChannelId::new(num >> 1),
            segment: num & 0b1 == 1,
        })
    }
}

impl<const LOCAL: bool> Encode for PrimaryPacketMetadata<LOCAL> {
    fn encode(&self, encoder: &mut impl Encoder) -> Result<(), ()> {
        if (self.channel_id.0 << 1) >> 1 != self.channel_id.0 {
            return Err(());
        }
        VarInt((self.channel_id.0 << 1) | (if self.segment { 1 } else { 0 })).encode(encoder)
    }
}
