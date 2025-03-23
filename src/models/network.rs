use clap::Parser;
use futures::{SinkExt, StreamExt};
use libp2p::multiaddr::Protocol;
use libp2p::rendezvous::Cookie;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{Multiaddr, PeerId, Swarm, identify, ping, rendezvous, gossipsub};
use std::collections::HashSet;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio::time::interval;
use futures::future::FutureExt;
use actix::prelude::*;
use actix_web_actors::ws;
use std::{collections::hash_map::DefaultHasher, error::Error as StdError, hash::{Hash, Hasher}, io};
use awc::Client;
use awc::ws::{Frame, Message as WsMessage};
use bytestring::ByteString;
use crate::errors::{WsError};

pub const NAMESPACE: &str = "rendezvous";

#[derive(NetworkBehaviour)]
pub struct MyBehaviour {
    pub identify: identify::Behaviour,
    pub rendezvous_server: rendezvous::server::Behaviour,
    pub rendezvous_client: rendezvous::client::Behaviour,
    pub gossip_sub: gossipsub::Behaviour,
    pub ping: ping::Behaviour,
}

#[derive(Parser, Debug)]
#[clap(name = "libp2p_rendezvous_client")]
pub struct Args {
    #[clap(long)]
    pub external_address: Option<Multiaddr>,

    #[clap(long)]
    pub rendezvous_point_address: Option<Multiaddr>,

    #[clap(long)]
    pub rendezvous_point: Option<PeerId>,

    #[clap(long)]
    pub websocket_address: Option<String>,
}

