use clap::Parser;
use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::rendezvous::Cookie;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{Multiaddr, PeerId, Swarm, identify, ping, rendezvous};
use std::collections::HashSet;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio::time::interval;
use futures::future::FutureExt;

pub const NAMESPACE: &str = "rendezvous";

#[derive(NetworkBehaviour)]
pub struct MyBehaviour {
    pub identify: identify::Behaviour,
    pub rendezvous_server: rendezvous::server::Behaviour,
    pub rendezvous_client: rendezvous::client::Behaviour,
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
}

pub async fn discover_peers(
    swarm: Arc<Mutex<Swarm<MyBehaviour>>>,
    rendezvous_point_address: Option<Multiaddr>,
    rendezvous_point: Option<PeerId>,
    discovered_peers_sender: mpsc::Sender<PeerId>,
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
                 * Server [ConnectionEstablished]
                 */
                SwarmEvent::ConnectionEstablished { peer_id, .. }
                if Some(peer_id) == rendezvous_point => {
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

                            if let Err(e) = swarm.dial(address_with_p2p) {
                                tracing::error!("Failed to dial peer {}: {}", peer, e);
                            } else {
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

