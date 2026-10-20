#!/usr/bin/env bash
# Optional tooling, not part of CI: live verification suites for one lane. It builds + starts the
# lane stack, provisions the bot accounts the suites declare, runs every rust/verify/*.vtest and
# writes a report.
#
#   Scripts/verify.sh                         # all suites
#   Scripts/verify.sh --only guild            # suites whose name contains "guild"
#   Scripts/verify.sh --report D:/tmp/report  # report directory
#
# Run from inside a lane worktree (Scripts/lane.sh new <name>); the stack, databases, ports and
# runtime directory are the lane's own. Never run outside a lane. See Docs/wiki/Verification.md.
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
LANES=${AAEMU_LANES:-D:/aaemu-lanes}
# Machine-local overrides (AAEMU_MYSQL_BIN, ...), never committed.
# shellcheck disable=SC1091
[ -f "$LANES/machine.conf" ] && source "$LANES/machine.conf"

GIT_DIR_ABS=$(git -C "$REPO" rev-parse --absolute-git-dir 2>/dev/null || true)
LANE=""
case "$GIT_DIR_ABS" in */worktrees/*) LANE=${GIT_DIR_ABS##*/} ;; esac
if [ -z "$LANE" ] || [ ! -f "$LANES/$LANE.env" ]; then
  echo "verify: run from inside a lane worktree (Scripts/lane.sh new <name>)" >&2
  exit 1
fi
# shellcheck disable=SC1090
source "$LANES/$LANE.env"   # LIVE TARGET PORT_PREFIX DB_GAME DB_LOGIN ...

MYSQL_BIN=${AAEMU_MYSQL_BIN:-/c/Program Files/MySQL/MySQL Server 8.0/bin}
MYSQL_PORT=${AAEMU_MYSQL_PORT:-3317}        # the private instance; NEVER 3306
MYSQL=("$MYSQL_BIN/mysql.exe" -h127.0.0.1 -P"$MYSQL_PORT" -uroot)
PROFILE=${AAEMU_PROFILE:-live}
TC="$TARGET/$PROFILE/aaemu-testclient.exe"
LOGIN=${AAEMU_LOGIN:-127.0.0.1:${PORT_PREFIX}237}
GAME_LOG="$LIVE/logs/rs-game.log"
PASSWORD=${AAEMU_VERIFY_PASSWORD:-testpass}
ONLY=""
REPORT="$LIVE/verify-report"

usage() { sed -n '2,11p' "$0"; }
while [ $# -gt 0 ]; do
  case "$1" in
    --only) ONLY=${2:-}; shift ;;
    --only=*) ONLY=${1#*=} ;;
    --report) REPORT=${2:-}; shift ;;
    --report=*) REPORT=${1#*=} ;;
    -h|--help) usage; exit 0 ;;
    *) echo "verify: unknown option $1" >&2; usage; exit 2 ;;
  esac
  shift
done

