#!/bin/bash
set -euo pipefail

if [[ $# != 2 || ( $2 != candidate && $2 != release ) ]]; then
    echo 'Usage: verify-dmg.sh IMAGE candidate|release' >&2
    exit 2
fi
image=$1
mode=$2
mount=$(/usr/bin/mktemp -d)
attached=false
cleanup() {
    if $attached; then /usr/bin/hdiutil detach -quiet "$mount" || return; fi
    /bin/rmdir "$mount"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
/usr/bin/hdiutil verify -quiet "$image"
/usr/bin/hdiutil attach -quiet -readonly -nobrowse -noautoopen -mountpoint "$mount" "$image"
attached=true

app="$mount/CVBBMSO.app"
client="$app/Contents/MacOS/cvbbmso-client"
helper="$app/Contents/Helpers/cvbbmso-pf"
info="$app/Contents/Info.plist"
app_id=edu.buaa.accoding.cvbbmso
key() { /usr/bin/plutil -extract "$1" raw "$info"; }
[[ -L "$mount/Applications" && $(/usr/bin/readlink "$mount/Applications") == /Applications ]]
[[ $(key CFBundleIdentifier) == "$app_id" && $(key CFBundleExecutable) == cvbbmso-client ]]
[[ $(key LSMinimumSystemVersion) == 14.0 ]]
revision=$(key CVBBMSOBuildRevision)
[[ $revision =~ ^[a-f0-9]{40}$ ]]
[[ -f $mount/README.md && ! -d $app/Contents/Library/LaunchDaemons ]]
arch=$(/usr/bin/lipo -archs "$client")
[[ $arch == arm64 || $arch == x86_64 ]]
for file in "$client" "$helper"; do
    [[ -x $file && $(/usr/bin/lipo -archs "$file") == "$arch" ]]
    /usr/bin/xcrun vtool -show-build "$file" | /usr/bin/awk '
        $1 == "platform" && $2 == "MACOS" { mac = 1 }
        $1 == "minos" && ($2 == "14.0" || $2 == "14.0.0") { supported = 1 }
        END { exit !(mac && supported) }
    '
    if [[ -n ${CVBBMSO_SERVER_ORIGIN:-} ]]; then
        LC_ALL=C /usr/bin/grep -aFq -- "$CVBBMSO_SERVER_ORIGIN" "$file"
    fi
done
/usr/bin/codesign --verify --strict --deep --all-architectures -- "$app"
if [[ $mode == release ]]; then
    team=${CVBBMSO_MACOS_TEAM_ID:?Set the expected Apple Team ID.}
    [[ $team =~ ^[A-Z0-9]{10}$ ]]
    /usr/bin/codesign --verify --test-requirement="=anchor apple generic and identifier \"$app_id\" and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = \"$team\"" "$app"
    /usr/bin/codesign --verify --test-requirement="=anchor apple generic and identifier \"$app_id.pf\" and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = \"$team\"" "$helper"
    /usr/bin/xcrun stapler validate "$image"
fi
printf 'Verified the CVBBMSO %s disk image (%s, macOS 14.0+) at %s.\n' "$mode" "$arch" "$revision"
