#!/bin/sh
set -eu
out="$1"
printf 'script-produced hello\n' > "$out"
