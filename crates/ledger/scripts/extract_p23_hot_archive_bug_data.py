#!/usr/bin/env python3
# Copyright 2026 Stellar Development Foundation and contributors. Licensed
# under the Apache License, Version 2.0.
#
# Extraction/verification script for the CAP-0076 P23 hot-archive bug
# remediation data table (issue #3061).
#
# Reads the two 478-entry base64-XDR string arrays from stellar-core's
#   stellar-core/src/ledger/P23HotArchiveBugData.cpp
# and emits the generated Rust module
#   crates/ledger/src/p23_hot_archive_bug_data.rs
# transcribing the literals *verbatim* (we transcribe, we do not author them).
#
# Provenance: stellar-core v26.0.1, commit e78c97ed0.
#
# Usage (run from repo root):
#   python3 crates/ledger/scripts/extract_p23_hot_archive_bug_data.py
#
# This is a committed, re-runnable extractor so the ~673 KB base64 blob is
# auditable and re-derivable on the next stellar-core submodule pin bump.

import hashlib
import os
import re
import sys

EXPECTED_COUNT = 478

# The two arrays we need. The third array in the source file
# (P23_CORRUPTED_AFFECTED_ASSETS) drives the SAC-event reconciler, which is out
# of scope for the bucketListHash remediation (see issue #3061 plan).
ARRAYS = [
    ("P23_CORRUPTED_HOT_ARCHIVE_ENTRIES", "P23_CORRUPTED_HOT_ARCHIVE_ENTRIES"),
    (
        "P23_CORRUPTED_HOT_ARCHIVE_ENTRY_CORRECT_STATE",
        "P23_CORRUPTED_HOT_ARCHIVE_ENTRY_CORRECT_STATE",
    ),
]


def repo_root():
    here = os.path.dirname(os.path.abspath(__file__))
    # crates/ledger/scripts -> repo root
    return os.path.abspath(os.path.join(here, "..", "..", ".."))


def parse_cpp_string_literals(body):
    """Parse a C++ initializer list of (concatenated) string literals.

    The C++ source writes each entry as one or more adjacent double-quoted
    string literals (the compiler concatenates them). Entries are separated by
    top-level commas. Returns a list of decoded entry strings (escapes
    resolved). Only the literals matter — whitespace and line breaks between
    them are insignificant.
    """
    entries = []
    current = []  # accumulates the current entry's literal chunks
    i = 0
    n = len(body)
    in_string = False
    saw_string_for_entry = False
    cur_chunk = []
    while i < n:
        c = body[i]
        if in_string:
            if c == "\\":
                # Handle the escapes that actually appear in this data: \" and
                # the standard set. We resolve them to their literal char.
                nxt = body[i + 1] if i + 1 < n else ""
                mapping = {
                    '"': '"',
                    "\\": "\\",
                    "n": "\n",
                    "t": "\t",
                    "r": "\r",
                    "/": "/",
                }
                if nxt in mapping:
                    cur_chunk.append(mapping[nxt])
                    i += 2
                    continue
                # Unknown escape — keep verbatim (shouldn't happen for base64).
                cur_chunk.append(nxt)
                i += 2
                continue
            if c == '"':
                in_string = False
                current.append("".join(cur_chunk))
                cur_chunk = []
                i += 1
                continue
            cur_chunk.append(c)
            i += 1
            continue
        # not in string
        if c == '"':
            in_string = True
            saw_string_for_entry = True
            i += 1
            continue
        if c == ",":
            if saw_string_for_entry:
                entries.append("".join(current))
                current = []
                saw_string_for_entry = False
            i += 1
            continue
        # whitespace / other — ignore
        i += 1
    if saw_string_for_entry:
        entries.append("".join(current))
    return entries


def extract_array(src, decl_name):
    """Extract the initializer list body for `<type> <decl_name> = { ... };`."""
    # Find the `<decl_name> = {` then capture until the matching `};`.
    m = re.search(re.escape(decl_name) + r"\s*=\s*\{", src)
    if not m:
        raise SystemExit(f"Could not find array declaration: {decl_name}")
    start = m.end()
    # Find closing `}` that ends the initializer (entries contain no braces).
    end = src.find("}", start)
    if end == -1:
        raise SystemExit(f"Could not find closing brace for: {decl_name}")
    body = src[start:end]
    return parse_cpp_string_literals(body)


def main():
    root = repo_root()
    # Allow overriding the stellar-core source path (e.g. when running from a
    # worktree where the submodule is not checked out). Defaults to the pinned
    # submodule under the repo root.
    src_path = os.environ.get(
        "P23_SRC",
        os.path.join(root, "stellar-core", "src", "ledger", "P23HotArchiveBugData.cpp"),
    )
    out_path = os.path.join(
        root, "crates", "ledger", "src", "p23_hot_archive_bug_data.rs"
    )
    with open(src_path, "r", encoding="utf-8") as f:
        src = f.read()

    src_sha = hashlib.sha256(src.encode("utf-8")).hexdigest()

    extracted = {}
    for decl_name, _ in ARRAYS:
        entries = extract_array(src, decl_name)
        if len(entries) != EXPECTED_COUNT:
            raise SystemExit(
                f"{decl_name}: expected {EXPECTED_COUNT} entries, got {len(entries)}"
            )
        extracted[decl_name] = entries

    lines = []
    lines.append(
        "// GENERATED FILE — DO NOT EDIT BY HAND.\n"
        "//\n"
        "// CAP-0076 / Protocol 23 hot-archive bug remediation data table\n"
        "// (478 corrupted/correct base64-XDR `LedgerEntry` pairs), transcribed\n"
        "// verbatim from stellar-core's `P23HotArchiveBugData.cpp`.\n"
        "//\n"
        "// Provenance: stellar-core v26.0.1, commit e78c97ed0.\n"
        f"// Source file SHA-256: {src_sha}\n"
        "//\n"
        "// Regenerate with:\n"
        "//   python3 crates/ledger/scripts/extract_p23_hot_archive_bug_data.py\n"
        "//\n"
        "// See issue #3061 and `p23_hot_archive_bug.rs`.\n"
    )
    lines.append(
        f"/// Number of hardcoded corrupted hot-archive entries ({EXPECTED_COUNT})."
    )
    lines.append(
        f"pub const P23_CORRUPTED_HOT_ARCHIVE_ENTRIES_COUNT: usize = {EXPECTED_COUNT};\n"
    )

    for decl_name, rust_name in ARRAYS:
        entries = extracted[decl_name]
        lines.append(
            f"/// Base64-XDR `LedgerEntry` literals: {rust_name}."
        )
        lines.append(
            f"pub static {rust_name}: [&str; P23_CORRUPTED_HOT_ARCHIVE_ENTRIES_COUNT] = ["
        )
        for e in entries:
            # Rust raw-free string; base64 contains only [A-Za-z0-9+/=], no
            # escaping required, but use a debug-safe quoting just in case.
            esc = e.replace("\\", "\\\\").replace('"', '\\"')
            lines.append(f'    "{esc}",')
        lines.append("];\n")

    with open(out_path, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")

    print(f"Wrote {out_path}")
    print(f"Source SHA-256: {src_sha}")
    for decl_name, _ in ARRAYS:
        print(f"  {decl_name}: {len(extracted[decl_name])} entries")


if __name__ == "__main__":
    main()
