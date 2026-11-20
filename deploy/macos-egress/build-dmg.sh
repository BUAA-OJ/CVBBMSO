#!/bin/bash
set -euo pipefail

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

if [[ $mode == release ]]; then
    : "${CVBBMSO_MACOS_TEAM_ID:?Set the trusted Apple Team ID.}"
    : "${CVBBMSO_MACOS_SIGN_IDENTITY:?Select a Developer ID Application identity.}"
    : "${CVBBMSO_MACOS_NOTARY_PROFILE:?Select a notarytool keychain profile.}"
    : "${CVBBMSO_SERVER_ORIGIN:?Set the monitoring HTTPS origin for this release.}"
    if [[ ! $CVBBMSO_MACOS_TEAM_ID =~ ^[A-Z0-9]{10}$ ]]; then
        echo 'Invalid Apple Team ID.' >&2
        exit 1
    fi
    /usr/bin/python3 - <<'PY'
import os
from urllib.parse import urlsplit

try:
    origin = urlsplit(os.environ['CVBBMSO_SERVER_ORIGIN'])
    valid = (
        origin.scheme == 'https' and origin.hostname and origin.path in ('', '/')
        and origin.username is None and origin.password is None
        and not origin.query and not origin.fragment and origin.port != 0
    )
except ValueError:
    valid = False
if not valid:
    raise SystemExit('CVBBMSO_SERVER_ORIGIN must be a valid HTTPS origin.')
PY
else
    unset CVBBMSO_MACOS_TEAM_ID CVBBMSO_SERVER_ORIGIN
fi

cd -- "$(dirname -- "$0")/../.."
root=$(pwd -P)
deployment="$root/deploy/macos-egress"
artifact_dir="$root/target/$target/release"
revision=$(git rev-parse --short=12 HEAD)
if [[ $mode == release && -n $(git status --porcelain) ]]; then
    echo 'Build a release from a clean checkout.' >&2
    exit 1
fi
version=$(cargo metadata --locked --no-deps --format-version 1 | /usr/bin/python3 -c \
    'import json, sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "cvbbmso-client"))')
MACOSX_DEPLOYMENT_TARGET=14.0 cargo build --locked --release --target "$target" --target-dir "$root/target" \
    -p cvbbmso-client -p cvbbmso-network --bin cvbbmso-client --bin cvbbmso-egress-service

work=$(/usr/bin/mktemp -d "$artifact_dir/.dmg.XXXXXXXX")
trap '/bin/rm -rf -- "$work"' EXIT
trap 'exit 1' HUP INT TERM
stage="$work/image"
app="$stage/CVBBMSO.app"
contents="$app/Contents"
app_id=edu.buaa.accoding.cvbbmso
service_id="$app_id.egress"
helper="$contents/MacOS/cvbbmso-egress-service"
/bin/mkdir -p "$contents/MacOS" "$contents/Resources" "$contents/Library/LaunchDaemons"
/usr/bin/install -m 0755 "$artifact_dir/cvbbmso-client" "$contents/MacOS/cvbbmso-client"
/usr/bin/install -m 0755 "$artifact_dir/cvbbmso-egress-service" "$helper"
/usr/bin/install -m 0644 "$root/crates/cvbbmso-client/icons/icon.icns" "$contents/Resources/icon.icns"
/usr/bin/install -m 0644 "$deployment/$service_id.plist" "$contents/Library/LaunchDaemons/$service_id.plist"
/usr/bin/install -m 0644 "$deployment/Info.plist" "$contents/Info.plist"
/usr/bin/plutil -insert CFBundleShortVersionString -string "$version" "$contents/Info.plist"
/usr/bin/plutil -insert CFBundleVersion -string "$version" "$contents/Info.plist"
/bin/ln -s /Applications "$stage/Applications"

if [[ $mode == release ]]; then
    identity=$CVBBMSO_MACOS_SIGN_IDENTITY
    team=$CVBBMSO_MACOS_TEAM_ID
    /usr/bin/plutil -replace SpawnConstraint.team-identifier -string "$team" "$contents/Library/LaunchDaemons/$service_id.plist"
    /usr/bin/codesign --force --options runtime --timestamp --sign "$identity" --identifier "$service_id" "$helper"
    /usr/bin/codesign --force --options runtime --timestamp --sign "$identity" --identifier "$app_id" "$app"
    /usr/bin/codesign --verify --strict --deep --all-architectures "$app"
    for item in "$helper" "$app"; do
        /usr/bin/codesign --verify --test-requirement="=anchor apple generic and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = \"$team\"" "$item"
    done
    leaf="CVBBMSO-$version-$target-$revision.dmg"
else
    leaf="CVBBMSO-$version-$target-$revision-unsigned-candidate.dmg"
fi

image="$work/$leaf"
/usr/bin/hdiutil create -quiet -format UDZO -fs HFS+ -volname CVBBMSO -srcfolder "$stage" "$image"
if [[ $mode == release ]]; then
    /usr/bin/codesign --timestamp --sign "$identity" "$image"
    /usr/bin/xcrun notarytool submit "$image" --keychain-profile "$CVBBMSO_MACOS_NOTARY_PROFILE" --wait --output-format json > "$work/notary.json"
    if [[ $(/usr/bin/plutil -extract status raw "$work/notary.json") != Accepted ]]; then
        echo 'Apple notarization did not accept the image.' >&2
        exit 1
    fi
    /usr/bin/xcrun stapler staple "$image"
    /usr/bin/xcrun stapler validate "$image"
fi
"$deployment/verify-dmg.sh" "$image" "$mode"
/bin/mv -f "$image" "$artifact_dir/$leaf"
/usr/bin/shasum -a 256 -- "$artifact_dir/$leaf"
