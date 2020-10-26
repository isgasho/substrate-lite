// Substrate-lite
// Copyright (C) 2019-2020  Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Background network service.
//!
//! The [`NetworkService`] manages background tasks dedicated to connecting to other nodes.
//! Importantly, its design is oriented towards the particular use case of the full node.
//!
//! The [`NetworkService`] spawns one background task (using the [`Config::tasks_executor`]) for
//! each active TCP socket, plus one for each TCP listening socket. Messages are exchanged between
//! the service and these background tasks.

// TODO: doc
// TODO: re-review this once finished

use core::{iter, pin::Pin, time::Duration};
use futures::{
    channel::{mpsc, oneshot},
    lock::{Mutex, MutexGuard},
    prelude::*,
};
use std::{io, net::SocketAddr, sync::Arc, time::Instant};
use substrate_lite::network::{
    libp2p::{
        connection,
        multiaddr::{Multiaddr, Protocol},
        peer_id::PeerId,
    },
    peerset, protocol, with_buffers,
};

/// Configuration for a [`NetworkService`].
pub struct Config {
    /// Closure that spawns background tasks.
    pub tasks_executor: Box<dyn FnMut(Pin<Box<dyn Future<Output = ()> + Send>>) + Send>,

    /// Addresses to listen for incoming connections.
    pub listen_addresses: Vec<Multiaddr>,

    /// List of node identities and addresses that are known to belong to the chain's peer-to-pee
    /// network.
    pub bootstrap_nodes: Vec<(PeerId, Multiaddr)>,

    /// Key used for the encryption layer.
    /// This is a Noise static key, according to the Noise specifications.
    /// Signed using the actual libp2p key.
    pub noise_key: connection::NoiseKey,
}

/// Event generated by [`NetworkService::next_event`].
#[derive(Debug)]
pub enum Event {
    Connected(PeerId),
}

pub struct NetworkService {
    /// Fields behind a mutex.
    guarded: Mutex<Guarded>,

    /// See [`Config::noise_key`].
    noise_key: Arc<connection::NoiseKey>,

    /// Receiver of events sent by background tasks.
    ///
    /// > **Note**: This field is not in [`Guarded`] despite being inside of a mutex. The mutex
    /// >           around this receiver is kept locked while an event is being waited for, and it
    /// >           would be undesirable to block access to the other fields of [`Guarded`] during
    /// >           that time.
    from_background: Mutex<mpsc::Receiver<FromBackground>>,

    /// Sending side of [`NetworkService::from_background`]. Clones of this field are created when
    /// a background task is spawned.
    to_foreground: mpsc::Sender<FromBackground>,
}

/// Fields of [`NetworkService`] behind a mutex.
struct Guarded {
    /// See [`Config::tasks_executor`].
    tasks_executor: Box<dyn FnMut(Pin<Box<dyn Future<Output = ()> + Send>>) + Send>,

    /// Holds the state of all the known nodes of the network, and of all the connections (pending
    /// or not).
    peerset: peerset::Peerset<(), mpsc::Sender<ToConnection>, mpsc::Sender<ToConnection>>,
}

