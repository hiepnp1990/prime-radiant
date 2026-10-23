#!/usr/bin/env bash
# Drives a disposable Prime Radiant instance: a fresh build, copies of the test fixtures, and a hub
# on its own socket. Every `cli` call is recorded as evidence that survives cleanup.
set -euo pipefail

usage() {
  cat <<'EOF'
verify.sh launch                       build, copy fixtures, start a hub; prints RUN=<dir>
verify.sh doctor RUN                   check the instance is ours, current, and answering
verify.sh cli RUN LABEL ARGS...        run prime-radiant ARGS, record it as evidence
                                       (@RUN@, @SOCK@, @DATA@, @SESSIONS@, @BIN@, @HOME@ in ARGS are replaced)
verify.sh home RUN LABEL ARGS...       like cli, with HOME and the XDG folders in RUN/home (@HOME@)
                                       and PRIME_RADIANT_HUB unset, so no real config, skill, or state is touched
verify.sh collector RUN start|stop|crash  run a collector over the fixture copies; crash is SIGKILL
                                       (its state file is RUN/state/collect-verify.json)
verify.sh hub RUN stop|start           stop the hub this run started, or start it again on the same data
verify.sh big-session RUN MIB          add a made-up transcript of about MIB mebibytes to the Claude Code copy
verify.sh append RUN TEXT              append a user line with TEXT to the fixture transcript copy
verify.sh token                        print a fresh one-word token, such as quokka3f9a1c2e
verify.sh wait-find RUN TOKEN [SECS]   poll find TOKEN through the hub until a hit quotes it; record the time
verify.sh cleanup RUN                  stop what this run started and delete RUN; keep the evidence
EOF
}

repo() { git -C "$(dirname "$0")" rev-parse --show-toplevel; }
load() {
  RUN=$1
  [ -f "$RUN/env" ] || { echo "not a verification run: $RUN" >&2; exit 2; }
  # shellcheck disable=SC1091
  . "$RUN/env"
}
alive() { [ -f "$1" ] && kill -0 "$(cat "$1")" 2>/dev/null; }

launch() {
  local root id run evidence
  root=$(repo)
  id=$(date -u +%Y%m%dT%H%M%SZ)-$$
  # A Unix socket path must fit in about 100 bytes, and macOS puts $TMPDIR deep under /var/folders.
  run=${PR_VERIFY_ROOT:-/tmp/prime-radiant-verify}/$id
  evidence=${PR_EVIDENCE_ROOT:-${TMPDIR:-/tmp}/prime-radiant-evidence}/$id
  mkdir -p "$run/bin" "$run/sessions" "$evidence"
  (cd "$root" && cargo build --release --locked --quiet)
  cp "$root/target/release/prime-radiant" "$run/bin/"
  for source in claude-code codex grok; do
    cp -R "$root/tests/fixtures/$source" "$run/sessions/$source"
  done
  cat >"$run/env" <<EOF
BIN='$run/bin/prime-radiant'
DATA='$run/data'
SOCK='$run/data/hub.sock'
SESSIONS='$run/sessions'
EVIDENCE='$evidence'
COMMIT='$(git -C "$root" rev-parse HEAD)'
EOF
  load "$run"
  start_hub
  echo "RUN=$RUN"
  echo "EVIDENCE=$EVIDENCE"
}

start_hub() {
  nohup "$BIN" hub serve --data "$DATA" >>"$RUN/hub.log" 2>&1 &
  echo $! >"$RUN/hub.pid"
  for _ in $(seq 50); do
    if "$BIN" stats --hub-socket "$SOCK" --json >/dev/null 2>&1; then
      return
    fi
    sleep 0.2
  done
  echo "hub did not answer within 10 s; see $RUN/hub.log" >&2
  exit 1
}

hub() {
  load "$1"
  case $2 in
    start) alive "$RUN/hub.pid" || start_hub ;;
    stop)
      if alive "$RUN/hub.pid" && ps -o command= -p "$(cat "$RUN/hub.pid")" | grep -q -- "$RUN"; then
        kill "$(cat "$RUN/hub.pid")"
      fi
      rm -f "$RUN/hub.pid"
      ;;
    *) usage; exit 2 ;;
  esac
}

doctor() {
  load "$1"
  local ok=0 head
  check() { if eval "$2"; then echo "ok    $1"; else echo "FAIL  $1"; ok=1; fi; }
  head=$(git -C "$(dirname "$0")" rev-parse HEAD)
  check "binary runs: $("$BIN" --version 2>&1)" '"$BIN" --version >/dev/null'
  check "built from the checked-out commit ${COMMIT:0:7}" '[ "$COMMIT" = "$head" ]'
  check "hub process from this run is alive" 'alive "$RUN/hub.pid"'
  check "hub serves this run's data folder" 'ps -o command= -p "$(cat "$RUN/hub.pid")" | grep -q -- "--data $DATA"'
  check "hub answers on $SOCK" '"$BIN" stats --hub-socket "$SOCK" --json >/dev/null'
  return $ok
}

