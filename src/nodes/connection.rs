use libp2p::{swarm::ConnectionId, PeerId, Multiaddr, Swarm};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;

#[derive(Debug)]
pub enum ConnectionEvent {
    Established(PeerId, ConnectionId),
    Closed(PeerId),
}

#[derive(Clone, Debug)]
pub struct ConnectionManager {
    connections: Arc<RwLock<HashMap<PeerId, Vec<ConnectionId>>>>,
}

impl ConnectionManager {
    pub fn new() -> Self {
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn get_connections(&self, peer_id: &PeerId) -> Vec<ConnectionId> {
        let connections = self.connections.read().await;
        connections.get(peer_id).cloned().unwrap_or_default()
    }

    pub async fn add_connection(&self, peer_id: PeerId, connection_id: ConnectionId) {
        let mut connections = self.connections.write().await;
        connections.entry(peer_id).or_default().push(connection_id);
    }

    pub async fn remove_connection(&self, peer_id: &PeerId, connection_id: ConnectionId) {
        let mut connections = self.connections.write().await;

        if let Some(conn_ids) = connections.get_mut(peer_id) {
            // Remove the specific connection ID
            conn_ids.retain(|&id| id != connection_id);

            // Clean up if no connections remain
            if conn_ids.is_empty() {
                connections.remove(peer_id);
            }
        }
    }

    pub async fn has_connection(&self, peer_id: &PeerId) -> bool {
        let connections = self.connections.read().await;
        connections.contains_key(peer_id)
    }
}
