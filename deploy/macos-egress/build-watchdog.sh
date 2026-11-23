#!/bin/bash
set -euo pipefail

if [[ $(/usr/bin/uname -s) != Darwin || $EUID == 0 ]]; then
    echo 'Build the watchdog as an unprivileged user on macOS.' >&2
    exit 1
fi

case "$(/usr/bin/uname -m)" in
    arm64) target=aarch64-apple-darwin ;;
    x86_64) target=x86_64-apple-darwin ;;
    *) echo 'Unsupported macOS architecture.' >&2; exit 1 ;;
esac

cd -- "$(dirname -- "$0")/../.."
workspace="$(pwd -P)"
revision="$(git rev-parse HEAD)"
if [[ -n $(git status --porcelain) ]]; then
    echo 'Build from a clean checkout of the client revision.' >&2
    exit 1
fi
cargo build --locked --release --target "$target" --target-dir "$workspace/target" \
    -p cvbbmso-network --bin cvbbmso-egress-watchdog

artifact="$workspace/target/$target/release/cvbbmso-egress-watchdog"
printf 'Commit %s\nTarget %s\n' "$revision" "$target"
rustc --version
/usr/bin/shasum -a 256 -- "$artifact"
