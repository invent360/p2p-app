mod models;

use crate::models::network::{discover_peers, Args, MyBehaviour, MyBehaviourEvent, NAMESPACE};
use clap::Parser;
use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::{
    Multiaddr, PeerId, Swarm, identify, noise, ping, rendezvous,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux,
};
use std::sync::Arc;
use std::{error::Error, time::Duration};
use tokio::sync::{Mutex, mpsc};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {

    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    let args = Args::parse();

    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|key| MyBehaviour {
            identify: identify::Behaviour::new(identify::Config::new(
                "rendezvous-example/1.0.0".to_string(),
                key.public(),
            )),
            rendezvous_server: rendezvous::server::Behaviour::new(
                rendezvous::server::Config::default(),
            ),
            rendezvous_client: rendezvous::client::Behaviour::new(key.clone()),
            ping: ping::Behaviour::new(ping::Config::new().with_interval(Duration::from_secs(1))),
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
    let discover_peers_handle = tokio::spawn(async move {
        if let (Some(rendezvous_point_address), Some(rendezvous_point)) = (args.rendezvous_point_address.clone(), args.rendezvous_point) {
            let discover_peers_handle = tokio::spawn(async move {
                if let Err(e) = discover_peers(
                    swarm_for_discover_peers,
                    Option::from(rendezvous_point_address),
                    Option::from(rendezvous_point),
                    discovered_peers_sender,
                )
                    .await
                {
                    tracing::error!("Error in discover_peers: {}", e);
                }
            });
        } else {
            tracing::warn!("Rendezvous point address or rendezvous point is not provided. Skipping discover_peers.");
        }
    });

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
                 * Client/Server [Registered]
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

                other => {
                    tracing::debug!("Unhandled {:?}", other);
                }
            }
        }
    });

    // Receive discovered peers in the main thread
    while let Some(peer) = discovered_peers_receiver.recv().await {
        tracing::info!("Discovered peer: {}", peer);
    }

    // Wait for the tasks to finish (they should never finish unless they panic)
    let _ = tokio::join!(discover_peers_handle, main_handle);

    Ok(())
}


