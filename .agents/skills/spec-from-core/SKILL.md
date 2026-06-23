---
name: spec-from-core
description: Generate or update Stellar Protocol Spec(s) from stellar-core C++ source
argument-hint: "[<SPEC>] [--apply] [--regenerate]"
---

Parse `$ARGUMENTS`:

- If any token matches one of `{README, SCP_SPEC, OVERLAY_SPEC,
  HERDER_SPEC, LEDGER_SPEC, TX_SPEC, BUCKETLISTDB_SPEC, CATCHUP_SPEC}`
  (case-insensitive, with or without a trailing `.md`), set `$SCOPE`
  to the matched spec. Otherwise `$SCOPE = all`.
- If `--apply` is present, `$MODE = apply`. Otherwise `$MODE = review`.
- If `--regenerate` is present, `$STRATEGY = regenerate`. Otherwise
  `$STRATEGY = update`.

# Stellar Protocol Spec from stellar-core

Generate or update one or more documents under `stellar-specs/` from
the stellar-core C++ reference implementation pinned in the
`stellar-core/` submodule. The specs are
**implementation-agnostic**: they describe observable protocol
behavior that any conforming implementation MUST reproduce. They are
the single source of truth for parity; pseudocode trees are retired.

## Spec ↔ stellar-core Mapping

| Spec | stellar-core source dirs |
|------|--------------------------|
| `README.md` | cross-cutting — indexes all 7 subsystem specs |
| `SCP_SPEC.md` | `src/scp/` |
| `OVERLAY_SPEC.md` | `src/overlay/` |
| `HERDER_SPEC.md` | `src/herder/` |
| `LEDGER_SPEC.md` | `src/ledger/`, `src/main/` (ledger close path) |
| `TX_SPEC.md` | `src/transactions/`, `src/main/` (apply path) |
| `BUCKETLISTDB_SPEC.md` | `src/bucket/` |
| `CATCHUP_SPEC.md` | `src/catchup/`, `src/history/` |

All paths are relative to `stellar-core/`. Implementation-only modules
(`util/`, `crypto/`, `database/`, `process/`, `work/`, `simulation/`,
`invariant/`) are deliberately unmapped — they contain no observable
protocol behavior. If a sweep surfaces consensus-deterministic logic
from one of these modules, surface it for human review rather than
silently folding it in.

## Strategy

### `$STRATEGY = update` (default)

Read the existing spec and propose targeted additions only. This is
the safe default — curated content is preserved.

1. Read the target spec file end-to-end. Build a section-level map.
2. Sweep the mapped stellar-core source files for
   **determinism-relevant content**:
   - **Guard / check sequences**: early returns with error codes.
     ORDER MATTERS — list every check in exact source order.
   - **State mutations**: writes to ledger entries, accounts,
     trustlines, offers, balances, sponsorship counts, fee pool,
     total coins.
   - **`releaseAssert` / `assert` invariants**: runtime correctness
     checks that crash on violation.
   - **Protocol-version branches**: `protocolVersionStartsFrom`,
     `protocolVersionIsBefore`, and similar conditional logic.
   - **Source comments** that convey invariants, ordering
     constraints, spec references (e.g., "see CAP-NNNN"), warnings,
     or domain knowledge.
3. Classify each finding against the spec:
   - Already covered at equal-or-greater detail → drop.
   - In spec but weaker (e.g., spec lists a check without its
     position in the order, or names a version branch without the
     threshold) → strengthen in place.
   - Absent and determinism-relevant → propose addition to the
     most-natural existing section.
   - Absent but implementation-internal (memory management, error
     wrapping, threading, SQL schemas, file system layouts, logging,
     metrics, caching strategies) → drop per "observable behavior
     only".
4. Preserve the canonical section order. Match the spec's existing
   prose+pseudocode style; do not paste raw C++.
5. Bump the spec's `**Date:**` to today.

### `$STRATEGY = regenerate` (overwrite)

Produce the spec fresh from the mapped source dirs, replacing any
existing content. Follows the document structure and writing style
below.

**Safety**: in `$STRATEGY = regenerate`, the skill MUST require
`$MODE = apply`. Review-mode regenerate is meaningless (it just
produces a draft that diverges from the live spec); without
`--apply`, refuse and tell the user to add it if they really want a
full overwrite.

