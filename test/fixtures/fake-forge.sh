#!/bin/sh
# Fake gh/glab for tests. Replays $FAKE_FORGE_DIR/<key>.<n>.json for the n-th call with a key,
# else <key>.json, where the key is the git hash-object of the argument list. A GraphQL
# `query=query Name(...)` or `query=mutation Name(...)` argument is keyed as `query=Name` and a
# `body=<text>` argument as `body=`. The request body (`--input -` or tea's `-d @-` stdin, or the `body=` text) of
# call number <c> in calls.log is saved as input-<c>. <key>.fail makes the call fail after its
# input is read, as when a write reaches the host but its response is lost; a non-empty
# <key>.fail is printed to stderr as the tool's error, such as `glab: 403 Forbidden (HTTP 403)`. In a response,
# {{LAST_NOTE}}, {{LAST_BODY}} and {{LAST_DESCRIPTION}} stand for the JSON-escaped "note",
# "body" or "description" of the latest request that sent one, the text the host then shows. A glab call (one with GITLAB_HOST set)
# first tries <key>.glab.json, so gh and glab can answer the same arguments differently. Any
# other call fails. With FAKE_FORGE_TEA set (fake-tea.sh does) the fake behaves like `tea api`: a
# trailing `limit=<n>&page=<p>` is not part of the key and calls.log keeps it, `<key>.items` (one
# JSON value per line) answers page <p> of <n> items, or every item for a request without a page,
# while a `<key>.json` answers every page, and only page 1 counts as a call for the n-th
# response; and the call exits 0 whatever the host answered, printing `HTTP/1.1 <status>` to
# stderr under `-i`: `200 OK`, or the content of `<key>.status` such as `404 Not Found` with
# the body of the answer on stdout. A `<key>.fail` whose text names `HTTP <code>` is such an
# answer with that status and an empty body; any other `<key>.fail` is tea itself failing.
# A `<key>.hook` is a shell script run once the call has read its input.
# It stands for another agt command that overlaps the call.
set -eu
dir=$FAKE_FORGE_DIR
body=
has_body=0
stdin=0
previous=
page=
limit=
include=0
logged=$*
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
    @-)
      [ "$previous" = -d ] && stdin=1
      ;;
    -i)
      include=1
      ;;
    *limit=[0-9]*\&page=[0-9]*)
      if [ -n "${FAKE_FORGE_TEA:-}" ]; then
        page=${arg##*page=}
        rest=${arg%\&page=*}
        limit=${rest##*limit=}
        arg=${rest%[?\&]limit=*}
      fi
      ;;
  esac
  previous=$arg
  set -- "$@" "$arg"
done
[ -n "${FAKE_FORGE_TEA:-}" ] || logged=$*
printf '%s\t%s\n' "${GITLAB_HOST:-}" "$logged" >> "$dir/calls.log"
call=$(($(wc -l < "$dir/calls.log")))
# A request loop that never ends in the tool under test stops here instead of hanging the suite.
if [ "$call" -gt 2000 ]; then
  echo "fake-forge: more than 2000 calls" >&2
  exit 1
fi
if [ "$stdin" = 1 ]; then
  cat > "$dir/input-$call"
elif [ "$has_body" = 1 ]; then
  printf '%s' "$body" > "$dir/input-$call"
fi
key=$(printf '%s\n' "$@" | git hash-object --stdin)
count=1
if [ -n "$page" ] && [ "$page" != 1 ]; then
  count=$(cat "$dir/$key.count")
else
  [ -f "$dir/$key.count" ] && count=$(($(cat "$dir/$key.count") + 1))
  printf '%s\n' "$count" > "$dir/$key.count"
fi
# Prints the status line `tea api -i` writes to stderr before the body.
announce() {
  [ -n "${FAKE_FORGE_TEA:-}" ] && [ "$include" = 1 ] || return 0
  if [ -f "$dir/$key.status" ]; then
    printf 'HTTP/1.1 %s\n\n' "$(cat "$dir/$key.status")" >&2
  else
    printf 'HTTP/1.1 200 OK\n\n' >&2
  fi
}
[ ! -f "$dir/$key.hook" ] || sh "$dir/$key.hook" >&2
if [ -f "$dir/$key.fail" ]; then
  code=$(sed -n 's/.*HTTP \([0-9][0-9][0-9]\).*/\1/p' "$dir/$key.fail" | head -n 1)
  if [ -n "${FAKE_FORGE_TEA:-}" ] && [ "$include" = 1 ] && [ -n "$code" ]; then
    printf 'HTTP/1.1 %s %s\n\n' "$code" "$(cat "$dir/$key.fail")" >&2
    exit 0
  fi
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
  placeholder=
  while IFS= read -r line || [ -n "$line" ]; do
    case $line in
      *'{{LAST_'*)
        placeholder=1
        break
        ;;
    esac
  done < "$1"
  if [ -z "$placeholder" ]; then
    cat "$1"
    return
  fi
  NOTE=$(last note) BODY=$(last body) DESC=$(last description) awk '{
    line = $0
    while ((at = index(line, "{{LAST_NOTE}}")) > 0) {
      line = substr(line, 1, at - 1) ENVIRON["NOTE"] substr(line, at + 13)
    }
    while ((at = index(line, "{{LAST_BODY}}")) > 0) {
      line = substr(line, 1, at - 1) ENVIRON["BODY"] substr(line, at + 13)
    }
    while ((at = index(line, "{{LAST_DESCRIPTION}}")) > 0) {
      line = substr(line, 1, at - 1) ENVIRON["DESC"] substr(line, at + 20)
    }
    print line
  }' "$1"
}
if [ -f "$dir/$key.items" ]; then
  announce
  # Without a page the endpoint is not paged: every item.
  awk -v from=$(((${page:-1} - 1) * ${limit:-0} + 1)) -v to=$((${page:-1} * ${limit:-0})) -v all="$([ -n "$page" ] || echo 1)" \
    'all || (NR >= from && NR <= to) { out = out (n++ ? "," : "") $0 } END { print "[" out "]" }' "$dir/$key.items"
  exit 0
fi
glab=
[ -n "${GITLAB_HOST:-}" ] && glab=$dir/$key.glab.json
for file in $glab "$dir/$key.$count.json" "$dir/$key.json"; do
  if [ -f "$file" ]; then
    announce
    reply "$file"
    exit 0
  fi
done
echo "fake-forge: unexpected call: $*" >&2
exit 1
