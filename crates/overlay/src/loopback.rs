use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::duplex;
use tokio::sync::{mpsc, Mutex};

use crate::connection::{Connection, ConnectionDirection, Listener};
use crate::{ConnectionFactory, OverlayError, PeerAddress, Result};

#[derive(Debug, Default)]
struct LoopbackRegistry {
    listeners: HashMap<u16, mpsc::Sender<Connection>>,
}

#[derive(Debug, Default, Clone)]
pub struct LoopbackConnectionFactory {
    registry: Arc<Mutex<LoopbackRegistry>>,
}

impl LoopbackConnectionFactory {
    fn socket_addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }
}

#[async_trait]
impl ConnectionFactory for LoopbackConnectionFactory {
    async fn connect(&self, addr: &PeerAddress, _timeout_secs: u64) -> Result<Connection> {
        let port = addr.port;
        let sender = {
            let registry = self.registry.lock().await;
            registry.listeners.get(&port).cloned()
        }
        .ok_or_else(|| OverlayError::ConnectionFailed(format!("loopback listener not found: {}", addr)))?;

        let (client, server) = duplex(1024 * 1024);
        let outbound = Connection::from_io(
            client,
            Self::socket_addr(port),
            ConnectionDirection::Outbound,
        )?;
        let inbound = Connection::from_io(
            server,
            Self::socket_addr(0),
            ConnectionDirection::Inbound,
        )?;

        sender
            .send(inbound)
            .await
            .map_err(|_| OverlayError::ConnectionFailed(format!("loopback listener dropped: {}", addr)))?;

        Ok(outbound)
    }

    async fn bind(&self, port: u16) -> Result<Listener> {
        let mut registry = self.registry.lock().await;
        let (tx, rx) = mpsc::channel(128);
        registry.listeners.insert(port, tx);
        Ok(Listener::from_loopback(Self::socket_addr(port), rx))
    }
}
