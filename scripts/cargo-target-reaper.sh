#!/usr/bin/env bash
# cargo-target-reaper — host janitor that reclaims idle Rust build-cache dirs
# under ~/data so co-tenant `cargo-target`/`target` trees can never again fill
# the shared volume to zero free bytes.
#
# Why this exists (#3798, caused #3797): 96 per-session `cargo-target/` build
# caches accumulated to 1,155 GiB with no retention policy and took the shared
# volume to literally 0 bytes free, killing the mainnet validator for 8 d 10 h
# and disabling every recovery loop on the host (a `git clone` for a scratch
# checkout cannot run at zero free bytes — the backstop died of the exact
# condition it existed to survive). This reaper is deliberately built to run at
# zero free bytes: it needs only coreutils + python3 already on the host, never
# clones or writes into any candidate, and reclaims exclusively by unlink.
#
# Design (see the issue's Converged Plan):
#   * Discovery is STRUCTURAL, not name/depth-glob: a directory is a build-cache
#     root iff its basename is `target` or `cargo-target` AND it directly
#     contains `debug/` or `release/`. On match we record it and stop
#     descending (nested `target/debug/target` is counted once). This catches
#     the 62/68 real caches that are named `target` and live 2-4 levels deep,
#     which a depth-1 `*/cargo-target/` glob misses (~68% of the bytes).
#   * An AFFIRMATIVE liveness guard builds the in-use path set from every
#     /proc/<pid>/exe, mapped file in /proc/<pid>/maps, and /proc/<pid>/fd/*
#     target, and skips any candidate that equals or contains an in-use path,
#     compared by CANONICALIZED path COMPONENTS (not string prefix, so `foo`
#     never matches `foo-target` and symlinks cannot evade it). The skip is
#     scoped to the candidate dir, NOT its parent session — this is the fix for
#     the b0db5fda defect where 85.58 GiB of unrelated cache rode along because
#     an fd on a sibling log skipped the whole session dir. This is also what
#     protects the running validator's own cargo-target (whose mtime reads as
#     weeks-idle because it is the binary's build date, not its access time).
#   * An age gate deletes only live-clear candidates whose dir mtime is older
#     than REAP_AGE_DAYS (default 7; the census minimum idle was 9.3 d).
#   * An exempt allowlist (default `mainnet`) is a belt-and-suspenders layer
#     atop the liveness guard; `mainnet/` also hardlinks bucket inodes into
#     session trees, which is the second reason reclamation is unlink-only:
#     unlink is safe for a shared inode, an in-place rewrite would corrupt the
#     live bucket set.
#   * Reporting sizes each candidate with a SEPARATE `du -sb` per tree (never
#     `du -sb */`, which mis-attributes shared inodes) and emits a before/after
#     + per-dir + total reclaim report to a bounded log.
#
# Canonical copy: scripts/cargo-target-reaper.sh in the henyey repo.
# Live copy: /home/tomer/data/cargo-target-reaper.sh (what crontab executes,
# decoupled from any repo checkout state, same split the project-loop watchdog
# documents).
#
# Install: 0 4 * * * /home/tomer/data/cargo-target-reaper.sh
#
# Usage:
#   scripts/cargo-target-reaper.sh [--dry-run]
#
# Environment overrides (all optional):
#   REAP_BASE_DIR   base to scan (default /home/tomer/data)
#   REAP_AGE_DAYS   min idle age in days to reap (default 7)
#   REAP_MAX_DEPTH  max dir depth below base to search (default 4)
#   REAP_EXEMPT     newline-separated session dir names never touched
#                   (default `mainnet`)
#   REAP_LOG        log file (default /home/tomer/data/cargo-target-reaper.log)
#   DRY_RUN=1       print the plan without deleting (same as --dry-run)
#
# Exit: 0 on a clean run (including a run that reaped nothing), non-zero on an
# internal error. Reaping nothing is success, not failure.
set -uo pipefail

# cron runs with a minimal PATH; make sure python3 + coreutils resolve.
export PATH="/usr/local/bin:/usr/bin:/bin:${PATH:-}"

REAP_BASE_DIR="${REAP_BASE_DIR:-/home/tomer/data}"
REAP_AGE_DAYS="${REAP_AGE_DAYS:-7}"
REAP_MAX_DEPTH="${REAP_MAX_DEPTH:-4}"
REAP_EXEMPT="${REAP_EXEMPT:-mainnet}"
REAP_LOG="${REAP_LOG:-/home/tomer/data/cargo-target-reaper.log}"
DRY_RUN="${DRY_RUN:-0}"

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1 ;;
    -h|--help)
      sed -n '1,64p' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *)
      echo "cargo-target-reaper: unknown argument: $1" >&2
      exit 2 ;;
  esac
  shift