pub async fn discover_peers(
    swarm: Arc<Mutex<Swarm<MyBehaviour>>>,
    rendezvous_point_address: Option<Multiaddr>,
    rendezvous_point: Option<PeerId>,
    discovered_peers_sender: mpsc::Sender<PeerId>,
    websocket_address: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {

    let mut swarm = swarm.lock().await;

    if let Some(addr) = rendezvous_point_address.clone() {
        swarm.dial(addr.clone())?;
    }

    let mut discover_tick = tokio::time::interval(Duration::from_secs(5));
    let mut cookie = None;

    loop {
        tokio::select! {
            event = async {
                swarm.select_next_some().await
            }.boxed().fuse() => match event {

                /*
                 * Incoming Connection
                 */
                SwarmEvent::IncomingConnection { local_addr, send_back_addr, .. } => {
                    tracing::info!(
                        "Incoming connection from {} to {}",
                        send_back_addr,
                        local_addr
                    );
                }

                /*
                 * Incoming Connection Error
                 */
                SwarmEvent::IncomingConnectionError { local_addr, send_back_addr, error, .. } => {
                    tracing::error!(
                        "Incoming connection error from {} to {}: {}",
                        send_back_addr,
                        local_addr,
                        error
                    );
                }

                /*
                 * Connection Closed
                 */
                SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                    if let Some(error) = cause {
                        tracing::error!(
                            "Connection closed with peer {}: {}",
                            peer_id,
                            error
                        );
                    } else {
                        tracing::info!(
                            "Connection closed with peer {}",
                            peer_id
                        );
                    }
                }

                    /*
                     * Outgoing Connection Error (Dial Failure)
                     */
                    SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                        if let Some(peer_id) = peer_id {
                            tracing::error!(
                                "Failed to dial peer {}: {}",
                                peer_id,
                                error
                            );
                        } else {
                            tracing::error!(
                                "Failed to dial peer: {}",
                                error
                            );
                        }
                    }

                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    if Some(peer_id) == rendezvous_point.clone() {
                        /*
                         * [1]:Discoverer Log
                         */
                        tracing::info!(
                            "Connecting to rendezvous point {}, & discovering nodes in '{}' namespace ...",
                            peer_id,
                            NAMESPACE
                        );

                        /*
                         * [2]:Registration at rdv server
                         */
                        if let Err(error) = swarm.behaviour_mut().rendezvous_client.register(
                            rendezvous::Namespace::from_static("rendezvous"),
                            peer_id,
                            None,
                        ) {
                            tracing::error!("Failed to register: {error}");
                        }
                        tracing::info!("Connection established with rendezvous point {}", peer_id);

                        /*
                         * [3] Discovery
                         */
                        if let Some(rdv_peer_id) = rendezvous_point {
                                swarm.behaviour_mut().rendezvous_client.discover(
                                    Some(rendezvous::Namespace::new(NAMESPACE.to_string()).unwrap()),
                                    None,
                                    None,
                                    rdv_peer_id,
                                );
                            }


                    } else {
                        tracing::info!("Publishing message to peer");
                        let topic = gossipsub::IdentTopic::new(peer_id.to_string());
                        let message = format!("Hello, peer {}!", peer_id);
                        if let Err(e) = swarm.behaviour_mut().gossip_sub.publish(topic, message.as_bytes()) {
                            tracing::error!("Failed to publish message: {}", e);
                        } else {
                            tracing::info!("Message published to topic: {}", peer_id);
                        }
                    }
                }

                /*
                 * Client [Registered]
                 */
                SwarmEvent::Behaviour(MyBehaviourEvent::RendezvousClient(
                    rendezvous::client::Event::Registered {
                        namespace,
                        ttl,
                        rendezvous_node,
                    },
                )) => {
                    tracing::info!(
                        "Registered for namespace '{}' at rendezvous point {} for the next {} seconds",
                        namespace,
                        rendezvous_node,
                        ttl
                    );
                }

                /*
                 * Client [RegisterFailed]
                 */
                SwarmEvent::Behaviour(MyBehaviourEvent::RendezvousClient(
                rendezvous::client::Event::RegisterFailed {
                    rendezvous_node,
                    namespace,
                    error,
                },
            )) => {
                tracing::error!(
                    "Failed to register: rendezvous_node={}, namespace={}, error_code={:?}",
                    rendezvous_node,
                    namespace,
                        error
                    );
                }

                /*
                 * Client [Discovered]
                 */
                SwarmEvent::Behaviour(MyBehaviourEvent::RendezvousClient(
                    rendezvous::client::Event::Discovered {
                        registrations,
                        cookie: new_cookie,
                        ..
                    }
                )) => {
                    cookie.replace(new_cookie);
                    tracing::info!("Discovered {} peers", registrations.len());

                    for registration in registrations {
                        for address in registration.record.addresses() {
                            let peer = registration.record.peer_id();
                            tracing::info!(%peer, %address, "Discovered peer");

                            let p2p_suffix = Protocol::P2p(peer);
                            let address_with_p2p = if !address.ends_with(&Multiaddr::empty().with(p2p_suffix.clone())) {
                                address.clone().with(p2p_suffix)
                            } else {
                                address.clone()
                            };

                            tracing::info!("Dialing discovered peer {}", address_with_p2p.clone());
                            if let Err(e) = swarm.dial(address_with_p2p) {
                                tracing::error!("Failed to dial peer {}: {}", peer, e);
                            } else {
                                 tracing::info!("Dial successful");

                                if let Err(e) = discovered_peers_sender.send(peer).await {
                                    tracing::error!("Failed to send discovered peer to main thread: {}", e);
                                }
                            }
                        }
                    }
                }

                // Handle ping events (optional)
                SwarmEvent::Behaviour(MyBehaviourEvent::Ping(ping::Event {
                    peer,
                    result: Ok(rtt),
                    ..
                })) => {
                    // Only log ping events if the peer is not the rendezvous point
                    if let Some(rendezvous_peer) = rendezvous_point.clone() {
                        if peer != rendezvous_peer {
                            tracing::info!(%peer, "Ping is {}ms", rtt.as_millis());
                        }
                    }
                }

                // Handle other unhandled events
                other => {
                    tracing::debug!("Unhandled SwarmEvent: {:?}", other);
                }
            },

            // Periodic discovery tick
            _ = discover_tick.tick(), if cookie.is_some() => {
                if let Some(rendezvous_point) = rendezvous_point.clone() {
                    swarm.behaviour_mut().rendezvous_client.discover(
                        Some(rendezvous::Namespace::new(NAMESPACE.to_string()).unwrap()),
                        cookie.clone(),
                        None,
                        rendezvous_point,
                    );
                    tracing::info!("Sent periodic discovery request to rendezvous point {}", rendezvous_point);
                }
            }
        }
    }
}


pub async fn connect_to_websocket(ws_url: String, peer_id: PeerId) -> Result<(), Box<dyn StdError + Send + Sync>> {
    let client = Client::new(); // Use the awc client

    // Connect to the WebSocket server and map the error to the custom error type
    let (response, mut framed) = client.ws(&ws_url)
        .connect()
        .await
        .map_err(|e| Box::new(WsError::ClientError(e)) as Box<dyn StdError + Send + Sync>)?;

    tracing::info!("Connected to WebSocket server for peer {}", peer_id);

    // Convert the formatted String to ByteString
    let message = ByteString::from(format!("Hello from peer {}", peer_id));

    // Send a message to the WebSocket server
    framed.send(WsMessage::Text(message)).await
        .map_err(|e| Box::new(WsError::ProtocolError(e)) as Box<dyn StdError + Send + Sync>)?;

    Ok(())
}
