#!/usr/bin/env bash
# One lane per agent session: its own git worktree + branch, cargo target dir, live-stack runtime
# dir, MySQL databases and port block, so parallel sessions never share files, build locks,
# ports or processes. See AGENTS.md ("Sessions work in lanes") and AGENT-QUICKSTART.md.
#
#   Scripts/lane.sh new <name> [--type fix|feat|chore|test|docs|refactor|perf] [--base <branch>]
#   Scripts/lane.sh list                 # lanes with branch, state, ports, running processes
#   Scripts/lane.sh info [<name>]        # one lane's paths and ports (default: the lane of $PWD)
#   Scripts/lane.sh sync [<name>]        # merge the latest base branch into the lane branch
#   Scripts/lane.sh check [<name>]       # cargo xtask ci in the lane (records the checked commit)
#   Scripts/lane.sh land [<name>]        # merge a checked, up-to-date lane into the base, then `done`
#   Scripts/lane.sh done [<name>] [--force|--keep-branch]  # stop + delete everything the lane owns
#   Scripts/lane.sh gc [--apply] [--min-age-hours N]  # find (and delete) leftovers of old sessions
#
# Paths are environment variables so another machine can override them. Secrets are read from
# $AAEMU_LIVE_TEMPLATE/cfg and never printed.
set -euo pipefail

WT_ROOT=${AAEMU_WT_ROOT:-D:/aaemu-wt}               # lane worktrees
TARGET_ROOT=${AAEMU_TARGET_ROOT:-D:/aaemu-target}   # cargo target dirs
LIVE_PREFIX=${AAEMU_LIVE_PREFIX:-D:/aaemu-live-}    # lane runtime dirs: <prefix><name>
LANES=${AAEMU_LANES:-D:/aaemu-lanes}                # lane registry (<name>.env), machine-local
LIVE_TEMPLATE=${AAEMU_LIVE_TEMPLATE:-D:/aaemu-live} # shared cfg/ (DB password, client password)
BASE_DEFAULT=${AAEMU_LANE_BASE:-rust-rewrite}
JOBS=${AAEMU_LANE_JOBS:-4}                          # cargo jobs per lane (RAM, not CPU, is the limit)
MYSQL_BIN=${AAEMU_MYSQL_BIN:-/c/Program Files/MySQL/MySQL Server 8.0/bin}
MYSQL_PORT=${AAEMU_MYSQL_PORT:-3317}                # private instance; NEVER the user's 3306
PORT_PREFIXES=(24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39)  # 21-23 belong to older stacks

# Always act on the main checkout, never on the worktree this copy of the script lives in:
# `land` and `done` are normally run from inside a lane, and must resolve git from the
# repository that owns the common git dir even after that lane's worktree is gone.
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
GIT_COMMON_DIR=$(git -C "$SCRIPT_DIR" rev-parse --path-format=absolute --git-common-dir 2>/dev/null) || GIT_COMMON_DIR=
if [ -n "$GIT_COMMON_DIR" ]; then REPO=$(cd "$GIT_COMMON_DIR/.." && pwd)
else REPO=$(cd "$SCRIPT_DIR/.." && pwd); fi
MYSQL=("$MYSQL_BIN/mysql.exe" -h127.0.0.1 -P"$MYSQL_PORT" -uroot)

die() { echo "lane: $*" >&2; exit 1; }
winpath() { cygpath -w "$1" 2>/dev/null || echo "$1"; }
reg() { echo "$LANES/$1.env"; }
load() { [ -f "$(reg "$1")" ] || die "no lane '$1' (Scripts/lane.sh list)"; # shellcheck disable=SC1090
  source "$(reg "$1")"; }
setvar() { # setvar <lane> KEY VALUE: rewrite one KEY= line of the registry file
  local f; f=$(reg "$1"); grep -v "^$2=" "$f" > "$f.tmp" || true; echo "$2=$3" >> "$f.tmp"; mv "$f.tmp" "$f"; }