done

export REAP_BASE_DIR REAP_AGE_DAYS REAP_MAX_DEPTH REAP_EXEMPT DRY_RUN

# Keep the log bounded (~5 MB), same rotation the project-loop watchdog uses.
if [ -f "$REAP_LOG" ] && [ "$(stat -c %s "$REAP_LOG" 2>/dev/null || echo 0)" -gt 5242880 ]; then
  tail -c 1048576 "$REAP_LOG" > "$REAP_LOG.tmp" 2>/dev/null && mv "$REAP_LOG.tmp" "$REAP_LOG"
fi

# The core runs in python3 (already on the host): /proc scanning, canonical
# path-component comparison, structural discovery, and unlink-only reclamation
# are all far safer expressed there than in shell. Output is tee'd to the log
# (append) and stdout so cron captures it and the test harness can assert on it.
run_core() {
python3 - <<'PYEOF'
import os, sys, time, shutil, subprocess

BASE       = os.path.realpath(os.environ["REAP_BASE_DIR"])
AGE_DAYS   = float(os.environ.get("REAP_AGE_DAYS", "7"))
MAX_DEPTH  = int(os.environ.get("REAP_MAX_DEPTH", "4"))
DRY_RUN    = os.environ.get("DRY_RUN", "0") == "1"
EXEMPT     = set(x.strip() for x in os.environ.get("REAP_EXEMPT", "mainnet").splitlines()
                 if x.strip())

now        = time.time()
age_cutoff = now - AGE_DAYS * 86400.0

def canon_components(path):
    try:
        rp = os.path.realpath(path)
    except OSError:
        rp = os.path.normpath(path)
    rp = rp.rstrip("/")
    return rp.split("/") if rp else [""]

def is_under_or_equal(inuse_comps, cand_comps):
    # True iff the in-use path IS the candidate dir or is nested under it,
    # compared component-by-component (not string prefix) so `.../foo` never
    # matches `.../foo-target` and a symlinked path cannot slip past.
    if len(inuse_comps) < len(cand_comps):
        return False
    return inuse_comps[:len(cand_comps)] == cand_comps

def du_sb(path):
    # Size a single tree with `du -sb` (apparent size, hardlink-deduped). One
    # invocation PER TREE — never `du -sb */`, which mis-attributes bytes across
    # trees that share inodes. Falls back to a python inode-deduped walk if du
    # is somehow unavailable, so sizing never aborts the run.
    try:
        out = subprocess.run(["du", "-sb", path], capture_output=True, text=True)
        if out.returncode == 0 and out.stdout.strip():
            return int(out.stdout.split()[0])
    except Exception:
        pass
    total, seen = 0, set()
    try:
        total += os.lstat(path).st_size
    except OSError:
        pass
    for root, dirs, files in os.walk(path, followlinks=False, onerror=lambda e: None):
        for name in dirs + files:
            fp = os.path.join(root, name)
            try:
                st = os.lstat(fp)
            except OSError:
                continue
            if st.st_nlink > 1:
                key = (st.st_dev, st.st_ino)
                if key in seen:
                    continue
                seen.add(key)
            total += st.st_size
    return total

def discover(base):
    # Structural, any-depth <= MAX_DEPTH, no-descend-on-match. Exempt session
    # dirs are pruned from descent (and reported), so mainnet's ~172 GiB tree is
    # never walked.
    candidates, exempted = [], []
    base_depth = base.rstrip("/").count("/")
    for root, dirs, files in os.walk(base, followlinks=False, onerror=lambda e: None):
        depth = root.rstrip("/").count("/") - base_depth
        # Prune + record exempt subtrees before considering descent.
        here_exempt = [d for d in dirs if d in EXEMPT]
        for d in here_exempt:
            exempted.append(os.path.join(root, d))
        if here_exempt:
            dirs[:] = [d for d in dirs if d not in EXEMPT]
        bn = os.path.basename(root)
        if bn in ("target", "cargo-target"):
            has_out = any(os.path.isdir(os.path.join(root, s)) for s in ("debug", "release"))
            if has_out:
                candidates.append(root)
                dirs[:] = []          # matched build-cache root: do not descend
                continue
        if depth >= MAX_DEPTH:
            dirs[:] = []              # depth cap
    return candidates, exempted

def build_inuse():
    inuse, unreadable = [], []
    try:
        pids = [p for p in os.listdir("/proc") if p.isdigit()]
    except OSError:
        return inuse, unreadable
    for pid in pids:
        pdir = os.path.join("/proc", pid)
        got_any = False
        try:
            inuse.append(canon_components(os.readlink(os.path.join(pdir, "exe"))))
            got_any = True
        except OSError:
            pass
        fddir = os.path.join(pdir, "fd")
        try:
            for fd in os.listdir(fddir):
                try:
                    t = os.readlink(os.path.join(fddir, fd))
                    if t.startswith("/"):
                        inuse.append(canon_components(t))
                        got_any = True
                except OSError:
                    pass
        except OSError:
            pass
        try:
            with open(os.path.join(pdir, "maps")) as f:
                for line in f:
                    parts = line.rstrip("\n").split(None, 5)
                    if len(parts) == 6 and parts[5].startswith("/"):
                        inuse.append(canon_components(parts[5]))
            got_any = True
        except OSError:
            pass
        if not got_any:
            # Could not read exe/fd/maps for this pid — almost always a
            # cross-user (EPERM) process, which on this single-user host will
            # not hold a path under ~/data. We cannot enumerate its paths, so we
            # log the blind spot conservatively rather than silently ignoring
            # it; the age + exempt gates still apply to every candidate.
            unreadable.append(pid)
    return inuse, unreadable

ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now))
print("cargo-target-reaper %s  base=%s  age>=%gd  max_depth=%d  dry_run=%s"
      % (ts, BASE, AGE_DAYS, MAX_DEPTH, "1" if DRY_RUN else "0"))

