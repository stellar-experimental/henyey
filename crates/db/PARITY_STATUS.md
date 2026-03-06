# stellar-core Parity Status

**Crate**: `henyey-db`
**Upstream**: `stellar-core/src/database/`, plus SQL operations from `src/overlay/PeerManager.*`, `src/overlay/BanManager*.*`, `src/herder/HerderPersistence*.*`, `src/main/PersistentState.*`, `src/ledger/LedgerHeaderUtils.*`, `src/transactions/TransactionSQL.*`, `src/history/HistoryManager*.*`
**Overall Parity**: 94%
**Last Updated**: 2026-03-05

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Connection management | Full | r2d2 pool replaces SOCI sessions |
| Schema migrations | Full | Independent versioning scheme |
| State persistence (storestate) | Full | Key-value get/set/delete |
| Ledger header SQL | Full | Store, load by seq, load by hash, max seq, stream to XDR, delete old |
| SCP history persistence | Full | Envelopes, quorum sets, stream to XDR |
| SCP state crash recovery | Full | Slot state and tx set persistence |
| Peer management SQL | Full | All CRUD and random peer queries |
| Ban list SQL | Full | Ban/unban/check/list |
| Transaction history SQL | Full | Store and load tx records, sets, results, stream to XDR |
| Publish queue SQL | Full | Enqueue/dequeue/list |
| Bucket list snapshots | Full | Rust-specific checkpoint storage |
| Quorum info table | None | getNodeQuorumSet not implemented |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `Database.h` / `Database.cpp` | `pool.rs`, `lib.rs`, `migrations.rs` | Connection management, initialization, schema upgrades |
| `DatabaseUtils.h` / `DatabaseUtils.cpp` | `queries/ledger.rs`, `queries/scp.rs` | `deleteOldEntriesHelper` inlined into query modules |
| `PersistentState.h` / `PersistentState.cpp` | `queries/state.rs`, `queries/scp.rs`, `scp_persistence.rs` | Split across state and SCP modules |
| `LedgerHeaderUtils.h` / `LedgerHeaderUtils.cpp` | `queries/ledger.rs` | Ledger header SQL operations |
| `TransactionSQL.h` / `TransactionSQL.cpp` | `queries/history.rs` | Transaction history storage and streaming |
| `HerderPersistence.h` / `HerderPersistenceImpl.cpp` | `queries/scp.rs`, `scp_persistence.rs` | SCP history, quorum set storage, and streaming |
| `PeerManager.h` / `PeerManager.cpp` | `queries/peers.rs` | Peer table CRUD and random queries |
| `BanManager.h` / `BanManagerImpl.cpp` | `queries/ban.rs` | Ban list management |
| `HistoryManagerImpl.cpp` (SQL subset) | `queries/publish_queue.rs` | Publish queue table operations |
| `schema.rs` | N/A | Rust-specific schema definitions |
| `error.rs` | N/A | Rust-specific error types |

## Component Mapping

### pool (`pool.rs`)

Corresponds to: `Database.h` (connection management)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Database::Database()` (constructor) | `Database::open()`, `Database::open_in_memory()` | Full |
| `Database::getSession()` | `Database::connection()` | Full |
| `Database::getRawSession()` | `Database::connection()` (same) | Full |
| `Database::getPool()` | `Database.pool` (r2d2 pool) | Full |
| `Database::canUsePool()` | Always true (pool always available) | Full |

### lib (`lib.rs`)

Corresponds to: `Database.h` (initialization and high-level API)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Database::initialize()` | `Database::initialize()` | Full |
| `Database::upgradeToCurrentSchema()` | `Database::upgrade()` | Full |
| `Database::getDBSchemaVersion()` | `Database::schema_version()` | Full |
| `decodeOpaqueXDR()` | Handled by `stellar_xdr::ReadXdr` | Full |

### migrations (`migrations.rs`)

