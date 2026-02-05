use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;
use tracing::{Level, event};

use crate::cli::Config;
use crate::sockets::InterfaceSocket;
use crate::{BROADCAST_MDNS, PACKET_SIZE};

pub async fn reflect(
    server_socket: UdpSocket,
    sockets: Vec<InterfaceSocket>,
    config: Arc<Config>,
    cancellation_token: CancellationToken,
) {
    let mut buffer = vec![0_u8; PACKET_SIZE];

    loop {
        let (recv_size, from_addr) = tokio::select! {
            () = cancellation_token.cancelled() => {
                break;
            },
            result = server_socket.recv_from(&mut buffer) => {
                match result {
                    Ok((recv_size, from_addr)) => (recv_size, from_addr),
                    Err(error) => {
                        event!(Level::ERROR, ?error, "recv()");
                        continue;
                    }
                }
            }
        };

        let is_loopback = sockets
            .iter()
            .any(|socket| socket.address == from_addr.ip());

        if is_loopback {
            continue;
        }

        event!(Level::INFO, "data from={} size={}", from_addr, recv_size);

        for socket in &sockets {
            // do not repeat packet back to the same network from which it originated
            if let SocketAddr::V4(socket_address) = from_addr {
                if socket_address.ip() & socket.mask == socket.network {
                    continue;
                }
            } else {
                event!(Level::INFO, "Got message from IPv6?");
            }

            if config.verbose {
                event!(Level::INFO, "repeating data to {}", socket.name);
            }

            // repeat data
            match socket
                .socket
                .send_to(&buffer[0..recv_size], &BROADCAST_MDNS)
                .await
            {
                Ok(sentsize) => {
                    if sentsize != recv_size {
                        event!(
                            Level::ERROR,
                            "send_packet size differs: sent={} actual={}",
                            recv_size,
                            sentsize
                        );
                    }
                },
                Err(error) => {
                    event!(Level::ERROR, ?error, "send()");
                },
            }
        }
    }

    // server_socket and sockets are dropped here
    event!(Level::INFO, "Shutting down...");
}
