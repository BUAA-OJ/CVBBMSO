#!/bin/bash
set -euo pipefail

origin='@CVBBMSO_TRIAL_ORIGIN@'
payload_sha256='@CVBBMSO_TRIAL_PAYLOAD_SHA256@'

if [[ $# -gt 1 || ( $# == 1 && ${1-} != --check ) ]]; then
    echo "Usage: ${0##*/} [--check]" >&2
    exit 2
fi
if [[ $EUID == 0 || $(/usr/bin/uname -m) != arm64 ]]; then
    echo 'Run this command as a regular user on Apple Silicon.' >&2
    exit 1
fi
macos=$(/usr/bin/sw_vers -productVersion)
if [[ ! ${macos%%.*} =~ ^[0-9]+$ || ${macos%%.*} -lt 14 ]]; then
    echo 'CVBBMSO Trial requires macOS 14 or later.' >&2
    exit 1
fi

url="${origin%/}/api/health"
# Use the system TLS trust and ignore user curl, CA, and proxy overrides.
if ! status=$(/usr/bin/env -u CURL_CA_BUNDLE -u SSL_CERT_FILE -u SSL_CERT_DIR \
    /usr/bin/curl -q --silent --show-error --fail --proto '=https' --noproxy '*' \
    --connect-timeout 5 --max-time 10 --output /dev/null --write-out '%{http_code}' --url "$url") \
    || [[ $status != 200 ]]; then
    printf 'HTTPS preflight failed for %s. Check the local reverse proxy and its trusted certificate.\n' "$url" >&2
    exit 1
fi
printf 'HTTPS preflight passed for %s.\n' "$url"
if [[ ${1-} == --check ]]; then
    exit 0
fi

bundle=$(CDPATH= cd -P -- "$(/usr/bin/dirname -- "$0")" && pwd -P)
# Pass the installer code already read by this process to sudo. A separate
# script or executable from the unpacked ZIP must not be reopened as root.
installer=$(/bin/cat <<'INSTALLER'
set -euo pipefail
umask 022

payload=$1
expected_sha256=$2
support='/Library/Application Support'
trial_home="$support/com.cvbbmso-trial"
helpers=/Library/PrivilegedHelperTools
helper="$helpers/com.cvbbmso.pf-trial"
launcher="$helpers/cvbbmso-pf-trial.sh"
installed_app="$trial_home/CVBBMSO Trial.app"
receipt="$trial_home/payload.sha256"
session_lock="$trial_home/session.lock"

fail() {
    printf '%s\n' "$*" >&2
    exit 1
}

exists() {
    [[ -e $1 || -L $1 ]]
}

protected() {
    local entry=$1 metadata mode listing
    metadata=$(/usr/bin/stat -f '%u:%OLp' "$entry") || fail "Cannot read $entry."
    mode=${metadata#*:}
    listing=$(/bin/ls -lde "$entry")
    if [[ -L $entry || $metadata != 0:* || ! $mode =~ ^[0-7]+$ || $listing == *$'\n'* ]] \
        || (( (8#$mode & 022) != 0 )); then
        fail "Regular users must not be able to change $entry."
    fi
}

protected /Library
protected "$support"
if ! exists "$helpers"; then
    /bin/mkdir -m 0755 "$helpers"
fi
protected "$helpers"

installed_sha256=
if exists "$launcher"; then
    protected "$trial_home"
    protected "$launcher"
    [[ -f $launcher && -x $launcher && -f $receipt ]] \
        || fail 'The installed trial is incomplete.'
    protected "$receipt"
    installed_sha256=$(< "$receipt")
    [[ $installed_sha256 =~ ^[a-f0-9]{64}$ ]] \
        || fail 'The installed trial has an invalid payload receipt.'
    if [[ $installed_sha256 == "$expected_sha256" ]]; then
        exec "$launcher" run
    fi
elif exists "$helper" || exists "$trial_home"; then
    fail 'A partial trial installation exists. Check the old installation before retrying.'
fi

[[ -f $payload && ! -L $payload && $expected_sha256 =~ ^[a-f0-9]{64}$ ]] \
    || fail 'The trial payload is missing. Extract the approved ZIP again.'
scratch=$(/usr/bin/mktemp -d "$helpers/.cvbbmso-trial.XXXXXXXX")
made_home=false
committed=false
cleanup() {
    if ! $committed; then
        if [[ -d $scratch/previous-app ]]; then
            /bin/rm -rf -- "$installed_app"
            /bin/mv -f -- "$scratch/previous-helper" "$helper"
            /bin/mv -f -- "$scratch/previous-launcher" "$launcher"
            /bin/mv -f -- "$scratch/previous-receipt" "$receipt"
            /bin/mv -- "$scratch/previous-app" "$installed_app"
        elif $made_home; then
            /bin/rm -f -- "$helper" "$launcher"
            /bin/rm -rf -- "$trial_home"
        fi
    fi
    /bin/rm -rf -- "$scratch"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

/bin/cp -- "$payload" "$scratch/payload.zip"
checksum=$(/usr/bin/shasum -a 256 < "$scratch/payload.zip")
[[ ${checksum%% *} == "$expected_sha256" ]] \
    || fail 'The trial payload checksum did not match. Extract the approved ZIP again.'
/usr/bin/ditto -x -k --noacl "$scratch/payload.zip" "$scratch/files"
app="$scratch/files/CVBBMSO Trial.app"
/usr/bin/codesign --verify --strict --all-architectures "$app"
/usr/sbin/chown -R root:wheel "$app"
/bin/chmod -R go-w "$app"
/usr/bin/install -o root -g wheel -m 0755 "$scratch/files/cvbbmso-pf-trial" "$scratch/helper"
/usr/bin/install -o root -g wheel -m 0755 "$scratch/files/pf-trial.sh" "$scratch/launcher"
printf '%s\n' "$expected_sha256" > "$scratch/receipt"

if [[ -n $installed_sha256 ]]; then
    protected "$installed_app"
    protected "$helper"
    "$launcher" recover
    protected "$session_lock"
    [[ -f $session_lock ]] || fail 'Invalid trial session lock.'
    # Take the same lock as the helper, on a descriptor held by this shell.
    exec 9<>"$session_lock"
    /usr/bin/perl -MFcntl=:flock -e '
        open(my $lock, "+<&=9") or die "Cannot open the trial session lock: $!\n";
        exit(!flock($lock, LOCK_EX | LOCK_NB));
    ' || fail 'A trial session is active. Close it before updating.'
    [[ $(< "$receipt") == "$installed_sha256" ]] \
        || fail 'The trial was updated by another launcher. Open this command again.'
    /bin/cp -p -- "$helper" "$scratch/previous-helper"
    /bin/cp -p -- "$launcher" "$scratch/previous-launcher"
    /bin/cp -p -- "$receipt" "$scratch/previous-receipt"
    /bin/mv -- "$installed_app" "$scratch/previous-app"
else
    # Reserving this directory serializes first launches.
    /bin/mkdir -m 0755 "$trial_home"
    made_home=true
fi

/bin/mv -- "$app" "$installed_app"
/bin/mv -f -- "$scratch/helper" "$helper"
"$helper" --trial-check
/bin/mv -f -- "$scratch/receipt" "$receipt"
/bin/mv -f -- "$scratch/launcher" "$launcher"
committed=true
cleanup
trap - EXIT HUP INT TERM
exec 9>&-
exec "$launcher" run
INSTALLER
)
exec /usr/bin/sudo -- /usr/bin/env -i LC_ALL=C /bin/bash -p -c "$installer" \
    cvbbmso-trial "$bundle/Payload.zip" "$payload_sha256"
