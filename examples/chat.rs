#![feature(addr_parse_ascii)]

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    num::NonZero,
    sync::Arc,
    time::Duration,
};

use bevy::{
    app::{App, Plugin},
    log::{self, Level, LogPlugin},
};
use theta::{
    net::{
        channel::{Channel, ChannelFrequency, OrderedChannel, ReliableChannel},
        connection::DefaultChannelProvider,
        udp::{UdpConnectionEvent, UdpEvent, UdpFakeLag, launch_udp_server_instance},
    },
    proto::{StrWithMaxLen, decode::Decode, encode::Encode},
};

fn main() {
    let log_plugin = LogPlugin::default();
    log_plugin.build(&mut App::new());
    log::debug!("Debug log");

    let mut provider = DefaultChannelProvider::new();

    let message_channel_id = provider.add(
        "message".into(),
        Channel::new(ChannelFrequency::HIGH)
            .with_reliability(ReliableChannel {
                resend_interval: NonZero::new(100).unwrap(),
                resend_attempts: u32::MAX,
                additional_capacity: 300,
            })
            .with_orderability(OrderedChannel {
                capacity: NonZero::new(100).unwrap(),
            }),
    );

    // let disconnect_channel_id = provider.add(
    //     "disconnect".into(),
    //     Channel::new(ChannelFrequency::NEVER).with_reliability(ReliableChannel {
    //         resend_interval: NonZero::new(100).unwrdap(),
    //         resend_attempts: u32::MAX,
    //         additional_capacity: 1,
    //     }),
    // );

    let mut buffer = String::new();
    println!("port:");
    std::io::stdin().read_line(&mut buffer).unwrap();
    let port: u16 = buffer.trim().parse().unwrap();

    let socket = Arc::new(UdpSocket::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), port)).unwrap());

    let instance = launch_udp_server_instance(
        socket,
        Arc::new(provider),
        UdpFakeLag {
            fake_sending_probability: 0.15,
            fake_reading_probability: 0.15,
        },
    );

    let mut new_connections_bunch = vec![];
    new_connections_bunch.reserve_exact(8);

    let mut connection_instances = HashMap::new();
    let mut swap_buffer_buffer = vec![];

    loop {
        buffer.clear();
        std::io::stdin().read_line(&mut buffer).unwrap();

        instance
            .new_connections
            .wait_and_take_bunch(&mut new_connections_bunch, Duration::ZERO);

        for event in new_connections_bunch.drain(..) {
            match event {
                UdpConnectionEvent::Connected { addr, buffer } => {
                    connection_instances.insert(addr, buffer);
                    println!("{:?} has connected", addr);
                }
                UdpConnectionEvent::TimedOut { addr } => {
                    println!("{:?} has timed out", addr);
                    connection_instances.remove(&addr);
                }
            }
        }

        for (addr, buffers) in connection_instances.iter() {
            let lock = buffers.read();
            let buffer = lock.get(message_channel_id).unwrap();
            buffer.take(&mut swap_buffer_buffer);
            for message in swap_buffer_buffer.drain(..) {
                let mut message = message.as_slice();
                let message = StrWithMaxLen::<5000>::decode(&mut message).unwrap();
                println!("{:?} -> you: {}", addr, message.0);
            }
        }

        match buffer.trim() {
            "0" => {
                // connect
                println!("addr:");
                buffer.clear();
                std::io::stdin().read_line(&mut buffer).unwrap();
                match SocketAddr::parse_ascii(buffer.trim().as_bytes()) {
                    Ok(addr) => {
                        instance.event_queue.push(UdpEvent::Connect { addr });
                    }
                    Err(err) => {
                        println!(
                            "err while parsing the given socket addr (ignoring the action): {:?}",
                            err
                        );
                    }
                }
            }
            "1" => {
                // disconnect
                println!("addr:");
                buffer.clear();
                std::io::stdin().read_line(&mut buffer).unwrap();
                match SocketAddr::parse_ascii(buffer.trim().as_bytes()) {
                    Ok(addr) => {
                        // just forget for starters
                        instance
                            .event_queue
                            .push(UdpEvent::CloseConnection { addr });
                        connection_instances.remove(&addr);
                    }
                    Err(err) => {
                        println!(
                            "err while parsing the given socket addr (ignoring the action): {:?}",
                            err
                        );
                    }
                }
            }
            "2" => {
                // send message
                println!("addr:");
                buffer.clear();
                std::io::stdin().read_line(&mut buffer).unwrap();
                match SocketAddr::parse_ascii(buffer.trim().as_bytes()) {
                    Ok(addr) => {
                        buffer.clear();
                        std::io::stdin().read_line(&mut buffer).unwrap();
                        let mut payload = vec![];
                        StrWithMaxLen::<5000>(&buffer).encode(&mut payload).unwrap();
                        instance.event_queue.push(UdpEvent::Send {
                            channel: message_channel_id,
                            payload: payload,
                            addr,
                        });
                        println!("you -> {:?}: {}", addr, buffer);
                    }
                    Err(err) => {
                        println!(
                            "err while parsing the given socket addr (ignoring the action): {:?}",
                            err
                        );
                    }
                }
            }
            _ => {
                println!("unknown action type: {}", buffer);
            }
        }
    }
}
