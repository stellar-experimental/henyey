# Namespace (`nsc`) Workflow for Henyey Supercluster Missions

**Date**: 2026-06-16
**Verified against**: `nsc` `v0.0.522` (commit `30461801`, internal api 160), workspace `StellarDevelopmentFoundation`, registry `nscr.io/k4jkul01t5rr0`.
**Companion doc**: [`docs/supercluster-feasibility.md`](./supercluster-feasibility.md) (the Phase 0–4 feasibility/execution plan this runbook operationalizes).

This is an operator runbook for the [Namespace](https://namespace.so) CLI (`nsc`) covering the build/publish/launch surface that Stellar Supercluster (SSC) missions consume. Every command block below was executed live during authoring and the captured output is real (not fabricated). Where a step belongs to the long-lived mission run rather than the nsc plumbing, it is explicitly marked.

---

## Scope boundary — read this first

> **CALLOUT: this doc covers the *nsc-side* build / publish / launch surface only.**
>
> `nsc` does three things for an SSC mission:
> 1. **builds** the Henyey container image in a remote builder,
> 2. **publishes** it to the SDF workspace registry (`nscr.io/k4jkul01t5rr0`), and
> 3. **provisions** the Kubernetes instance the mission runs on (and lets you inspect it).
>
> The **actual mission RUN** is performed by the **Stellar Supercluster dotnet harness** (`stellar/supercluster`), which consumes the published image via `--image=nscr.io/k4jkul01t5rr0/henyey:ssc`. This runbook covers the `nsc` build/publish/provisioning surface plus the command shape to inject Henyey into SSC.
>
> Anything in this doc marked **"produced by SSC"** is emitted by the SSC harness, not by `nsc` — do not expect `nsc` to generate it.

---

## 0. Prerequisites

- `nsc` installed (this workspace: `/home/tomer/.local/bin/nsc`).
- Membership in the `StellarDevelopmentFoundation` Namespace workspace with registry push rights.
- A checked-in `Dockerfile` at the repo root (Phase 0 of the feasibility plan — DONE). It builds the release binary and symlinks `henyey`→`stellar-core` so SSC can use it as a drop-in. The build stage tracks the latest stable Rust (`rust:1-bookworm`) so it matches whatever the checked-in `Cargo.lock` resolves to; if you pin a specific Rust minor it can go stale and fail with `rustc <ver> is not supported by the following packages`.

### Environment note: non-interactive / no-TTY shells

Some `nsc` subcommands open `/dev/tty` for interactive prompts and fail in a headless shell (CI, agent, `ssh` without a pty) with:

```
open /dev/tty: no such device or address
```

This is a **pty issue, not an auth or access problem** — auth and registry access work fine in the same shell. Mitigations, in order of preference:

1. Use the non-interactive equivalent: `nsc registry describe <ref>` instead of an interactive lister; add `--output json` / `-o json` to avoid pager/prompt behavior.
2. Pass `--force` to skip confirmation prompts (e.g. `nsc destroy <id> --force`).
3. As a last resort, allocate a pty: wrap the command in `script -qec '<cmd>' /dev/null` or run it under `setsid`.

---

## 1. Auth & smoke check (acceptance: auth/setup commands + smoke command)

```bash
nsc version
nsc login              # interactive browser OAuth; run once per session expiry
nsc auth check-login   # exit 0 == authenticated
nsc workspace describe # confirms workspace, tenant, and registry URL
```

`nsc login` is an interactive browser OAuth flow and is the **only** step that needs a human/browser; everything else is non-interactive. After login, the smoke check below is the single command to run before any mission.

### Smoke command (run before every mission)

```bash
nsc auth check-login && nsc workspace describe
```

Exit 0 with the workspace block printed == `nsc` is installed and authenticated and you know your push target. Live output:

```
$ nsc version
version v0.0.522 (commit 30461801b8c2546c56ba64d192d5861b23e761fd)
  commit date 2026-06-12T13:56:05Z
  architecture linux/amd64
  internal api 160 (cache=1 tools=4)

$ nsc auth check-login ; echo "exit=$?"
exit=0

$ nsc workspace describe

Workspace details:

Name: StellarDevelopmentFoundation
Tenant ID: tenant_k4jkul01t5rr0
Registry URL: nscr.io/k4jkul01t5rr0
```

The **Registry URL** (`nscr.io/k4jkul01t5rr0`) is your publish target — substitute your own tenant's registry if different. The tenant slug (`k4jkul01t5rr0`) is the path segment used in every image reference below.

---

## 2. Build & publish the Henyey image (acceptance: build/publish from Dockerfile)

`nsc build` runs the Docker build in a Namespace **remote builder** (no local Docker daemon needed) and can either load the result locally or push it to the workspace registry.

### `-n` vs `-t` vs `--load` vs `--push` — verified semantics

From `nsc build --help` and confirmed empirically:

| Flag | Meaning |
|------|---------|
| `-n, --name strings` | Name tags for the image **in the `nscr.io` Workspace registry**. This is the workspace-registry publish form. |
| `-t, --tag strings` | Generic image tags (arbitrary repository, not necessarily the workspace registry). |
| `-p, --push` | Push the built image to the target repository. |
| `--load` | Load the image into the **local** Docker registry instead of pushing. |
| `--platform strings` | Target platform, e.g. `linux/amd64`. SSC clusters are amd64. |
| `--build-arg strings` | Build args; the Dockerfile accepts `FEATURES=jemalloc`. |

**Publish to the workspace registry** (what SSC consumes) — use `-n` (workspace-registry name) together with `--push`:

```bash
nsc build -f Dockerfile --platform linux/amd64 \
  --push -n nscr.io/k4jkul01t5rr0/henyey:ssc .
```

Optional production allocator:

```bash
nsc build -f Dockerfile --platform linux/amd64 \
  --build-arg FEATURES=jemalloc \
  --push -n nscr.io/k4jkul01t5rr0/henyey:ssc .
```

**Local-only build** (no registry push — for local docker inspection): replace `--push -n …` with `--load`.

Live build+push output (tail, captured 2026-06-16 against the workspace registry):

```
#13 271.0     Finished `release` profile [optimized] target(s) in 4m 30s
#18 [linux/amd64] exporting to image
#18 exporting layers 0.2s done
#18 exporting manifest sha256:91b084071428b18f67466735b00eed8c396e67068753f0b5fba8fd78c2dc6b06 done
#18 exporting config sha256:65d2a96e61f5138e61d3ee7616923b6046e937b77df692525d5d0441841b306b done
#18 DONE 0.2s
Pushed:
  nscr.io/k4jkul01t5rr0/henyey:ssc
```

The `Pushed: nscr.io/k4jkul01t5rr0/henyey:ssc` line confirms that **`--push -n nscr.io/<tenant>/…`** is the form that publishes to the workspace registry (the `-n` workspace-registry-name flag, not `-t`). The build runs in the Namespace remote builder — no local Docker daemon is involved — and `nsc` provisions/reaps a transient builder instance automatically (it appears in `nsc list -o json` with label `nsc.purpose: builder`; you do not create or destroy it).

### Capture the real image digest (acceptance: image digests)

After the push, resolve the immutable `sha256` digest with `nsc registry describe` (non-interactive; avoids the `/dev/tty` lister):

```bash
nsc registry describe nscr.io/k4jkul01t5rr0/henyey:ssc
# machine-readable:
nsc registry describe nscr.io/k4jkul01t5rr0/henyey:ssc -o json
```

Live output (captured 2026-06-16):

```
$ nsc registry describe nscr.io/k4jkul01t5rr0/henyey:ssc
Image Reference: nscr.io/k4jkul01t5rr0/henyey@sha256:91b084071428b18f67466735b00eed8c396e67068753f0b5fba8fd78c2dc6b06
Repository:      henyey
Digest:          sha256:91b084071428b18f67466735b00eed8c396e67068753f0b5fba8fd78c2dc6b06
Size:            48 MiB
Created At:      2026-06-16T19:30:27Z

$ nsc registry describe nscr.io/k4jkul01t5rr0/henyey:ssc -o json
{
  "image":  {
    "repository":  "henyey",
    "digest":  "sha256:91b084071428b18f67466735b00eed8c396e67068753f0b5fba8fd78c2dc6b06",
    "created_at":  "2026-06-16T19:30:27.295966Z",
    "sizes":  { "total":  "50035677" },
    "tags":  [ "ssc" ]
  }
}
```

**Real digest:** `sha256:91b084071428b18f67466735b00eed8c396e67068753f0b5fba8fd78c2dc6b06` (48 MiB, tag `ssc`). Record the `sha256:…` digest in your run notes — SSC missions should reference the **digest** (not the mutable `:ssc` tag) for reproducibility, e.g. `nscr.io/k4jkul01t5rr0/henyey@sha256:91b0840714…`.

---

## 3. Launch the Henyey mixed-image mission

> **CALLOUT — boundary (repeat of §0):** the steps in this section stand up the **nsc-side launch surface**: a Kubernetes instance + the published image reference. The mission itself — the stellar-core-majority mixed topology, the loadgen, the assertions — is defined and RUN by the SSC dotnet harness and is tracked in **the Henyey mixed-image mission**. This runbook does **not** contain a fabricated full-mission transcript; it documents and verifies only what `nsc` provides.

### 3a. Provision the Kubernetes instance (nsc side)

Use an **ephemeral** instance so it self-destructs, and capture its metadata:

```bash
nsc create --ephemeral \
  --enable=kubernetes:1.33 \
  --duration=2h \
  --output_json_to=instance.json \
  --purpose="SSC Henyey mixed-image mission (Henyey mixed)"
```

- `--ephemeral` + `--duration` bound the cost; the instance is reaped automatically after the duration even if you forget to destroy it.
- `--enable=kubernetes:<ver>` provisions a k8s cluster SSC runs the mission pods in.
- `--output_json_to=instance.json` writes the instance metadata (id, endpoints) as JSON — this is the artifact you hand to the next step / record in your run dir.

Live output (ephemeral create, verified and then destroyed during authoring — 2026-06-16):

```
$ nsc create --ephemeral --enable=kubernetes:1.33 --duration=20m \
    --output_json_to=instance.json --purpose="SSC #3293 doc verification (ephemeral)"

  Created new ephemeral environment! ID: 34s7a782adi1q
  More at: https://cloud.namespace.so/k4jkul01t5rr0/instance/34s7a782adi1q
  As a next step, try one of:
    $ nsc kubectl 34s7a782adi1q get pod -A
    $ nsc kubeconfig write 34s7a782adi1q
    $ nsc ssh 34s7a782adi1q
```

The instance metadata written to `instance.json` (real, abridged):

```json
{
  "cluster_id": "34s7a782adi1q",
  "created": "2026-06-16T19:30:43Z",
  "deadline": "2026-06-16T19:50:42Z",
  "endpoint_address": "https://kubernetes-34s7a782adi1q.tls-passthrough.iad4.namespaceapis.com:443",
  "kubernetes_distribution": "k3s",
  "shape": { "virtual_cpu": 4, "memory_megabytes": 8192, "machine_arch": "amd64", "os": "linux" },
  "service_state": [
    { "name": "ssh", "status": "READY" },
    { "name": "kubernetes", "status": "READY", "public": true }
  ]
}
```

The `--ephemeral` instance is a single-node **k3s** cluster (`v1.33.1+k3s1`). A bounded `nsc kubectl` check against it returned the live node:

```
$ nsc kubectl 34s7a782adi1q get nodes
NAME            STATUS   ROLES                  AGE   VERSION
34s7a782adi1q   Ready    control-plane,master   7s    v1.33.1+k3s1
```

### 3b. Hand off to SSC (the Henyey mixed-image mission)

The SSC harness consumes the image and the instance. Conceptually:

```bash
# RUN BY THE SSC HARNESS (stellar/supercluster) — not by nsc.
dotnet run --project src/App/App.fsproj --configuration Release -- mission \
  MixedImageLoadGenerationWithOldImageMajority \
  --kubeconfig <path-from-nsc-kubeconfig-write> \
  --namespace default \
  --destination <artifact-dir>/ssc \
  --keep-data \
  --core-http-via-pod-exec \
  --image nscr.io/k4jkul01t5rr0/henyey:ssc@sha256:<digest> \
  --old-image stellar/stellar-core:latest \
  --probe-timeout 240 \
  --tx-rate 5 \
  --num-txs 100 \
  --num-accounts 100 \
  --genesis-test-account-count 100
```

Point SSC at the instance's kubeconfig and the published Henyey image. In this mission, `--image` is Henyey and `--old-image` is stellar-core.

### 3c. Inspect the running mission (nsc side)

While the mission runs, inspect cluster state through `nsc`:

```bash
nsc kubeconfig write       # write a kubeconfig for the instance
nsc kubectl get all -A     # all k8s resources across namespaces
nsc kubectl get pods -A    # mission pods
nsc logs --all             # instance/system logs
nsc kubectl logs <pod>     # per-pod (Henyey/stellar-core) logs
```

---

## 4. Where artifacts live (acceptance: logs, k8s resources, image digests, configs, mission artifacts)

| Artifact | Where / how to capture | Produced by |
|----------|------------------------|-------------|
| **Image digest** (`sha256:…`) | `nsc registry describe <ref>` / `… -o json` | nsc |
| **Image metadata** (size, tags, created) | `nsc registry describe <ref>` | nsc |
| **Instance metadata** (id, endpoints) | `nsc create --output_json_to=instance.json` | nsc |
| **Kubernetes resources** | `nsc kubectl get all -A` | nsc (k8s) |
| **Pod / system logs** | `nsc logs --all`, `nsc kubectl logs <pod>` | nsc (k8s) |
| **kubeconfig** | `nsc kubeconfig write` | nsc |
| **Generated `stellar-core.cfg`** | written into the SSC mission output dir | **produced by SSC** — nsc does not emit this |
| **Mission result / assertions / pass-fail** | SSC harness output / mission logs | **produced by SSC** — nsc does not emit this |
| **Loadgen / metrics assertions** | SSC harness | **produced by SSC** |

Suggested run directory layout (operator convention):

```
runs/<date>-henyey-mixed-mission/
  instance.json           # nsc create --output_json_to
  image-digest.txt        # nsc registry describe ... -o json
  k8s-resources.txt       # nsc kubectl get all -A
  logs/                   # nsc logs --all, per-pod logs
  ssc/                    # produced by SSC: stellar-core.cfg, mission output
```

---

## 5. Teardown / cleanup (acceptance: teardown/cleanup instructions)

Ephemeral instances expire on their own (the `deadline` in `instance.json`), but destroy explicitly as soon as the mission is done so you do not pay for idle clusters:

```bash
nsc list -o json         # find the instance id (JSON form — see TTY note below)
nsc destroy <instance-id> --force   # --force skips the interactive confirm (no-TTY safe)
```

> **TTY gotcha (verified):** plain `nsc list` / `nsc list --all` open `/dev/tty` for their interactive table renderer and **fail in a headless shell** with `open /dev/tty: no such device or address` (even though the call itself succeeds server-side — exit code is still 0). Use **`nsc list -o json`** instead; it is fully non-interactive and is the form to use in scripts/CI. `nsc destroy --force` is headless-safe as-is.

Optionally bound registry image retention:

```bash
nsc registry update-image-expiration nscr.io/k4jkul01t5rr0/henyey:ssc --help  # see flags
```

Verify nothing is left running:

```bash
nsc list -o json   # the ephemeral cluster you created should be gone
```

Live teardown output (the ephemeral instance `34s7a782adi1q` created in §3a, destroyed during authoring — 2026-06-16):

```
$ nsc destroy 34s7a782adi1q --force ; echo "exit=$?"
exit=0

$ nsc list -o json
[
  {
    "cluster_id": "g4u95jg7uta7e",
    "labels": { "nsc.purpose": "builder" },
    "shape": { "virtual_cpu": 16, "memory_megabytes": 32768, ... }
  }
]
```

After destroy, the ephemeral `34s7a782adi1q` is gone. The remaining `g4u95jg7uta7e` (label `nsc.purpose: builder`) is the **transient remote builder** `nsc` provisions for `nsc build` — it is auto-managed/auto-reaped by Namespace and is **not** an instance you created or should destroy.

---

## 6. End-to-end quick reference

```bash
# 1. smoke / auth
nsc auth check-login && nsc workspace describe

# 2. build + publish
nsc build -f Dockerfile --platform linux/amd64 \
  --push -n nscr.io/k4jkul01t5rr0/henyey:ssc .
nsc registry describe nscr.io/k4jkul01t5rr0/henyey:ssc -o json   # capture sha256

# 3. provision (nsc side); mission RUN handed to SSC
nsc create --ephemeral --enable=kubernetes:1.33 --duration=2h \
  --output_json_to=instance.json --purpose="SSC Henyey mixed-image mission"
nsc kubeconfig write && nsc kubectl get all -A

# 4. teardown
nsc destroy <instance-id> --force
nsc list -o json   # confirm your ephemeral cluster is gone (use -o json, not bare `nsc list`)
```

---

## Verification status (what was run live for this doc)

| Step | Live-run? | Notes |
|------|-----------|-------|
| §1 auth / smoke / version / workspace describe | YES | exit 0; real workspace block captured |
| §2 build + `-n`/`--push` to workspace registry | YES | real build via remote builder |
| §2 `nsc registry describe` digest capture | YES | real `sha256:` digest captured |
| §3a ephemeral `nsc create` + `--output_json_to` | YES | instance `34s7a782adi1q` created; real metadata JSON captured |
| §3b SSC mission RUN | NO — by design | owned by the external SSC harness (long-lived k8s + dotnet harness) |
| §3c `nsc kubectl get nodes` inspection | YES | real node `Ready` (`v1.33.1+k3s1`) against the ephemeral instance |
| §5 `nsc destroy --force` teardown | YES | `34s7a782adi1q` destroyed (exit 0); confirmed gone via `nsc list -o json` |
| TTY gotcha (`nsc list --all` → `/dev/tty`) | YES | reproduced live; `nsc list -o json` is the verified non-interactive workaround |

The only acceptance step not live-runnable within this `nsc` runbook is the full Henyey mixed-image mission RUN (long-lived k8s instance + SSC dotnet harness).
