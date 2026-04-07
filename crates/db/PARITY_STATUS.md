# stellar-core Parity Status

**Crate**: `henyey-db`
**Upstream**: `stellar-core/src/database/`, plus SQL/persistence helpers from `src/main/PersistentState.*`, `src/ledger/LedgerHeaderUtils.*`, `src/transactions/TransactionSQL.*`, `src/herder/HerderPersistence*.*`, `src/overlay/PeerManager.*`, `src/overlay/BanManager*.*`, and the publish-queue subset of `src/history/HistoryManager*.*`
**Overall Parity**: 94%
**Last Updated**: 2026-04-07

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Connection pool and initialization | Full | r2d2/rusqlite covers open, pool, schema bootstrap |
| Schema migrations | Full | Independent SQLite schema versioning through v8 |
| Persistent state (`storestate`) | Full | Key-value reads, writes, deletes, and LCL helpers |
| Ledger header storage | Full | Store/load/hash/stream/delete operations implemented |
| Transaction history storage | Full | Tx rows, txsets, txresults, range queries, cleanup |
| SCP history and quorum sets | Partial | Missing node-to-qset lookup (`quoruminfo`) |
| SCP crash recovery state | Partial | Hash listing and tx-set cleanup remain incomplete |
| Peer persistence | Full | CRUD, random selection, and failure pruning implemented |
| Ban list persistence | Full | Ban, unban, contains, and list operations implemented |
| Publish queue persistence | Full | Queue, dequeue, HAS fetch, and LCL cleanup implemented |
| RPC retention tables | Full | Rust-only events, ledger-close-meta, bucket snapshots |
| Misc DB split and backend helpers | None | Intentional single-SQLite-file design |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `Database.h` / `Database.cpp` | `src/pool.rs`, `src/database/mod.rs`, `src/migrations.rs` | Connection management, initialization, schema upgrade flow |
| `DatabaseUtils.h` / `DatabaseUtils.cpp` | `src/queries/ledger.rs`, `src/queries/scp.rs`, `src/queries/history.rs`, `src/queries/ledger_close_meta.rs`, `src/queries/events.rs` | Old-entry deletion logic inlined per query module |
| `DatabaseTypeSpecificOperation.h` | — | Intentional omission: SQLite-only design needs no backend dispatch |
| `DatabaseConnectionString.h` / `DatabaseConnectionString.cpp` | — | Intentional omission: SQLite paths have no passwords |
| `PersistentState.h` / `PersistentState.cpp` | `src/queries/state.rs`, `src/queries/scp.rs`, `src/scp_persistence.rs` | Storestate access plus SCP slot and tx-set persistence |
| `LedgerHeaderUtils.h` / `LedgerHeaderUtils.cpp` | `src/queries/ledger.rs` | Ledger header storage, lookup, streaming, retention |
| `TransactionSQL.h` / `TransactionSQL.cpp` | `src/queries/history.rs` | Transaction-set and tx-result checkpoint streaming |
| `HerderPersistence.h` / `HerderPersistenceImpl.h` / `HerderPersistenceImpl.cpp` | `src/queries/scp.rs`, `src/scp_persistence.rs` | SCP envelopes, quorum sets, crash-recovery state |
| `PeerManager.h` / `PeerManager.cpp` | `src/queries/peers.rs`, `src/database/network.rs` | SQL-backed peer record persistence subset |
| `BanManager.h` / `BanManagerImpl.cpp` | `src/queries/ban.rs`, `src/database/network.rs` | SQL-backed ban-list persistence subset |
| `HistoryManagerImpl.cpp` (publish queue SQL subset) | `src/queries/publish_queue.rs`, `src/database/network.rs` | Persistent publish queue state |
| N/A | `src/queries/events.rs` | Rust-only contract event index |
| N/A | `src/queries/ledger_close_meta.rs` | Rust-only full `LedgerCloseMeta` storage |
| N/A | `src/queries/bucket_list.rs` | Rust-only bucket snapshot storage |
| N/A | `src/schema.rs`, `src/error.rs` | Rust-only schema constants and error surface |

## Component Mapping

