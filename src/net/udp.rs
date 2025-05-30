use std::{
    borrow::Cow,
    io,
    net::{SocketAddr, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use bevy::{
    log,
    platform_support::collections::{HashMap, hash_map::Entry},
};
use parking_lot::RwLock;
use rand::Rng;

use crate::{clone_all, util::channel::ManyToSingleChannel};

use super::connection::{
    ChannelData, ChannelId, ChannelProvider, Connection, ConnectionChannel,
    ConnectionChannelMessageSwapBuffer, ConnectionState, GlobalChannelData, GlobalChannelId,
    MAX_PACKET_LEN, PacketSender, handle_message, initialize_connection, send_packet,
    update_connection,
};

#[derive(Debug)]
struct UdpPacket {
    addr: SocketAddr,
    payload: Vec<u8>,
}

struct UdpPacketSender<'a> {
    buffer: &'a ManyToSingleChannel<UdpPacket>,
    addr: SocketAddr,
}

impl<'a> PacketSender for UdpPacketSender<'a> {
    fn send_packet(&mut self, packet: Cow<[u8]>) -> Result<(), ()> {
        self.buffer.push(UdpPacket {
            addr: self.addr,
            payload: packet.to_vec(),
        });
        Ok(())
    }
}

const CLOSED_CHECKING_TIMEOUT: u64 = 100;

fn udp_write_loop(
    buffer: Arc<ManyToSingleChannel<UdpPacket>>,
    socket: Arc<UdpSocket>,
    closed: Arc<AtomicBool>,
    fake_lag: UdpFakeLag,
) -> io::Result<()> {
    let mut bunch = vec![];
    bunch.reserve_exact(16);

    let mut rng = rand::rng();

    loop {
        buffer.wait_and_take_bunch(&mut bunch, Duration::from_millis(CLOSED_CHECKING_TIMEOUT));

        for packet in bunch.drain(..) {
            log::debug!("Sending packet: {:?}", packet);
            if rng.random::<f64>() >= fake_lag.fake_sending_probability {
                match socket.send_to(&packet.payload, packet.addr) {
                    Ok(len) if len < packet.payload.len() => {
                        log::error!("Couldn't send packet's full payload");
                    }
                    Ok(_) => {}
                    Err(err) => {
                        log::error!("Received error when tried to send a packet: {:?}", err)
                    }
                }
            }
        }

        if closed.load(Ordering::Relaxed) {
            break;
        }
    }

    Ok(())
}

fn udp_read_loop(
    event_queue: Arc<ManyToSingleChannel<UdpEvent>>,
    socket: Arc<UdpSocket>,
    closed: Arc<AtomicBool>,
    fake_lag: UdpFakeLag,
) -> io::Result<()> {
    socket.set_read_timeout(Some(Duration::from_millis(CLOSED_CHECKING_TIMEOUT)))?;

    let mut buf = vec![0u8; MAX_PACKET_LEN];

    let mut rng = rand::rng();

    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, addr)) => {
                if rng.random::<f64>() >= fake_lag.fake_reading_probability {
                    event_queue.push(UdpEvent::Read {
                        payload: buf[..len].to_vec(),
                        addr,
                    });
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                // nothing
            }
            Err(err) => {
                log::error!("Error while trying to receive from udp socket: {:?}", err);
            }
        }

        if closed.load(Ordering::Relaxed) {
            break;
        }
    }

    Ok(())
}

#[derive(Debug)]
pub enum UdpEvent {
    Read {
        payload: Vec<u8>,
        addr: SocketAddr,
    },
    CloseConnection {
        addr: SocketAddr,
    },
    CloseServer,
    Connect {
        addr: SocketAddr,
    },
    Send {
        channel: ChannelId<GlobalChannelId>,
        payload: Vec<u8>,
        addr: SocketAddr,
    },
}

const READ_TIMEOUT: u64 = 50;
const DELTA_MIN: u64 = 25;
const CONNECTION_TIMEOUT: u128 = 30_000;