impl NetworkService {
    /// Initializes the network service with the given configuration.
    pub async fn new(mut config: Config) -> Result<Arc<Self>, InitError> {
        // Channel used for the background to communicate to the foreground.
        // Once this channel is full, background tasks that need to send a message to the network
        // service will block and wait for some space to be available.
        //
        // The ideal size of this channel depends on the volume of messages, the time it takes for
        // the network service to be polled after being waken up, and the speed of messages
        // processing. All these components are pretty hard to know in advance, and as such we go
        // for the approach of choosing an arbitrary constant value.
        let (to_foreground, from_background) = mpsc::channel(256);

        // For each listening address in the configuration, create a background task dedicated to
        // listening on that address.
        for listen_address in config.listen_addresses {
            // Try to parse the requested address and create the corresponding listening socket.
            let tcp_listener: async_std::net::TcpListener = {
                let mut iter = listen_address.iter();
                let proto1 = match iter.next() {
                    Some(p) => p,
                    None => return Err(InitError::BadListenMultiaddr(listen_address)),
                };
                let proto2 = match iter.next() {
                    Some(p) => p,
                    None => return Err(InitError::BadListenMultiaddr(listen_address)),
                };

                if iter.next().is_some() {
                    return Err(InitError::BadListenMultiaddr(listen_address));
                }

                let addr = match (proto1, proto2) {
                    (Protocol::Ip4(ip), Protocol::Tcp(port)) => SocketAddr::from((ip, port)),
                    (Protocol::Ip6(ip), Protocol::Tcp(port)) => SocketAddr::from((ip, port)),
                    _ => return Err(InitError::BadListenMultiaddr(listen_address)),
                };

                match async_std::net::TcpListener::bind(addr).await {
                    Ok(l) => l,
                    Err(err) => {
                        return Err(InitError::ListenerIo(listen_address, err));
                    }
                }
            };

            // Spawn a background task dedicated to this listener.
            let mut to_foreground = to_foreground.clone();
            (config.tasks_executor)(Box::pin(async move {
                loop {
                    // TODO: add a way to immediately interrupt the listener if the network service is destroyed (or fails to create altogether), in order to immediately liberate the port

                    let (socket, _addr) = match tcp_listener.accept().await {
                        Ok(v) => v,
                        Err(_) => {
                            // Errors here can happen if the accept failed, for example if no file
                            // descriptor is available.
                            // A wait is added in order to avoid having a busy-loop failing to
                            // accept connections.
                            futures_timer::Delay::new(Duration::from_secs(2)).await;
                            continue;
                        }
                    };

                    if to_foreground
                        .send(FromBackground::NewConnection {
                            socket,
                            is_initiator: false,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }))
        }

        // The peerset, created below, is a data structure that helps keep track of the state of
        // the current peers and connections.
        let mut peerset = peerset::Peerset::new(peerset::Config {
            randomness_seed: rand::random(),
            peers_capacity: 50,
            num_overlay_networks: 1,
        });

        // Add to overlay #0 the nodes known to belong to the network.
        for (peer_id, address) in config.bootstrap_nodes {
            let mut node = peerset.node_mut(peer_id).or_default();
            node.add_known_address(address);
            node.add_to_overlay(0);
        }

        Ok(Arc::new(NetworkService {
            guarded: Mutex::new(Guarded {
                tasks_executor: config.tasks_executor,
                peerset,
            }),
            noise_key: Arc::new(config.noise_key),
            from_background: Mutex::new(from_background),
            to_foreground,
        }))
    }

    /// Returns the number of established TCP connections, both incoming and outgoing.
    pub async fn num_established_connections(&self) -> usize {
        self.guarded
            .lock()
            .await
            .peerset
            .num_established_connections()
    }

    /// Sends a blocks request to the given peer.
    // TODO: more docs
    // TODO: proper error type
    pub async fn blocks_request(
        self: &Arc<Self>,
        target: PeerId,
        config: protocol::BlocksRequestConfig,
    ) -> Result<Vec<protocol::BlockData>, ()> {
        let mut guarded = self.guarded.lock().await;

        let connection = match guarded.peerset.node_mut(target) {
            peerset::NodeMut::Known(n) => n.connections().next().ok_or(())?,
            peerset::NodeMut::Unknown(n) => return Err(()),
        };

        let (send_back, receive_result) = oneshot::channel();

        // TODO: is awaiting here a good idea? if the background task is stuck, we block the entire `Guarded`
        // It is possible for the channel to be closed, if the background task has ended but the
        // frontend hasn't processed this yet.
        guarded
            .peerset
            .connection_mut(connection)
            .unwrap()
            .into_user_data()
            .send(ToConnection::BlocksRequest { config, send_back })
            .await
            .map_err(|_| ())?;

        // Everything must be unlocked at this point.
        drop(guarded);

        // Wait for the result of the request. Can take a long time (i.e. several seconds).
        match receive_result.await {
            Ok(r) => r,
            Err(_) => Err(()),
        }
    }

    /// Returns the next event that happens in the network service.
    ///
    /// If this method is called multiple times simultaneously, the events will be distributed
    /// amongst the different calls in an unpredictable way.
    pub async fn next_event(&self) -> Event {
        loop {
            self.fill_out_slots(&mut self.guarded.lock().await).await;

            match self.from_background.lock().await.next().await.unwrap() {
                FromBackground::NewConnection {
                    socket,
                    is_initiator,
                } => {
                    // A new socket has been accepted by a listener.
                    // Add the socket to the local state, and spawn the task of that connection.
                    /*let (tx, rx) = mpsc::channel(8);
                    let mut guarded = self.guarded.lock().await;
                    let connection_id = guarded.peerset
                    (guarded.tasks_executor)(Box::pin(connection_task(
                        future::ok(socket),
                        is_initiator,
                        self.noise_key.clone(),
                        connection_id,
                        self.to_foreground.clone(),
                        rx,
                    )));*/
                    // TODO: there's nothing in place for pending incoming at the moment
                    todo!()
                }
                FromBackground::HandshakeError { connection_id, .. } => {
                    let mut guarded = self.guarded.lock().await;
                    guarded.peerset.pending_mut(connection_id).unwrap().remove();
                }
                FromBackground::HandshakeSuccess {
                    connection_id,
                    peer_id,
                    accept_tx,
                } => {
                    let mut guarded = self.guarded.lock().await;
                    let id = guarded
                        .peerset
                        .pending_mut(connection_id)
                        .unwrap()
                        .into_established(|tx| tx)
                        .id();
                    accept_tx.send(id).unwrap();
                    return Event::Connected(peer_id);
                }
                FromBackground::Disconnected { connection_id } => {
                    let mut guarded = self.guarded.lock().await;
                    guarded
                        .peerset
                        .connection_mut(connection_id)
                        .unwrap()
                        .remove();
                }
                FromBackground::NotificationsOpenResult {
                    connection_id,
                    result,
                } => todo!(),
                FromBackground::NotificationsCloseResult { connection_id } => todo!(),

                FromBackground::NotificationsOpenDesired { connection_id } => todo!(),

                FromBackground::NotificationsCloseDesired { connection_id } => todo!(),
            }
        }
    }

    /// Spawns new outgoing connections in order to fill empty outgoing slots.
    ///
    /// Must be passed as parameter an existing lock to a [`Guarded`].
    async fn fill_out_slots<'a>(&self, guarded: &mut MutexGuard<'a, Guarded>) {
        // Solves borrow checking errors regarding the borrow of multiple different fields at the
        // same time.
        let guarded = &mut **guarded;

        // TODO: very wip
        while let Some(mut node) = guarded.peerset.random_not_connected(0) {
            // TODO: collecting into a Vec, annoying
            for address in node.known_addresses().cloned().collect::<Vec<_>>() {
                let tcp_socket = match multiaddr_to_socket(&address) {
                    Ok(s) => s,
                    Err(()) => {
                        node.remove_known_address(&address).unwrap();
                        continue;
                    }
                };

                let (tx, rx) = mpsc::channel(8);
                let connection_id = node.add_outbound_attempt(address.clone(), tx);
                (guarded.tasks_executor)(Box::pin(connection_task(
                    tcp_socket,
                    true,
                    self.noise_key.clone(),
                    connection_id,
                    self.to_foreground.clone(),
                    rx,
                )));
            }

            break;
        }
    }
}

/// Error when initializing the network service.
#[derive(Debug, derive_more::Display)]
pub enum InitError {
    /// I/O error when initializing a listener.
    #[display(fmt = "I/O error when creating listener for {}: {}", _0, _1)]
    ListenerIo(Multiaddr, io::Error),
    /// A listening address passed through the configuration isn't valid.
    BadListenMultiaddr(Multiaddr),
}

/// Message sent to a background task dedicated to a connection.
enum ToConnection {
    /// Start a block request. See [`NetworkService::blocks_request`].
    BlocksRequest {
        config: protocol::BlocksRequestConfig,
        send_back: oneshot::Sender<Result<Vec<protocol::BlockData>, ()>>,
    },
    OpenNotifications,
    CloseNotifications,
}

/// Messsage sent from a background task and dedicated to the main [`NetworkService`]. Processed
/// in [`NetworkService::next_event`].
enum FromBackground {
    /// A new socket has arrived on a listening endpoint, or we have reached a remote.
    NewConnection {
        socket: async_std::net::TcpStream,
        is_initiator: bool,
    },

    HandshakeError {
        connection_id: peerset::PendingId,
        error: HandshakeError,
    },
    HandshakeSuccess {
        connection_id: peerset::PendingId,
        peer_id: PeerId,
        accept_tx: oneshot::Sender<peerset::ConnectionId>,
    },

    /// Connection has closed.
    ///
    /// This only concerns connections onto which the handshake had succeeded. For connections on
    /// which the handshake hadn't succeeded, a [`FromBackground::HandshakeError`] is emitted
    /// instead.
    Disconnected {
        connection_id: peerset::ConnectionId,
    },

    /// Response to a [`ToConnection::OpenNotifications`].
    NotificationsOpenResult {
        connection_id: peerset::ConnectionId,
        /// Outcome of the opening. If `Ok`, the notifications protocol is now open. If `Err`, it
        /// is still closed.
        result: Result<(), ()>,
    },

    /// Response to a [`ToConnection::CloseNotifications`].
    ///
    /// Contrary to [`FromBackground::NotificationsOpenResult`], a closing request never fails.
    NotificationsCloseResult {
        connection_id: peerset::ConnectionId,
    },

    /// The remote requests that a notification substream be opened.
    ///
    /// No action has been taken. Send [`ToConnection::OpenNotifications`] to open the substream,
    /// or [`ToConnection::CloseNotifications`] to reject the request from the remote.
    NotificationsOpenDesired {
        connection_id: peerset::ConnectionId,
    },

    /// The remote requests that a notification substream be closed.
    ///
    /// No action has been taken. Send [`ToConnection::CloseNotifications`] in order to close the
    /// substream.
    ///
    /// If this follows a [`FromBackground::NotificationsOpenDesired`], it cancels it.
    NotificationsCloseDesired {
        connection_id: peerset::ConnectionId,
    },
}

/// Asynchronous task managing a specific TCP connection.
async fn connection_task(
    tcp_socket: impl Future<Output = Result<async_std::net::TcpStream, io::Error>>,
    is_initiator: bool,
    noise_key: Arc<connection::NoiseKey>,
    connection_id: peerset::PendingId,
    mut to_foreground: mpsc::Sender<FromBackground>,
    mut to_connection: mpsc::Receiver<ToConnection>,
) {
    // Finishing any ongoing connection process.
    let tcp_socket = match tcp_socket.await {
        Ok(s) => s,
        Err(_) => {
            let _ = to_foreground.send(FromBackground::HandshakeError {
                connection_id,
                error: HandshakeError::Io,
            });
            return;
        }
    };

    // The socket is wrapped around a `WithBuffers` object containing a read buffer and a write
    // buffer. These are the buffers whose pointer is passed to `read(2)` and `write(2)` when
    // reading/writing the socket.
    let tcp_socket = with_buffers::WithBuffers::new(tcp_socket);
    futures::pin_mut!(tcp_socket);

    // Connections start with a handshake where the encryption and multiplexing protocols are
    // negotiated.
    let (connection_prototype, peer_id) =
        match perform_handshake(&mut tcp_socket, &noise_key, is_initiator).await {
            Ok(v) => v,
            Err(error) => {
                let _ = to_foreground.send(FromBackground::HandshakeError {
                    connection_id,
                    error,
                });
                return;
            }
        };

    // Configure the `connection_prototype` to turn it into an actual connection.
    // The protocol names are hardcoded here.
    let mut connection = connection_prototype.into_connection::<_, oneshot::Sender<_>, (), _, _>(
        connection::established::Config {
            in_request_protocols: iter::once("/foo"), // TODO: should be empty; hack because iterator type is identical to notification protocols list
            in_notifications_protocols: iter::once("/dot/block-announces/1"), // TODO: correct protocolId
            ping_protocol: "/ipfs/ping/1.0.0",
            randomness_seed: rand::random(),
        },
    );

    // Notify the outside of the transition from handshake to actual connection, and obtain an
    // updated `connection_id` in return.
    // It is possible for the outside to refuse the connection after the handshake (if e.g. the
    // `PeerId` isn't the one that is expected), in which case the task stops entirely.
    let connection_id = {
        let (accept_tx, accept_rx) = oneshot::channel();

        if to_foreground
            .send(FromBackground::HandshakeSuccess {
                connection_id,
                peer_id,
                accept_tx,
            })
            .await
            .is_err()
        {
            return;
        }

        match accept_rx.await {
            Ok(id) => id,
            Err(_) => return,
        }
    };

    // Set to a timer after which the state machine of the connection needs an update.
    let mut poll_after: futures_timer::Delay;

    loop {
        let (read_buffer, write_buffer) = match tcp_socket.buffers() {
            Ok(b) => b,
            Err(_) => {
                let _ = to_foreground.send(FromBackground::Disconnected { connection_id });
                return;
            }
        };

        let now = Instant::now();

        let read_write =
            match connection.read_write(now, read_buffer.map(|b| b.0), write_buffer.unwrap()) {
                Ok(rw) => rw,
                Err(_) => {
                    let _ = to_foreground.send(FromBackground::Disconnected { connection_id });
                    return;
                }
            };
        connection = read_write.connection;

        if let Some(wake_up) = read_write.wake_up_after {
            if wake_up > now {
                let dur = wake_up - now;
                poll_after = futures_timer::Delay::new(dur);
            } else {
                poll_after = futures_timer::Delay::new(Duration::from_secs(0));
            }
        } else {
            poll_after = futures_timer::Delay::new(Duration::from_secs(3600));
        }

        tcp_socket.advance(read_write.read_bytes, read_write.written_bytes);

        let has_event = read_write.event.is_some();

        match read_write.event {
            Some(connection::established::Event::Response {
                response,
                user_data,
                ..
            }) => {
                if let Ok(response) = response {
                    let decoded = protocol::decode_block_response(&response).unwrap();
                    let _ = user_data.send(Ok(decoded));
                } else {
                    let _ = user_data.send(Err(()));
                }
                continue;
            }
            _ => {}
        }

        if has_event || read_write.read_bytes != 0 || read_write.written_bytes != 0 {
            continue;
        }

        // TODO: maybe optimize the code below so that multiple messages are pulled from `to_connection` at once

        futures::select! {
            _ = tcp_socket.as_mut().process().fuse() => {},
            timeout = (&mut poll_after).fuse() => { // TODO: no, ref mut + fuse() = probably panic
                // Nothing to do, but guarantees that we loop again.
            },
            message = to_connection.select_next_some().fuse() => {
                match message {
                    ToConnection::BlocksRequest { config, send_back } => {
                        let start = config.start.clone();
                        let request = protocol::build_block_request(config)
                            .fold(Vec::new(), |mut a, b| {
                                a.extend_from_slice(b.as_ref());
                                a
                            });
                        connection.add_request(Instant::now(), "/dot/sync/2", request, send_back);
                    }
                    ToConnection::OpenNotifications => {
                        // TODO: finish
                        let id = connection.open_notifications_substream(
                            Instant::now(),
                            "/dot/block-announces/1",
                            Vec::new(), // TODO:
                            ()
                        );
                        todo!()
                    },
                    ToConnection::CloseNotifications => {
                        todo!()
                    },
                }
            }
        }
    }
}

/// Builds a future that connects to the given multiaddress. Returns an error if the multiaddress
/// protocols aren't supported.
fn multiaddr_to_socket(
    addr: &Multiaddr,
) -> Result<impl Future<Output = Result<async_std::net::TcpStream, io::Error>>, ()> {
    let mut iter = addr.iter();
    let proto1 = iter.next().ok_or(())?;
    let proto2 = iter.next().ok_or(())?;

    if iter.next().is_some() {
        return Err(());
    }

    // Ensure ahead of time that the multiaddress is supported.
    match (&proto1, &proto2) {
        (Protocol::Ip4(_), Protocol::Tcp(_))
        | (Protocol::Ip6(_), Protocol::Tcp(_))
        | (Protocol::Dns(_), Protocol::Tcp(_))
        | (Protocol::Dns4(_), Protocol::Tcp(_))
        | (Protocol::Dns6(_), Protocol::Tcp(_)) => {}
        _ => return Err(()),
    }

    let proto1 = proto1.acquire();
    let proto2 = proto2.acquire();

    Ok(async move {
        match (proto1, proto2) {
            (Protocol::Ip4(ip), Protocol::Tcp(port)) => {
                async_std::net::TcpStream::connect(SocketAddr::new(ip.into(), port)).await
            }
            (Protocol::Ip6(ip), Protocol::Tcp(port)) => {
                async_std::net::TcpStream::connect(SocketAddr::new(ip.into(), port)).await
            }
            // TODO: for DNS, do things a bit more explicitly? with for example a library that does the resolution?
            // TODO: differences between DNS, DNS4, DNS6 not respected
            (Protocol::Dns(addr), Protocol::Tcp(port))
            | (Protocol::Dns4(addr), Protocol::Tcp(port))
            | (Protocol::Dns6(addr), Protocol::Tcp(port)) => {
                async_std::net::TcpStream::connect((&*addr, port)).await
            }
            _ => unreachable!(),
        }
    })
}

/// Drives the handshake of the given connection.
///
/// # Panic
///
/// Panics if the `tcp_socket` is closed in the writing direction.
///
async fn perform_handshake(
    tcp_socket: &mut Pin<&mut with_buffers::WithBuffers<async_std::net::TcpStream>>,
    noise_key: &connection::NoiseKey,
    is_initiator: bool,
) -> Result<(connection::established::ConnectionPrototype, PeerId), HandshakeError> {
    let mut handshake = connection::handshake::Handshake::new(is_initiator);

    // Delay that triggers after we consider the remote is considered unresponsive.
    // The constant here has been chosen arbitrary.
    let timeout = futures_timer::Delay::new(Duration::from_secs(20));
    futures::pin_mut!(timeout);

    loop {
        match handshake {
            connection::handshake::Handshake::Success {
                remote_peer_id,
                connection,
            } => {
                break Ok((connection, remote_peer_id));
            }
            connection::handshake::Handshake::NoiseKeyRequired(key) => {
                handshake = key.resume(noise_key).into()
            }
            connection::handshake::Handshake::Healthy(healthy) => {
                let (read_buffer, write_buffer) = match tcp_socket.buffers() {
                    Ok(v) => v,
                    Err(_) => return Err(HandshakeError::Io),
                };

                // Update the handshake state machine with the received data, and writes in the
                // write buffer..
                let (new_state, num_read, num_written) = {
                    let read_buffer = read_buffer.ok_or(HandshakeError::UnexpectedEof)?.0;
                    // `write_buffer` can only be `None` if `close` has been manually called,
                    // which never happens.
                    let write_buffer = write_buffer.unwrap();
                    healthy.read_write(read_buffer, write_buffer)?
                };
                handshake = new_state;
                tcp_socket.advance(num_read, num_written);

                if num_read != 0 || num_written != 0 {
                    continue;
                }

                // Wait either for something to happen on the socket, or for the timeout to
                // trigger.
                {
                    let process_future = tcp_socket.as_mut().process();
                    futures::pin_mut!(process_future);
                    match future::select(process_future, &mut timeout).await {
                        future::Either::Left(_) => {}
                        future::Either::Right(_) => return Err(HandshakeError::Timeout),
                    }
                }
            }
        }
    }
}

#[derive(Debug, derive_more::Display, derive_more::From)]
enum HandshakeError {
    Io,
    Timeout,
    UnexpectedEof,
    Protocol(connection::handshake::HandshakeError),
}
