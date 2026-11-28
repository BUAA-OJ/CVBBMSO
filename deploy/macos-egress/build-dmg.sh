#!/bin/bash
set -euo pipefail
umask 022

mode=${1-}
if [[ $mode != candidate && $mode != release ]] || [[ $# != 1 ]]; then
    echo 'Usage: build-dmg.sh candidate|release' >&2
    exit 2
fi
if [[ $(/usr/bin/uname -s) != Darwin || $EUID == 0 ]]; then
    echo 'Build the disk image as an unprivileged user on macOS.' >&2
    exit 1
fi
case "$(/usr/bin/uname -m)" in
    arm64) target=aarch64-apple-darwin ;;
    x86_64) target=x86_64-apple-darwin ;;
    *) echo 'Unsupported macOS architecture.' >&2; exit 1 ;;
esac

: "${CVBBMSO_SERVER_ORIGIN:?Set the monitoring HTTPS origin.}"
/usr/bin/python3 - <<'PY'
import os
from urllib.parse import urlsplit

source = os.environ['CVBBMSO_SERVER_ORIGIN']
try:
    origin = urlsplit(source)
    valid = (
        source.startswith('https://') and origin.hostname and origin.path in ('', '/')
        and origin.username is None and origin.password is None
        and '?' not in source and '#' not in source and origin.port != 0
        and not any(character.isspace() for character in source)
    )
except ValueError:
    valid = False
if not valid:
    raise SystemExit('CVBBMSO_SERVER_ORIGIN must be a valid HTTPS origin.')
PY

sign_options=(--sign -)
if [[ $mode == release ]]; then
    : "${CVBBMSO_MACOS_TEAM_ID:?Set the trusted Apple Team ID.}"
    : "${CVBBMSO_MACOS_SIGN_IDENTITY:?Select a Developer ID Application identity.}"
    : "${CVBBMSO_MACOS_NOTARY_PROFILE:?Select a notarytool keychain profile.}"
    if [[ ! $CVBBMSO_MACOS_TEAM_ID =~ ^[A-Z0-9]{10}$ ]]; then
        echo 'Invalid Apple Team ID.' >&2
        exit 1
    fi
    sign_options=(--options runtime --timestamp --sign "$CVBBMSO_MACOS_SIGN_IDENTITY")
fi

cd -- "$(dirname -- "$0")/../.."
root=$(pwd -P)
deployment="$root/deploy/macos-egress"
if [[ -n $(git status --porcelain) ]]; then
    echo 'Build the disk image from a clean checkout to identify its source.' >&2
    exit 1
fi
revision=$(git rev-parse HEAD)
unset TAURI_CONFIG
metadata=$(cargo metadata --locked --no-deps --format-version 1)
target_dir=$(printf '%s' "$metadata" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
version=$(printf '%s' "$metadata" | /usr/bin/python3 -c \
    'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "cvbbmso-client"))')
MACOSX_DEPLOYMENT_TARGET=14.0 cargo build --locked --release --target "$target" \
    -p cvbbmso-client -p cvbbmso-network --bin cvbbmso-client --bin cvbbmso-pf

artifact_dir="$target_dir/$target/release"
work=$(/usr/bin/mktemp -d "$artifact_dir/.dmg.XXXXXXXX")
trap '/bin/rm -rf -- "$work"' EXIT
trap 'exit 1' HUP INT TERM
stage="$work/image"
app="$stage/CVBBMSO.app"
contents="$app/Contents"
app_id=edu.buaa.accoding.cvbbmso
helper="$contents/Helpers/cvbbmso-pf"
/bin/mkdir -p "$contents/MacOS" "$contents/Helpers" "$contents/Resources"
/usr/bin/install -m 0755 "$artifact_dir/cvbbmso-client" "$contents/MacOS/cvbbmso-client"
/usr/bin/install -m 0755 "$artifact_dir/cvbbmso-pf" "$helper"
/usr/bin/install -m 0644 "$root/crates/cvbbmso-client/icons/icon.icns" "$contents/Resources/icon.icns"
/usr/bin/install -m 0644 "$deployment/Info.plist" "$contents/Info.plist"
/usr/bin/plutil -insert CFBundleShortVersionString -string "$version" "$contents/Info.plist"
/usr/bin/plutil -insert CFBundleVersion -string "$version" "$contents/Info.plist"
/usr/bin/plutil -insert CVBBMSOBuildRevision -string "$revision" "$contents/Info.plist"
/usr/bin/codesign --force "${sign_options[@]}" --identifier "$app_id.pf" "$helper"
/usr/bin/codesign --force "${sign_options[@]}" --identifier "$app_id" "$app"
/usr/bin/install -m 0644 "$root/docs/deployment/macos-client.md" "$stage/README.md"
/bin/ln -s /Applications "$stage/Applications"

leaf="CVBBMSO-$version-$target-${revision:0:12}.dmg"
if [[ $mode == candidate ]]; then
    leaf="${leaf%.dmg}-candidate.dmg"
fi
image="$work/$leaf"
/usr/bin/hdiutil create -quiet -format UDZO -fs HFS+ -volname CVBBMSO -srcfolder "$stage" "$image"
if [[ $mode == release ]]; then
    /usr/bin/codesign --timestamp --sign "$CVBBMSO_MACOS_SIGN_IDENTITY" "$image"
    /usr/bin/xcrun notarytool submit "$image" --keychain-profile "$CVBBMSO_MACOS_NOTARY_PROFILE" \
        --wait --output-format json > "$work/notary.json"
    [[ $(/usr/bin/plutil -extract status raw "$work/notary.json") == Accepted ]]
    /usr/bin/xcrun stapler staple "$image"
fi
"$deployment/verify-dmg.sh" "$image" "$mode"
/bin/mv -f -- "$image" "$artifact_dir/$leaf"
(
    cd -- "$artifact_dir"
    /usr/bin/shasum -a 256 -- "$leaf" > "$leaf.sha256"
    /usr/bin/shasum -a 256 -c "$leaf.sha256"
)
printf '%s\n' "$artifact_dir/$leaf"