fn udp_event_loop(
    buffer: Arc<ManyToSingleChannel<UdpPacket>>,
    event_queue: Arc<ManyToSingleChannel<UdpEvent>>,
    new_connections: Arc<ManyToSingleChannel<UdpConnectionEvent>>,
    socket: Arc<UdpSocket>,
    closed: Arc<AtomicBool>,
    provider: &impl ChannelProvider,
) -> io::Result<()> {
    let mut connections: HashMap<SocketAddr, Connection, _> = HashMap::new();

    let create_packet_sender = |addr: SocketAddr| UdpPacketSender {
        buffer: &buffer,
        addr,
    };

    let mut bunch = vec![];
    bunch.reserve_exact(16);

    let mut time0 = Instant::now();
    'outer: loop {
        event_queue.wait_and_take_bunch(&mut bunch, Duration::from_millis(READ_TIMEOUT));

        for event in bunch.drain(..) {
            log::debug!("Received event: {:?}", event);

            match event {
                UdpEvent::Read { payload, addr } => {
                    let entry = connections.entry(addr);

                    let new_connection = matches!(entry, Entry::Vacant(_));

                    let Connection {
                        state,
                        id_mapper,
                        acknowledge_buffer,
                        resend_key_duplicate_checker,
                        resender,
                        segment_keys_counter,
                        orders,
                        orderers,
                        joiner,
                        buffers,
                        last_time_received_packet,
                    } = entry.or_default();

                    let state_transfer_before = matches!(state, ConnectionState::Transfer);

                    if new_connection {
                        if let Err(err) = initialize_connection(
                            state,
                            id_mapper,
                            acknowledge_buffer,
                            resender,
                            segment_keys_counter,
                            orders,
                            &mut create_packet_sender(addr),
                            provider,
                            &buffers,
                        ) {
                            log::error!(
                                "Failed to initialize the connection {:?} (hence forgetting about it): {:?}",
                                addr,
                                err
                            );
                            connections.remove(&addr);
                            continue;
                        }
                    }

                    if let Err(err) = handle_message(
                        &payload,
                        state,
                        id_mapper,
                        provider,
                        acknowledge_buffer,
                        resend_key_duplicate_checker,
                        resender,
                        segment_keys_counter,
                        orders,
                        &mut create_packet_sender(addr),
                        orderers,
                        joiner,
                        last_time_received_packet,
                        buffers,
                    ) {
                        log::warn!("Received error when tried to handle a message: {:?}", err);
                    }

                    if !state_transfer_before && matches!(state, ConnectionState::Transfer) {
                        new_connections.push(UdpConnectionEvent::Connected {
                            buffer: Arc::clone(buffers),
                            addr,
                        });
                    }
                }
                UdpEvent::CloseConnection { addr } => {
                    connections.remove(&addr);
                }
                UdpEvent::CloseServer => {
                    closed.store(true, Ordering::Relaxed);
                    break 'outer;
                }
                UdpEvent::Connect { addr } => {
                    let entry = connections.entry(addr);

                    match entry {
                        Entry::Occupied(_) => {
                            log::warn!(
                                "Tried to establish a connection with already connected client: {:?}",
                                addr
                            );
                            continue;
                        }
                        Entry::Vacant(entry) => {
                            let Connection {
                                state,
                                id_mapper,
                                acknowledge_buffer,
                                resender,
                                segment_keys_counter,
                                orders,
                                buffers,
                                ..
                            } = entry.insert(Connection::default());

                            if let Err(err) = initialize_connection(
                                state,
                                id_mapper,
                                acknowledge_buffer,
                                resender,
                                segment_keys_counter,
                                orders,
                                &mut create_packet_sender(addr),
                                provider,
                                &buffers,
                            ) {
                                log::error!(
                                    "Failed to initialize the connection {:?} (hence forgetting about it): {:?}",
                                    addr,
                                    err
                                );
                                connections.remove(&addr);
                                continue;
                            }
                        }
                    }
                }
                UdpEvent::Send {
                    channel,
                    payload,
                    addr,
                } => {
                    let Some(Connection {
                        state,
                        id_mapper,
                        resender,
                        segment_keys_counter,
                        orders,
                        buffers,
                        ..
                    }) = connections.get_mut(&addr)
                    else {
                        log::error!(
                            "Received UdpEvent::Send on connection that doesn't exist (addr: {:?})",
                            addr
                        );
                        continue;
                    };

                    let state_transfer_before = matches!(state, ConnectionState::Transfer);

                    let Some(local_id) = id_mapper.local_inner.local_id(*state, channel) else {
                        log::error!(
                            "Received UdpEvent::Send with channel that is not registered by the connection (addr: {:?}, channel: {:?})",
                            addr,
                            channel
                        );
                        continue;
                    };

                    let Some(channel) = ConnectionChannel::new(*state, local_id, channel, provider)
                    else {
                        log::error!(
                            "Couldn't instantiate ConnectionChannel instances (addr: {:?}, channel: {:?}, state: {:?})",
                            addr,
                            channel,
                            state
                        );
                        continue;
                    };

                    if let Err(err) = send_packet(
                        channel,
                        &payload,
                        *state,
                        resender,
                        segment_keys_counter,
                        orders,
                        &mut create_packet_sender(addr),
                        None,
                    ) {
                        log::error!("Received error when tried to send a packet: {:?}", err);
                    }

                    if !state_transfer_before && matches!(state, ConnectionState::Transfer) {
                        new_connections.push(UdpConnectionEvent::Connected {
                            buffer: Arc::clone(buffers),
                            addr,
                        });
                    }
                }
            }
        }

        let delta = time0.elapsed().as_millis() as u64;

        if delta > DELTA_MIN {
            time0 = Instant::now();

            let mut to_remove = vec![];

            for (
                addr,
                Connection {
                    state,
                    id_mapper,
                    acknowledge_buffer,
                    resender,
                    segment_keys_counter,
                    orders,
                    last_time_received_packet,
                    ..
                },
            ) in connections.iter_mut()
            {
                // The acknowledgement technique that is used in the "protocol" makes it so,
                // that everytime there is something to acknowledge,
                // thus sending acknowledgment packets regardless of whether the connection is used,
                // thus if the client didn't get anything in a long time, it is either
                // because the other client has forgotten about us or there is a major malfunction inbetween.
                if last_time_received_packet.duration_since(time0).as_millis() > CONNECTION_TIMEOUT
                {
                    to_remove.push(*addr);
                    continue;
                }

                if let Err(err) = update_connection(
                    delta as u32,
                    state,
                    id_mapper,
                    acknowledge_buffer,
                    resender,
                    segment_keys_counter,
                    orders,
                    &mut create_packet_sender(*addr),
                ) {
                    log::warn!(
                        "Received error when tried to update connection {:?}: {:?}",
                        addr,
                        err
                    );
                }
            }

            new_connections.push_many(
                to_remove
                    .into_iter()
                    .inspect(|to_remove| {
                        log::warn!(
                            "Got no message from {:?} in 
                            a long time ({CONNECTION_TIMEOUT}ms), hence forgetting about it",
                            to_remove
                        );

                        connections.remove(to_remove);
                    })
                    .map(|addr| UdpConnectionEvent::TimedOut { addr }),
            );
        }
    }

    Ok(())
}

