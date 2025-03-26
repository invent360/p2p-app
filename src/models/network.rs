use clap::Parser;
use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::rendezvous::Cookie;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{Multiaddr, PeerId, Swarm, identify, ping, rendezvous, gossipsub, kad};
use std::collections::HashSet;
use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use actix::Actor;
use actix_web::dev::Server;
use actix_web::{web, App, HttpServer};
use actix_cors::Cors;
use actix_web::middleware::Logger;
use tokio::sync::{Mutex, mpsc};
use tokio::time::interval;
use futures::future::FutureExt;
use crate::actors;

pub const NAMESPACE: &str = "rendezvous";

#[derive(NetworkBehaviour)]
pub struct MyBehaviour {
    pub identify: identify::Behaviour,
    pub rendezvous_server: rendezvous::server::Behaviour,
    pub rendezvous_client: rendezvous::client::Behaviour,
    pub kademlia: kad::Behaviour<kad::store::MemoryStore>,
}

#[derive(Parser, Debug)]
#[clap(name = "p2p_rendezvous_client")]
pub struct Args {
    #[clap(long)]
    pub external_address: Option<Multiaddr>,

    #[clap(long)]
    pub rendezvous_point_address: Option<Multiaddr>,

    #[clap(long)]
    pub rendezvous_point: Option<PeerId>,

    #[clap(long)]
    pub bootstrap_peer: Option<PeerId>,
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
                   SwarmEvent::ConnectionEstablished { peer_id, .. } => {

                    tracing::info!("ConnectionEstablished: peer {}", peer_id);

                    if Some(peer_id) == rendezvous_point.clone() {

                        /*
                         * [1]:Registration at rdv server
                         */
                        tracing::info!(
                            "Connecting to rendezvous point {}, & discovering nodes in '{}' namespace ...",
                            peer_id,
                            NAMESPACE
                        );

                        if let Err(error) = swarm.behaviour_mut().rendezvous_client.register(
                            rendezvous::Namespace::from_static("rendezvous"),
                            peer_id,
                            None,
                        ) {
                            tracing::error!("Failed to register: {error}");
                        }

                        /*
                         * [2] Discovery
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

                            if peer == swarm.local_peer_id().clone() {
                                tracing::info!(%peer, "Discovered peer is myself, skipping dial");
                                continue;
                            }

                            if let Some(rdv_peer_id) = rendezvous_point {
                                if peer == rdv_peer_id {
                                    tracing::info!(%peer, "Discovered peer is rendezvous point, skipping dial");
                                    continue;
                                }
                            }

                            if let Err(e) = discovered_peers_sender.send(peer).await {
                                    tracing::error!("Failed to send discovered peer to main thread: {}", e);
                            }
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


pub async fn connect_to_websocket(ws_url: String, peer_id: PeerId) -> Result<(), Box<dyn std::error::Error>> {
/*    let client = Client::new(); // Use the awc client

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
        .map_err(|e| Box::new(WsError::ProtocolError(e)) as Box<dyn StdError + Send + Sync>)?;*/


    Ok(())
}

pub async fn start_websocket() -> Result<Server, Box<dyn std::error::Error + Send + Sync>> {
    let app_state = Arc::new(AtomicUsize::new(0));
    let room_actor = actors::room_actor::RoomActor::new(app_state.clone()).start();

    let server = HttpServer::new(move || {
        App::new()
            .wrap(
                Cors::default()
                    .allow_any_origin()
                    .allow_any_method()
                    .allow_any_header()
                    .max_age(3600),
            )
            .app_data(web::Data::from(app_state.clone()))
            .app_data(web::Data::new(room_actor.clone()))
            .route("/ws", web::get().to(actors::session_actor::WsChatSession::connect))
            .wrap(Logger::default())
    })
        .workers(2)
        .bind("127.0.0.1:8001")?
        .run();

    Ok(server)
}
