#!/bin/sh
# Fake gh/glab for tests. Replays $FAKE_FORGE_DIR/<key>.<n>.json for the n-th call with a key,
# else <key>.json, where the key is the git hash-object of the argument list. A GraphQL
# `query=query Name(...)` or `query=mutation Name(...)` argument is keyed as `query=Name` and a
# `body=<text>` argument as `body=`. The request body (`--input -` stdin or the `body=` text) of
# call number <c> in calls.log is saved as input-<c>. <key>.fail makes the call fail after its
# input is read, as when a write reaches the host but its response is lost. Any other call fails.
set -eu
dir=$FAKE_FORGE_DIR
body=
has_body=0
stdin=0
previous=
for arg do
  shift
  case $arg in
    query=query\ * | query=mutation\ *)
      name=${arg#query=* }
      arg="query=${name%%(*}"
      ;;
    body=*)
      body=${arg#body=}
      has_body=1
      arg=body=
      ;;
    -)
      [ "$previous" = --input ] && stdin=1
      ;;
  esac
  previous=$arg
  set -- "$@" "$arg"
done
printf '%s\t%s\n' "${GITLAB_HOST:-}" "$*" >> "$dir/calls.log"
call=$(wc -l < "$dir/calls.log" | tr -d ' ')
if [ "$stdin" = 1 ]; then
  cat > "$dir/input-$call"
elif [ "$has_body" = 1 ]; then
  printf '%s' "$body" > "$dir/input-$call"
fi
key=$(printf '%s\n' "$@" | git hash-object --stdin)
count=1
[ -f "$dir/$key.count" ] && count=$(($(cat "$dir/$key.count") + 1))
printf '%s\n' "$count" > "$dir/$key.count"
if [ -f "$dir/$key.fail" ]; then
  echo "fake-forge: simulated failure: $*" >&2
  exit 1
fi
for file in "$dir/$key.$count.json" "$dir/$key.json"; do
  if [ -f "$file" ]; then
    cat "$file"
    exit 0
  fi
done
echo "fake-forge: unexpected call: $*" >&2
exit 1
