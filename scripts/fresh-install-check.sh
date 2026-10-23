#!/usr/bin/env bash
# Runs the README quickstart on one machine from nothing: a throwaway HOME under /tmp, the binary installed into its
# ~/.local/bin from this checkout, and copies of tests/fixtures where each harness keeps its sessions. Nothing after
# the build uses the network. A CI runner cannot run login services, so the hub and the collector run in the
# background as the services would, and `service install --dry-run` prints the definitions they would get.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
session=33831ab7-5c2a-48a4-a8d7-0b5125040f4d
# A Unix socket path must fit in about 100 bytes, and macOS puts $TMPDIR deep under /var/folders.
fresh=$(mktemp -d /tmp/pr-fresh.XXXXXX)
hub_pid=
collector_pid=

cleanup() {
  for pid in $collector_pid $hub_pid; do
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  rm -rf "$fresh"
  echo "removed $fresh"
}
trap cleanup EXIT

say() { printf '\n== %s\n' "$*"; }
run() {
  printf '$ %s\n' "$*"
  "$@"
}
fail() {
  echo "FAIL: $*" >&2
  exit 1
}
# Runs a command and prints it with its stdout, which `holds` then checks for each text it is given.
shows() {
  local status=0
  out=$("$@") || status=$?
  printf '$ %s\n%s\n' "$*" "$out"
  return $status
}
holds() {
  for text in "$@"; do
    grep -qF -- "$text" <<<"$out" || fail "the output does not hold $text"
  done
}
# Polls `$@` once a second until it succeeds, for up to `limit` seconds.
wait_until() {
  local limit=$1
  shift
  for _ in $(seq "$limit"); do
    if "$@" >/dev/null 2>&1; then
      return
    fi
    sleep 1
  done
  return 1
}

say "install the binary into $fresh/.local/bin"
# cargo reads its own home and toolchain, so it runs before HOME changes.
run cargo install --path "$repo" --locked --root "$fresh/.local" --quiet

export HOME=$fresh
export PATH="$HOME/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
unset XDG_CONFIG_HOME XDG_STATE_HOME XDG_DATA_HOME PRIME_RADIANT_HUB
run prime-radiant --version
[ "$(command -v prime-radiant)" = "$HOME/.local/bin/prime-radiant" ] || fail "prime-radiant on PATH is not the installed one"

say "put copies of the fixtures where Claude Code, Codex, and Grok keep their sessions"
mkdir -p "$HOME/.claude"
cp -R "$repo/tests/fixtures/claude-code" "$HOME/.claude/projects"
cp -R "$repo/tests/fixtures/codex" "$HOME/.codex"
cp -R "$repo/tests/fixtures/grok" "$HOME/.grok"

say "the hub: the service definition, then the same command in the background"
shows prime-radiant service install hub --dry-run
holds "$HOME/.local/share/prime-radiant"
prime-radiant hub serve --data "$HOME/.local/share/prime-radiant" >"$HOME/hub.log" 2>&1 &
hub_pid=$!
wait_until 30 prime-radiant stats || fail "the hub did not answer within 30 s: $(cat "$HOME/hub.log")"

say "the collector: the service definition, then the same command in the background"
shows prime-radiant service install collector --origin server --dry-run
holds "$HOME/.claude/projects" "$HOME/.codex" "$HOME/.grok" "$HOME/.local/share/prime-radiant/hub.sock"
prime-radiant collect --origin server >"$HOME/collector.log" 2>&1 &
collector_pid=$!

say "save the route to the hub, and give each harness the skill"
run prime-radiant config set hub-socket "$HOME/.local/share/prime-radiant/hub.sock"
run prime-radiant skill install

say "check the setup"
wait_until 30 test -f "$HOME/.local/state/prime-radiant/collect-server.json" || fail "the collector wrote no state"
shows prime-radiant doctor || fail "doctor exited $?"
grep -v '^ok    ' <<<"$out" && fail "doctor printed more than ok lines"

say "watch the first sync"
live() { prime-radiant status --json | grep -q '"name": "live"'; }
wait_until 60 live || fail "the collector did not go live within 60 s: $(cat "$HOME/collector.log")"
run prime-radiant status

say "find the fixture session by its ID and by a word"
found() { prime-radiant find "$@" | grep -qF "claude-code/$session"; }
wait_until 30 found "$session" --limit 1 || fail "find $session --limit 1 did not name the session"
run prime-radiant find "$session" --limit 1
found json || fail "find json did not name the session"
run prime-radiant find json

say "reach the hub as a laptop does, through hub connect with no --data"
bridge="cd && .local/bin/prime-radiant hub connect"
found "$session" --hub-command "$bridge" || fail "find through $bridge did not name the session"
run prime-radiant find "$session" --limit 1 --hub-command "$bridge"

say "pass: the quickstart works from nothing"
