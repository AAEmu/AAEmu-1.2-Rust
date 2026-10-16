#!/usr/bin/env bash
# Local live stack for agents and developers: private MySQL + Rust login/game servers + the real
# ArcheAge 1.2 client started with aaemu-launcher. See AGENT-QUICKSTART.md.
#
#   Scripts/live-stack.sh build      # cargo build --profile live --bins (AAEMU_PROFILE=release for a full one)
#   Scripts/live-stack.sh sync       # copy app/ configs + data into the runtime dirs
#   Scripts/live-stack.sh db-reset   # drop + recreate the two throwaway databases
#   Scripts/live-stack.sh start      # MySQL, login, game; waits until the game registered
#   Scripts/live-stack.sh client     # launch the 1.2 client (ticket login, no typing)
#   Scripts/live-stack.sh status     # what is running + last log lines
#   Scripts/live-stack.sh shot       # screenshot to $LIVE/shots/now.png (view it with an image reader)
#   Scripts/live-stack.sh stop       # this stack's client + servers (--mysql also stops MySQL)
#
# Lane mode: run from a lane worktree (Scripts/lane.sh new <name>) or with AAEMU_LANE=<name> and
# the stack uses the lane's runtime dir, binaries, port block and databases, so several sessions
# can each run their own stack. Without a lane it is the shared D:/aaemu-live stack on 21xxx.
#
# Every path is an environment variable so another machine can override it. Secrets (DB and test
# account passwords) live only in $LIVE/cfg and the runtime Config.Local.json files, never in git.
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
LANES=${AAEMU_LANES:-D:/aaemu-lanes}
LANE=${AAEMU_LANE:-}
if [ -z "$LANE" ]; then
  for f in "$LANES"/*.env; do
    [ -f "$f" ] || continue
    wt=$(sed -n 's/^WT=//p' "$f"); [ -d "$wt" ] || continue
    [ "$(cd "$wt" && pwd -P)" = "$(cd "$REPO" && pwd -P)" ] && LANE=$(basename "$f" .env)
  done
fi
if [ -n "$LANE" ]; then
  [ -f "$LANES/$LANE.env" ] || { echo "no lane '$LANE' (Scripts/lane.sh list)" >&2; exit 1; }
  # shellcheck disable=SC1090
  source "$LANES/$LANE.env"   # LIVE TARGET PORT_PREFIX DB_GAME DB_LOGIN ...
  : "${AAEMU_LIVE:=$LIVE}" "${AAEMU_BIN:=$TARGET/${AAEMU_PROFILE:-live}}"
  : "${AAEMU_LOGIN:=127.0.0.1:${PORT_PREFIX}237}"
fi
TEMPLATE=${AAEMU_LIVE_TEMPLATE:-D:/aaemu-live}              # shared cfg/ (passwords), never printed
LIVE=${AAEMU_LIVE:-D:/aaemu-live}                         # runtime dirs, logs, MySQL data
PROFILE=${AAEMU_PROFILE:-live}                            # cargo profile: live (fast build) or release
BIN=${AAEMU_BIN:-D:/aaemu-target/main/$PROFILE}            # cargo target dir (see rust/.cargo/config.toml)
MYSQL_BIN=${AAEMU_MYSQL_BIN:-/c/Program Files/MySQL/MySQL Server 8.0/bin}
MYSQL_PORT=${AAEMU_MYSQL_PORT:-3317}                      # private instance; NEVER the user's 3306
CLIENT=${AAEMU_CLIENT:-D:/AA1.2/bin32/archeage.exe}       # read-only client install
LOGIN=${AAEMU_LOGIN:-127.0.0.1:21237}
CLIENT_USER=${AAEMU_USER:-clienttest}
PWFILE=${AAEMU_PASSWORD_FILE:-$TEMPLATE/cfg/client-test-password.txt}
DRIVER_DLL=${AAEMU_DRIVER_DLL:-$(dirname "$BIN")/aadriver/aadriver.dll}
CLIENT_PAK=${AAEMU_CLIENT_PAK:-D:/AA1.2/game_pak}
COMPACT=${AAEMU_COMPACT_DB:-$TEMPLATE/rs-game/Data/compact.sqlite3}   # shared static game data
DB_LOGIN=${DB_LOGIN:-rs_login}
DB_GAME=${DB_GAME:-rs_game}
# Servers run from a copy in $LIVE/bin in lane mode, so the lane can rebuild while its stack runs.
RUN=$BIN; [ -n "$LANE" ] && RUN=$LIVE/bin
LOGIN_PORT=${LOGIN##*:}

MYSQL=("$MYSQL_BIN/mysql.exe" -h127.0.0.1 -P"$MYSQL_PORT" -uroot)
winpath() { cygpath -w "$1" 2>/dev/null || echo "$1"; }
# Only this stack's processes: servers by executable path, clients by their -auth_port. Other
# sessions' stacks on the same machine are never touched.
pids_of() {
  case "$1" in
    archeage.exe) powershell -NoProfile -Command "Get-CimInstance Win32_Process -Filter \"Name='archeage.exe'\" | Where-Object { \$_.CommandLine -match '-auth_port $LOGIN_PORT( |\$)' } | ForEach-Object { \$_.ProcessId }" ;;
    *) powershell -NoProfile -Command "Get-Process -ErrorAction SilentlyContinue | Where-Object { \$_.Path -eq '$(winpath "$RUN/$1")' } | ForEach-Object { \$_.Id }" ;;
  esac 2>/dev/null | tr -d '\r'
}
running() { [ -n "$(pids_of "$1")" ]; }
mysql_up() { "${MYSQL[@]}" -N -e "select 1" >/dev/null 2>&1; }

cmd_build() { (cd "$REPO/rust" && cargo build --profile "$PROFILE" --bins); }

cmd_sync() {
  local app="$REPO/rust/crates/aaemu-game/app"
  mkdir -p "$LIVE/rs-login" "$LIVE/rs-game/Data" "$LIVE/logs" "$LIVE/shots"
  cp "$REPO/rust/crates/aaemu-login/app/Config.json" "$LIVE/rs-login/"
  cp "$app/Config.json" "$LIVE/rs-game/"
  rm -rf "$LIVE/rs-game/Configurations" "$LIVE/rs-game/Scripts"
  cp -r "$app/Configurations" "$app/Scripts" "$LIVE/rs-game/"
  cp -r "$app/Data/." "$LIVE/rs-game/Data/"   # includes the git-ignored compact.sqlite3
  # a fresh lane worktree has no compact.sqlite3 (git-ignored): use the shared copy
  if [ ! -f "$LIVE/rs-game/Data/compact.sqlite3" ]; then
    if [ -f "$COMPACT" ]; then cp "$COMPACT" "$LIVE/rs-game/Data/"
    else echo "WARNING: no compact.sqlite3 (set AAEMU_COMPACT_DB)"; fi
  fi
  [ -n "$LANE" ] && lane_configs
  for f in "$LIVE/rs-login/Config.Local.json" "$LIVE/rs-game/Config.Local.json"; do
    [ -f "$f" ] || echo "WARNING: missing $f (ports, DB, ClientData) - see rust/LIVE-TESTING.md section 2"
  done
  echo "synced app/ into $LIVE/rs-login and $LIVE/rs-game"
}

# Lane mode: the Config.Local.json pair for the lane's ports and databases (written once).
lane_configs() {
  local pw p=$PORT_PREFIX; pw=$(tr -d '\r\n' < "$TEMPLATE/cfg/db_password.txt")
  [ -f "$LIVE/rs-login/Config.Local.json" ] || cat > "$LIVE/rs-login/Config.Local.json" <<EOF
{
  "SecretKey": "lane-$LANE",
  "AutoAccount": true,
  "SkipHostResolve": true,
  "InternalNetwork": { "Host": "127.0.0.1", "Port": ${p}234 },
  "Network": { "Host": "127.0.0.1", "Port": ${p}237, "NumConnections": 100 },
  "Connections": { "AutoApplyUpdates": true, "MySQLProvider": { "Database": "$DB_LOGIN", "Host": "127.0.0.1", "Port": "$MYSQL_PORT", "User": "aaemu", "Password": "$pw" } },
  "GameServers": [ { "Id": 1, "Name": "Lane-$LANE", "Host": "127.0.0.1", "Port": ${p}239, "Hidden": false } ]
}
EOF
  [ -f "$LIVE/rs-game/Config.Local.json" ] || cat > "$LIVE/rs-game/Config.Local.json" <<EOF
{
  "Id": 1,
  "SecretKey": "lane-$LANE",
  "Network": { "Host": "127.0.0.1", "Port": ${p}239, "NumConnections": 100 },
  "StreamNetwork": { "Host": "127.0.0.1", "Port": ${p}250 },
  "WebApiNetwork": { "Host": "127.0.0.1", "Port": ${p}280 },
  "LoginNetwork": { "Host": "127.0.0.1", "Port": "${p}234" },
  "Connections": { "AutoApplyUpdates": true, "MySQLProvider": { "Database": "$DB_GAME", "Host": "127.0.0.1", "Port": "$MYSQL_PORT", "User": "aaemu", "Password": "$pw" } },
  "ClientData": { "Sources": [ "$CLIENT_PAK" ] }
}
EOF
}

cmd_mysql() {
  if mysql_up; then return; fi
  ("$MYSQL_BIN/mysqld.exe" --defaults-file="$LIVE/mysql/my.ini" >/dev/null 2>&1 &)
  for _ in $(seq 1 60); do mysql_up && return; sleep 1; done
  echo "MySQL did not start; see $LIVE/logs/mysqld.err" >&2; exit 1
}

cmd_db_reset() {
  cmd_mysql
  if running aaemu-game.exe || running aaemu-login.exe; then
    echo "stop the servers first (Scripts/live-stack.sh stop)" >&2; exit 1
  fi
  for k in login game; do
    local db=$DB_LOGIN; [ $k = game ] && db=$DB_GAME
    "${MYSQL[@]}" -e "DROP DATABASE IF EXISTS \`$db\`; CREATE DATABASE \`$db\` CHARACTER SET utf8mb4"
    # first two lines select the aaemu_* schema; the sed is for MySQL < 8.0.29
    tail -n +3 "$REPO/SQL/aaemu_$k.sql" | sed 's/CREATE TRIGGER IF NOT EXISTS/CREATE TRIGGER/' \
      | "${MYSQL[@]}" "$db"
    echo "recreated $db"
  done
}

# Keeps the previous run's console log as <name>.prev.log (a crashed run's tail is in there).
keep_prev() { [ -s "$1" ] && mv -f "$1" "${1%.log}.prev.log" || true; }

cmd_start() {
  cmd_mysql
  if [ -n "$LANE" ]; then
    # a lane's databases are created on first start
    "${MYSQL[@]}" -N -e "SHOW DATABASES LIKE '$DB_GAME'" | grep -q . || cmd_db_reset
    if ! running aaemu-game.exe && ! running aaemu-login.exe; then
      mkdir -p "$RUN"; cp -f "$BIN"/aaemu-login.exe "$BIN"/aaemu-game.exe "$BIN"/aaemu-launcher.exe "$RUN/"
    fi
  fi
  if ! running aaemu-login.exe; then
    keep_prev "$LIVE/logs/rs-login.log"
    (cd "$LIVE/rs-login" && NO_COLOR=1 RUST_LOG=${RUST_LOG:-info} "$RUN/aaemu-login.exe" \
      > "$LIVE/logs/rs-login.log" 2>&1 &)
    sleep 3
  fi
  if ! running aaemu-game.exe; then
    keep_prev "$LIVE/logs/rs-game.log"
    (cd "$LIVE/rs-game" && NO_COLOR=1 RUST_LOG=${RUST_LOG:-info} "$RUN/aaemu-game.exe" "$LIVE/rs-game" \
      > "$LIVE/logs/rs-game.log" 2>&1 &)
  fi
  echo "waiting for the game server (data + physics load takes ~1 min)..."
  for _ in $(seq 1 180); do
    if grep -q "Successfully registered on LoginServer" "$LIVE/logs/rs-game.log" 2>/dev/null; then
      echo "stack ready: login $LOGIN, game registered"; return
    fi
    running aaemu-game.exe || { echo "game server exited; tail $LIVE/logs/rs-game.log" >&2; exit 1; }
    sleep 2
  done
  echo "timed out waiting for registration; check $LIVE/logs/rs-game.log" >&2; exit 1
}

cmd_client() {
  running archeage.exe && { echo "client already running"; return; }
  local driver_args=()
  [ -n "$LANE" ] && driver_args+=(--multi-client)   # other sessions may have a client open
  if [ -f "$DRIVER_DLL" ]; then
    driver_args+=(--driver-dll "$(cygpath -w "$DRIVER_DLL" 2>/dev/null || echo "$DRIVER_DLL")")
  fi
  ("$RUN/aaemu-launcher.exe" --client "$(cygpath -w "$CLIENT" 2>/dev/null || echo "$CLIENT")" \
     --login "$LOGIN" --user "$CLIENT_USER" --password-file "$PWFILE" \
     "${driver_args[@]}" \
     > "$LIVE/logs/launcher.log" 2>&1 &)
  sleep 8; cat "$LIVE/logs/launcher.log"
  echo "login appears in rs-login.log as '$CLIENT_USER connected.' after client loading"
}

cmd_status() {
  # (the machine may run other mysqld services, so probe the private port instead of the process)
  [ -n "$LANE" ] && echo "lane  $LANE (ports ${PORT_PREFIX}xxx, $DB_LOGIN/$DB_GAME, $LIVE)"
  if mysql_up; then echo "UP    mysql :$MYSQL_PORT"; else echo "down  mysql :$MYSQL_PORT"; fi
  for p in aaemu-login.exe aaemu-game.exe archeage.exe; do
    if running "$p"; then echo "UP    $p"; else echo "down  $p"; fi
  done
  for f in rs-login rs-game; do echo "--- $f.log"; tail -3 "$LIVE/logs/$f.log" 2>/dev/null | cut -c1-160; done
  for d in rs-login rs-game; do
    [ -s "$LIVE/$d/Logs/Crash.log" ] && echo "!!! crash reports: $LIVE/$d/Logs/Crash.log"
  done
  true
}

cmd_shot() {
  powershell -NoProfile -Command "Add-Type -AssemblyName System.Windows.Forms,System.Drawing; \
\$b=[System.Windows.Forms.Screen]::PrimaryScreen.Bounds; \$bmp=New-Object System.Drawing.Bitmap \$b.Width,\$b.Height; \
[System.Drawing.Graphics]::FromImage(\$bmp).CopyFromScreen(\$b.Location,[System.Drawing.Point]::Empty,\$b.Size); \
(New-Object System.Drawing.Bitmap \$bmp,(\$b.Width/2),(\$b.Height/2)).Save('$(cygpath -w "$LIVE/shots/now.png" 2>/dev/null || echo "$LIVE/shots/now.png")')"
  echo "$LIVE/shots/now.png"
}

cmd_stop() {
  # Only this stack's client and servers; the private MySQL is shared by every lane, so it is
  # only shut down on request.
  for p in archeage.exe aaemu-game.exe aaemu-login.exe; do
    for id in $(pids_of "$p"); do taskkill //F //PID "$id" >/dev/null 2>&1 || true; done
  done
  if [ "${1:-}" = --mysql ] && mysql_up; then "${MYSQL[@]}" -e "SHUTDOWN"; fi
  echo "stopped"
}

case "${1:-}" in
  build) cmd_build ;; sync) cmd_sync ;; db-reset) cmd_db_reset ;; start) cmd_start ;;
  client) cmd_client ;; status) cmd_status ;; shot) cmd_shot ;; stop) cmd_stop "${2:-}" ;;
  *) sed -n '2,18p' "$0"; exit 1 ;;
esac