Corresponds to: `Database.h` (schema version management)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Database::applySchemaUpgrade()` | `run_migrations()` | Full |
| `Database::putSchemaVersion()` | `set_schema_version()` | Full |
| `Database::getDBSchemaVersion()` | `get_schema_version()` | Full |

### schema (`schema.rs`)

Corresponds to: Table creation in `PersistentState::dropAll()`, `LedgerHeaderUtils::dropAll()`, `HerderPersistence::dropAll()`, `PeerManager::dropAll()`, `BanManager::dropAll()`, `HistoryManager::dropAll()`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PersistentState::dropAll()` | `CREATE_SCHEMA` (storestate table) | Full |
| `LedgerHeaderUtils::dropAll()` | `CREATE_SCHEMA` (ledgerheaders table) | Full |
| `HerderPersistence::dropAll()` | `CREATE_SCHEMA` (scphistory, scpquorums tables) | Full |
| `PeerManager::dropAll()` | `CREATE_SCHEMA` (peers table) | Full |
| `BanManager::dropAll()` | `CREATE_SCHEMA` (ban table) | Full |
| `HistoryManager::dropAll()` | `CREATE_SCHEMA` (publishqueue table) | Full |

### state (`queries/state.rs`)

Corresponds to: `PersistentState.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PersistentState::getState()` | `StateQueries::get_state()` | Full |
| `PersistentState::setState()` | `StateQueries::set_state()` | Full |
| `PersistentState::getFromDb()` | `StateQueries::get_state()` | Full |
| `PersistentState::updateDb()` | `StateQueries::set_state()` | Full |
| N/A | `StateQueries::delete_state()` | Full (Rust addition) |
| N/A | `StateQueries::get_last_closed_ledger()` | Full (convenience) |
| N/A | `StateQueries::set_last_closed_ledger()` | Full (convenience) |

### ledger (`queries/ledger.rs`)

Corresponds to: `LedgerHeaderUtils.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `storeInDatabase()` | `LedgerQueries::store_ledger_header()` | Full |
| `decodeFromData()` | Handled in `load_ledger_header()` via XDR | Full |
| `loadBySequence()` | `LedgerQueries::load_ledger_header()` | Full |
| `loadByHash()` | `LedgerQueries::load_ledger_header_by_hash()` | Full |
| `loadMaxLedgerSeq()` | `LedgerQueries::get_latest_ledger_seq()` | Full |
| `deleteOldEntries()` | `LedgerQueries::delete_old_ledger_headers()` | Full |
| `copyToStream()` | `LedgerQueries::copy_ledger_headers_to_stream()` | Full |
| N/A | `LedgerQueries::get_ledger_hash()` | Full (Rust addition) |

### history (`queries/history.rs`)

Corresponds to: `TransactionSQL.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `populateCheckpointFilesFromDB()` | `HistoryQueries::copy_tx_history_to_streams()` | Full |
| `dropSupportTransactionFeeHistory()` | Not needed (deprecated table) | Full |
| `dropSupportTxSetHistory()` | Not needed (deprecated table) | Full |
| `dropSupportTxHistory()` | Handled by schema creation | Full |
| N/A | `HistoryQueries::store_transaction()` | Full (Rust addition) |
| N/A | `HistoryQueries::load_transaction()` | Full (Rust addition) |
| N/A | `HistoryQueries::store_tx_history_entry()` | Full (Rust addition) |
| N/A | `HistoryQueries::load_tx_history_entry()` | Full (Rust addition) |
| N/A | `HistoryQueries::store_tx_result_entry()` | Full (Rust addition) |
| N/A | `HistoryQueries::load_tx_result_entry()` | Full (Rust addition) |

### scp (`queries/scp.rs`)

