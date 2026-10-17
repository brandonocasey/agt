#!/bin/sh
# Fake gh/glab for tests. Replays $FAKE_FORGE_DIR/<key>.<n>.json for the n-th call with a key,
# else <key>.json, where the key is the git hash-object of the argument list. A GraphQL
# `query=query Name(...)` or `query=mutation Name(...)` argument is keyed as `query=Name` and a
# `body=<text>` argument as `body=`. The request body (`--input -` stdin or the `body=` text) of
# call number <c> in calls.log is saved as input-<c>. <key>.fail makes the call fail after its
# input is read, as when a write reaches the host but its response is lost; a non-empty
# <key>.fail is printed to stderr as the tool's error, such as `glab: 403 Forbidden (HTTP 403)`. In a response,
# {{LAST_NOTE}} and {{LAST_BODY}} stand for the JSON-escaped "note" or "body" of the latest
# request that sent one, the text the host then shows. A glab call (one with GITLAB_HOST set)
# first tries <key>.glab.json, so gh and glab can answer the same arguments differently. Any
# other call fails.
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
  if [ -s "$dir/$key.fail" ]; then
    cat "$dir/$key.fail" >&2
  else
    echo "fake-forge: simulated failure: $*" >&2
  fi
  exit 1
fi
# The JSON-escaped string field $1 of the latest request that sent one.
last() {
  latest=
  for name in $(ls "$dir" | grep '^input-' | sort -t- -k2 -n); do
    if grep -q "\"$1\":\"" "$dir/$name"; then
      latest=$dir/$name
    fi
  done
  [ -n "$latest" ] || return 0
  FIELD="\"$1\":\"" awk '{
    rest = substr($0, index($0, ENVIRON["FIELD"]) + length(ENVIRON["FIELD"]))
    out = ""
    for (i = 1; i <= length(rest); i++) {
      c = substr(rest, i, 1)
      if (c == "\\") { out = out c substr(rest, i + 1, 1); i++; continue }
      if (c == "\"") break
      out = out c
    }
    printf "%s", out
  }' "$latest"
}
reply() {
  if ! grep -q '{{LAST_' "$1"; then
    cat "$1"
    return
  fi
  NOTE=$(last note) BODY=$(last body) awk '{
    line = $0
    while ((at = index(line, "{{LAST_NOTE}}")) > 0) {
      line = substr(line, 1, at - 1) ENVIRON["NOTE"] substr(line, at + 13)
    }
    while ((at = index(line, "{{LAST_BODY}}")) > 0) {
      line = substr(line, 1, at - 1) ENVIRON["BODY"] substr(line, at + 13)
    }
    print line
  }' "$1"
}
glab=
[ -n "${GITLAB_HOST:-}" ] && glab=$dir/$key.glab.json
for file in $glab "$dir/$key.$count.json" "$dir/$key.json"; do
  if [ -f "$file" ]; then
    reply "$file"
    exit 0
  fi
done
echo "fake-forge: unexpected call: $*" >&2
exit 1
