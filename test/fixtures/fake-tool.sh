#!/bin/sh
# Stands in for a package manager: logs "<name> <args>" to $FAKE_LOG, prints a line, and fails
# when $FAKE_FAIL equals "<name> <first argument>".
name=$(basename "$0")
echo "$name $*" >> "$FAKE_LOG"
echo "ran $name $*"
[ "$FAKE_FAIL" = "$name $1" ] && exit 1
exit 0