## Mode

### `$MODE = review` (default)

Report proposed additions/changes in the conversation. Do not write
any files. Format:

```
## Spec Update: <SPEC>

**Strategy:** update | regenerate

### Proposed additions
- §N.M: <one-line summary>

### Proposed strengthenings
- §N.M: <one-line summary of weaker → stronger change>

### Intentionally skipped (implementation-internal)
- <one-line summary, grouped>

### Structural concerns
- (if any — e.g., a finding has no natural section to land in)
```

### `$MODE = apply`

Write changes to disk:

- `update`: edit the spec file in place. Apply each addition and
  strengthening; bump `**Date:**`.
- `regenerate`: overwrite the spec file with freshly derived content
  per the canonical document structure below.

## Scope

### `$SCOPE = <single spec>`

Process only that spec. The mapped source dirs in the table above
are the only stellar-core inputs.

### `$SCOPE = all` (no argument)

- `update`: process all 8 specs. Specs cover independent files, so
  parallel passes are safe. Recommend launching one subagent per
  spec for speed.
- `regenerate`: process the 7 per-subsystem specs first, then the
  README (which indexes them). Requires `--apply`, same as
  single-spec regenerate.

═══════════════════════════════════════════════════════════════════
EXTRACTION PRINCIPLES (used in both strategies)
═══════════════════════════════════════════════════════════════════

1. **OBSERVABLE BEHAVIOR ONLY**. Extract the protocol's observable
   behavior — message formats, state transitions, validation rules,
   ordering guarantees, resource limits, error codes, and
   deterministic outputs. Strip away all implementation internals:
   threading models, SQL schemas, caching strategies, file system
   layouts, metrics, logging, memory management.

2. **IMPLEMENTATION AGNOSTIC**. Never use C++ syntax, class names
   with `::` notation, `#include` directives, or language-specific
   constructs. Use camelCase pseudocode for algorithms. Reference
   XDR types by name (e.g., `StellarValue`, `TransactionEnvelope`)
   without reproducing their definitions.

3. **DETERMINISM IS THE LITMUS TEST**. If a behavior affects
   consensus or ledger state determinism, it MUST be specified. If
   it doesn't, it SHOULD be omitted. Exception: include
   implementation-internal details only when they affect observable
   correctness (e.g., the single-child LedgerTxn invariant).

4. **PROTOCOL VERSION AWARENESS**. The primary target is whatever
   protocol version the `stellar-core/` submodule is pinned to. Use
   `@version(≥N)` and `@version(<N)` annotations to document
   behavioral differences across versions. Capture every version
   threshold the source uses, not just recent ones. Support range
   is protocol 24+.

═══════════════════════════════════════════════════════════════════
DOCUMENT STRUCTURE
═══════════════════════════════════════════════════════════════════

Every spec file MUST follow this structure.

**TITLE** (subsystem specs): use the form
`# Stellar <Subsystem> [<Modifier>] Specification`. Examples:
"Stellar Consensus Protocol (SCP) Specification", "Stellar Ledger
Close Pipeline Specification", "Stellar Catchup, Replay, and History
Publishing Specification". The `<Modifier>` MAY name the scope when
the subsystem has multiple aspects.

**HEADER:**

```
# <Full Descriptive Title>

**Version:** <N> (<qualifier>)
**Status:** Informational
**Date:** YYYY-MM-DD
```

The `<qualifier>` is `stellar-core v<N.x> / Protocol <N>` for specs
coupled to ledger versioning. Specs with their own version axis MAY
use a domain-specific qualifier (e.g., OVERLAY uses
`Overlay Protocol v38–v39`).

**TABLE OF CONTENTS**: manually numbered, with Markdown anchor
links. Use plain integers (1, 2, 3...), not hierarchical numbering
(1.1, 1.2) at the TOC level.

**SECTIONS** (in this canonical order):

