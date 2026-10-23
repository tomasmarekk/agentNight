#!/usr/bin/env bash
# One night on a Linux or macOS runner (dry run): a stand-in Claude Code session works,
# the switch is clicked, the session finishes and agentNight reaches the shutdown.
set -euo pipefail

if [ "${GITHUB_ACTIONS:-}" != true ]; then
  echo "This script only runs on GitHub Actions runners." >&2
  exit 1
fi

here="$(cd "$(dirname "$0")" && pwd)"
night="$RUNNER_TEMP/night"
shots="$RUNNER_TEMP/screenshots"
log="$shots/agentnight.log"
app="$GITHUB_WORKSPACE/src-tauri/target/release/agent-night"
mkdir -p "$night/.claude/sessions" "$shots"

now_ms() { echo $(($(date +%s) * 1000)); }

shot() {
  if [ "$(uname)" = Darwin ]; then
    screencapture -x "$shots/$1.png"
  else
    import -window root "$shots/$1.png"
  fi
}

status() {
  printf '{"pid":%d,"sessionId":"smoke","cwd":"%s/demo","startedAt":%d,"status":"%s","statusUpdatedAt":%d,"name":"Overnight refactor","entrypoint":"cli"}' \
    "$agent" "$night" "$started" "$1" "$(now_ms)" >"$night/.claude/sessions/$agent.json"
}

# Clicks a point given in window coordinates.
click() {
  if [ "$(uname)" = Darwin ]; then
    swift "$here/click.swift" "$app_pid" "$1" "$2"
  else
    local wid X Y
    wid=$(xdotool search --sync --name '^agentNight$' | head -1)
    eval "$(xdotool getwindowgeometry --shell "$wid")"
    xdotool mousemove $((X + $1)) $((Y + $2)) click 1
  fi
}

if [ "$(uname)" != Darwin ]; then
  Xvfb :99 -screen 0 1280x800x24 &
  export DISPLAY=:99
  sleep 2
  # A plain XEmbed system tray so the tray icon has somewhere to appear.
  stalonetray --geometry 6x1+0+0 --icon-size 32 --background '#333333' &
  sleep 1
fi

# Any long-running program named `claude` looks like a Claude Code process.
cp /bin/sleep "$night/claude"
"$night/claude" 3600 &
agent=$!
started=$(now_ms)
status busy

export AGENTNIGHT_HOME="$night" AGENTNIGHT_DRY_RUN=1
export AGENTNIGHT_GRACE_SECS=10 AGENTNIGHT_ONLINE_GRACE_SECS=5 AGENTNIGHT_COUNTDOWN_SECS=10
"$app" >"$log" 2>&1 &
app_pid=$!
sleep 15
kill -0 "$app_pid" || { cat "$log"; echo "agentNight exited"; exit 1; }

"$app" --scan | tee "$shots/scan.json"
grep -q '"state": "working"' "$shots/scan.json"
shot 1-working

click 291 81
sleep 4
shot 2-armed

status idle
sleep 18
shot 3-countdown
for _ in $(seq 1 30); do
  grep -q "dry run, not shutting down" "$log" && break
  sleep 2
done
shot 4-done
cat "$log"
grep -q "dry run, not shutting down" "$log"
kill "$app_pid" "$agent" 2>/dev/null || true
echo "agentNight reached the shutdown after the session finished."
