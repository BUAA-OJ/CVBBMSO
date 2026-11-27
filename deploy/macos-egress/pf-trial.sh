#!/bin/bash -p
set -euo pipefail
umask 077

if [[ $# != 1 ]]; then
    printf '%s\n' 'Usage: cvbbmso-pf-trial.sh run|status|recover' >&2
    exit 2
fi

if [[ $EUID != 0 || $0 != /Library/PrivilegedHelperTools/cvbbmso-pf-trial.sh ]]; then
    printf '%s\n' 'Run the root-installed script at /Library/PrivilegedHelperTools/cvbbmso-pf-trial.sh with sudo.' >&2
    exit 1
fi

case "$1" in
    run) mode=--trial ;;
    status) mode=--trial-check ;;
    recover) mode=--trial-recover ;;
    *) printf '%s\n' 'Usage: cvbbmso-pf-trial.sh run|status|recover' >&2; exit 2 ;;
esac
exec /usr/bin/env -i LC_ALL=C /Library/PrivilegedHelperTools/com.cvbbmso.pf-trial "$mode"
