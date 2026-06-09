//! Runtime-assert test that jemalloc actually PARSED henyey's compiled-in
//! `malloc_conf`, not merely that the config bytes are present in the binary.
//!
//! # Why this lives in `tests/` (an integration test), not a unit test
//!
//! The config is exported from `crates/henyey/src/main.rs` via
//! `#[export_name = "_rjem_malloc_conf"]` and is only read by jemalloc when
//! henyey's `Jemalloc` global allocator is linked. A `henyey-ledger` (or any
//! library-crate) unit test does NOT pull in the binary's `#[export_name]`
//! static, so it would observe an *unconfigured* jemalloc and give a false
//! pass/fail. This integration test compiles into a test binary that links
//! henyey's allocator config, so `mallctl` reads the real, applied options.
//!
//! # What it proves
//!
//! It reads jemalloc's *effective* options via `mallctl` (`opt.retain`,
//! `opt.dirty_decay_ms`, `opt.muzzy_decay_ms`, `opt.background_thread`) and
//! asserts they match henyey's `malloc_conf`. On the buggy `#[export_name =
//! "malloc_conf"]` (the unprefixed symbol the prefixed tikv-jemalloc build
//! never reads), jemalloc runs DEFAULTS — `retain=true`, decay=10000ms,
//! `background_thread=false` — and these asserts FAIL. After the symbol fix
//! (`_rjem_malloc_conf`), jemalloc parses the config and the asserts PASS.
//! This is the acceptance gate for #3237.

// The integration test binary does not itself install henyey's `Jemalloc`
// global allocator (that lives in `main.rs`, which is the binary, not a lib).
// We therefore link the same allocator here so the test process runs under the
// configured jemalloc and `mallctl` observes the applied options. Without this
// the test would run under the system allocator and `mallctl` would not be the
// configured jemalloc.
#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// Re-export the same compiled-in config the binary uses. The byte string and
// the `_rjem_malloc_conf` export name MUST stay in sync with
// `crates/henyey/src/main.rs`. Because this is the prefixed symbol that
// jemalloc actually reads, the strong definition here overrides jemalloc's
// weak `_rjem_malloc_conf` default in this test binary exactly as it does in
// the real binary.
#[cfg(feature = "jemalloc")]
#[allow(non_upper_case_globals)]
#[export_name = "_rjem_malloc_conf"]
pub static malloc_conf: &[u8] =
    b"background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:1000,retain:false\0";

/// Reads jemalloc's effective options via `mallctl` and asserts they match the
/// values in henyey's `malloc_conf`. This is the real proof the config is
/// applied — see the module docs.
///
/// FAILS on the unprefixed `malloc_conf` export (jemalloc reports defaults);
/// PASSES once the export uses the prefixed `_rjem_malloc_conf` symbol.
#[cfg(feature = "jemalloc")]
#[test]
fn test_jemalloc_config_is_applied_at_runtime() {
    use tikv_jemalloc_ctl::{opt, raw};

    // Force at least one allocation through jemalloc so the allocator is
    // definitely initialized before we query its options.
    let _warm = vec![0u8; 4096];

    // `opt.retain`: default `true`; henyey sets `retain:false`.
    let retain: bool = unsafe { raw::read(b"opt.retain\0") }.expect("read opt.retain via mallctl");
    assert!(
        !retain,
        "jemalloc opt.retain must be false (henyey sets retain:false). \
         Got retain={retain}. If true, jemalloc is running DEFAULTS — the \
         malloc_conf export is NOT being read (wrong symbol name?)."
    );

    // `opt.dirty_decay_ms`: default 10000; henyey sets 1000.
    // jemalloc types this as `ssize_t`, which is byte-identical to Rust `isize`
    // on every platform we build for; `raw::read::<T>` is a sized memcpy, so
    // reading it as `isize` avoids pulling in a `libc` dev-dependency.
    let dirty_decay: isize =
        unsafe { raw::read(b"opt.dirty_decay_ms\0") }.expect("read opt.dirty_decay_ms via mallctl");
    assert_eq!(
        dirty_decay, 1000,
        "jemalloc opt.dirty_decay_ms must be 1000 (henyey sets dirty_decay_ms:1000). \
         Got {dirty_decay} (default is 10000 — config not applied)."
    );

    // `opt.muzzy_decay_ms`: default 10000; henyey sets 1000.
    let muzzy_decay: isize =
        unsafe { raw::read(b"opt.muzzy_decay_ms\0") }.expect("read opt.muzzy_decay_ms via mallctl");
    assert_eq!(
        muzzy_decay, 1000,
        "jemalloc opt.muzzy_decay_ms must be 1000 (henyey sets muzzy_decay_ms:1000). \
         Got {muzzy_decay} (default is 10000 — config not applied)."
    );

    // `opt.background_thread`: default false; henyey sets true.
    let bg: bool = opt::background_thread::read().expect("read opt.background_thread");
    assert!(
        bg,
        "jemalloc opt.background_thread must be true (henyey sets background_thread:true). \
         Got {bg} (default is false — config not applied)."
    );
}
