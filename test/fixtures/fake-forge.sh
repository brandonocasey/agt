#!/bin/sh
# Fake gh/glab for tests. Replays $FAKE_FORGE_DIR/<git hash-object of the argument list>.json,
# where a GraphQL `query=query Name(...)` argument is keyed as `query=Name`. Any other call,
# and any GraphQL mutation, fails, so a passing test proves no write call was made.
set -eu
for arg do
  shift
  case $arg in
    query=mutation*)
      echo "fake-forge: mutation refused: $arg" >&2
      exit 1
      ;;
    query=query\ *)
      name=${arg#query=query }
      arg="query=${name%%(*}"
      ;;
  esac
  set -- "$@" "$arg"
done
printf '%s\t%s\n' "${GITLAB_HOST:-}" "$*" >> "$FAKE_FORGE_DIR/calls.log"
key=$(printf '%s\n' "$@" | git hash-object --stdin)
if [ -f "$FAKE_FORGE_DIR/$key.json" ]; then
  cat "$FAKE_FORGE_DIR/$key.json"
  exit 0
fi
echo "fake-forge: unexpected call: $*" >&2
exit 1
