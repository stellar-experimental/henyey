use async_trait::async_trait;

use crate::connection::{Connection, Listener};
use crate::Result;
use std::net::SocketAddr;

#[async_trait]
pub trait ConnectionFactory: Send + Sync {
    async fn connect(&self, addr: SocketAddr, timeout_secs: u64) -> Result<Connection>;

    async fn bind(&self, port: u16) -> Result<Listener>;

    /// Per-peer outbound message channel capacity.
    ///
    /// Controls the mpsc channel size between the overlay manager and each
    /// peer's send loop. When the channel is full, `broadcast()` and
    /// `try_send_to()` drop messages (logged + counted).
    ///
    /// Sized to hold a full max-size ledger of transaction messages plus
    /// SCP/advert/demand overhead. stellar-core's FlowControl allows up to
    /// `getLastMaxTxSetSizeOps()` queued TRANSACTION messages per peer
    /// (12,600 at the mission tx-set cap) and NEVER capacity-drops SCP
    /// messages (FlowControl.cpp:476-542). The previous 256 was ~50x below
    /// core's allowance: at ~2000 tx/s sustained (13.5k-tx ledgers) every
    /// node dropped thousands of broadcasts per minute ("Broadcast
    /// backpressure" warns), silently losing submitted txs — the 2026-07-04
    /// campaign-2 ceiling at 2000 tx/s. Memory is bounded lazily (tokio mpsc
    /// does not preallocate): worst case ~16k x ~400 B = ~6.5 MB per peer.
    fn outbound_channel_capacity(&self) -> usize {
        16384
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TcpConnectionFactory;

#[async_trait]
impl ConnectionFactory for TcpConnectionFactory {
    async fn connect(&self, addr: SocketAddr, timeout_secs: u64) -> Result<Connection> {
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
    fn test_tcp_outbound_channel_capacity_holds_max_ledger() {
        // Must hold a full max-size ledger of tx broadcasts (12,600 ops at
        // the mission cap) plus SCP/advert overhead — see method docs.
        assert!(TcpConnectionFactory.outbound_channel_capacity() >= 16384);
    }

    #[test]
    fn test_loopback_outbound_channel_capacity_is_2048() {
        assert_eq!(
            LoopbackConnectionFactory::default().outbound_channel_capacity(),
            2048
        );
    }
}
