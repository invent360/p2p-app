mod models;
mod handlers;
mod actors;
mod errors;

use crate::models::network::{discover_peers, start_websocket, Args, MyBehaviour, MyBehaviourEvent, NAMESPACE};
use clap::Parser;
use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId, Swarm, identify, noise, ping, rendezvous, swarm::{NetworkBehaviour, SwarmEvent}, tcp, yamux, gossipsub, kad, StreamProtocol};
use std::sync::Arc;
use std::{ io, thread, time::Duration};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::AtomicUsize;
use actix::Actor;
use actix_cors::Cors;
use actix_web::dev::Server;
use actix_web::{web, App, HttpServer};
use actix_web::middleware::Logger;
use tokio::sync::{Mutex, mpsc};
use std::sync::mpsc as std_msc;
use anyhow::bail;
use async_std::prelude::FutureExt;
use libp2p::kad::{GetProvidersOk, QueryResult, RecordKey};
use tokio::task;
use tracing_subscriber::EnvFilter;

const BOOTNODES: [&str; 4] = [
    "QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN",
    "QmQCU2EcMqAqQPR2i9bChDtGNJchTbq5TbXJJ16u19uLTa",
    "QmbLHAnMoJPWSCR5Zhtx6BHJX9KiKNN6tpvbUcqanj75Nb",
    "QmcZf59bWwK5XFi76CZX8cbJ4BhTzzA3gU1ZjYZcYW3dwt",
];
const IPFS_PROTO_NAME: StreamProtocol = StreamProtocol::new("/ipfs/kad/1.0.0");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {

    let local_set = tokio::task::LocalSet::new();

    local_set.run_until(async {
        // Initialize tracing
        let _ = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .try_init();

        // Parse arguments
        let args = Args::parse();

        // Create and configure the swarm
        let mut swarm = libp2p::SwarmBuilder::with_new_identity()
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )?
            .with_behaviour(|key| {
                // Create a Kademlia behaviour.
                let mut cfg = kad::Config::new(IPFS_PROTO_NAME);
                cfg.set_query_timeout(Duration::from_secs(5 * 60));
                let store = kad::store::MemoryStore::new(key.public().to_peer_id());
                let kademlia = kad::Behaviour::with_config(key.public().to_peer_id(), store, cfg);

                Ok(MyBehaviour {
                    identify: identify::Behaviour::new(identify::Config::new(
                        "rendezvous-example/1.0.0".to_string(),
                        key.public(),
                    )),
                    rendezvous_server: rendezvous::server::Behaviour::new(
                        rendezvous::server::Config::default(),
                    ),
                    rendezvous_client: rendezvous::client::Behaviour::new(key.clone()),
                    kademlia,
                })
            })?
            .build();

        // Listen on the specified address
        swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

        // Start WebSocket server
/*        let websocket_server = start_websocket().await?;
        let websocket_handle = websocket_server.handle();
        let server_task = local_set.spawn_local(websocket_server);*/

        // Perform other setup tasks
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

        // Spawn the discover_peers task if we have the required arguments
        let discover_peers_handle = if let (Some(rendezvous_point_address), Some(rendezvous_point)) =
            (args.rendezvous_point_address.clone(), args.rendezvous_point)
        {
            Some(local_set.spawn_local(async move {
                if let Err(e) = discover_peers(
                    swarm_for_discover_peers,
                    Some(rendezvous_point_address),
                    Some(rendezvous_point),
                    discovered_peers_sender,
                )
                    .await
                {
                    tracing::error!("Error in discover_peers: {}", e);
                }
            }))
        } else {
            tracing::warn!("Rendezvous point address or rendezvous point is not provided. Skipping discover_peers.");
            None
        };

        let swarm_for_main = Arc::clone(&swarm);
/*        for peer in &BOOTNODES {
            swarm_for_main.lock().await
                .behaviour_mut()
                .kademlia
                .add_address(&peer.parse()?, "/dnsaddr/bootstrap.libp2p.io".parse()?);
        }*/

        // Remove the IPFS bootstrap nodes and replace with:
        if let Some(bootstrap_peer) = args.bootstrap_peer {
            swarm_for_main.lock().await
                .behaviour_mut()
                .kademlia.add_address(&bootstrap_peer, "/ip4/127.0.0.1/tcp/4001".parse()?);
        }

        // Create a future for the swarm event loop
        let swarm_loop = async move {
            loop {
                let event = {
                    let mut swarm = swarm_for_main.lock().await;
                    swarm.select_next_some().await
                };

                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        tracing::info!("Listening on {address:?}");

                        let local_peer_id = {
                            let swarm = swarm_for_main.lock().await;
                            *swarm.local_peer_id()
                        };

                        tracing::info!("Searching for the closest peers to our ID {}", local_peer_id);

                        // Always try to get closest peers first
                        {
                            let mut swarm = swarm_for_main.lock().await;
                            swarm.behaviour_mut().kademlia.get_closest_peers(local_peer_id);
                        }

                        // If that fails, the error handler will trigger bootstrap
                    }



                    // Handle successful closest peers query
                    SwarmEvent::Behaviour(MyBehaviourEvent::Kademlia(
                                              kad::Event::OutboundQueryProgressed {
                                                  result: kad::QueryResult::GetClosestPeers(Ok(ok)),
                                                  ..
                                              },
                                          )) => {
                        tracing::info!("Found {} closest peers: {:?}", ok.peers.len(), ok.peers);
                    }

                    // Handle failed closest peers query
                    SwarmEvent::Behaviour(MyBehaviourEvent::Kademlia(
                                              kad::Event::OutboundQueryProgressed {
                                                  result: kad::QueryResult::GetClosestPeers(Err(e)),
                                                  ..
                                              },
                                          )) => {
                        tracing::warn!("Failed to get closest peers: {:?}", e);
                        // Retry after delay
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        let mut swarm = swarm_for_main.lock().await;
                        swarm.behaviour_mut().kademlia.bootstrap().unwrap();
                    }

                    // Handle bootstrap completed
                   /* SwarmEvent::Behaviour(MyBehaviourEvent::Kademlia(
                                              kad::Event::OutboundQueryProgressed {
                                                  result: kad::QueryResult::Bootstrap(Ok(ok)),
                                                  ..
                                              },
                                          )) => {
                        tracing::info!("Successfully bootstrapped DHT");
                        let local_peer_id = {
                            let swarm = swarm_for_main.lock().await;
                            *swarm.local_peer_id()
                        };
                        let mut swarm = swarm_for_main.lock().await;
                        swarm.behaviour_mut().kademlia.get_closest_peers(local_peer_id);
                    }*/
                    // Replace your bootstrap event handler with:
                    // After bootstrap completes
                    SwarmEvent::Behaviour(MyBehaviourEvent::Kademlia(
                                              kad::Event::OutboundQueryProgressed {
                                                  result: kad::QueryResult::Bootstrap(Ok(_)),
                                                  ..
                                              },
                                          )) => {
                        tracing::info!("Successfully bootstrapped DHT");
                        let record_key = RecordKey::new(&NAMESPACE);
                        let mut swarm = swarm_for_main.lock().await;

                        match swarm.behaviour_mut().kademlia.start_providing(record_key.clone()) {
                            Ok(query_id) => {
                                tracing::info!("Started providing record (query id: {:?})", query_id);
                            }
                            Err(e) => {
                                tracing::error!("Failed to start providing record: {}", e);
                            }
                        }

                        // Then query for providers
                        swarm.behaviour_mut().kademlia.get_providers(record_key);
                        //swarm.behaviour_mut().kademlia.get_closest_peers(swarm.local_peer_id());
                    }


                    // Other event handlers...
                    other => {
                        tracing::debug!("Unhandled event: {:?}", other);
                    }
                }
            }
        };

        // Receive discovered peers in the main thread
        let peers_task = local_set.spawn_local(async move {
            while let Some(peer) = discovered_peers_receiver.recv().await {
                tracing::info!("Dialing discovered peer @ main {}", peer.clone());
            }
            tracing::warn!("Discovered peers channel closed");
        });

        tokio::select! {
        _ = swarm_loop => {
            tracing::error!("Swarm loop exited unexpectedly");
        }
/*        _ = server_task => {
            tracing::info!("WebSocket server task completed");
        }*/
        _ = peers_task => {
            tracing::warn!("Discovered peers handler exited");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received Ctrl-C, shutting down");
        }
    }

        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }).await?;

    Ok(())
}