mysql_up() { "${MYSQL[@]}" -N -e "select 1" >/dev/null 2>&1; }

# mkdir is atomic in Git Bash as well as on Unix (flock is not shipped with Git for Windows).
# Hold this across allocation AND publication, and serialize destructive cleanup with it too.
# Never reclaim a lock by PID or age: PIDs are reused, and a killed shell may leave git running.
# An interrupted/stale lock fails closed. After verifying ALL creators and their children have
# stopped, inspect its files and any partial worktree/branch/target before removing it manually.
allocation_lock() {
  ALLOCATION_LOCK="$LANES/.allocation.lock"
  ALLOCATION_OWNER=""
  local timeout=${AAEMU_LANE_LOCK_TIMEOUT:-30} start=$SECONDS
  [[ "$timeout" =~ ^[0-9]{1,6}$ ]] || die "AAEMU_LANE_LOCK_TIMEOUT must be seconds (0-999999)"
  timeout=$((10#$timeout))
  mkdir -p "$LANES"
  trap 'allocation_unlock' EXIT
  trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM
  until mkdir "$ALLOCATION_LOCK" 2>/dev/null; do
    # The owner may release it between the failed mkdir and this loop; retry even if gone.
    if (( SECONDS - start >= timeout )); then
      die "allocation/cleanup locked ($ALLOCATION_LOCK); retry. If interrupted, verify all creators and children stopped and inspect partial resources before manual recovery; never remove a lock by PID or age"
    fi
    sleep 0.1
  done
  # A unique marker also prevents our EXIT trap from releasing a replacement owner's lock.
  # Interruption before the marker exists deliberately leaves a stale lock, never an unsafe unlock.
  ALLOCATION_OWNER=$(mktemp -d "$ALLOCATION_LOCK/owner.XXXXXXXXXX")
}
allocation_owned() { [ -n "${ALLOCATION_OWNER:-}" ] && [ -d "$ALLOCATION_OWNER" ]; }
allocation_unlock() {
  allocation_owned || return 0
  rm -f "$ALLOCATION_OWNER/lane.env"
  rmdir "$ALLOCATION_OWNER" && rmdir "$ALLOCATION_LOCK"
}

# The lane whose worktree contains $PWD (or the script), unless a name was given.
current() {
  local n=${1:-}
  if [ -z "$n" ]; then
    local here; here=$(pwd -P)
    for f in "$LANES"/*.env; do
      [ -f "$f" ] || continue
      local wt; wt=$(grep '^WT=' "$f" | cut -d= -f2-)
      [ -d "$wt" ] || continue
      case "$(cd "$here" && pwd -P)/" in "$(cd "$wt" 2>/dev/null && pwd -P)/"*) n=$(basename "$f" .env) ;; esac
    done
  fi
  [ -n "$n" ] || die "not inside a lane worktree; pass the lane name"
  echo "$n"
}

# Processes (Id Name) whose executable lives under a directory; argument is a POSIX path.
procs_under() {
  powershell -NoProfile -Command "Get-Process | Where-Object { \$_.Path -and \$_.Path.StartsWith('$(winpath "$1")', 'OrdinalIgnoreCase') } | ForEach-Object { '{0} {1}' -f \$_.Id, \$_.ProcessName }" 2>/dev/null | tr -d '\r'
}
# Clients started for a login port (the launcher passes -auth_port to archeage.exe).
clients_on() {
  powershell -NoProfile -Command "Get-CimInstance Win32_Process -Filter \"Name='archeage.exe'\" | Where-Object { \$_.CommandLine -match '-auth_port $1( |$)' } | ForEach-Object { '{0} archeage' -f \$_.ProcessId }" 2>/dev/null | tr -d '\r'
}
port_free() { ! netstat -ano 2>/dev/null | grep LISTENING | grep -q ":$1 "; }

cmd_new() (
  local name="" type=fix base=$BASE_DEFAULT
  while [ $# -gt 0 ]; do
    case "$1" in
      --type) type=$2; shift ;; --base) base=$2; shift ;;
      -*) die "unknown option $1" ;; *) name=$1 ;;
    esac; shift
  done
  [[ "$name" =~ ^[a-z0-9][a-z0-9-]{1,30}$ ]] || die "name: 2-31 chars of a-z 0-9 - (e.g. quest-crash)"
  allocation_lock
  if [ -e "$(reg "$name")" ] || [ -L "$(reg "$name")" ]; then
    die "lane '$name' exists (Scripts/lane.sh info $name)"
  fi
  # Never share a target dir between worktrees: identical artifact names make cargo's mtime
  # fingerprints miss edits, so tests can silently run another worktree's code.
  local wt="$WT_ROOT/$name" branch="$type/$name" target="$TARGET_ROOT/$name"
  if [ -e "$wt" ] || [ -L "$wt" ]; then die "$wt already exists"; fi
  if [ -e "$target" ] || [ -L "$target" ]; then die "$target already exists (Scripts/lane.sh gc)"; fi
  git -C "$REPO" check-ref-format "refs/heads/$branch" || die "invalid branch '$branch'"
  if git -C "$REPO" show-ref --verify --quiet "refs/heads/$branch"; then die "branch '$branch' already exists"; fi
  local head; head=$(git -C "$REPO" rev-parse --verify "$base^{commit}")

  local prefix=""
  for p in "${PORT_PREFIXES[@]}"; do
    grep -qs "^PORT_PREFIX=$p$" "$LANES"/*.env && continue
    port_free "${p}237" && port_free "${p}239" && port_free "${p}234" || continue
    prefix=$p; break
  done
  [ -n "$prefix" ] || die "no free port block (24xxx-39xxx); finish old lanes first"

  # Until lane.env is published the lock reserves all four resources. Roll back only artifacts
  # we know we created, while still holding that lock. Ambiguous/partial Git failures retain it
  # for inspection rather than deleting a pre-existing branch or allowing prefix reuse.
  local target_started=0 target_created=0 worktree_started=0 worktree_created=0 published=0
  new_exit() {
    local status=$? clean=1
    trap - EXIT
    if ! allocation_owned; then
      echo "lane: allocation lock ownership lost; leaving resources untouched" >&2
      exit 1
    fi
    if [ "$published" = 0 ] && [ ! "$(reg "$name")" -ef "$ALLOCATION_OWNER/lane.env" ]; then
      if [ "$worktree_created" = 1 ]; then
        rm -f "$wt/rust/.cargo/config.toml" || clean=0
        # No --force: unexpected changes or removal errors require inspection.
        git -C "$REPO" worktree remove "$wt" || clean=0
        if [ "$clean" = 1 ]; then
          git -C "$REPO" update-ref -d "refs/heads/$branch" "$head" || clean=0
        fi
      elif [ "$worktree_started" = 1 ]; then
        if [ -e "$wt" ] || git -C "$REPO" show-ref --verify --quiet "refs/heads/$branch"; then clean=0; fi
      fi
      if [ "$target_created" = 1 ] && [ "$clean" = 1 ]; then rmdir "$target" || clean=0; fi
      if [ "$target_started" = 1 ] && [ "$target_created" = 0 ]; then
        if [ -e "$target" ] || [ -L "$target" ]; then clean=0; fi
      fi
    fi
    if [ "$clean" = 1 ]; then allocation_unlock
    else echo "lane: partial creation retained with $ALLOCATION_LOCK; inspect $wt, $branch and $target before manual recovery" >&2; fi
    exit "$status"
  }
  trap new_exit EXIT
  # Leave the reservation details inside the lock for inspection after a hard interruption.
  local db=${name//-/_}
  cat > "$ALLOCATION_OWNER/lane.env" <<EOF
LANE=$name
BRANCH=$branch
BASE=$base
WT=$wt
TARGET=$target
PORT_PREFIX=$prefix
LIVE=$LIVE_PREFIX$name
DB_GAME=lane_${db}_game
DB_LOGIN=lane_${db}_login
CREATED=$(date +%Y-%m-%dT%H:%M:%S)
CHECKED=
EOF
  mkdir -p "$WT_ROOT" "$TARGET_ROOT"
  target_started=1
  mkdir "$target"
  target_created=1
  worktree_started=1
  git -C "$REPO" worktree add -q "$wt" -b "$branch" "$base"
  worktree_created=1
  mkdir -p "$wt/rust/.cargo"
  cat > "$wt/rust/.cargo/config.toml" <<EOF
# Machine-local (gitignored), written by Scripts/lane.sh for lane '$name'.
[build]
jobs = $JOBS
target-dir = "$target"

[target.x86_64-pc-windows-msvc]
linker = "rust-lld"

# Line tables instead of full debug info: much faster links and a fraction of the disk.
# Panics and backtraces still name file:line.
[profile.dev]
debug = "line-tables-only"
[profile.test]
debug = "line-tables-only"
EOF
  # Atomic, no-clobber publication of the unchanged registry format. A link also lets the EXIT
  # trap recognize successful publication if a signal arrives before the next shell command.
  allocation_owned || die "allocation lock ownership lost; leaving resources untouched"
  ln -T "$ALLOCATION_OWNER/lane.env" "$(reg "$name")"
  published=1
  echo "lane '$name' ready"
  cmd_info "$name"
)

cmd_info() {
  local n; n=$(current "${1:-}"); load "$n"
  cat <<EOF
  worktree   $WT   (branch $BRANCH, base $BASE)
  target     $TARGET
  live stack $LIVE   (Scripts/live-stack.sh from inside the worktree picks this lane up)
  ports      login ${PORT_PREFIX}237 / internal ${PORT_PREFIX}234, game ${PORT_PREFIX}239, stream ${PORT_PREFIX}250, web ${PORT_PREFIX}280
  databases  $DB_LOGIN, $DB_GAME on 127.0.0.1:$MYSQL_PORT
  checked    ${CHECKED:-never}
EOF
}

cmd_list() {
  local any=0
  for f in "$LANES"/*.env; do
    [ -f "$f" ] || continue; any=1
    ( # shellcheck disable=SC1090
      source "$f"
      local state="gone"
      if [ -d "$WT" ]; then
        local dirty; dirty=$(git -C "$WT" status --porcelain 2>/dev/null | grep -vc 'rust/.cargo/' || true)
        state="dirty=$dirty"
        if git -C "$REPO" merge-base --is-ancestor "$BRANCH" "$BASE" 2>/dev/null; then state="$state merged"; fi
      fi
      local run; run=$( { procs_under "$LIVE"; clients_on "${PORT_PREFIX}237"; } | wc -l)
      printf '%-20s %-28s ports %sxxx  %-18s procs=%s  checked=%s\n' "$LANE" "$BRANCH" "$PORT_PREFIX" "$state" "$run" "${CHECKED:0:9}"
    )
  done
  [ $any = 1 ] || echo "no lanes (Scripts/lane.sh new <name>)"
}

cmd_sync() {
  local n; n=$(current "${1:-}"); load "$n"
  git -C "$WT" merge --no-edit "$BASE" || die "merge conflict in $WT: resolve, commit, then check again"
  echo "lane $n is up to date with $BASE ($(git -C "$WT" rev-parse --short HEAD))"
}

cmd_check() {
  local n; n=$(current "${1:-}"); load "$n"
  [ -z "$(git -C "$WT" status --porcelain | grep -v 'rust/.cargo/')" ] || die "commit first (check records the commit it tested)"
  local head; head=$(git -C "$WT" rev-parse HEAD)
  (cd "$WT/rust" && cargo xtask ci)
  setvar "$n" CHECKED "$head"
  echo "lane $n: xtask ci passed at ${head:0:9}"
}

cmd_land() {
  local n; n=$(current "${1:-}"); load "$n"
  [ -z "$(git -C "$WT" status --porcelain | grep -v 'rust/.cargo/')" ] || die "uncommitted changes in $WT"
  local head; head=$(git -C "$WT" rev-parse HEAD)
  git -C "$REPO" merge-base --is-ancestor "$BASE" "$head" \
    || die "$BASE moved on: Scripts/lane.sh sync $n && Scripts/lane.sh check $n, then land"
  [ "${CHECKED:-}" = "$head" ] || die "HEAD ${head:0:9} was not checked: Scripts/lane.sh check $n"
  local main
  main=$(git -C "$REPO" worktree list --porcelain | awk -v b="refs/heads/$BASE" '/^worktree /{w=substr($0,10)} $0=="branch " b {print w}')
  [ -n "$main" ] || die "no worktree has $BASE checked out"
  [ -z "$(git -C "$main" status --porcelain --untracked-files=no)" ] \
    || die "$main has uncommitted changes; nobody may work in the integration checkout"
  mkdir "$LANES/.land.lock" 2>/dev/null || die "another lane is landing ($LANES/.land.lock); retry"
  trap 'rmdir "$LANES/.land.lock" 2>/dev/null || true' EXIT
  git -C "$main" merge --no-ff --no-edit "$BRANCH" -m "Merge branch '$BRANCH' into $BASE"
  rmdir "$LANES/.land.lock"; trap - EXIT
  echo "landed $BRANCH into $BASE ($(git -C "$main" rev-parse --short HEAD)); the tree equals the checked lane, no re-test needed"
  cmd_done "$n"
}

stop_lane_procs() { # stop_lane_procs <live dir> <login port> <target dir or empty>
  local ids
  ids=$( { procs_under "$1"; clients_on "$2"; [ -n "$3" ] && procs_under "$3"; } | awk '{print $1}' | sort -u)
  for id in $ids; do taskkill //F //PID "$id" >/dev/null 2>&1 || true; done
  [ -z "$ids" ] || { echo "stopped processes: $(echo $ids)"; sleep 2; }
}

cmd_done() (
  local name="" force=0 keep_branch=0
  for a in "$@"; do case "$a" in --force) force=1 ;; --keep-branch) keep_branch=1 ;; *) name=$a ;; esac; done
  allocation_lock
  local n; n=$(current "$name"); load "$n"
  cd "$REPO"  # never depend on a cwd inside the worktree this command removes
  if [ -d "$WT" ] && [ $force = 0 ]; then
    [ -z "$(git -C "$WT" status --porcelain | grep -v 'rust/.cargo/')" ] \
      || die "uncommitted work in $WT (commit + land it, or --force to discard)"
    if [ $keep_branch = 0 ] && ! git -C "$REPO" merge-base --is-ancestor "$BRANCH" "$BASE" \
       && git -C "$REPO" cherry "$BASE" "$BRANCH" | grep -q '^+'; then
      die "$BRANCH has commits that are not in $BASE (land it, --keep-branch to keep only the branch, or --force to discard)"
    fi
  fi

  stop_lane_procs "$LIVE" "${PORT_PREFIX}237" "$TARGET"
  if mysql_up; then "${MYSQL[@]}" -e "DROP DATABASE IF EXISTS \`$DB_GAME\`; DROP DATABASE IF EXISTS \`$DB_LOGIN\`;"; fi
  rm -rf "$LIVE" "$TARGET"
  if [ -d "$WT" ]; then
    rm -f "$WT/rust/.cargo/config.toml"
    git -C "$REPO" worktree remove --force "$WT" 2>/dev/null || { rm -rf "$WT" 2>/dev/null || true; }
  fi
  git -C "$REPO" worktree prune
  if [ $keep_branch = 0 ]; then git -C "$REPO" branch -D "$BRANCH" >/dev/null 2>&1 || true
  else echo "kept branch $BRANCH"; fi
  rm -f "$(reg "$n")"
  [ -d "$WT" ] && echo "note: $WT is still open in some process (a shell's cwd?); gc removes the empty folder later"
  echo "lane $n removed (worktree, target dir, runtime dir, databases$( [ $keep_branch = 1 ] || echo ', branch'))"
)

cmd_gc() (
  local apply=0 min_age=24
  while [ $# -gt 0 ]; do
    case "$1" in --apply) apply=1 ;; --min-age-hours) min_age=$2; shift ;; *) die "unknown option $1" ;; esac; shift
  done
  # A dry run is read-only. Applying GC must not delete an unpublished creator's target/worktree.
  [ "$apply" = 0 ] || allocation_lock
  # Everything that is referenced stays: lane registry entries and any worktree's cargo target-dir.
  local keep=()
  for f in "$LANES"/*.env; do [ -f "$f" ] && keep+=("$(grep '^TARGET=' "$f" | cut -d= -f2-)" "$(grep '^LIVE=' "$f" | cut -d= -f2-)" "$(grep '^WT=' "$f" | cut -d= -f2-)"); done
  while read -r w; do
    keep+=("$w")
    local t; t=$(sed -n 's/^target-dir *= *"\(.*\)"/\1/p' "$w/rust/.cargo/config.toml" 2>/dev/null || true)
    [ -n "$t" ] && keep+=("$t")
  done < <(git -C "$REPO" worktree list --porcelain | sed -n 's/^worktree //p')
  kept() { local x; for x in "${keep[@]}"; do [ "${x%/}" = "${1%/}" ] && return 0; done; return 1; }
  young() { [ -n "$(find "$1" -maxdepth 1 -newermt "$min_age hours ago" -print -quit 2>/dev/null)" ]; }

  echo "== candidates (unreferenced, idle, untouched for ${min_age}h) =="
  local found=0
  for d in "$TARGET_ROOT"/*/ "$LIVE_PREFIX"*/ "$WT_ROOT"/*/; do
    [ -d "$d" ] || continue; d=${d%/}
    kept "$d" && continue
    if [ -n "$(procs_under "$d")" ]; then echo "  in use, skipped: $d"; continue; fi
    if young "$d"; then echo "  recent, skipped: $d"; continue; fi
    found=1; echo "  $(du -sh "$d" 2>/dev/null | cut -f1)	$d"
    [ $apply = 0 ] || rm -rf "$d"
  done
  for f in "$LANES"/*.env; do
    [ -f "$f" ] || continue
    [ -d "$(grep '^WT=' "$f" | cut -d= -f2-)" ] || { found=1; echo "  stale lane entry $(basename "$f" .env) (worktree gone): Scripts/lane.sh done $(basename "$f" .env) --force"; }
  done
  if mysql_up; then
    local lanedbs; lanedbs=$(cat "$LANES"/*.env 2>/dev/null | sed -n 's/^DB_\(GAME\|LOGIN\)=//p' || true)
    while read -r db; do
      case "$db" in information_schema|mysql|performance_schema|sys|rs_game|rs_login) continue ;; esac
      echo "$lanedbs" | grep -qx "$db" && continue
      case "$db" in
        lane_*) found=1; echo "  orphan lane database: $db"; [ $apply = 0 ] || "${MYSQL[@]}" -e "DROP DATABASE \`$db\`" ;;
        *) echo "  other database (not touched; drop by hand if nobody needs it): $db" ;;
      esac
    done < <("${MYSQL[@]}" -N -e "SHOW DATABASES" | tr -d '\r')
  fi
  [ $found = 1 ] || echo "  nothing to clean"
  [ $apply = 1 ] || echo "(dry run: add --apply to delete the candidates above)"
)

case "${1:-}" in
  new) shift; cmd_new "$@" ;; list) cmd_list ;; info) shift; cmd_info "${1:-}" ;;
  sync) shift; cmd_sync "${1:-}" ;; check) shift; cmd_check "${1:-}" ;; land) shift; cmd_land "${1:-}" ;;
  done) shift; cmd_done "$@" ;; gc) shift; cmd_gc "$@" ;;
  *) sed -n '2,15p' "$0"; exit 1 ;;
esac
