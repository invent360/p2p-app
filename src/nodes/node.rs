use futures::StreamExt;
use libp2p::{kad, noise, tcp, yamux, Multiaddr, PeerId, StreamProtocol, Swarm};
use libp2p::swarm::{ConnectionId, SwarmEvent};
use libp2p::identity;
use libp2p::mdns::{self, tokio::Behaviour as MdnsBehaviour};
use libp2p::gossipsub;
use libp2p::autonat;
use libp2p::relay;
use libp2p::identify;
use std::time::Duration;
use std::num::NonZeroUsize;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;

use crate::models::network::MyBehaviour;
use crate::nodes::connection::{ConnectionEvent, ConnectionManager};

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

struct Node {
    id: usize,
    connection_manager: ConnectionManager,
    swarm: Option<Swarm<MyBehaviour>>,
}

impl Node {
    pub fn new(id: usize) -> Node {
        Node {
            id,
            connection_manager: ConnectionManager::new(),
            swarm: None,
        }
    }

    pub fn new_swarm(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let swarm = libp2p::SwarmBuilder::with_new_identity()
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )?
            .with_quic()
            .with_dns()?
            .with_behaviour(|key| {
                let store = kad::store::MemoryStore::new(key.public().to_peer_id());
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

                let kademlia_behaviour = kad::Behaviour::with_config(key.public().to_peer_id(), store, cfg);

                let mdns_behaviour = MdnsBehaviour::new(
                    mdns::Config::default(),
                    key.public().to_peer_id(),
                )?;

                let message_id_fn = |message: &gossipsub::Message| {
                    let mut s = DefaultHasher::new();
                    message.data.hash(&mut s);
                    gossipsub::MessageId::from(s.finish().to_string())
                };

                let gossip_sub_config = gossipsub::ConfigBuilder::default()
                    .heartbeat_interval(Duration::from_secs(10))
                    .validation_mode(gossipsub::ValidationMode::Strict)
                    .message_id_fn(message_id_fn)
                    .build()
                    .map_err(|msg| io::Error::new(io::ErrorKind::Other, msg))?;

                let gossip_behaviour = gossipsub::Behaviour::new(
                    gossipsub::MessageAuthenticity::Signed(key.clone()),
                    gossip_sub_config,
                )?;

                let auto_nat_behaviour = autonat::Behaviour::new(
                    key.public().to_peer_id(),
                    autonat::Config {
                        retry_interval: Duration::from_secs(10),
                        refresh_interval: Duration::from_secs(30),
                        boot_delay: Duration::from_secs(5),
                        throttle_server_period: Duration::from_secs(30),
                        ..Default::default()
                    },
                );

                let relay_behaviour = relay::Behaviour::new(key.public().to_peer_id(), Default::default());

                Ok(MyBehaviour {
                    identify: identify::Behaviour::new(identify::Config::new(
                        "/ipfs/1.0.0".to_string(),
                        key.public(),
                    )),
                    kademlia: kademlia_behaviour,
                    mdns: mdns_behaviour,
                    gossip_sub: gossip_behaviour,
                    auto_nat: auto_nat_behaviour,
                    relay: relay_behaviour
                })
            })?
            .build();

        self.swarm = Some(swarm);
        Ok(())
    }

    pub fn swarm(&mut self) -> &mut Swarm<MyBehaviour> {
        self.swarm.as_mut().expect("Swarm not initialized")
    }

    pub async fn dial_node(
        &mut self,
        node_address: Multiaddr,
        node_peer_id: PeerId,
    ) -> Result<ConnectionId, Box<dyn std::error::Error + Send + Sync>> {
        // First get immutable references
        let connection_manager = &self.connection_manager;

        // Check for existing connection first
        if let Some(&conn_id) = connection_manager.get_connections(&node_peer_id).await.first() {
            println!("Reusing existing connection to {}", node_peer_id);
            return Ok(conn_id);
        }

        // Then get mutable access to swarm
        let swarm = self.swarm();
        swarm.behaviour_mut().kademlia.add_address(&node_peer_id, node_address.clone());

        match swarm.dial(node_address.clone()) {
            Ok(()) => {
                // Wait for connection to be established
                let connection_id = loop {
                    match swarm.next().await {
                        Some(SwarmEvent::ConnectionEstablished {
                                 peer_id,
                                 connection_id,
                                 ..
                             }) if peer_id == node_peer_id => {
                            // Now we need both mutable swarm and connection_manager
                            // So we scope the mutable swarm borrow
                            {
                                let connection_manager = &self.connection_manager;
                                connection_manager.add_connection(peer_id, connection_id).await;
                            }
                            break connection_id;
                        }
                        Some(SwarmEvent::OutgoingConnectionError {
                                 peer_id: Some(p),
                                 error,
                                 ..
                             }) if p == node_peer_id => {
                            return Err(error.into());
                        }
                        _ => continue,
                    }
                };
                Ok(connection_id)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn handle_connection_events(
        &mut self,
        mut events_rx: tokio::sync::mpsc::Receiver<ConnectionEvent>,
    ) {
        loop {
            // Get mutable swarm first
            let swarm = self.swarm();

            tokio::select! {
                event = swarm.select_next_some() => {
                    let connection_manager = &self.connection_manager;
                    match event {
                        SwarmEvent::ConnectionEstablished { peer_id, connection_id, .. } => {
                            connection_manager.add_connection(peer_id, connection_id).await;
                        }
                        SwarmEvent::ConnectionClosed { peer_id, connection_id, .. } => {
                            connection_manager.remove_connection(&peer_id, connection_id).await;
                        }
                        _ => {}
                    }
                }
                event = events_rx.recv() => {
                    let connection_manager = &self.connection_manager;
                    if let Some(event) = event {
                        match event {
                            ConnectionEvent::Established(peer_id, conn_id) => {
                                connection_manager.add_connection(peer_id, conn_id).await;
                            }
                            ConnectionEvent::Closed(peer_id, conn_id) => {
                                connection_manager.remove_connection(&peer_id, conn_id).await;
                            }
                        }
                    }
                }
            }
        }
    }
}