Corresponds to: `HerderPersistence.h`, `HerderPersistenceImpl.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `saveSCPHistory()` | `ScpQueries::store_scp_history()` | Full |
| `copySCPHistoryToStream()` | `ScpQueries::copy_scp_history_to_stream()` | Full |
| `getNodeQuorumSet()` | Not implemented (quoruminfo table) | None |
| `getQuorumSet()` | `ScpQueries::load_scp_quorum_set()` | Full |
| `deleteOldEntries()` | `ScpQueries::delete_old_scp_entries()` | Full |
| N/A | `ScpQueries::store_scp_quorum_set()` | Full |
| N/A | `ScpQueries::load_scp_history()` | Full |

### scp_persistence (`scp_persistence.rs`, `queries/scp.rs`)

Corresponds to: `PersistentState.h` (SCP slot state methods)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `getSCPStateAllSlots()` | `ScpStatePersistenceQueries::load_all_scp_slot_states()` | Full |
| `setSCPStateForSlot()` | `ScpStatePersistenceQueries::save_scp_slot_state()` | Full |
| `setSCPStateV1ForSlot()` | `save_scp_slot_state()` + `save_tx_set_data()` | Full |
| `getTxSetsForAllSlots()` | `ScpStatePersistenceQueries::load_all_tx_set_data()` | Full |
| `getTxSetHashesForAllSlots()` | Achievable via `load_all_tx_set_data()` | Partial |
| `hasTxSet()` | `ScpStatePersistenceQueries::has_tx_set_data()` | Full |
| `deleteTxSets()` | `ScpStatePersistenceQueries::delete_old_tx_set_data()` | Partial |

### peers (`queries/peers.rs`)

Corresponds to: `PeerManager.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PeerManager::ensureExists()` | `PeerQueries::store_peer()` (INSERT OR REPLACE) | Full |
| `PeerManager::update()` (type) | `PeerQueries::store_peer()` | Full |
| `PeerManager::update()` (backoff) | `PeerQueries::store_peer()` | Full |
| `PeerManager::update()` (both) | `PeerQueries::store_peer()` | Full |
| `PeerManager::load()` | `PeerQueries::load_peer()` | Full |
| `PeerManager::store()` | `PeerQueries::store_peer()` | Full |
| `PeerManager::loadRandomPeers()` | `PeerQueries::load_random_peers()` and variants | Full |
| `PeerManager::removePeersWithManyFailures()` | `PeerQueries::remove_peers_with_failures()` | Full |
| `PeerManager::getPeersToSend()` | Via `load_random_peers` variants | Full |
| `PeerManager::loadAllPeers()` | `PeerQueries::load_peers(None)` | Full |
| `PeerManager::storePeers()` | Via iterated `store_peer()` | Full |

### ban (`queries/ban.rs`)

Corresponds to: `BanManager.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `BanManager::banNode()` | `BanQueries::ban_node()` | Full |
| `BanManager::unbanNode()` | `BanQueries::unban_node()` | Full |
| `BanManager::isBanned()` | `BanQueries::is_banned()` | Full |
| `BanManager::getBans()` | `BanQueries::load_bans()` | Full |

### publish_queue (`queries/publish_queue.rs`)

Corresponds to: `HistoryManager.h` (SQL publish queue subset)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `HistoryManager::dropAll()` | `CREATE_SCHEMA` (publishqueue table) | Full |
| N/A | `PublishQueueQueries::enqueue_publish()` | Full |
| N/A | `PublishQueueQueries::remove_publish()` | Full |
| N/A | `PublishQueueQueries::load_publish_queue()` | Full |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `getPreparedStatement()` / `clearPreparedStatementCache()` | rusqlite manages prepared statement lifecycle internally |
| `StatementContext` / `SessionWrapper` | SOCI-specific abstractions not needed with rusqlite |
| `getInsertTimer()` / `getSelectTimer()` / `getDeleteTimer()` / `getUpdateTimer()` / `getUpsertTimer()` | Metrics deferred to higher-level layers |
| `isSqlite()` | Always true; SQLite is the only supported backend |
| `getSimpleCollationClause()` | SQLite-only; collation clause not needed |
| `doDatabaseTypeSpecificOperation()` / `DatabaseTypeSpecificOperation` | SQLite-only; no backend dispatch needed |
| `removePasswordFromConnectionString()` | SQLite uses file paths, no passwords to remove |
| `setCurrentTransactionReadOnly()` | PostgreSQL-only feature |
| `dropTxMetaIfExists()` | No tx meta table in Rust schema |
| `shouldRebuildForOfferTable()` / `clearRebuildForOfferTable()` / `setRebuildForOfferTable()` | No offer table rebuild in Rust; handled differently |
| `migrateToSlotStateTable()` | Different schema evolution; Rust uses unified schema from start |
| `dropSupportTransactionFeeHistory()` / `dropSupportTxSetHistory()` | Deprecated tables never created in Rust |
| `LedgerHeaderUtils::getFlags()` / `LedgerHeaderUtils::isValid()` | Validation logic, not database operations; belongs in ledger crate |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `HerderPersistence::getNodeQuorumSet()` | Medium | quoruminfo table lookup (node -> qset hash) |
| `PersistentState::getTxSetHashesForAllSlots()` | Low | Distinct hash-only query; achievable via existing API |
| `PersistentState::deleteTxSets()` | Low | Currently a no-op; tx set cleanup not linked to slots |

