#!/usr/bin/env bash
# Build and test apknative on the VPS.
#
# The VPS is the build host: sources are synced up, cargo runs there, and the
# resulting binary is pulled back. Auth is SSH-key only — no password is stored.
#
#   ./vps.sh sync    # copy sources up
#   ./vps.sh build   # sync + cargo build --release (default)
#   ./vps.sh test    # sync + run the regression suite there
#   ./vps.sh ci      # build + regression suite + end-to-end APK launch
#   ./vps.sh fetch   # pull the release binary back to ./target/release
#   ./vps.sh clean   # cargo clean on the VPS

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VPS="${APKNATIVE_VPS:-vps}"
# Fixed remote path: $HOME here would expand locally, not on the VPS.
REMOTE="${APKNATIVE_REMOTE:-/root/apknative}"
CARGO='. "$HOME/.cargo/env" &&'

sync() {
  tar -C "$ROOT" --exclude=./target --exclude=./testdata/build -czf - . \
    | ssh "$VPS" "mkdir -p $REMOTE && tar -C $REMOTE -xzf -"
  echo "synced -> $VPS:$REMOTE"
}

case "${1:-build}" in
  sync)
    sync
    ;;
  build)
    sync
    ssh "$VPS" "cd $REMOTE && $CARGO cargo build --release 2>&1 | tail -25"
    ;;
  test)
    sync
    ssh "$VPS" "cd $REMOTE && $CARGO APKNATIVE_BIN=$REMOTE/target/release/apknative bash testdata/run_tests.sh 2>&1 | tail -40"
    ;;
  ci)
    # Full gate: build, JVM-parity suite, then the end-to-end APK launch.
    sync
    ssh "$VPS" "cd $REMOTE && $CARGO cargo build --release 2>&1 | grep -E '^error' -A 8 | head -30; true"
    ssh "$VPS" "cd $REMOTE && $CARGO APKNATIVE_BIN=$REMOTE/target/release/apknative bash testdata/run_tests.sh 2>&1 | tail -12"
    ssh "$VPS" "cd $REMOTE && $CARGO APKNATIVE_BIN=$REMOTE/target/release/apknative bash testdata/run_apk_test.sh 2>&1 | tail -12"
    ;;
  fetch)
    mkdir -p "$ROOT/target/release"
    scp -q "$VPS:$REMOTE/target/release/apknative" "$ROOT/target/release/apknative"
    chmod +x "$ROOT/target/release/apknative"
    echo "fetched -> $ROOT/target/release/apknative"
    ;;
  clean)
    ssh "$VPS" "cd $REMOTE && $CARGO cargo clean"
    ;;
  *)
    sed -n '2,12p' "${BASH_SOURCE[0]}"
    exit 2
    ;;
esac