cli() {
  load "$1"
  local label=$2 n file args=() arg status
  shift 2
  for arg in "$@"; do
    arg=${arg//@SOCK@/$SOCK}
    arg=${arg//@RUN@/$RUN}
    arg=${arg//@DATA@/$DATA}
    arg=${arg//@SESSIONS@/$SESSIONS}
    arg=${arg//@HOME@/$RUN/home}
    args+=("${arg//@BIN@/$BIN}")
  done
  n=$(find "$EVIDENCE" -maxdepth 1 -name '[0-9][0-9]-*.txt' | wc -l | tr -d ' ')
  file=$(printf '%s/%02d-%s.txt' "$EVIDENCE" "$((n + 1))" "$label")
  set +e
  "$BIN" "${args[@]}" >"$file.out" 2>"$file.err"
  status=$?
  set -e
  {
    printf '$ prime-radiant'; printf ' %q' "${args[@]}"; printf '\nexit %s\n--- stdout\n' "$status"
    cat "$file.out"; printf -- '--- stderr\n'; cat "$file.err"
  } >"$file"
  cat "$file.out"
  cat "$file.err" >&2
  rm -f "$file.out" "$file.err"
  return $status
}

home() {
  load "$1"
  mkdir -p "$RUN/home"
  env -u PRIME_RADIANT_HUB HOME="$RUN/home" XDG_CONFIG_HOME="$RUN/home/.config" \
    XDG_STATE_HOME="$RUN/home/.local/state" XDG_DATA_HOME="$RUN/home/.local/share" bash "$0" cli "$@"
}

collector() {
  load "$1"
  case $2 in
    start)
      nohup "$BIN" collect --origin verify --hub-socket "$SOCK" --sweep-seconds 5 \
        --claude-code "$SESSIONS/claude-code" --codex "$SESSIONS/codex" --grok "$SESSIONS/grok" \
        --state-file "$RUN/state/collect-verify.json" >>"$RUN/collector.log" 2>&1 &
      echo $! >"$RUN/collector.pid"
      ;;
    stop | crash)
      if alive "$RUN/collector.pid"; then
        kill "$([ "$2" = crash ] && echo -9 || echo -TERM)" "$(cat "$RUN/collector.pid")"
      fi
      rm -f "$RUN/collector.pid"
      ;;
    *) usage; exit 2 ;;
  esac
}

append() {
  load "$1"
  local transcript
  transcript=$(find "$SESSIONS/claude-code" -maxdepth 2 -name '*.jsonl' -not -name 'agent-*' | sort | head -1)
  python3 - "$transcript" "$2" <<'EOF'
import json, sys, uuid, datetime
path, text = sys.argv[1], sys.argv[2]
last = [json.loads(l) for l in open(path) if l.strip()][-1]
line = {"parentUuid": last["uuid"], "isSidechain": False, "type": "user", "sessionId": last["sessionId"],
        "cwd": last.get("cwd"), "uuid": str(uuid.uuid4()),
        "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z"),
        "message": {"role": "user", "content": text}}
with open(path, "a") as f:
    f.write(json.dumps(line) + "\n")
EOF
  date +%s.%N >"$RUN/appended-at"
  echo "appended to $transcript"
}

big_session() {
  load "$1"
  python3 - "$SESSIONS/claude-code/-home-dev-generated-project" "$2" <<'EOF'
import json, os, sys, uuid
folder, mib = sys.argv[1], int(sys.argv[2])
os.makedirs(folder, exist_ok=True)
session = str(uuid.uuid4())
filler = "lorem ipsum dolor sit amet " * 600
path = os.path.join(folder, session + ".jsonl")
with open(path, "w") as f:
    f.write(json.dumps({"type": "user", "sessionId": session, "uuid": str(uuid.uuid4()), "parentUuid": None,
                        "cwd": "/home/dev/generated-project", "timestamp": "2026-03-01T09:00:00.000Z",
                        "message": {"role": "user", "content": "A made-up request for a large session."}}) + "\n")
    size, n = 0, 0
    while size < mib << 20:
        n += 1
        line = json.dumps({"type": "progress", "sessionId": session, "data": "step %d %s" % (n, filler)}) + "\n"
        f.write(line)
        size += len(line)
print("wrote %d bytes to %s" % (os.path.getsize(path), path))
EOF
}

wait_find() {
  load "$1"
  local text=$2 limit=${3:-30} start now
  start=$(cat "$RUN/appended-at" 2>/dev/null || date +%s.%N)
  while :; do
    # find matches any query word, so require the hit's snippet to quote the whole token.
    if "$BIN" find "$text" --hub-socket "$SOCK" --json --limit 1 | grep -qF -- "$text"; then
      now=$(date +%s.%N)
      awk -v a="$start" -v b="$now" -v t="$text" 'BEGIN { printf "findable after %.2f s: %s\n", b - a, t }' |
        tee -a "$EVIDENCE/freshness.txt"
      return
    fi
    if awk -v a="$start" -v b="$(date +%s.%N)" -v l="$limit" 'BEGIN { exit !(b - a > l) }'; then
      echo "not findable after $limit s: $text" | tee -a "$EVIDENCE/freshness.txt" >&2
      return 1
    fi
    sleep 0.2
  done
}

cleanup() {
  load "$1"
  local pid
  for pid in "$RUN/collector.pid" "$RUN/hub.pid"; do
    if alive "$pid" && ps -o command= -p "$(cat "$pid")" | grep -q -- "$RUN"; then
      kill "$(cat "$pid")"
    fi
  done
  cp "$RUN"/*.log "$EVIDENCE/" 2>/dev/null || true
  rm -rf "$RUN"
  echo "removed $RUN; evidence kept in $EVIDENCE"
}

case ${1:-} in
  launch) launch ;;
  doctor) doctor "$2" ;;
  cli) shift; cli "$@" ;;
  home) shift; home "$@" ;;
  collector) collector "$2" "$3" ;;
  hub) hub "$2" "$3" ;;
  big-session) big_session "$2" "$3" ;;
  append) append "$2" "$3" ;;
  wait-find) wait_find "$2" "$3" "${4:-30}" ;;
  token) echo "quokka$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')" ;;
  cleanup) cleanup "$2" ;;
  *) usage; exit 2 ;;
esac