## Architectural Differences

1. **Database Library**
   - **stellar-core**: SOCI with SQLite3 and optional PostgreSQL backends
   - **Rust**: rusqlite with r2d2 connection pooling
   - **Rationale**: Simplified to SQLite-only per project requirements; r2d2 provides thread-safe pooling

2. **XDR Storage Format**
   - **stellar-core**: Stores XDR as base64-encoded TEXT columns
   - **Rust**: Stores XDR as raw BLOB columns
   - **Rationale**: BLOBs are more efficient (no encoding overhead) and simpler with rusqlite's native blob support

3. **Schema Versioning**
   - **stellar-core**: MIN_SCHEMA_VERSION = 21, SCHEMA_VERSION = 25; migrations tied to upstream releases
   - **Rust**: CURRENT_VERSION = 5; independent versioning starting from fresh schema
   - **Rationale**: No need to support legacy upstream schema versions; Rust schema evolves independently

4. **SQLite Configuration**
   - **stellar-core**: `cache_size = -20000` (20MB), `mmap_size = 104857600` (100MB), `busy_timeout = 10000`
   - **Rust**: `cache_size = -64000` (64MB), no mmap, `busy_timeout = 30000`, `foreign_keys = ON`, `temp_store = MEMORY`
   - **Rationale**: Tuned for single-backend operation; larger cache compensates for no mmap; longer busy timeout for pool contention

5. **Query Module Organization**
   - **stellar-core**: SQL operations scattered across domain crates (ledger, herder, overlay, etc.)
   - **Rust**: All SQL operations consolidated in the `db` crate via domain-specific query traits
   - **Rationale**: Single point of database access simplifies connection management and schema evolution

6. **SCP State Persistence**
   - **stellar-core**: Uses separate `slotstate` table (migrated from `storestate` in schema v24)
   - **Rust**: Uses `storestate` table with prefixed keys (`scpstate:`, `txset:`)
   - **Rationale**: Simpler implementation; prefix-based key namespacing avoids need for separate table

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Database core | 5 TEST_CASE / 4 SECTION | 5 `#[test]` in `migrations.rs` | Schema and version tests covered |
| Connection string | 1 TEST_CASE / 18 SECTION | 0 | Intentionally omitted (SQLite only) |
| Ledger headers | Tested in LedgerManager tests | 8 `#[test]` in `ledger.rs` | Store, load, max seq, hash, stream, delete |
| SCP persistence | Tested in Herder tests | 8 `#[test]` in `scp.rs` | Envelopes, quorum sets, streaming, cleanup |
| SCP state recovery | Tested in Herder tests | 2 `#[test]` in `scp_persistence.rs` | Slot state and tx set persistence |
| Peer management | Tested in OverlayManager tests | 6 `#[test]` in `peers.rs` | CRUD, random queries, cleanup |
| Ban list | Tested in BanManager tests | 0 (inline in `ban.rs`) | Functions tested via Database API |
| Transaction history | Tested in TransactionSQL tests | 10 `#[test]` in `history.rs` | Store/load transactions, sets, results, streaming |
| Bucket list | N/A (Rust-specific) | 2 `#[test]` in `bucket_list.rs` | Rust-only checkpoint storage |
| State queries | Tested in PersistentState tests | 3 `#[test]` in `state.rs` | Get, set, delete, LCL |
| Publish queue | Tested in HistoryManager tests | 0 (inline in `publish_queue.rs`) | Functions tested via Database API |

### Test Gaps

- **MVCC / concurrent access**: stellar-core has an explicit `sqlite MVCC test` (`checkMVCCIsolation`). No equivalent Rust test exists.
- **Ban list unit tests**: The `ban.rs` module has no `#[cfg(test)]` block; bans are only tested through the `Database` API.
- **Publish queue unit tests**: The `publish_queue.rs` module has no `#[cfg(test)]` block.
- **Integration tests**: No integration tests exist in `crates/db/tests/`. All tests are unit tests in source modules.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 49 |
| Gaps (None + Partial) | 3 |
| Intentional Omissions | 18 |
| **Parity** | **49 / (49 + 3) = 94%** |