```
## 1. Introduction
  ### 1.1 Purpose and Scope (REQUIRED)
    - State what the spec covers and what is out of scope
    - MUST include the boilerplate sentence (substituting the
      subsystem's observable outputs as appropriate):
      "This specification is **implementation agnostic**. It is
      derived exclusively from the vetted stellar-core C++
      implementation (v<N.x>). Any conforming implementation that
      produces identical <observable outputs for this subsystem>
      for all valid inputs is considered correct."
    - Explicit out-of-scope list
  ### 1.2 Conventions and Terminology (REQUIRED)
    - RFC 2119 boilerplate
    - Glossary table: Term | Definition
    - MAY include the Relationship to Other Specifications table
      here (see §1.x below)
  ### 1.3 Notation (RECOMMENDED)
    - Explain pseudocode conventions, version annotations, XDR
      references — include only if the spec uses notation beyond
      what the global Pseudocode Notation section already covers
  ### 1.x Relationship to Other Specifications (RECOMMENDED)
    - Table: | Specification | Relationship |
    - May live under §1.2 or as a separate §1.3 / §1.4; numbering
      is flexible

## 2. Architecture/Protocol Overview
  - High-level design, component relationships, design goals
  - Include a Mermaid diagram showing component architecture

## 3. Data Types (RECOMMENDED — title flexible)
  - Title MAY be "Data Types", "Data Types and Encoding", or
    "Data Encoding"
  - OPTIONAL where the subsystem has no distinct XDR types of its
    own (e.g., HERDER, whose types are inherited from TX/LEDGER —
    in that case document type usage inline with the algorithms)
  - XDR type references (by name, not full definitions)
  - Field-level tables: Field | Type | Description
  - Sort order, encoding conventions

## 4–N. Core Specification Sections (domain-specific)
  - State machines: named states, transition tables, forbidden
    transitions
  - Algorithms: numbered step sequences with pseudocode
  - Validation rules: enumerated checks with XDR result codes
  - Error handling: result codes mapped to SCREAMING_SNAKE_CASE
    XDR enums
  - Lifecycle sequences: step-by-step multi-phase processes
  - Network-facing specs MAY include a "Security Considerations"
    section in lieu of (or in addition to) Invariants

## N-3. Invariants and Safety Properties (RECOMMENDED)
  - OPTIONAL where the subsystem has no consensus-deterministic
    invariants of its own (e.g., HERDER's invariants live in
    SCP+LEDGER+TX; OVERLAY's safety is covered under "Security
    Considerations")
  - Each invariant SHOULD have a stable identifier of the form
    `INV-<X><N>` where `<X>` is the 1-2 letter spec sigil
    (`S` SCP, `O` OVERLAY, `H` HERDER, `L` LEDGER, `T` TX,
    `B` BUCKETLISTDB, `C` CATCHUP). Example: `INV-L8`. Stable IDs
    let code reference invariants directly
    (`// Invariant: INV-L8`).
  - Formal statements using MUST/SHALL keywords
  - Each invariant: what it guarantees, why it matters, what breaks
    if violated

## N-2. Constants (RECOMMENDED)
  - SHOULD split into "Protocol Constants" (MUST NOT change) and
    "Recommended Parameters" (RECOMMENDED defaults) where both
    coexist
  - MAY use a single flat table where only consensus-fixed values
    are present
  - Large constant sets MAY use categorized subsections (e.g.,
    OVERLAY's Wire / Timing / Capacity / Flooding split)
  - Table format: Constant | Value | Description | Section
    - The `Section` column SHOULD link back to the body section
      that defines the constant's role (e.g.,
      `[8.3](#83-initial-capacity-grant)`), making Constants act
      as an index back into the body

## N-1. References (REQUIRED — numbered top-level section)
  - Numbered top-level `## N. References` placed AFTER Constants
    and BEFORE Appendices
  - Table form: `Reference | Description`
  - Footnote-style link definitions (`[name]: <url>`) MAY be used
    inline in the body; place all such definitions at the very end
    of the file, AFTER the Appendices

## N. Appendices
  - Named alphabetically: Appendix A, Appendix B, ...
  - Content types: sequence diagrams, flowcharts, state machine
    diagrams, decision matrices, worked examples, detailed
    procedures
  - KEY RULE: Appendices illustrate; the body specifies. An
    appendix MUST NOT introduce new normative requirements.
```

**SEPARATORS**: a `---` horizontal rule between every top-level `##`
section.

**SUBSECTION NUMBERING**: `### 3.1 Title`, `### 3.2 Title`. Up to
4 levels (`#### X.Y.Z`) MAY be used when natural subdivision exists
(e.g., handshake substeps, per-step descriptions). 5+ levels SHOULD
be avoided; use bold inline headings or numbered lists instead.

