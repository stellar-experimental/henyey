# Pseudocode: crates/overlay/src/lib.rs

## Overview

"P2P networking for henyey."

Top-level crate module defining configuration types, peer identity types,
and the local node identity. Orchestrates all overlay sub-modules.

### Sub-modules

```
auth, ban_manager, codec, connection, error, flood,
flow_control, item_fetcher, manager, message_handlers,
metrics, peer, peer_manager, survey
```

---

### STRUCT OverlayConfig

```
CONST VERSION_STRING = "henyey 0.0.1"
CONST LEDGER_VERSION = 25
CONST OVERLAY_VERSION = 38
CONST OVERLAY_MIN_VERSION = 35
CONST DEFAULT_LISTENING_PORT = 11625

STRUCT OverlayConfig:
  max_inbound_peers: integer      default 64
  max_outbound_peers: integer     default 8
  target_outbound_peers: integer  default 8
  listen_port: integer            default 11625
  known_peers: list of PeerAddress
  preferred_peers: list of PeerAddress
  network_passphrase: string
  auth_timeout_secs: integer      default 30
  connect_timeout_secs: integer   default 10
  flood_ttl_secs: integer         default 300
  listen_enabled: boolean         default true
  is_validator: boolean           default true
    "When false (watcher mode), the overlay filters out non-essential
     flood messages (Transaction, FloodAdvert, FloodDemand, and Survey
     messages) before they enter the broadcast channel, reducing channel
     pressure by ~90% on mainnet."
  version_string: string
  peer_event_tx: optional channel sender
```

---

### OverlayConfig::testnet

```
function testnet() → OverlayConfig:
  → OverlayConfig {
      known_peers: [
        "core-testnet1.stellar.org:11625",
        "core-testnet2.stellar.org:11625",
        "core-testnet3.stellar.org:11625",
      ],
      network_passphrase: "Test SDF Network ; September 2015",
      listen_enabled: false,
      ..defaults
    }
```

---

### OverlayConfig::mainnet

```
function mainnet() → OverlayConfig:
  → OverlayConfig {
      known_peers: [
        "Tier 1 validators: SDF (3), LOBSTR (5), SatoshiPay (3),
         Blockdaemon (3), Franklin Templeton (3), PublicNode (3)"
      ],
      network_passphrase: "Public Global Stellar Network ; September 2015",
      listen_enabled: false,
      ..defaults
    }
```

---

### ENUM PeerEvent

```
ENUM PeerEvent:
  Connected(address, peer_type)
  Failed(address, peer_type)
```

---

### ENUM PeerType

```
ENUM PeerType:
  Inbound    "peer connected to us"
  Outbound   "we connected to the peer"
```

---

### STRUCT PeerAddress

```
STRUCT PeerAddress:
  host: string    "IP address or hostname"
  port: integer   "TCP port (standard: 11625)"
```

### PeerAddress::new

```
function new(host, port) → PeerAddress:
  → PeerAddress { host, port }
```

### to_socket_addr

```
function to_socket_addr(self) → string:
  → "{host}:{port}"
```

### is_private

"Private addresses should not be shared with other peers."

```
function is_private(self) → boolean:
  if host parses as IPv4:
    → true if 10.x.x.x
    → true if 172.16-31.x.x
    → true if 192.168.x.x
    → true if 127.x.x.x (loopback)
    → false
  if host parses as IPv6:
    → true if ::1 (loopback)
    → false
  if host is "localhost":
    → true
  → false
```

---

### STRUCT PeerId

"Unique identifier for a peer based on their Ed25519 public key."

```
STRUCT PeerId:
  public_key: Ed25519PublicKey (32 bytes)
```

### PeerId::from_bytes

```
function from_bytes(bytes) → PeerId:
  → PeerId wrapping Ed25519 key from bytes
```

### PeerId::from_strkey

```
function from_strkey(strkey) → PeerId:
  parse strkey as Stellar public key (G...)
  → PeerId from parsed bytes
```

### to_strkey

```
function to_strkey(self) → string:
  → Stellar strkey encoding of public key (G...)
```

### to_hex

```
function to_hex(self) → string:
  → hex encoding of 32-byte public key
```

---

### TRAIT MessageHandler

```
TRAIT MessageHandler:
  async function handle_message(peer_id, message)
```

---

### STRUCT LocalNode

"Local node identity and configuration for overlay authentication."

```
STRUCT LocalNode:
  secret_key: Ed25519SecretKey
  network_id: NetworkId
  version_string: string
  ledger_version: integer
  overlay_version: integer
  overlay_min_version: integer
  listening_port: integer
```

### LocalNode::new_testnet

```
function new_testnet(secret_key) → LocalNode:
  → LocalNode {
      secret_key,
      network_id: testnet network id,
      version_string: VERSION_STRING,
      ledger_version: LEDGER_VERSION,
      overlay_version: OVERLAY_VERSION,
      overlay_min_version: OVERLAY_MIN_VERSION,
      listening_port: DEFAULT_LISTENING_PORT,
    }
```

### LocalNode::new_mainnet

```
function new_mainnet(secret_key) → LocalNode:
  → same as new_testnet but with mainnet network id
```

### LocalNode::new

```
function new(secret_key, network_passphrase) → LocalNode:
  → same but network_id derived from passphrase
```

### public_key / xdr_public_key / peer_id

```
function public_key(self) → PublicKey:
  → secret_key.public_key()

function xdr_public_key(self) → XDR PublicKey:
  → convert public_key to XDR format

function peer_id(self) → PeerId:
  → PeerId from xdr_public_key
```

---

## Summary

| Metric        | Source | Pseudocode |
|---------------|--------|------------|
| Lines (logic) | 620    | 126        |
| Functions     | 16     | 16         |
