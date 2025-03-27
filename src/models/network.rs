use clap::Parser;
use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::rendezvous::Cookie;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{Multiaddr, PeerId, Swarm, identify, ping, rendezvous, gossipsub, kad::{self, store::MemoryStore}, mdns::{self, tokio::Behaviour as MdnsBehaviour}, autonat, relay};
use std::collections::HashSet;
use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use actix::Actor;
use actix_web::dev::Server;
use actix_web::{web, App, HttpServer};
use actix_cors::Cors;
use actix_web::middleware::Logger;
use tokio::sync::{Mutex, mpsc};
use tokio::time::interval;
use futures::future::FutureExt;
use serde_json::{json, Value};
use crate::{actors, Metrics, MAX_RETRIES};

pub const NAMESPACE: &str = "rendezvous";

#[derive(NetworkBehaviour)]
pub struct MyBehaviour {
    pub identify: identify::Behaviour,
    pub mdns: MdnsBehaviour,
    pub kademlia: kad::Behaviour<MemoryStore>,
    pub auto_nat: autonat::Behaviour,
    pub relay: relay::Behaviour,
    pub gossip_sub: gossipsub::Behaviour,
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
    metrics: Arc<Metrics>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut swarm = swarm.lock().await;

    if let Some(addr) = rendezvous_point_address.clone() {
        if let Err(e) = swarm.dial(addr.clone()) {
            metrics.failed_connections.fetch_add(1, Ordering::Relaxed);
            tracing::error!(address = %addr, error = %e, "Failed to dial rendezvous point");
            return Err(e.into());
        }
    }

    let mut discover_tick = tokio::time::interval(Duration::from_secs(5));
    //let mut cookie = None;
    //let mut retry_count = 0;

    loop {
        tokio::select! {
            event = async {
                swarm.select_next_some().await
            }.boxed().fuse() => match event {
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    metrics.successful_connections.fetch_add(1, Ordering::Relaxed);
                    tracing::info!(
                        peer_id = %peer_id,
                        "Connection established"
                    );

                    if Some(peer_id) == rendezvous_point.clone() {
                        tracing::info!(
                            rendezvous_point = %peer_id,
                            namespace = NAMESPACE,
                            "Connecting to rendezvous point and discovering nodes"
                        );
                    }
                }


                other => {
                    tracing::debug!(
                        event = ?other,
                        "Unhandled SwarmEvent in discover_peers"
                    );
                }
            },

/*            _ = discover_tick.tick(), if cookie.is_some() => {
                if let Some(rendezvous_point) = rendezvous_point.clone() {
                    metrics.discovery_retries.fetch_add(1, Ordering::Relaxed);
                    tracing::info!(
                        rendezvous_point = %rendezvous_point,
                        "Sent periodic discovery request"
                    );
                }
            }*/
        }
    }
}

pub async fn connect_to_websocket(ws_url: String, peer_id: PeerId) -> Result<(), Box<dyn std::error::Error>> {
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
            // Enhanced error handler for WebSocket connections
            .app_data(web::JsonConfig::default().error_handler(|err, _req| {
                actix_web::error::InternalError::from_response(
                    "",
                    actix_web::HttpResponse::BadRequest()
                        .content_type("application/json")
                        .body(serde_json::to_string(&json!({
                            "error": "Invalid JSON",
                            "details": err.to_string()
                        })).unwrap()),
                )
                    .into()
            }))
    })
        .workers(2)
        .bind("127.0.0.1:8007")
        .map_err(|e| {
            tracing::error!("Failed to bind WebSocket server: {}", e);
            e
        })?
        .run();

    Ok(server)
}
