#!/bin/bash
# dev.sh [COMMAND...]: copy this repo to the host (host.ssh, into host.dir,
# from the configuration; see winstream_config.py) and run COMMAND there (default:
# cargo build). This copy is the source of truth; build output (target/)
# only lives on the host.
#
#   ./dev.sh                               # build
#   ./dev.sh cargo run -p server -- ...    # run the server
#   WINSTREAM_CONFIG=other.toml ./dev.sh          # another configuration

set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
vars="$("$HERE/winstream_config.py" shell)" || exit 1
eval "$vars"

tar -C "$HERE" --exclude=./target --exclude=./.git --exclude=./out -czf - . \
    | ssh "$WINSTREAM_HOST" "mkdir -p $WINSTREAM_REMOTE_DIR && tar -xzf - -C $WINSTREAM_REMOTE_DIR"
ssh "$WINSTREAM_HOST" "cd $WINSTREAM_REMOTE_DIR && ${*:-cargo build}"
