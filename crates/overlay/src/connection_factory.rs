use async_trait::async_trait;

use crate::connection::{Connection, Listener};
use crate::{PeerAddress, Result};

#[async_trait]
pub trait ConnectionFactory: Send + Sync {
    async fn connect(&self, addr: &PeerAddress, timeout_secs: u64) -> Result<Connection>;

    async fn bind(&self, port: u16) -> Result<Listener>;

    /// Per-peer outbound message channel capacity.
    ///
    /// Controls the mpsc channel size between the overlay manager and each
    /// peer's send loop. When the channel is full, `broadcast()` and
    /// `try_send_to()` drop messages (logged + counted). OverLoopback
    /// overrides this to a larger value because app-backed simulation nodes
    /// drain the channel more slowly than production TCP peers.
    fn outbound_channel_capacity(&self) -> usize {
        256
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TcpConnectionFactory;

#[async_trait]
impl ConnectionFactory for TcpConnectionFactory {
    async fn connect(&self, addr: &PeerAddress, timeout_secs: u64) -> Result<Connection> {
        Connection::connect(addr, timeout_secs).await
    }

    async fn bind(&self, port: u16) -> Result<Listener> {
        Listener::bind(port).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loopback::LoopbackConnectionFactory;

    #[test]
    fn test_tcp_outbound_channel_capacity_is_256() {
        assert_eq!(TcpConnectionFactory.outbound_channel_capacity(), 256);
    }

    #[test]
    fn test_loopback_outbound_channel_capacity_is_2048() {
        assert_eq!(
            LoopbackConnectionFactory::default().outbound_channel_capacity(),
            2048
        );
    }
}
