mod models;
mod errors;

use crate::models::network::{connect_to_websocket, discover_peers, Args, MyBehaviour, MyBehaviourEvent, NAMESPACE};
use clap::Parser;
use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::{
    gossipsub, identify, noise, ping, rendezvous, Multiaddr, PeerId, Swarm,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux,
};
use std::sync::Arc;
use std::{collections::hash_map::DefaultHasher, error::Error, hash::{Hash, Hasher}, io, time::Duration};
use actix_web::{web, App, HttpServer};
use tokio::sync::{Mutex, mpsc};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing_subscriber::EnvFilter;
use crate::models::socket::ws_index;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    let args = Args::parse();

    // Start the Actix WebSocket server if websocket_address is provided
    let ws_server = if let Some(ws_addr) = &args.websocket_address {
        let ws_addr = ws_addr.clone();
        Some(tokio::spawn(async move {
            HttpServer::new(|| {
                App::new()
                    .route("/ws/", web::get().to(ws_index))
            })
                .bind(&ws_addr)
                .expect("Failed to bind WebSocket server")
                .run()
                .await
                .expect("WebSocket server failed");
        }))
    } else {
        None
    };

    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|key| {
            // Configure GossipSub
            let message_id_fn = |message: &gossipsub::Message| {
                let mut s = DefaultHasher::new();
                message.data.hash(&mut s);
                gossipsub::MessageId::from(s.finish().to_string())
            };

            let gossip_sub_config = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(10)) // Adjust as needed
                .validation_mode(gossipsub::ValidationMode::Strict) // Enforce message signing
                .message_id_fn(message_id_fn)
                .build()
                .map_err(|msg| io::Error::new(io::ErrorKind::Other, msg))?;

            let mut gossip_sub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossip_sub_config,
            )?;

            // Subscribe to a topic named after the PeerId
            let peer_id_topic = gossipsub::IdentTopic::new(key.public().to_peer_id().to_string());
            gossip_sub.subscribe(&peer_id_topic)?;

            Ok(MyBehaviour {
                identify: identify::Behaviour::new(identify::Config::new(
                    "rendezvous-example/1.0.0".to_string(),
                    key.public(),
                )),
                rendezvous_server: rendezvous::server::Behaviour::new(
                    rendezvous::server::Config::default(),
                ),
                rendezvous_client: rendezvous::client::Behaviour::new(key.clone()),
                ping: ping::Behaviour::new(ping::Config::new().with_interval(Duration::from_secs(1))),
                gossip_sub, // Add GossipSub behaviour
            })
        })?
        .build();

    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    if let Some(addr) = args.external_address {
        swarm.add_external_address(addr.clone());
    } else {
        tracing::warn!("No external address provided.");
    }

    if let Some(addr) = args.rendezvous_point_address.clone() {
        if let Err(e) = swarm.dial(addr.clone()) {
            tracing::error!("Failed to dial rendezvous point address {:?}: {}", addr, e);
        }
    } else {
        tracing::warn!("No rendezvous point address provided.");
    }

    // Wrap the Swarm in an Arc<Mutex> to share it between tasks
    let swarm = Arc::new(Mutex::new(swarm));

    // Create a channel to receive discovered peers
    let (discovered_peers_sender, mut discovered_peers_receiver) = mpsc::channel(32);

    // Clone the Arc<Mutex<Swarm>> for the discover_peers task
    let swarm_for_discover_peers = Arc::clone(&swarm);

    // Spawn the discover_peers task
    let websocket_address = args.websocket_address.clone();

    // Spawn the discover_peers task
    let discover_peers_handle = tokio::spawn(async move {
        if let (Some(rendezvous_point_address), Some(rendezvous_point)) = (args.rendezvous_point_address.clone(), args.rendezvous_point) {
            if let Err(e) = discover_peers(
                swarm_for_discover_peers,
                Option::from(rendezvous_point_address),
                Option::from(rendezvous_point),
                discovered_peers_sender,
                websocket_address, // Use the cloned websocket_address
            )
                .await
            {
                tracing::error!("Error in discover_peers: {}", e);
            }
        } else {
            tracing::warn!("Rendezvous point address or rendezvous point is not provided. Skipping discover_peers.");
        }
    });

    // Spawn a task to handle user input for sending messages
    let swarm_for_main = Arc::clone(&swarm);
    let main_handle = tokio::spawn(async move {
        loop {
            let event = {
                let mut swarm = swarm_for_main.lock().await;
                swarm.select_next_some().await
            };

            match event {
                /*
                 * Server/Client [NewListenAddr]
                 */
                SwarmEvent::NewListenAddr { address, .. } => {
                    tracing::info!("Listening on {address:?}");
                }

                /*
                 * Client/Server [ConnectionEstablished]
                 */
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    tracing::info!("Connected to {}", peer_id);

                }

                /*
                 * Server [Registered]
                 */
                SwarmEvent::Behaviour(MyBehaviourEvent::RendezvousServer(
                                          rendezvous::server::Event::PeerRegistered { peer, registration },
                                      )) => {
                    tracing::info!(
                        "Peer {} registered for namespace '{}'",
                        peer,
                        registration.namespace
                    );
                }

                /*
                 * Server [DiscoverServed]
                 */
                SwarmEvent::Behaviour(MyBehaviourEvent::RendezvousServer(
                                          rendezvous::server::Event::DiscoverServed {
                                              enquirer,
                                              registrations,
                                          },
                                      )) => {
                    tracing::info!(
                        "Served peer {} with {} registrations",
                        enquirer,
                        registrations.len()
                    );
                }

                /*
                 * Server [ConnectionClosed]
                 */
                SwarmEvent::ConnectionClosed {
                    peer_id,
                    cause: Some(error),
                    ..
                } => {
                    tracing::info!("Disconnected from {}", peer_id);
                    if let Some(rendezvous_point) = args.rendezvous_point {
                        if peer_id == rendezvous_point {
                            tracing::error!("Lost connection to rendezvous point {}", error);
                        }
                    }
                }

                /*
                 * Handle GossipSub Events
                 */
                SwarmEvent::Behaviour(MyBehaviourEvent::GossipSub(gossipsub::Event::Message {
                                                                      propagation_source: peer_id,
                                                                      message_id: id,
                                                                      message,
                                                                  })) => {
                    tracing::info!(
                        "Received message: '{}' with id: {} from peer: {} on topic: {:?}",
                        String::from_utf8_lossy(&message.data),
                        id,
                        peer_id,
                        message.topic
                    );
                }

                other => {
                    tracing::debug!("Unhandled {:?}", other);
                }
            }
        }
    });

    // Receive discovered peers in the main thread
    while let Some(peer) = discovered_peers_receiver.recv().await {
        tracing::info!("Discovered peer: {}", peer);

        let topic = gossipsub::IdentTopic::new(peer.to_string());
        let message = format!("Hello, peer {}!", peer);
        let mut swarm = swarm.lock().await;
        if let Err(e) = swarm.behaviour_mut().gossip_sub.publish(topic, message.as_bytes()) {
            tracing::error!("Failed to publish message: {}", e);
        } else {
            tracing::info!("Message published to topic: {}", peer);
        }

/*        if let Some(ws_addr) = &args.websocket_address {
            let ws_addr = ws_addr.clone();
            let peer = peer.clone(); // Ensure `peer` is `Send`

            tokio::spawn(async move {
                if let Err(e) = connect_to_websocket(ws_addr, peer).await {
                    tracing::error!("Failed to connect to WebSocket: {}", e);
                }
            });
        }*/
    }

    // Wait for the tasks to finish (they should never finish unless they panic)
    if let Some(ws_server) = ws_server {
        let _ = tokio::join!(ws_server, discover_peers_handle, main_handle);
    } else {
        let _ = tokio::join!(discover_peers_handle, main_handle);
    }

    Ok(())
}
