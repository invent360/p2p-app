use std::fmt;
use std::error::Error as StdError;
use actix_web_actors::ws::ProtocolError;
#[derive(Debug)]
pub enum WsError {
    ClientError(awc::error::WsClientError),
    ProtocolError(ProtocolError),
}

impl fmt::Display for WsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WsError::ClientError(e) => write!(f, "WebSocket client error: {}", e),
            WsError::ProtocolError(e) => write!(f, "WebSocket protocol error: {}", e),
        }
    }
}

impl StdError for WsError {}

// Ensure the error type is Send + Sync
unsafe impl Send for WsError {}
unsafe impl Sync for WsError {}
