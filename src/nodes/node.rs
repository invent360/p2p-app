use futures::StreamExt;
use libp2p::{kad, Multiaddr, PeerId, Swarm};
use libp2p::kad::store::MemoryStore;
use libp2p::swarm::{ConnectionId, SwarmEvent};
use crate::models::network::MyBehaviour;
use crate::nodes::connection::{ConnectionEvent, ConnectionManager};

struct Node {
    id: usize,
    connection_manager: ConnectionManager,
}

impl Node {
    pub fn new(id: usize) -> Node {
        Node { id, connection_manager: ConnectionManager::new() }
    }
}

impl Node {
    pub async fn dial_node(
        swarm: &mut Swarm<MyBehaviour>,  // Use your concrete type
        connection_manager: &ConnectionManager,
        node_address: Multiaddr,
        node_peer_id: PeerId,
    ) -> Result<ConnectionId, Box<dyn std::error::Error + Send + Sync>>
    {
        // Check for existing connection first
        if !connection_manager.get_connections(&node_peer_id).await.is_empty() {
            let conn_id = connection_manager.get_connections(&node_peer_id).await[0];
            println!("Reusing existing connection to {}", node_peer_id);
            return Ok(conn_id);
        }

        // No existing connection, dial new one
        println!("Establishing new connection to {}", node_peer_id);
        swarm.behaviour_mut().kademlia.add_address(&node_peer_id, node_address.clone());

        match swarm.dial(node_address.clone()) {
            Ok(()) => {
                // Wait for connection to be established
                let connection_id = loop {
                    match swarm.next().await {
                        Some(SwarmEvent::ConnectionEstablished {
                                 peer_id,
                                 connection_id,
                                 num_established: _,
                                 endpoint: _,
                                 concurrent_dial_errors: _,
                                 established_in: _,
                             })
                        if peer_id == node_peer_id => {
                            connection_manager.add_connection(peer_id, connection_id).await;
                            break connection_id;
                        }
                        Some(SwarmEvent::OutgoingConnectionError {
                                 peer_id: Some(p),
                                 connection_id: _,
                                 error
                             }) if p == node_peer_id => {
                            return Err(error.into());
                        }
                        _ => continue, // Ignore other events
                    }
                };
                Ok(connection_id)
            }
            Err(e) => Err(e.into()),
        }
    }


    pub async fn handle_connection_events<B>(
        mut swarm: Swarm<B>,
        connection_manager: ConnectionManager,
        mut events_rx: tokio::sync::mpsc::Receiver<ConnectionEvent>,
    ) where
        B: libp2p::swarm::NetworkBehaviour,
    {
        loop {
            tokio::select! {
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::ConnectionEstablished { peer_id, connection_id, .. } => {
                        connection_manager.add_connection(peer_id, connection_id).await;
                    }
                    SwarmEvent::ConnectionClosed { peer_id, connection_id, .. } => {
                        // Handle connection closed event
                    }
                    _ => {}
                }
            }
            event = events_rx.recv() => {
                if let Some(event) = event {
                    match event {
                        ConnectionEvent::Established(peer_id, conn_id) => {
                            connection_manager.add_connection(peer_id, conn_id).await;
                        }
                        ConnectionEvent::Closed(peer_id) => {
                            // Handle connection closed
                        }
                    }
                }
            }
        }
        }
    }
}
