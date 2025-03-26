mod models;
mod handlers;
mod actors;
mod errors;

use crate::models::network::{discover_peers, Args, start_websocket, MyBehaviour, MyBehaviourEvent, NAMESPACE};
use clap::Parser;
use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId, Swarm, identify, ping, rendezvous, gossipsub, kad::{self, store::MemoryStore}, mdns::{self, tokio::Behaviour as MdnsBehaviour}, StreamProtocol, tcp, noise, yamux};
use std::sync::Arc;
use std::{io, thread, time::Duration};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use actix::Actor;
use actix_cors::Cors;
use actix_web::dev::Server;
use actix_web::{web, App, HttpServer};
use actix_web::middleware::Logger;
use tokio::sync::{Mutex, mpsc};
use std::sync::mpsc as std_msc;
use anyhow::bail;
use async_std::prelude::FutureExt;
use libp2p::swarm::SwarmEvent;
use tokio::task;
use tracing_subscriber::EnvFilter;
use serde_json::{json, Value};
use std::collections::HashMap;
use libp2p::kad::RoutingUpdate;

// Metrics structure
#[derive(Default)]
struct Metrics {
    peers_discovered: AtomicUsize,
    connection_attempts: AtomicUsize,
    successful_connections: AtomicUsize,
    failed_connections: AtomicUsize,
    discovery_retries: AtomicUsize,
}

const BOOTNODES: [&str; 4] = [
    "QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN",
    "QmQCU2EcMqAqQPR2i9bChDtGNJchTbq5TbXJJ16u19uLTa",
    "QmbLHAnMoJPWSCR5Zhtx6BHJX9KiKNN6tpvbUcqanj75Nb",
    "QmcZf59bWwK5XFi76CZX8cbJ4BhTzzA3gU1ZjYZcYW3dwt",
];

const BOOTSTRAP_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_RETRIES: usize = 5;
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);

