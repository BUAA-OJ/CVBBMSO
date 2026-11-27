#!/bin/bash
set -euo pipefail

origin='@CVBBMSO_TRIAL_ORIGIN@'
launcher='@CVBBMSO_TRIAL_LAUNCHER@'

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

if [[ ! -x $launcher ]]; then
    echo 'Ask the administrator to install the approved trial launcher before opening the app.' >&2
    exit 1
fi
for entry in /Library /Library/PrivilegedHelperTools "$launcher"; do
    metadata=$(/usr/bin/stat -f '%u:%OLp' "$entry")
    mode=${metadata#*:}
    listing=$(/bin/ls -lde "$entry")
    if [[ -L $entry || $metadata != 0:* || ! $mode =~ ^[0-7]+$ || $listing == *$'\n'* ]] \
        || (( (8#$mode & 022) != 0 )); then
        echo 'The administrator trial launcher path is not protected from changes by regular users.' >&2
        exit 1
    fi
done
if [[ ! -f $launcher ]]; then
    echo 'The administrator trial launcher is not a regular file.' >&2
    exit 1
fi
exec /usr/bin/sudo -- "$launcher" run
