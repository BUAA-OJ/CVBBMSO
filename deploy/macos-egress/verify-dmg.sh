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
info="$app/Contents/Info.plist"
label=edu.buaa.accoding.cvbbmso.egress
daemon="$app/Contents/Library/LaunchDaemons/$label.plist"
key() { /usr/bin/plutil -extract "$1" raw "$2"; }
[[ -L "$mount/Applications" && $(/usr/bin/readlink "$mount/Applications") == /Applications ]]
[[ $(key CFBundleIdentifier "$info") == edu.buaa.accoding.cvbbmso ]]
[[ $(key CFBundleExecutable "$info") == cvbbmso-client ]]
[[ $(key CVBBMSOPrivilegedServiceVersion "$info") == 1 ]]
[[ $(key LSMinimumSystemVersion "$info") == 14.0 ]]
[[ -x "$app/Contents/MacOS/cvbbmso-client" ]]
[[ $(key Label "$daemon") == "$label" ]]
program=$(key BundleProgram "$daemon")
[[ $program == Contents/MacOS/cvbbmso-egress-service && -x "$app/$program" ]]
[[ $(/usr/libexec/PlistBuddy -c "Print :MachServices:$label" "$daemon") == true ]]
[[ $(key SpawnConstraint.signing-identifier "$daemon") == "$label" ]]
[[ $(key SpawnConstraint.validation-category "$daemon") == 6 ]]
team=$(key SpawnConstraint.team-identifier "$daemon")
if [[ $mode == release ]]; then
    [[ $team =~ ^[A-Z0-9]{10}$ ]]
    /usr/bin/codesign --verify --strict --deep --all-architectures "$app"
    /usr/bin/codesign --verify --test-requirement="=anchor apple generic and identifier \"$label\" and certificate leaf[subject.OU] = \"$team\" and certificate leaf[field.1.2.840.113635.100.6.1.13] exists" "$app/$program"
    /usr/bin/xcrun stapler validate "$image"
else
    [[ -z $team ]]
fi
printf 'Verified %s disk image. Installed service path: /Applications/CVBBMSO.app/%s\n' "$mode" "$program"
