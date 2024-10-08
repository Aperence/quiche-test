use core::str;
use std::{collections::HashMap, env, net::SocketAddr};

use quiche_test::shared::{
    generate_cid_and_reset_token, read_loop, write_loop, MAX_NUMBER_SOCKETS,
};

#[macro_use]
extern crate log;

use quiche::{self, ConnectionId};
use ring::rand::*;

const MAX_BUF_SIZE: usize = 65507;

pub type ClientMap = HashMap<ConnectionId<'static>, quiche::Connection>;

fn main() {
    let args: Vec<String> = env::args().collect();

    let messages = &args[1..];

    let mut received = vec![false; messages.len()];

    let mut idx_message: u64 = 0;

    let mut buf = [0; MAX_BUF_SIZE];
    let mut out = [0; MAX_BUF_SIZE];

    env_logger::builder().format_timestamp_nanos().init();

    // Setup the event loop.
    let mut poll = mio::Poll::new().unwrap();
    let mut events = mio::Events::with_capacity(1024);

    // Create the UDP listening socket, and register it with the event loop.
    let mut sockets = vec![];

    for i in 0..messages.len() {
        let port = 9000 + i;
        let mut socket =
            mio::net::UdpSocket::bind(format!("127.0.0.1:{port}").parse().unwrap()).unwrap();
        poll.registry()
            .register(&mut socket, mio::Token(i), mio::Interest::READABLE)
            .unwrap();

        sockets.push(socket);
    }

    let local_addrs: Vec<SocketAddr> = sockets.iter().map(|s| s.local_addr().unwrap()).collect();

    // Create the configuration for the QUIC connections.
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();

    config.set_application_protos(&[b"http/0.9"]).unwrap();
    config.verify_peer(false);
    config.set_initial_max_streams_bidi(100);
    config.set_initial_max_streams_uni(100);

    config.set_initial_max_data(1000000);
    config.set_initial_max_stream_data_bidi_local(1000000);
    config.set_initial_max_stream_data_bidi_remote(1000000);
    config.set_initial_max_stream_data_uni(1000000);

    config.set_active_connection_id_limit(20);

    let mut keylog = None;

    if let Some(keylog_path) = std::env::var_os("SSLKEYLOGFILE") {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(keylog_path)
            .unwrap();

        keylog = Some(file);

        config.log_keys();
    }

    let rng = SystemRandom::new();
    let _ = ring::hmac::Key::generate(ring::hmac::HMAC_SHA256, &rng).unwrap();

    let local = sockets[0].local_addr().unwrap();

    let mut scid = [0; quiche::MAX_CONN_ID_LEN];
    rng.fill(&mut scid[..]).unwrap();
    let scid = quiche::ConnectionId::from_vec(scid.to_vec());

    let mut peer_addrs = vec![];
    for i in 0..20 {
        let port = 8000 + i % MAX_NUMBER_SOCKETS;
        peer_addrs.push(format!("127.0.0.1:{port}").parse().unwrap())
    }

    let mut conn = quiche::connect(
        Some("127.0.0.1:8000"),
        &scid,
        local,
        peer_addrs[0],
        &mut config,
    )
    .unwrap();

    if let Some(keylog) = keylog {
        conn.set_keylog(Box::new(keylog));
    }

    let (write, send_info) = conn.send(&mut out).expect("initial send failed");

    while let Err(e) = sockets[0].send_to(&out[..write], send_info.to) {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            trace!(
                "{} -> {}: send() would block",
                sockets[0].local_addr().unwrap(),
                send_info.to
            );
            continue;
        }

        return;
    }

    loop {
        if conn.is_closed() {
            info!(
                "connection closed, {:?} {:?}",
                conn.stats(),
                conn.path_stats().collect::<Vec<quiche::PathStats>>()
            );

            return;
        }

        poll.poll(&mut events, conn.timeout()).unwrap();

        if events.is_empty() {
            trace!("timed out");

            conn.on_timeout();
        }

        for event in &events {
            let socket = sockets.get(event.token().0).unwrap();

            read_loop(&events, &mut conn, socket, &mut buf);

            // core of the client
            if conn.is_established() {
                while conn.scids_left() > 0 {
                    let (scid, reset_token) = generate_cid_and_reset_token(&rng);

                    if conn.new_scid(&scid, reset_token, false).is_err() {
                        break;
                    }
                }

                for stream_id in conn.readable() {
                    while let Ok((read, fin)) = conn.stream_recv(stream_id, &mut buf) {
                        let msg = str::from_utf8(&buf[..read]).unwrap();
                        println!("Received '{}' from server on stream {}", msg, stream_id);
                        if fin {
                            received[(stream_id / 4) as usize] = true
                        }
                    }
                }
                if received.iter().all(|b| *b) {
                    conn.close(true, 0x00, b"closing").unwrap();
                }

                send_stream_new_path(
                    &mut conn,
                    &local_addrs,
                    &peer_addrs,
                    messages,
                    &mut idx_message,
                );
            }

            while let Some(qe) = conn.path_event_next() {
                match qe {
                    quiche::PathEvent::New(..) => unreachable!(),

                    quiche::PathEvent::Validated(local_addr, peer_addr) => {
                        info!("Path ({}, {}) is now validated", local_addr, peer_addr);
                        conn.migrate(local_addr, peer_addr).unwrap();
                    }

                    quiche::PathEvent::FailedValidation(local_addr, peer_addr) => {
                        info!("Path ({}, {}) failed validation", local_addr, peer_addr);
                    }

                    quiche::PathEvent::Closed(local_addr, peer_addr) => {
                        info!(
                            "Path ({}, {}) is now closed and unusable",
                            local_addr, peer_addr
                        );
                    }

                    quiche::PathEvent::ReusedSourceConnectionId(cid_seq, old, new) => {
                        info!(
                            "Peer reused cid seq {} (initially {:?}) on {:?}",
                            cid_seq, old, new
                        );
                    }

                    quiche::PathEvent::PeerMigrated(..) => unreachable!(),
                }
            }

            // write function
            write_loop(&mut conn, &sockets, &mut out);
        }
    }
}

fn send_stream_new_path(
    conn: &mut quiche::Connection,
    local_addrs: &Vec<SocketAddr>,
    peer_addrs: &Vec<SocketAddr>,
    messages: &[String],
    idx_message: &mut u64,
) {
    let idx_message_us = *idx_message as usize;
    if idx_message_us >= messages.len() {
        return;
    }
    let local_addr = local_addrs[idx_message_us];
    let peer_addr = peer_addrs[idx_message_us];
    let path_validated = conn.is_path_validated(local_addr, peer_addr);
    if path_validated.is_err() && conn.available_dcids() > 0 {
        // path doesn't exist, first probe it
        conn.probe_path(local_addr, peer_addr).unwrap();
    } else if path_validated.is_ok_and(|validated| validated) {
        // path is validated, send on this new path
        let message = messages.get(idx_message_us).unwrap();
        conn.stream_send(*idx_message * 4, message.as_bytes(), true)
            .unwrap();
        *idx_message += 1;
    }
}
