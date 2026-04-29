## 🔍 Critic Response (Round 2/5)

<details>
<summary>Full critique (click to expand)</summary>

The proposal is directionally correct and addresses the core bug: henyey currently spends queue flood budget before app-level per-peer “already seen” filtering, while stellar-core only decrements budget when the broadcast visitor returns `PROCESSED`.

However, I would revise before implementation. The plan slightly overclaims parity and leaves a few edge semantics under-specified.

Required changes:

1. Define `BroadcastBudget` precisely, including `dex_ops_remaining: Option<usize>`. `None` must mean “DEX uncapped”, not “zero remaining”, and carry-over updates must preserve that distinction.

2. Make visitor result depend on actual send outcome, or explicitly scope the deviation. In stellar-core, `broadcastTx()` returns `PROCESSED` only when `broadcastMessage()` succeeds; already-broadcast and skipped/damped txs are `SKIPPED`. The proposal says `Processed` when at least one peer needs the hash, before `try_send_to()` can fail. That preserves current henyey behavior but is not precise parity.

3. Do not claim full `broadcastSome()` parity. henyey still lacks stellar-core’s `allowTxBroadcast()` arbitrage damping and its ban-on-damping path, already noted in `crates/herder/PARITY_STATUS.md`. The proposal can fix advert-budget parity, but it should update parity docs to say this is still partial.

4. Specify stale-candidate behavior after snapshotting. Since the visitor runs after dropping the queue read lock, a tx may be removed before adverts are sent. This is probably acceptable because the current vector API already has the same race, but the new API should document that `BroadcastCandidate` is an advisory hash/op metadata view, not a guarantee the tx remains queued.

5. Add an app-level test for send failure or consciously document it as unchanged. The current plan tests all-seen skipping, but not the case where a peer needs the hash and `try_send_to()` fails. That is important because it determines whether the new carry-over-from-`BroadcastBudget` path silently changes retry budgeting.

6. Update `crates/herder/PARITY_STATUS.md` and any README/API comments that currently describe `broadcast_some()` as vector-returning or “Full” parity. The current docs refer directly to `broadcast_some()` and would become misleading after the API change.

VERDICT: REVISE

</details>

**Verdict: REVISE**

1. Define `BroadcastBudget` precisely, including `dex_ops_remaining: Option<usize>`. `None` must mean DEX uncapped, not zero remaining, and carry-over updates must preserve that distinction.
2. Make visitor result depend on actual send outcome, or explicitly scope the deviation from stellar-core's send-success-based `PROCESSED` semantics.
3. Do not claim full `broadcastSome()` parity; update parity docs to say advert-budget parity is improved but arbitrage damping remains partial.
4. Specify stale-candidate behavior after snapshotting.
5. Add an app-level send-failure test or document send-failure budgeting as unchanged.
6. Update `crates/herder/PARITY_STATUS.md` and any README/API comments that describe `broadcast_some()`.