pub struct UdpServerInstance {
    pub event_queue: Arc<ManyToSingleChannel<UdpEvent>>,
    pub new_connections: Arc<ManyToSingleChannel<UdpConnectionEvent>>,
}

pub enum UdpConnectionEvent {
    Connected {
        addr: SocketAddr,
        buffer: Arc<RwLock<GlobalChannelData<ConnectionChannelMessageSwapBuffer>>>,
    },
    TimedOut {
        addr: SocketAddr,
    },
}

#[derive(Default, Clone, Copy)]
pub struct UdpFakeLag {
    pub fake_sending_probability: f64,
    pub fake_reading_probability: f64,
}

pub fn launch_udp_server_instance(
    socket: Arc<UdpSocket>,
    provider: Arc<impl ChannelProvider + Send + Sync + 'static>,
    fake_lag: UdpFakeLag,
) -> UdpServerInstance {
    let event_queue = Arc::new(ManyToSingleChannel::new());
    let new_connections = Arc::new(ManyToSingleChannel::new());
    let buffer = Arc::new(ManyToSingleChannel::new());
    let closed = Arc::new(AtomicBool::new(false));

    let _write_handle = {
        let (buffer, socket, closed) = clone_all!(&buffer, &socket, &closed);
        std::thread::spawn(move || {
            udp_write_loop(buffer, socket, closed, fake_lag).unwrap();
        })
    };

    let _read_handle = {
        let (event_queue, socket, closed) = clone_all!(&event_queue, &socket, &closed);
        std::thread::spawn(move || {
            udp_read_loop(event_queue, socket, closed, fake_lag).unwrap();
        })
    };

    let _loop_handle = {
        let (buffer, event_queue, new_connections, socket, closed) =
            clone_all!(&buffer, &event_queue, &new_connections, &socket, &closed);
        std::thread::spawn(move || {
            udp_event_loop(
                buffer,
                event_queue,
                new_connections,
                socket,
                closed,
                Arc::as_ref(&provider),
            )
            .unwrap();
        })
    };

    UdpServerInstance {
        event_queue,
        new_connections,
    }
}