═══════════════════════════════════════════════════════════════════
WRITING STYLE
═══════════════════════════════════════════════════════════════════

- Third-person impersonal: "The herder validates...", "A node
  MUST...".
- Present tense: "The ledger close pipeline executes...".
- No first or second person ("we", "you").
- Formal register: precise and clear, not obtusely academic.
- RFC 2119 keywords used extensively:
    MUST/MUST NOT — absolute protocol requirements
    SHALL/SHALL NOT — equivalent to MUST, especially for invariants
    SHOULD/SHOULD NOT — recommendations and best practices
    MAY/OPTIONAL — permitted but not required behavior
    RECOMMENDED — suggested defaults
- Sentences are clear and moderately short.
- No emojis, no informal language.

═══════════════════════════════════════════════════════════════════
DIAGRAMS
═══════════════════════════════════════════════════════════════════

All visual diagrams MUST use Mermaid syntax (\`\`\`mermaid code
blocks). Use `<br/>` for line breaks inside node labels (NOT `\n`).

Diagram types:
- `stateDiagram-v2` — state machines, lifecycle transitions
- `graph TD` / `LR` — architecture, component trees, data flow
- `sequenceDiagram` — message exchanges between participants
- `block-beta` — protocol stacks / layered architectures
- `flowchart TD` / `LR` — decision trees, processing pipelines

Placement:
- Architecture diagrams in Section 2.
- State machine diagrams inline with their definitions.
- Complex flow/sequence diagrams in appendices.

═══════════════════════════════════════════════════════════════════
CROSS-REFERENCING
═══════════════════════════════════════════════════════════════════

- Reference other specs: plain-text `SPEC_NAME §N.N` notation
  (e.g., `HERDER_SPEC §12`, `TX_SPEC §3`). Do not wrap in a markdown
  link — section numbering is the stable anchor.
- Each spec SHOULD include a "Relationship to Other Specifications"
  table in §1 listing companion specs and the integration points.
  Columns: `Specification | Relationship`.
- External references use footnote-style Markdown link definitions
  (`[name]: <url>`) placed at the very end of the file, AFTER the
  Appendices.

═══════════════════════════════════════════════════════════════════
PSEUDOCODE NOTATION
═══════════════════════════════════════════════════════════════════

For algorithms embedded in spec sections:

- **Variables**: `camelCase` (e.g., `closeTime`, `txSetHash`).
- **Functions**: `functionName()` (e.g., `getNodeWeight()`).
- **XDR result codes**: `SCREAMING_SNAKE_CASE` (e.g.,
  `MANAGE_SELL_OFFER_MALFORMED`).
- **Protocol-version guards**: `@version(≥N)`, `@version(<N)`,
  `@version(=N)` annotations on the conditional logic.
- **Control flow**: standard `if`, `else`, `for each`, `while`,
  `return`, indentation. Language-agnostic.

Use prose with embedded pseudocode (the established style across
the suite). Do not invent a custom DSL — there is no GUARD / MUTATE
/ ASSERT / CONST keyword set. Where ordering of checks matters,
either number them or describe the order explicitly in prose.

═══════════════════════════════════════════════════════════════════
SPECIFIC CONTENT TO EXTRACT PER SPEC
═══════════════════════════════════════════════════════════════════

**README.md:**
  - 7-subsystem architecture diagram (Mermaid `graph TD`).
  - Specifications table (Document | Subsystem | Description).
  - End-to-end data flow trace (~9 steps: submission → next round)
    with cross-references to all specs.
  - Shared conventions: XDR encoding, cryptographic primitives
    table, network ID, hash chaining, determinism requirement.
  - Scope boundaries: In Scope / Out of Scope table.

**SCP_SPEC.md** (from `src/scp/`):
  - Two-phase consensus (nomination + ballot protocol).
  - Federated Byzantine Agreement model.
  - Quorum set semantics, validation, normalization.
  - Quorum slice test, V-blocking set test, transitive quorum test.
  - Federated accept and ratify primitives.
  - Driver interface (pure virtual methods, validation levels).
  - Slot model (lifecycle, state flags, envelope routing).
  - Nomination protocol (state variables, round leaders, flow,
    emission).
  - Ballot protocol (PREPARE / CONFIRM / EXTERNALIZE, all 5 steps
    of advanceSlot).
  - Message processing and statement ordering.
  - Timer model (nomination timer, ballot timer, heard-from-quorum).
  - Invariants: ballot state, nomination, phase transitions, value
    locking, commit voiding, EXTERNALIZE finality.

**OVERLAY_SPEC.md** (from `src/overlay/`):
  - Network architecture (peer-to-peer gossip).
  - Protocol stack (TCP, record marking, XDR, authenticated
    messages).
  - Connection lifecycle: discovery, establishment, handshake.
  - Authentication: HELLO/AUTH exchange, AuthCert, Curve25519 key
    derivation.
  - Message framing: AuthenticatedMessage, HMAC computation, replay
    protection.
  - Complete message type registry (all message types).
  - Flow control protocol: capacity model, grants, priority queues,
    load shedding.
  - Transaction flooding: push/pull model, advert batching, demand
    scheduling.
  - Peer management: database, selection, backoff, preferred peers,
    banning.
  - Survey protocol: lifecycle, request/response, encryption,
    topology data.
  - Security considerations: threat model, auth, integrity, DoS,
    eclipse attacks.
  - Full XDR schema as Appendix A.

**HERDER_SPEC.md** (from `src/herder/`):
  - Herder state machine: BOOTING / SYNCING / TRACKING (3 states).
  - Consensus round lifecycle: triggering, nomination,
    externalization.
  - StellarValue construction and validation.
  - Transaction set construction: phase structure, surge pricing,
    lane model.
  - Parallel Soroban tx sets: stages, clusters, footprint conflicts.
  - Transaction set validation (XDR structure + semantic +
    per-phase).
  - Transaction set apply ordering (sequential + parallel).
  - Candidate combination (tx set selection, close time, upgrade
    merging).
  - Transaction queue: capacity, reception pipeline, aging,
    replace-by-fee, ban mechanism.
  - Surge pricing and eviction: fee rate comparison, lane model,
    selection.
  - Transaction broadcasting: flood rate limiting, best-fee-first.
  - SCP envelope management: states, reception flow, dependency
    fetching, caching.
  - Protocol upgrades: types, scheduling, validation, merging.

**LEDGER_SPEC.md** (from `src/ledger/`, `src/main/`):
  - Ledger close pipeline (multi-step sequence).
  - Apply state phases: SETTING_UP_STATE / READY_TO_APPLY / APPLYING
    / COMMITTING.
  - Transaction application: fee processing, sequential + parallel
    phases.
  - LedgerTxn nested transactional state: hierarchy, nesting rules,
    entry operations (load / create / erase), sealing, commit /
    rollback semantics, entry merge rules (the
    Parent\Child × INIT/LIVE/DELETED matrix).
  - Protocol upgrades: types, lifecycle, validation, application.
  - Ledger header management: update sequence, skip list, hash
    computation.
  - Network configuration: Soroban settings, cost model, rent fees,
    resource limits.
  - Soroban state management: in-memory state, TTL co-location,
    module cache.
  - Commit and persistence: seal-and-store, HAS, checkpoints.
  - Ledger close meta: versions, contents, streaming.
  - Genesis ledger: constants and procedure.

**TX_SPEC.md** (from `src/transactions/`, `src/main/`):
  - Transaction lifecycle: submission → validation → application →
    result.
  - Two-phase ledger model (fee phase + application phase).
  - Data types: envelope types, body, fee-bump, operations,
    preconditions, results.
  - Transaction validation: structural, precondition, source
    account, signature (full checking algorithm), fee source
    balance, operation-level, Soroban resource.
  - Fee framework: structure, surge pricing, effective fee, fee-bump
    semantics, Soroban refunds.
  - Transaction application pipeline: entry point, pre-apply,
    commonValid, operation application, threshold levels, source
    account resolution.
  - Operation execution: all operation types (CreateAccount through
    RestoreFootprint), sponsorship framework, DEX conversion engine.
  - Soroban execution: structure, fee model, validation,
    InvokeHostFunction, ExtendFootprintTTL, RestoreFootprint,
    parallel execution.
  - State management: nested LedgerTxn model, entry states, commit /
    rollback, single-child invariant, entry loading, root commit,
    last-modified stamping.
  - Metadata construction: versions, structure, change types,
    recording.
  - Event emission: Soroban events, classic SAC events, XLM
    reconciliation.
  - Error handling: tx-level result codes, op-level result codes.

**BUCKETLISTDB_SPEC.md** (from `src/bucket/`):
  - Architecture: design goals, entry flow.
  - Data types: BucketEntry, BucketMetadata, HotArchiveBucketEntry,
    sort order.
  - BucketList structure: levels, level sizing formulas, update
    period, spill condition, oldest ledger tracking, BucketList
    hash, tombstone retention.
  - Bucket lifecycle: creation, entry conversion, Level 0 update,
    prepare / snap / commit.
  - Merge algorithm: protocol version calculation, merge loop,
    shadow elision, equal-key merge rules (pre / post INITENTRY),
    tombstone elision, in-memory merge.
  - Asynchronous merge: FutureBucket state machine, construction,
    start / dedup, resolution, HAS integration, MergeKey, merge-map.
  - BucketManager: adoption, garbage collection, statistics.
  - Indexing: in-memory index, disk index, Bloom filter, entry
    cache, persistence.
  - Snapshot and query layer: BucketSnapshotManager, point lookup,
    bulk load, pool share trust line query, inflation winners query,
    entry type scan.
  - Hot Archive BucketList: purpose, structure, entry types, merge
    rules.
  - Eviction: iterator, starting position, scan process, entry
    types, validity.
  - Catchup integration: bucket application, application order,
    state reconstruction.
  - Serialization: HistoryArchiveState, bucket directory layout,
    checkpoint alignment.

**CATCHUP_SPEC.md** (from `src/catchup/`, `src/history/`):
  - Architecture: two parallel workflows (publishing + catchup).
  - Catchup strategies: minimal, recent, complete.
  - History archive structure: file layout, path construction,
    checkpoint frequency.
  - Checkpoint publishing pipeline: incremental building,
    finalization, HAS queue, upload, backpressure, crash recovery.
  - Catchup configuration and range computation.
  - Ledger apply manager: buffering, process ledger decision tree,
    sequential application, online catchup trigger.
  - Catchup pipeline: phases (fetch HAS, download/verify chain,
    build sequence, apply buffered).
  - Ledger chain verification: trust establishment, verification
    algorithm.
  - Bucket application: download, application algorithm, post-apply
    state setup.
  - Transaction replay: per-checkpoint workflow, ordering,
    backpressure, gaps.
  - Buffered ledger application: drain SCP-buffered ledgers.
  - Error handling: retry semantics, archive rotation, fatal
    failure, crash recovery.

═══════════════════════════════════════════════════════════════════
PROCESS
═══════════════════════════════════════════════════════════════════

For each spec in scope:

1. Read all source files in the corresponding stellar-core
   directory.
2. Identify state machines, algorithms, validation rules, data
   structures, message formats, error codes, constants, invariants.
3. Extract the observable protocol behavior, discarding
   implementation internals.
4. **`$STRATEGY = update`**: classify findings against the existing
   spec and apply only additions / strengthenings.
   **`$STRATEGY = regenerate`**: organize into the canonical section
   structure above and write the full document.
5. Write in the specified style, using RFC 2119 keywords for
   requirements.
6. Create Mermaid diagrams for visual representations.
7. Cross-reference companion specs using `SPEC_NAME §N.N` notation.
8. Place illustrative material (complex diagrams, worked examples,
   decision matrices) in appendices.

## Output by mode

- **`$MODE = review`**: produce the structured report described
  under "Mode" above. Do not write any spec files. For
  `$STRATEGY = regenerate` in review mode, refuse with: "Regenerate
  overwrites curated content. Re-run with `--apply` to confirm."
- **`$MODE = apply`**: write each spec file in `$SCOPE` to disk and
  print a short summary of what changed.

## Notes

- The skill operates against whatever version the `stellar-core/`
  submodule is currently pinned to. To target a different version,
  bump the submodule first.
- For full-scope update passes (`$SCOPE = all`, `$STRATEGY = update`,
  `$MODE = apply`), the 8 specs are independent — run as parallel
  subagents.
- Source-level `// Spec: XXX_SPEC §N` comments in `crates/*/src/`
  are the anchor between Rust code and these specs. When the skill
  adds new sections that may be referenced from code, prefer
  numbering inside an existing top-level `##` section to avoid
  invalidating existing anchors.