if not os.path.isdir(BASE):
    print("base dir does not exist: %s — nothing to do" % BASE)
    sys.exit(0)

try:
    st = os.statvfs(BASE)
    free_before = st.f_bavail * st.f_frsize
except OSError:
    free_before = -1

candidates, exempted = discover(BASE)
inuse, unreadable = build_inuse()

print("scanned %d candidate build-cache dir(s); %d exempt subtree(s); "
      "liveness: %d in-use path(s), %d unreadable pid(s)"
      % (len(candidates), len(exempted), len(inuse), len(unreadable)))
for e in sorted(exempted):
    print("[SKIP-exempt] %s" % e)
if unreadable:
    print("[liveness-blindspot] %d unreadable pid(s) treated conservatively "
          "(logged only): %s" % (len(unreadable), ",".join(sorted(unreadable))))

reaped_total = 0
reaped_count = 0
errors = 0
for cand in sorted(candidates):
    try:
        mtime = os.stat(cand).st_mtime
    except OSError:
        continue
    age_days = (now - mtime) / 86400.0
    cand_comps = canon_components(cand)

    live_hit = None
    for ic in inuse:
        if is_under_or_equal(ic, cand_comps):
            live_hit = "/".join(ic)
            break

    size = du_sb(cand)
    if live_hit is not None:
        print("[SKIP-live]   %13d  age=%5.1fd  %s  (in use: %s)"
              % (size, age_days, cand, live_hit))
        continue
    if mtime > age_cutoff:
        print("[SKIP-young]  %13d  age=%5.1fd  %s" % (size, age_days, cand))
        continue

    if DRY_RUN:
        print("[REAP-dry]    %13d  age=%5.1fd  %s" % (size, age_days, cand))
        reaped_total += size
        reaped_count += 1
        continue

    # Unlink-only reclamation: shutil.rmtree = os.unlink/os.rmdir only. We never
    # open, truncate, or redirect into any candidate path.
    try:
        shutil.rmtree(cand)
        print("[REAP]        %13d  age=%5.1fd  %s" % (size, age_days, cand))
        reaped_total += size
        reaped_count += 1
    except OSError as e:
        print("[REAP-ERROR]  %13d  age=%5.1fd  %s  (%s)" % (size, age_days, cand, e))
        errors += 1

try:
    st = os.statvfs(BASE)
    free_after = st.f_bavail * st.f_frsize
except OSError:
    free_after = -1

print("free before: %d bytes; free after: %d bytes" % (free_before, free_after))
print("RECLAIMED: %d bytes across %d dir(s)%s"
      % (reaped_total, reaped_count, " (dry-run: nothing deleted)" if DRY_RUN else ""))

sys.exit(1 if errors else 0)
PYEOF
}

if [ -n "${REAP_LOG:-}" ]; then
  # tee to the log (append) and to stdout. pipefail is set, so the pipeline's
  # exit status reflects a python failure over tee's success.
  run_core | tee -a "$REAP_LOG"
  exit "${PIPESTATUS[0]}"
else
  run_core
fi
