#!/bin/sh
# Fake gh/glab that replays numbered steps from $FAKE_STEPS_DIR in call order. Step <n> has
# <n>.includes (text the arguments must contain), and optionally <n>.stdout, <n>.stderr (written
# as they are), <n>.code (exit status, default 0) and <n>.delay (seconds to wait first). A call
# with no step left, or arguments that miss the step's text, fails with status 70. Every call's
# arguments are appended to calls.log.
dir=$FAKE_STEPS_DIR
n=0
[ -f "$dir/state" ] && n=$(cat "$dir/state")
printf '%s\n' "$*" >> "$dir/calls.log"
if [ ! -f "$dir/$n.includes" ]; then
  echo "unexpected call $n: $*" >&2
  exit 70
fi
want=$(cat "$dir/$n.includes")
case "$*" in
  *"$want"*) ;;
  *)
    echo "call $n expected $want; got $*" >&2
    exit 70
    ;;
esac
echo $((n + 1)) > "$dir/state"
[ -f "$dir/$n.delay" ] && sleep "$(cat "$dir/$n.delay")"
[ -f "$dir/$n.stdout" ] && cat "$dir/$n.stdout"
[ -f "$dir/$n.stderr" ] && cat "$dir/$n.stderr" >&2
code=0
[ -f "$dir/$n.code" ] && code=$(cat "$dir/$n.code")
exit "$code"
