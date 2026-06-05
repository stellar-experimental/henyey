#!/usr/bin/env bash
# Shared dedup-filing library. Source this file; do not execute directly.
#
# All operations are stateless — callers MUST hold their own flock during
# the entire load → prune → check/act → record/update/remove → write sequence.
#
# Usage:
#   source scripts/lib/dedup-filing.sh
#   DEDUP_DATA=$(dedup_load "$DEDUP_FILE")
#   DEDUP_DATA=$(dedup_prune "$DEDUP_DATA" "24h")
#   if entry=$(dedup_check "$DEDUP_DATA" "$key"); then echo "hit: $entry"; fi
#   DEDUP_DATA=$(dedup_record "$DEDUP_DATA" "$key" "issue_number=123")
#   dedup_write "$DEDUP_FILE" "$DEDUP_DATA"

# Cross-shell self-location. Under bash, ${BASH_SOURCE[0]} is this file's path.
# Under zsh, BASH_SOURCE is unset/empty, so the :- fallback yields ${(%):-%x}
# — a zsh prompt-expansion that expands to the path of the file being sourced.
# Bash never evaluates the literal zsh default (BASH_SOURCE[0] is always set
# when sourced under bash), so the construct is harmless there. This resolves
# to the script dir under BOTH shells from any caller cwd (#3137).
_dedup_src="${BASH_SOURCE[0]:-${(%):-%x}}"
_DEDUP_SCRIPT="$(cd "$(dirname "$_dedup_src")" && pwd)/dedup-filing.py"
unset _dedup_src

dedup_load()         { python3 "$_DEDUP_SCRIPT" load "$1"; }
dedup_prune()        { printf '%s' "$1" | python3 "$_DEDUP_SCRIPT" prune "$2"; }
dedup_check()        { printf '%s' "$1" | python3 "$_DEDUP_SCRIPT" check "$2"; }
dedup_record()       { printf '%s' "$1" | python3 "$_DEDUP_SCRIPT" record "$2" "${@:3}"; }
dedup_remove()       { printf '%s' "$1" | python3 "$_DEDUP_SCRIPT" remove "$2"; }
dedup_update_field() { printf '%s' "$1" | python3 "$_DEDUP_SCRIPT" update-field "$2" "$3" "$4"; }
dedup_write()        { printf '%s' "$2" | python3 "$_DEDUP_SCRIPT" write "$1"; }
