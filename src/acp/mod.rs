//! Camada ACP: enquadramento, tipos JSON-RPC, WebSocket mínimo e o cliente
//! sobre o canal `/acp` do gateway do daemon.

pub mod client;
pub mod types;
pub mod ws;

pub use client::{AcpClient, AcpClientError};
pub use types::{Inbound, RpcError, SessionUpdate};
pub use ws::WsError;
