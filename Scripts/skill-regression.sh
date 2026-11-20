#!/usr/bin/env bash
# Run the focused skills tests and compare the real-data class audit with the reviewed outcomes.
set -euo pipefail

if [[ $# != 2 ]]; then
  printf 'Usage: bash Scripts/skill-regression.sh <compact.sqlite3> <report.tsv>\n' >&2
  exit 2
fi

absolute_path() {
  local parent
  parent=$(cd -- "$(dirname -- "$1")" && pwd -P) || return
  printf '%s/%s\n' "$parent" "$(basename -- "$1")"
}

compact=$(absolute_path "$1")
report=$(absolute_path "$2")
if [[ ! -f "$compact" || ! -r "$compact" ]]; then
  printf 'Cannot read the compact database at %s\n' "$compact" >&2
  exit 2
fi
if [[ -d "$report" || "$report" -ef "$compact" ]]; then
  printf 'Choose a separate file path for the report.\n' >&2
  exit 2
fi

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
baseline="$repo/rust/crates/aaemu-game/tests/fixtures/player_skill_outcomes.tsv"
if [[ "$report" -ef "$baseline" ]]; then
  printf 'Choose a separate path from the checked-in baseline.\n' >&2
  exit 2
fi

scratch=$(mktemp -d "$(dirname -- "$report")/.skill-regression.XXXXXX")
staged="$scratch/report.tsv"
trap 'rm -f -- "$staged"; rmdir -- "$scratch"' EXIT
if command -v cygpath >/dev/null 2>&1; then
  export AAEMU_COMPACT_DB="$(cygpath -m "$compact")"
  export AUDIT_SKILL_REPORT="$(cygpath -m "$staged")"
else
  export AAEMU_COMPACT_DB="$compact"
  export AUDIT_SKILL_REPORT="$staged"
fi
unset AUDIT_SKILLS

cd -- "$repo/rust"
cargo test --locked -p aaemu-game --lib 'skills::'
audit_status=0
cargo test --locked -p aaemu-game --lib player_skill_audit -- --ignored --nocapture || audit_status=$?
if [[ -s "$staged" ]]; then
  mv -f -- "$staged" "$report"
elif [[ "$audit_status" == 0 ]]; then
  printf 'The audit did not write a fresh report.\n' >&2
  exit 1
fi
if [[ "$audit_status" != 0 ]]; then
  exit "$audit_status"
fi
diff -u -L reviewed -L observed <(tr -d '\r' < "$baseline") <(
  awk 'BEGIN { FS = OFS = "\t" } NR == 1 || $4 == "class" || $6 != "not_cast" { print $1, $6, $7 }' "$report"
)
awk -F '\t' '
  NR > 1 { count[$6]++ }
  END {
    printf "%d skill rows: %d client-path, %d internal fallback, %d uncast in the sweep.\n",
      NR - 1, count["client"], count["internal_fallback"], count["not_cast"]
  }
' "$report"
printf 'Skill outcomes match the baseline. Full catalog written to %s\n' "$report"