### database core (`src/pool.rs`, `src/database/mod.rs`, `src/migrations.rs`)

Corresponds to: `Database.h`, `DatabaseUtils.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Database::Database()` | `Database::open()`, `Database::open_in_memory()` | Full |
| `Database::getSession()` | `Database::with_connection()` / `Database::connection()` | Full |
| `Database::getRawSession()` | `Database::connection()` | Full |
| `Database::getPool()` | `Database.pool` (r2d2 pool) | Full |
| `Database::canUsePool()` | Pool always available on file-backed DBs | Full |
| `Database::initialize()` | `Database::initialize()` | Full |
| `Database::getMainDBSchemaVersion()` | `migrations::get_schema_version()` | Full |
| `Database::upgradeToCurrentSchema()` | `migrations::run_migrations()` | Full |
| `decodeOpaqueXDR()` | `stellar_xdr::ReadXdr` | Full |
| `DatabaseUtils::deleteOldEntriesHelper()` | Inlined bounded-delete SQL helpers per query module | Full |

### schema and persistent state (`src/schema.rs`, `src/queries/state.rs`)

Corresponds to: `PersistentState.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PersistentState::maybeDropAndCreateNew()` | `CREATE_SCHEMA` + `initialize_schema()` | Full |
| `PersistentState::getState()` | `StateQueries::get_state()` | Full |
| `PersistentState::setMainState()` | `StateQueries::set_state()` | Full |
| `PersistentState::getFromDb()` | `StateQueries::get_state()` | Full |
| `PersistentState::updateDb()` | `StateQueries::set_state()` | Full |
| `PersistentState::getStoreStateName()` | `state_keys` constants plus prefixed key helpers | Full |

### ledger headers (`src/queries/ledger.rs`)

Corresponds to: `LedgerHeaderUtils.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `storeInDatabase()` | `LedgerQueries::store_ledger_header()` | Full |
| `decodeFromData()` | `LedgerQueries::load_ledger_header()` XDR decode | Full |
| `loadByHash()` | `LedgerQueries::load_ledger_header_by_hash()` | Full |
| `loadBySequence()` | `LedgerQueries::load_ledger_header()` | Full |
| `loadMaxLedgerSeq()` | `LedgerQueries::get_latest_ledger_seq()` | Full |
| `deleteOldEntries()` | `LedgerQueries::delete_old_ledger_headers()` | Full |
| `copyToStream()` | `LedgerQueries::copy_ledger_headers_to_stream()` | Full |

### transaction history (`src/queries/history.rs`)

Corresponds to: `TransactionSQL.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `populateCheckpointFilesFromDB()` (tx sets) | `HistoryQueries::copy_tx_history_to_streams()` | Full |
| `populateCheckpointFilesFromDB()` (tx results) | `HistoryQueries::copy_tx_history_to_streams()` | Full |
| `storeTransaction()` | `HistoryQueries::store_transaction()` | Full |
| `loadTransaction()` | `HistoryQueries::load_transaction()` | Full |
| `deleteOldEntries()` (txhistory, txsets, txresults) | `HistoryQueries::delete_old_tx_history()` | Full |

### SCP history (`src/queries/scp.rs`)