const IPFS_PROTO_NAME: StreamProtocol = StreamProtocol::new("/ipfs/kad/1.0.0");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let local_set = tokio::task::LocalSet::new();
    let metrics = Arc::new(Metrics::default());

    local_set.run_until(async {
        // Initialize tracing with JSON output
        let _ = tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_env_filter(EnvFilter::from_default_env())
            .try_init();

        // Parse arguments
        let args = Args::parse();

        // Create and configure the swarm with optimized Kademlia parameters
        let mut swarm = libp2p::SwarmBuilder::with_new_identity()
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                || yamux::Config::default(),  // Wrap in a closure
            )?
            .with_dns()?
            .with_behaviour(|key| {

                let store = MemoryStore::new(key.public().to_peer_id());
                let mut cfg = kad::Config::new(IPFS_PROTO_NAME);

                // Optimized Kademlia parameters
                cfg.set_query_timeout(Duration::from_secs(20));
                cfg.set_replication_factor(NonZeroUsize::new(15).unwrap());
                cfg.set_record_ttl(Some(Duration::from_secs(24 * 60 * 60))); // 24 hours
                cfg.set_replication_interval(Some(Duration::from_secs(30 * 60))); // 30 minutes
                cfg.set_publication_interval(Some(Duration::from_secs(12 * 60 * 60))); // 12 hours
                cfg.set_provider_record_ttl(Some(Duration::from_secs(24 * 60 * 60))); // 24 hours
                cfg.set_provider_publication_interval(Some(Duration::from_secs(6 * 60 * 60))); // 6 hours
                cfg.set_periodic_bootstrap_interval(Some(Duration::from_secs(5 * 60))); // 5 minutes
                cfg.set_kbucket_size(NonZeroUsize::new(15).unwrap());
                cfg.set_kbucket_pending_timeout(Duration::from_secs(30));

                let store = MemoryStore::new(key.public().to_peer_id());
                let kad = kad::Behaviour::with_config(key.public().to_peer_id(), store, cfg);

                let mdns = MdnsBehaviour::new(
                    mdns::Config::default(),
                    key.public().to_peer_id(),
                )?;

                Ok(MyBehaviour {
                    identify: identify::Behaviour::new(identify::Config::new(
                        "rendezvous-example/1.0.0".to_string(),
                        key.public(),
                    )),
                    rendezvous_server: rendezvous::server::Behaviour::new(
                        rendezvous::server::Config::default(),
                    ),
                    rendezvous_client: rendezvous::client::Behaviour::new(key.clone()),
                    kademlia: kad,
                    mdns,
                })
            })?
            .build();

        // Add bootnodes with error handling
        for peer in &BOOTNODES {
            match swarm.behaviour_mut().kademlia.add_address(
                &peer.parse()?,
                "/dnsaddr/bootstrap.libp2p.io".parse()?,
            ) {
                RoutingUpdate::Failed => {
                    tracing::warn!("Failed to add boot node {}", peer);
                }
                RoutingUpdate::Pending => {
                    tracing::debug!("Boot node {} added as pending", peer);
                }
                RoutingUpdate::Success => {
                    tracing::debug!("Boot node {} added successfully", peer);
                }
            }
        }

        // Listen on the specified address with retry logic
        let mut retries = 0;
        let listen_result = loop {
            match swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?) {
                Ok(_) => break Ok(()),
                Err(e) if retries < MAX_RETRIES => {
                    let delay = INITIAL_RETRY_DELAY * (retries as u32 + 1);
                    tracing::warn!("Failed to listen (attempt {}): {}. Retrying in {:?}", retries + 1, e, delay);
                    tokio::time::sleep(delay).await;
                    retries += 1;
                }
                Err(e) => break Err(e),
            }
        };
        listen_result?;

        // Small delay to ensure listening is established
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Get closest peers to our own ID
        let local_peer_id = *swarm.local_peer_id();
        tracing::info!(peer_id = %local_peer_id, "Searching for the closest peers to our ID");
        swarm.behaviour_mut().kademlia.get_closest_peers(local_peer_id.to_bytes());

        // Start WebSocket server with enhanced error handling
        let websocket_server = match start_websocket().await {
            Ok(server) => server,
            Err(e) => {
                tracing::error!("Failed to start WebSocket server: {}", e);
                return Err(e);
            }
        };
        let websocket_handle = websocket_server.handle();
        let server_task = local_set.spawn_local(websocket_server);

        // Perform other setup tasks
        if let Some(addr) = args.external_address {
            swarm.add_external_address(addr.clone());
            tracing::info!(address = %addr, "Added external address");
        } else {
            tracing::warn!("No external address provided.");
        }

        if let Some(addr) = args.rendezvous_point_address.clone() {
            match swarm.dial(addr.clone()) {
                Ok(_) => tracing::info!(address = %addr, "Successfully dialed rendezvous point"),
                Err(e) => {
                    tracing::error!(address = %addr, error = %e, "Failed to dial rendezvous point address");
                    metrics.failed_connections.fetch_add(1, Ordering::Relaxed);
                }
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
        let metrics_for_discover = Arc::clone(&metrics);

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
                    metrics_for_discover,
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
        let metrics_for_swarm = Arc::clone(&metrics);

        // Create a future for the swarm event loop
        let swarm_loop = async move {
            let mut retry_count = 0;
            loop {
                let event = {
                    let mut swarm = swarm_for_main.lock().await;
                    swarm.select_next_some().await
                };

                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        metrics_for_swarm.connection_attempts.fetch_add(1, Ordering::Relaxed);
                        tracing::info!(address = %address, "Listening on address");

                        let local_peer_id = {
                            let swarm = swarm_for_main.lock().await;
                            *swarm.local_peer_id()
                        };

                        let mut swarm = swarm_for_main.lock().await;
                        let kademlia = &mut swarm.behaviour_mut().kademlia;

                        tracing::info!("Bootstrapping Kademlia and finding closest peers");

                        match kademlia.bootstrap() {
                            Ok(_) => {
                                metrics_for_swarm.successful_connections.fetch_add(1, Ordering::Relaxed);
                                kademlia.get_closest_peers(local_peer_id.to_bytes());
                                retry_count = 0; // Reset retry counter on success
                            }
                            Err(e) => {
                                metrics_for_swarm.failed_connections.fetch_add(1, Ordering::Relaxed);
                                metrics_for_swarm.discovery_retries.fetch_add(1, Ordering::Relaxed);

                                // Exponential backoff
                                let delay = BOOTSTRAP_RETRY_DELAY * (retry_count as u32 + 1);
                                tracing::warn!(
                                    error = %e,
                                    retry_count,
                                    delay_secs = delay.as_secs(),
                                    "Initial bootstrap failed, will retry"
                                );

                                retry_count += 1;
                                if retry_count > MAX_RETRIES {
                                    tracing::error!("Max retries reached for bootstrap");
                                    break;
                                }

                                // Need to drop the lock before spawning async task
                                drop(swarm);

                                let swarm_for_retry = Arc::clone(&swarm_for_main);
                                tokio::spawn(async move {
                                    tokio::time::sleep(delay).await;
                                    let mut swarm = swarm_for_retry.lock().await;
                                    if let Err(e) = swarm.behaviour_mut().kademlia.bootstrap() {
                                        tracing::warn!("Bootstrap retry failed: {}", e);
                                    }
                                });
                            }
                        }
                    }

                    SwarmEvent::Behaviour(MyBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                        for (peer_id, addr) in list {
                            metrics_for_swarm.peers_discovered.fetch_add(1, Ordering::Relaxed);
                            tracing::info!(
                                peer_id = %peer_id,
                                address = %addr,
                                source = "mdns",
                                "Discovered local peer"
                            );
                        }
                    }
                    SwarmEvent::Behaviour(MyBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                        for (peer_id, _multiaddr) in list {
                            tracing::info!(
                                peer_id = %peer_id,
                                "mDNS discover peer has expired"
                            );
                        }
                    },

                    // Handle successful closest peers query
                    SwarmEvent::Behaviour(MyBehaviourEvent::Kademlia(
                                              kad::Event::OutboundQueryProgressed {
                                                  result: kad::QueryResult::GetClosestPeers(Ok(ok)),
                                                  ..
                                              },
                                          )) => {
                        if ok.peers.is_empty() {
                            tracing::warn!("Query finished with no closest peers");
                        } else {
                            metrics_for_swarm.peers_discovered.fetch_add(ok.peers.len(), Ordering::Relaxed);
                            tracing::info!(
                                peer_count = ok.peers.len(),
                                "Discovered closest peers"
                            );

                            // Create structured JSON output
                            let peers_json: Vec<Value> = ok.peers.iter().enumerate().map(|(i, peer)| {
                                json!({
                                    "peer_number": i + 1,
                                    "peer_id": peer.peer_id.to_string(),
                                    "addresses": peer.addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>()
                                })
                            }).collect();

                            tracing::info!(
                                peers = %serde_json::to_string_pretty(&peers_json).unwrap_or_default(),
                                "Peer details"
                            );
                        }
                    }

                    // Handle failed closest peers query
                    SwarmEvent::Behaviour(MyBehaviourEvent::Kademlia(
                                              kad::Event::OutboundQueryProgressed {
                                                  result: kad::QueryResult::GetClosestPeers(Err(e)),
                                                  ..
                                              },
                                          )) => {
                        metrics_for_swarm.failed_connections.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            error = %e,
                            "Failed to get closest peers"
                        );

                        // Exponential backoff for retry
                        let delay = Duration::from_secs(5) * (retry_count as u32 + 1);
                        tokio::time::sleep(delay).await;

                        let mut swarm = swarm_for_main.lock().await;
                        if let Err(e) = swarm.behaviour_mut().kademlia.bootstrap() {
                            tracing::warn!("Bootstrap failed: {}", e);
                        }
                    }

                    // Other event handlers...
                    other => {
                        tracing::debug!(event = ?other, "Unhandled event");
                    }
                }
            }
        };

        // Receive discovered peers in the main thread
        let peers_metrics = Arc::clone(&metrics);
        let peers_task = local_set.spawn_local(async move {
            while let Some(peer) = discovered_peers_receiver.recv().await {
                peers_metrics.peers_discovered.fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    peer_id = %peer,
                    "Dialing discovered peer @ main"
                );
            }
            tracing::warn!("Discovered peers channel closed");
        });

        // Periodic metrics logging
        let metrics_task = local_set.spawn_local({
            let metrics = Arc::clone(&metrics);
            async move {
                let mut interval = tokio::time::interval(Duration::from_secs(60));
                loop {
                    interval.tick().await;
                    tracing::info!(
                        metrics = %serde_json::to_string(&json!({
                            "peers_discovered": metrics.peers_discovered.load(Ordering::Relaxed),
                            "connection_attempts": metrics.connection_attempts.load(Ordering::Relaxed),
                            "successful_connections": metrics.successful_connections.load(Ordering::Relaxed),
                            "failed_connections": metrics.failed_connections.load(Ordering::Relaxed),
                            "discovery_retries": metrics.discovery_retries.load(Ordering::Relaxed),
                        })).unwrap_or_default(),
                        "Metrics snapshot"
                    );
                }
            }
        });

        tokio::select! {
            _ = swarm_loop => {
                tracing::error!("Swarm loop exited unexpectedly");
            }
            _ = server_task => {
                tracing::info!("WebSocket server task completed");
            }
            _ = peers_task => {
                tracing::warn!("Discovered peers handler exited");
            }
            _ = metrics_task => {
                tracing::warn!("Metrics task exited");
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received Ctrl-C, shutting down");

                // Print final metrics
                tracing::info!(
                    metrics = %serde_json::to_string_pretty(&json!({
                        "peers_discovered": metrics.peers_discovered.load(Ordering::Relaxed),
                        "connection_attempts": metrics.connection_attempts.load(Ordering::Relaxed),
                        "successful_connections": metrics.successful_connections.load(Ordering::Relaxed),
                        "failed_connections": metrics.failed_connections.load(Ordering::Relaxed),
                        "discovery_retries": metrics.discovery_retries.load(Ordering::Relaxed),
                    })).unwrap_or_default(),
                    "Final metrics"
                );
            }
        }

        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }).await?;

    Ok(())
}