shopt -s nullglob
all=("$REPO"/rust/verify/*.vtest)
if [ ${#all[@]} -eq 0 ]; then
  echo "verify: no suites in $REPO/rust/verify" >&2
  exit 1
fi
suites=()
for f in "${all[@]}"; do
  name=$(basename "$f" .vtest)
  if [ -n "$ONLY" ]; then
    case "$name" in *"$ONLY"*) ;; *) continue ;; esac
  fi
  suites+=("$f")
done
if [ ${#suites[@]} -eq 0 ]; then
  echo "verify: no suite matches --only '$ONLY'" >&2
  exit 1
fi

echo "== lane $LANE: ${#suites[@]} suite(s), stack $LIVE, login $LOGIN"
"$REPO/Scripts/live-stack.sh" build
(cd "$REPO/rust" && cargo build --profile "$PROFILE" -p aaemu-testclient)
[ -x "$TC" ] || { echo "verify: missing $TC" >&2; exit 1; }

"$REPO/Scripts/live-stack.sh" sync
# Minimal guild-creation requirements in the lane's RUNTIME config only (never in app/):
# the suite creates a two-member guild without gold or leveling.
STAGE="$LIVE/rs-game/Configurations/Expedition.json"
if [ -f "$STAGE" ]; then
  sed -i -E \
    -e 's/("Cost"[[:space:]]*:[[:space:]]*)[0-9]+/\1 0/' \
    -e 's/("Level"[[:space:]]*:[[:space:]]*)[0-9]+/\1 1/' \
    -e 's/("PartyMemberCount"[[:space:]]*:[[:space:]]*)[0-9]+/\1 2/' \
    "$STAGE"
  grep -Eq '"PartyMemberCount"[[:space:]]*:[[:space:]]*2' "$STAGE" || {
    echo "verify: could not patch $STAGE (format changed?)" >&2; exit 1; }
else
  echo "verify: missing $STAGE" >&2
  exit 1
fi

# The spoof check reads a debug line from the express-text handler; give the game log filter
# that module, then restart this lane's stack so the filter and the patched config apply.
export RUST_LOG="${RUST_LOG:-info},aaemu_game::systems::services::express_text=debug"
"$REPO/Scripts/live-stack.sh" stop

export AAEMU_VERIFY_GAME_LOG="$GAME_LOG"
MYSQL_EXE=$(cygpath -m "$MYSQL_BIN/mysql.exe" 2>/dev/null || echo "$MYSQL_BIN/mysql.exe")
export AAEMU_VERIFY_MYSQL="\"$MYSQL_EXE\" -h127.0.0.1 -P$MYSQL_PORT -uroot $DB_GAME"

# ---- accounts and stale guild state (before the game server loads them) ----
accounts=()
declare -A gm_of=()
for f in "${suites[@]}"; do
  while read -r acct gm; do
    [ -n "$acct" ] || continue
    accounts+=("$acct")
    gm_of["$acct"]=$gm
  done < <("$TC" verify --suite "$f" --list-bots)
done
mapfile -t accounts < <(printf '%s\n' "${accounts[@]}" | sort -u)

mysql_up() { "${MYSQL[@]}" -N -e "select 1" >/dev/null 2>&1; }
if ! mysql_up; then
  # The private MySQL is shared and normally up; on a fresh machine this brings it up (the
  # stack is restarted below so the game loads the cleaned state).
  echo "== starting the stack once to bring MySQL up"
  "$REPO/Scripts/live-stack.sh" start
  "$REPO/Scripts/live-stack.sh" stop
fi
if "${MYSQL[@]}" -N -e "SHOW DATABASES LIKE '$DB_GAME'" | grep -q .; then
  # A rerun starts from clean guild state for these accounts (the restart clears teams; this
  # clears the persisted guild rows). Touches only the suite's own characters.
  names=$(printf "'%s'," "${accounts[@]}")
  names=${names%,}
  "${MYSQL[@]}" "$DB_GAME" -e "
    DELETE FROM expedition_role_policies WHERE expedition_id IN
      (SELECT id FROM expeditions WHERE owner IN
         (SELECT id FROM characters WHERE name IN ($names)));
    DELETE FROM expeditions WHERE owner IN
      (SELECT id FROM characters WHERE name IN ($names));
    DELETE FROM expedition_members WHERE character_id IN
      (SELECT id FROM characters WHERE name IN ($names));
    UPDATE characters SET expedition_id=0 WHERE name IN ($names);"
fi

"$REPO/Scripts/live-stack.sh" start

# AutoAccount login creates each account; the GM flag is set in THIS lane's game database.
echo "== provisioning ${#accounts[@]} bot account(s)"
for acct in "${accounts[@]}"; do
  "$TC" run --scenario login --login "$LOGIN" --user "$acct" --password "$PASSWORD" \
    >/dev/null 2>&1 || { echo "verify: AutoAccount login failed for $acct" >&2; exit 1; }
  id=$("${MYSQL[@]}" -N -e "SELECT id FROM \`$DB_LOGIN\`.users WHERE username='$acct'")
  [ -n "$id" ] || { echo "verify: no login account $acct" >&2; exit 1; }
  "${MYSQL[@]}" -e "INSERT INTO \`$DB_GAME\`.accounts (account_id) VALUES ($id) \
    ON DUPLICATE KEY UPDATE account_id = VALUES(account_id);"
  lvl=0
  [ "${gm_of[$acct]:-0}" = 1 ] && lvl=100
  "${MYSQL[@]}" -e "UPDATE \`$DB_GAME\`.accounts SET access_level=$lvl WHERE account_id=$id;"
done

# ---- run the suites ----
mkdir -p "$REPORT"
stamp=$(date '+%Y-%m-%d %H:%M:%S')
fail=0
json_suites=""
md_rows=""
md_details=""

json_escape() {
  local s=$1
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  s=${s//$'\r'/}
  s=${s//$'\n'/\\n}
  s=${s//$'\t'/\\t}
  printf '%s' "$s"
}

for f in "${suites[@]}"; do
  name=$(basename "$f" .vtest)
  logfile="$REPORT/$name.log"
  offset=0
  [ -f "$GAME_LOG" ] && offset=$(wc -c <"$GAME_LOG")
  start=$(date +%s%3N)
  set +e
  "$TC" verify --suite "$f" --login "$LOGIN" --password "$PASSWORD" 2>&1 | tee "$logfile"
  rc=${PIPESTATUS[0]}
  set -e
  end=$(date +%s%3N)
  ms=$((end - start))
  result=PASS
  if [ $rc -ne 0 ]; then
    result=FAIL
    fail=1
  fi
  failing=$(grep -m1 -E '^FAIL |expected .* got|expected packet|unexpected packet|no new line matching' "$logfile" || true)
  [ -n "$failing" ] || failing=$(grep -m1 'FAIL' "$logfile" || true)

  log_lines=""
  if [ -f "$GAME_LOG" ]; then
    log_lines=$(tail -c +$((offset + 1)) "$GAME_LOG" 2>/dev/null \
      | grep -E 'WARN|ERROR|ExpressEmotion|ReplyExpeditionInvitation' || true)
    log_lines=$(printf '%s\n' "$log_lines" | tail -n 40)
  fi

  json_lines=""
  if [ -n "$log_lines" ]; then
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      [ -n "$json_lines" ] && json_lines="$json_lines,"
      json_lines="$json_lines\"$(json_escape "$line")\""
    done <<<"$log_lines"
  fi

  [ -n "$json_suites" ] && json_suites="$json_suites,"
  json_suites="$json_suites{\"suite\":\"$(json_escape "$name")\",\"passed\":$([ $rc -eq 0 ] && echo true || echo false),\"duration_ms\":$ms,\"failing_line\":\"$(json_escape "$failing")\",\"log_lines\":[$json_lines]}"

  md_rows="$md_rows| $name | $result | $(awk "BEGIN{printf \"%.1f\", $ms/1000}") s | $([ -n "$failing" ] && echo "\`$(printf '%s' "$failing" | sed 's/|/\\|/g')\`" || echo "") |"$'\n'
  md_details="$md_details"$'\n'"### $name — $result ($(awk "BEGIN{printf \"%.1f\", $ms/1000}") s)"$'\n\n'
  [ -n "$failing" ] && md_details="$md_details"$'Failing line: `'"$(printf '%s' "$failing" | sed 's/`/\\`/g')"$'`\n\n'
  if [ -n "$log_lines" ]; then
    md_details="$md_details"$'Relevant game log lines:\n\n```\n'"$log_lines"$'\n```\n'
  fi
done

cat >"$REPORT/report.json" <<EOF
{
  "lane": "$(json_escape "$LANE")",
  "generated_at": "$(json_escape "$stamp")",
  "login": "$(json_escape "$LOGIN")",
  "game_log": "$(json_escape "$GAME_LOG")",
  "suites": [$json_suites]
}
EOF

cat >"$REPORT/report.md" <<EOF
# Live verification report

- Lane: $LANE
- Login: $LOGIN
- Game database: $DB_GAME
- Generated: $stamp
- Raw output: \`$REPORT/<suite>.log\`

| Suite | Result | Duration | Failing line |
| --- | --- | --- | --- |
${md_rows}
${md_details}
EOF

echo
echo "== report: $REPORT/report.md and report.json"
if [ $fail -ne 0 ]; then
  echo "== FAIL: at least one suite failed"
  exit 1
fi
echo "== PASS: all ${#suites[@]} suite(s) passed"
