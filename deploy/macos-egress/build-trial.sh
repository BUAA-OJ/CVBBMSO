#!/bin/bash
set -euo pipefail
umask 022

if [[ $# != 1 ]]; then
    echo 'Usage: build-trial.sh HTTPS_ORIGIN' >&2
    exit 2
fi
if [[ $(/usr/bin/uname -s) != Darwin || $(/usr/bin/uname -m) != arm64 || $EUID == 0 ]]; then
    echo 'Build the trial as a regular user on native Apple Silicon macOS.' >&2
    exit 1
fi
origin=$1
/usr/bin/python3 - "$origin" <<'PY'
import re
import sys
from urllib.parse import urlsplit

source = sys.argv[1]
try:
    origin = urlsplit(source)
    valid = re.fullmatch(r'https://(?:[a-zA-Z0-9.-]+|\[[0-9a-fA-F:.]+\])(?::[0-9]{1,5})?/?', source)
    valid = valid and origin.hostname and origin.port != 0
except ValueError:
    valid = False
if not valid:
    raise SystemExit('Set a valid HTTPS origin, without credentials, a path, query, or fragment.')
PY

cd -- "$(dirname -- "$0")/../.."
root=$(pwd -P)
deployment="$root/deploy/macos-egress"
if [[ -n $(git status --porcelain) ]]; then
    echo 'Build the trial from a clean checkout so the package has an exact source revision.' >&2
    exit 1
fi
revision=$(git rev-parse HEAD)
target=aarch64-apple-darwin
app_id=edu.buaa.accoding.cvbbmso.trial

export CVBBMSO_SERVER_ORIGIN="$origin"
export TAURI_CONFIG='{"productName":"CVBBMSO Trial","identifier":"edu.buaa.accoding.cvbbmso.trial","bundle":{"macOS":{"minimumSystemVersion":"14.0"}}}'
unset CVBBMSO_MACOS_TEAM_ID
metadata=$(cargo metadata --locked --no-deps --format-version 1)
target_dir=$(printf '%s' "$metadata" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
version=$(printf '%s' "$metadata" | /usr/bin/python3 -c \
    'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "cvbbmso-client"))')
MACOSX_DEPLOYMENT_TARGET=14.0 cargo build --locked --release --target "$target" \
    -p cvbbmso-client -p cvbbmso-network --features cvbbmso-client/macos-pf-trial \
    --bin cvbbmso-client --bin cvbbmso-pf-trial

artifact_dir="$target_dir/$target/release"
for binary in cvbbmso-client cvbbmso-pf-trial; do
    file="$artifact_dir/$binary"
    [[ $(/usr/bin/lipo -archs "$file") == arm64 ]]
    /usr/bin/xcrun vtool -show-build "$file" | /usr/bin/awk '
        $1 == "platform" && $2 == "MACOS" { mac = 1 }
        $1 == "minos" && ($2 == "14.0" || $2 == "14.0.0") { supported = 1 }
        END { exit !(mac && supported) }
    '
    LC_ALL=C /usr/bin/grep -aFq -- "$origin" "$file"
done

work=$(/usr/bin/mktemp -d "$artifact_dir/CVBBMSO-Trial-${revision:0:12}.XXXXXXXX")
trap '/bin/rm -rf -- "$work"' EXIT
trap 'exit 1' HUP INT TERM
stage="$work/CVBBMSO Trial"
payload="$work/payload"
app="$payload/CVBBMSO Trial.app"
contents="$app/Contents"
/bin/mkdir -p "$contents/MacOS" "$contents/Resources" "$stage"
/usr/bin/install -m 0755 "$artifact_dir/cvbbmso-client" "$contents/MacOS/cvbbmso-client"
/usr/bin/install -m 0644 "$root/crates/cvbbmso-client/icons/icon.icns" "$contents/Resources/icon.icns"
/usr/bin/install -m 0644 "$deployment/Info.plist" "$contents/Info.plist"
/usr/bin/plutil -replace CFBundleIdentifier -string "$app_id" "$contents/Info.plist"
/usr/bin/plutil -replace CFBundleName -string 'CVBBMSO Trial' "$contents/Info.plist"
/usr/bin/plutil -insert CFBundleShortVersionString -string "$version" "$contents/Info.plist"
/usr/bin/plutil -insert CFBundleVersion -string "$version" "$contents/Info.plist"
/usr/bin/plutil -remove CVBBMSOPrivilegedServiceVersion "$contents/Info.plist"
/usr/bin/plutil -lint "$contents/Info.plist"
/usr/bin/codesign --force --sign - --identifier "$app_id" "$app"
/usr/bin/codesign --verify --strict --all-architectures "$app"

/usr/bin/install -m 0755 "$artifact_dir/cvbbmso-pf-trial" "$payload/cvbbmso-pf-trial"
/usr/bin/install -m 0755 "$deployment/pf-trial.sh" "$payload/pf-trial.sh"
/usr/bin/ditto -c -k --sequesterRsrc "$payload" "$stage/Payload.zip"
checksum=$(/usr/bin/shasum -a 256 < "$stage/Payload.zip")
/usr/bin/sed -e "s|@CVBBMSO_TRIAL_ORIGIN@|$origin|" -e "s|@CVBBMSO_TRIAL_PAYLOAD_SHA256@|${checksum%% *}|" \
    "$deployment/start-trial.command" > "$stage/Start CVBBMSO Trial.command"
/bin/chmod 0755 "$stage/Start CVBBMSO Trial.command"
/bin/bash -n "$stage/Start CVBBMSO Trial.command"
/bin/rm -r -- "$payload"
/usr/bin/install -m 0644 "$root/docs/deployment/macos-pf-trial.md" "$stage/README.md"
printf 'source_revision=%s\nmonitor_origin=%s\n' "$revision" "$origin" > "$stage/BUILD.txt"

archive="$work/CVBBMSO-Trial-macos-arm64.zip"
/usr/bin/ditto -c -k --sequesterRsrc --keepParent "$stage" "$archive"
(
    cd -- "$work"
    leaf=${archive##*/}
    /usr/bin/shasum -a 256 -- "$leaf" > "$leaf.sha256"
    /usr/bin/shasum -a 256 -c "$leaf.sha256"
)
trap - EXIT
printf '%s\n' "$archive"
