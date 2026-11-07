#!/bin/sh
# Fake tea for tests. `tea logins list --output json` prints $FAKE_FORGE_DIR/tea-logins.json (an
# empty list without it), and every other call is answered by fake-forge.sh as `tea api` would:
# the `limit` and `page` query parameters agt adds are not part of the answer's key, and an HTTP
# error is a status line on stderr with exit 0.
set -eu
if [ "$*" = "logins list --output json" ]; then
  printf '\t%s\n' "$*" >> "$FAKE_FORGE_DIR/calls.log"
  if [ -f "$FAKE_FORGE_DIR/tea-logins.json" ]; then
    cat "$FAKE_FORGE_DIR/tea-logins.json"
  else
    echo '[]'
  fi
  exit 0
fi
FAKE_FORGE_TEA=1 exec "$(dirname "$0")/fake-forge.sh" "$@"
