#!/bin/sh
# Watch the background work of the Claude Code process in this pane and show it
# in the zellij-vtabs sidebar next to the state icon (local change): a finished
# turn (the check) may still have shell commands running in the background or
# monitors armed, and Claude will wake up when they report.
#
# Both are child processes of Claude, started through its shell snapshot. A
# shell command (Bash tool) writes into its task output file, a monitor streams
# its stdout to Claude over a socket. A foreground Bash command looks like a
# background one, so while a turn runs its own command is counted too.
#
# Started by vtabs-work.sh on every hook event; one watcher per pane (flock).
# It polls every 2 s, sends `zellij-vtabs::background::<pane>` with the payload
# "<shells> <monitors>" on a change (and every 30 s, for a reloaded plugin), and
# exits once nothing is left or Claude is gone.
[ -n "$ZELLIJ" ] && [ -n "$ZELLIJ_PANE_ID" ] || exit 0

dir="${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}/zellij-vtabs"
mkdir -p "$dir" 2>/dev/null || exit 0
exec 9>"$dir/bg-${ZELLIJ_SESSION_NAME:-}-$ZELLIJ_PANE_ID.lock"
flock -n 9 || exit 0

# Claude passes its pid to hooks; else find the Claude process of this pane
claude="${CLAUDE_PID:-}"
if [ -z "$claude" ]; then
  for p in $(pgrep -u "$(id -u)" -x claude); do
    env=$(tr '\0' '\n' < "/proc/$p/environ" 2>/dev/null) || continue
    case "$env" in
      *"ZELLIJ_PANE_ID=$ZELLIJ_PANE_ID"*) ;;
      *) continue ;;
    esac
    printf '%s\n' "$env" | grep -qx "ZELLIJ_SESSION_NAME=${ZELLIJ_SESSION_NAME:-}" || continue
    claude=$p
    break
  done
fi
[ -n "$claude" ] || exit 0

count() {
  shells=0; monitors=0
  for p in $(pgrep -P "$claude"); do
    grep -q shell-snapshots "/proc/$p/cmdline" 2>/dev/null || continue
    case "$(readlink "/proc/$p/fd/1" 2>/dev/null)" in
      */tasks/*.output) shells=$((shells + 1)) ;;
      socket:*|pipe:*)  monitors=$((monitors + 1)) ;;
    esac
  done
  echo "$shells $monitors"
}

send() {
  if [ -n "$VTABS_WORK_DRYRUN" ]; then
    echo "background $1"
    return
  fi
  timeout 3 zellij pipe --name "zellij-vtabs::background::$ZELLIJ_PANE_ID" -- "$1" < /dev/null >/dev/null 2>&1
}

last=""; ticks=0
while kill -0 "$claude" 2>/dev/null; do
  cur=$(count)
  if [ "$cur" != "$last" ] || [ "$ticks" -ge 15 ]; then
    # nothing running and nothing shown: no need to tell the sidebar
    [ "$cur" = "0 0" ] && [ -z "$last" ] && exit 0
    send "$cur"
    last=$cur; ticks=0
  fi
  [ "$cur" = "0 0" ] && exit 0
  ticks=$((ticks + 1))
  sleep 2
done
[ -n "$last" ] && [ "$last" != "0 0" ] && send "0 0"
exit 0