Corresponds to: `HerderPersistence.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `saveSCPHistory()` | `ScpQueries::store_scp_history()` + `store_scp_quorum_set()` | Full |
| `copySCPHistoryToStream()` | `ScpQueries::copy_scp_history_to_stream()` | Full |
| `getNodeQuorumSet()` | No equivalent | None |
| `getQuorumSet()` | `ScpQueries::load_scp_quorum_set()` | Full |
| `deleteOldEntries()` | `ScpQueries::delete_old_scp_entries()` | Full |

### SCP crash recovery (`src/queries/scp.rs`, `src/scp_persistence.rs`)

Corresponds to: `PersistentState.h` SCP-state methods

| stellar-core | Rust | Status |
|--------------|------|--------|
| `getSCPStateAllSlots()` | `ScpStatePersistenceQueries::load_all_scp_slot_states()` | Full |
| `getTxSetsForAllSlots()` | `ScpStatePersistenceQueries::load_all_tx_set_data()` | Full |
| `getTxSetHashesForAllSlots()` | Derived by loading full tx sets | Partial |
| `setSCPStateV1ForSlot()` | `save_scp_slot_state()` + `save_tx_set_data()` | Full |
| `hasTxSet()` | `ScpStatePersistenceQueries::has_tx_set_data()` | Full |
| `deleteTxSets()` | `delete_old_tx_set_data()` no-op | Partial |

### peer records (`src/queries/peers.rs`, `src/database/network.rs`)

Corresponds to: `PeerManager.h` SQL subset

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PeerManager::ensureExists()` | `PeerQueries::store_peer()` | Full |
| `PeerManager::load()` | `PeerQueries::load_peer()` | Full |
| `PeerManager::store()` | `PeerQueries::store_peer()` | Full |
| `PeerManager::loadRandomPeers()` | `load_random_peers()` and specialized variants | Full |
| `PeerManager::removePeersWithManyFailures()` | `remove_peers_with_failures()` | Full |
| `PeerManager::getPeersToSend()` | Random-peer query helpers | Full |
| `PeerManager::loadAllPeers()` | `load_peers(None)` | Full |
| `PeerManager::storePeers()` | Repeated `store_peer()` calls | Full |

### ban list (`src/queries/ban.rs`, `src/database/network.rs`)

Corresponds to: `BanManager.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `banNode()` | `BanQueries::ban_node()` | Full |
| `unbanNode()` | `BanQueries::unban_node()` | Full |
| `isBanned()` | `BanQueries::is_banned()` | Full |
| `getBans()` | `BanQueries::load_bans()` | Full |

### publish queue (`src/queries/publish_queue.rs`, `src/database/network.rs`)

Corresponds to: `HistoryManager` persistent publish-queue subset

| stellar-core | Rust | Status |
|--------------|------|--------|
| DB-backed queued checkpoint state | `enqueue_publish()`, `remove_publish()`, `load_publish_queue()` | Full |
| `restoreCheckpoint()` queue cleanup subset | `remove_above_lcl()` | Full |
| DB-backed HAS lookup | `load_publish_has()` | Full |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `Database::getPreparedStatement()`, `StatementContext`, `SessionWrapper` | rusqlite manages statements directly; no SOCI wrapper layer |
| `getInsertTimer()`, `getSelectTimer()`, `getDeleteTimer()`, `getUpdateTimer()`, `getUpsertTimer()` | SQL timing metrics are not implemented in this crate |
| `setCurrentTransactionReadOnly()` | PostgreSQL-specific behavior; crate is SQLite-only |
| `canUseMiscDB()`, `getMiscDBSchemaVersion()`, `getMiscSession()`, `getRawMiscSession()`, `getMiscPool()`, `getMiscDBName()` | Rust keeps one SQLite database file instead of splitting main/misc DBs |
| `getSimpleCollationClause()` | SQLite-only deployment does not need backend-specific collation injection |
| `doDatabaseTypeSpecificOperation()`, `DatabaseTypeSpecificOperation` | No backend dispatch layer in the SQLite-only design |
| `removePasswordFromConnectionString()` | SQLite paths have no password-bearing connection string |
| `PersistentState::createMisc()`, `PersistentState::setMiscState()` | No separate misc database |
| `shouldRebuildForOfferTable()`, `clearRebuildForOfferTable()`, `setRebuildForOfferTable()` | Offer-table rebuild flow is not modeled in Rust DB state |
| `dropTxMetaIfExists()` | No separate legacy txmeta table exists in the Rust schema |
| `LedgerHeaderUtils::getFlags()`, `LedgerHeaderUtils::isValid()` | Validation belongs in ledger logic, not the DB crate |
| `PeerManager::update()` (type/backoff logic) | Higher-level peer heuristics live in the overlay crate, not in DB |
| `PeerManager::countPeers()` | Not needed; peer loading handles limits directly |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `HerderPersistence::getNodeQuorumSet()` | Medium | Missing `quoruminfo`-style node-to-quorum-set lookup |
| `PersistentState::getTxSetHashesForAllSlots()` | Low | Requires loading full tx sets instead of a hash-only query |
| `PersistentState::deleteTxSets()` | Low | Current cleanup hook is a no-op |

## Architectural Differences

1. **Backend model**
   - **stellar-core**: SOCI over SQLite and PostgreSQL, with backend-specific helpers.
   - **Rust**: rusqlite plus r2d2, with SQLite as the only supported backend.
   - **Rationale**: The repository standardizes on SQLite, so the DB crate drops cross-backend abstraction.

2. **Database layout**
   - **stellar-core**: Modern SQLite deployments split data across main and misc databases for concurrency.
   - **Rust**: All tables live in one SQLite database.
   - **Rationale**: Simpler initialization and schema management; concurrency tradeoffs are accepted for now.

3. **XDR storage format**
   - **stellar-core**: Many historical XDR payloads are base64-encoded text.
   - **Rust**: XDR payloads are generally stored as raw BLOBs.
   - **Rationale**: BLOB storage avoids encoding overhead and matches rusqlite ergonomics.

4. **Schema versioning**
   - **stellar-core**: Main schema version 28 plus misc schema version 2.
   - **Rust**: Independent schema version 8 with fresh-schema assumptions.
   - **Rationale**: The Rust node does not preserve legacy upgrade history from older stellar-core releases.

5. **SCP recovery tables**
   - **stellar-core**: Uses dedicated misc tables, including quorum-info lookups and tx-set bookkeeping.
   - **Rust**: Stores SCP slot state and tx sets under prefixed `storestate` keys.
   - **Rationale**: Keeps crash-recovery persistence small, but leaves a few parity gaps.

6. **Rust-specific retention data**
   - **stellar-core**: Delegates most event and transaction-serving storage to external services.
   - **Rust**: Stores contract events, `LedgerCloseMeta`, and bucket snapshots directly in the DB crate.
   - **Rationale**: Henyey serves RPC-oriented features directly from node storage.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Database core | 10 `TEST_CASE` / 13 `SECTION` in `DatabaseTests.cpp` | 5 `#[test]` in `migrations.rs` | Rust covers schema/version flow but not misc-DB split or MVCC |
