#!/usr/bin/env bash
set -euo pipefail
tmp=$(mktemp -d "${TMPDIR:-/tmp}/squad-optimize.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
printf 'zero\n' > "$tmp/harness.md"
printf 'tasks:\n  - id: test\n    cmd: '\''grep -q improved "$SQUAD_EVAL_HARNESS"'\''\n' > "$tmp/suite.yaml"
cargo build --quiet --bin squad
output=$(target/debug/squad eval optimize --suite "$tmp/suite.yaml" --harness "$tmp/harness.md" --proposer-cmd 'echo improved > "$SQUAD_OPT_HARNESS_OUT"' --out-dir "$tmp/out" --json)
[[ "$output" == *'"winner_label": "r1-cand1"'* ]]
[[ -s "$tmp/out/winner.diff" ]]
