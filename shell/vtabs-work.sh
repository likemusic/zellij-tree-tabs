#!/bin/sh
# Track Claude Code work state per pane and drive the zellij-vtabs indicator,
# counting outstanding subagents so the spinner stays lit for the WHOLE task.
#
# Why a counter: `Stop` fires once per main-agent turn, but subagents run on a
# separate lifecycle. Without counting, the main agent yielding to background
# subagents fires `Stop` and clears the spinner while work is still going. We
# keep a per-pane "main active" flag + "outstanding subagents" count, and only
# show the completed check when BOTH are done.
#
# Usage (from ~/.claude/settings.json hooks, each backgrounded):
#   UserPromptSubmit -> vtabs-work.sh prompt
#   SubagentStart    -> vtabs-work.sh subagent-start
#   SubagentStop     -> vtabs-work.sh subagent-stop
#   Stop             -> vtabs-work.sh stop
#   Notification     -> vtabs-work.sh notify
#   SessionEnd       -> vtabs-work.sh end
#   PostToolUse      -> vtabs-work.sh tool   (relights the spinner once after a Notification
#                       pause, e.g. an answered permission prompt)
#
# Set VTABS_WORK_DRYRUN=1 to print the computed signal instead of piping it
# (used by the tests).
[ -n "$ZELLIJ" ] && [ -n "$ZELLIJ_PANE_ID" ] || exit 0

event="$1"
dir="${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}/zellij-vtabs"
mkdir -p "$dir" 2>/dev/null || exit 0
state="$dir/pane-$ZELLIJ_PANE_ID"

# Atomic read-modify-write of the "main subs" counts under a lock, then decide
# which signal to emit. The lock is held only for the bookkeeping, never for the
# (slow, timeout-guarded) pipe send below.
signal=$(
  exec 9>"$state.lock"
  flock 9
  main=0; subs=0; paused=0
  [ -f "$state" ] && read -r main subs paused < "$state" 2>/dev/null
  : "${main:=0}" "${subs:=0}" "${paused:=0}"
  case "$main$subs$paused" in *[!0-9]*) main=0; subs=0; paused=0 ;; esac
  case "$event" in
    prompt)          main=1; subs=0; paused=0 ;;             # new user turn: start fresh
    subagent-start)  subs=$((subs + 1)) ;;
    subagent-stop)   subs=$((subs - 1)); [ "$subs" -lt 0 ] && subs=0 ;;
    stop)            main=0; paused=0 ;;
    # Claude also notifies 60 s after a finished turn ("waiting for your input"):
    # nothing is running then, so it is idle, not a question — keep the check.
    notify)          [ "$main" = 0 ] && [ "$subs" -le 0 ] && exit 0
                     paused=1 ;;                             # needs input; counts unchanged
    tool)            [ "$paused" = 1 ] || exit 0; paused=0 ;; # work resumed after the pause
    end)             main=0; subs=0 ;;
    *)               exit 0 ;;
  esac
  if [ "$event" = end ]; then
    rm -f "$state"
  else
    printf '%s %s %s\n' "$main" "$subs" "$paused" > "$state"
  fi
  # Only `stop` (main-turn end) with no outstanding subagents shows the check.
  # subagent-stop never completes — the main agent is re-invoked to process the
  # result and its own `stop` finalizes, so the spinner survives the tail too.
  case "$event" in
    notify) echo waiting ;;
    end)    echo clear-working ;;
    stop)   if [ "$subs" -le 0 ]; then echo completed; else echo working; fi ;;
    *)      echo working ;;
  esac
)

[ -n "$signal" ] || exit 0

# Sound is gated on the computed signal, so it fires only for the real main-task
# events — the completed check and the needs-input notification — and stays
# silent on intermediate `stop`s and all subagent activity. (Icon name doubles
# as the freedesktop .oga basename.)
sound=""
case "$signal" in
  completed) sound="complete" ;;
  waiting)   sound="message-new-instant" ;;
esac

if [ -n "$VTABS_WORK_DRYRUN" ]; then
  echo "$signal ${sound:--}"
  exit 0
fi

[ -n "$sound" ] && (canberra-gtk-play -i "$sound" >/dev/null 2>&1 \
  || pw-play "/usr/share/sounds/freedesktop/stereo/$sound.oga" >/dev/null 2>&1 || true &)
exec timeout 3 zellij pipe --name "zellij-vtabs::$signal::$ZELLIJ_PANE_ID" < /dev/null