| Connection string helpers | 1 `TEST_CASE` / 20 `SECTION` in `DatabaseConnectionStringTest.cpp` | 0 | Intentional omission in SQLite-only design |
| State queries | Indirect upstream coverage via `PersistentState` tests | 3 `#[test]` in `state.rs` | Basic key-value coverage present |
| Ledger headers | Indirect upstream coverage via ledger/history tests | 8 `#[test]` in `ledger.rs` | Good CRUD, hash, and stream coverage |
| Transaction history | Indirect upstream coverage via history/transaction tests | 10 `#[test]` in `history.rs` | Strong coverage for txsets and txresults |
| SCP persistence | Indirect upstream coverage via herder tests | 8 `#[test]` in `scp.rs` + 2 in `scp_persistence.rs` | Missing direct coverage for remaining parity gaps |
| Peer management | 8 `TEST_CASE` / 38 `SECTION` in `PeerManagerTests.cpp` | 6 `#[test]` in `peers.rs` | SQL subset covered; higher-level peer heuristics live elsewhere |
| Ban list | Indirect upstream coverage via overlay tests | 0 | No dedicated `ban.rs` unit tests |
| Publish queue | Indirect upstream coverage via history tests | 2 `#[test]` in `publish_queue.rs` | Focused on overwrite/idempotency behavior |
| Rust-only retention tables | N/A | 4 `#[test]` in `events.rs`, 5 in `ledger_close_meta.rs`, 2 in `bucket_list.rs` | Good coverage for Rust-only tables |

### Test Gaps

- No Rust test covers the upstream SQLite MVCC scenario from `DatabaseTests.cpp`.
- No direct unit test covers the missing `getNodeQuorumSet()` parity gap.
- `ban.rs` has no dedicated unit tests (functionality exercised through integration tests).
- `delete_old_tx_set_data()` remains effectively untested because it is currently a no-op.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 51 |
| Gaps (None + Partial) | 3 |
| Intentional Omissions | 26 |
| **Parity** | **51 / (51 + 3) = 94%** |
