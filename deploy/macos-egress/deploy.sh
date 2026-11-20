#!/bin/bash -p
set -euo pipefail
export LC_ALL=C
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
umask 077

anchor=com.cvbbmso.egress
cleanup_label=com.cvbbmso.egress-cleanup
owner=0
group=0
stage=
lock=

set_paths() {
    system_root=$1
    library="${system_root%/}/Library"
    helper_dir="$library/PrivilegedHelperTools"
    helper="$helper_dir/com.cvbbmso.egress-watchdog"
    support="$library/Application Support"
    state_dir="$support/com.cvbbmso"
    launch_dir="$library/LaunchDaemons"
}

die() {
    printf '%s\n' "$*" >&2
    exit 1
}

exists() {
    [[ -e $1 || -L $1 ]]
}

protected() {
    local path=$1 type=$2 uid mode links listing
    [[ ! -L $path ]] || die "Symbolic link at $path."
    if [[ $type == directory ]]; then
        [[ -d $path ]] || die "Expected a directory at $path."
    else
        [[ -f $path ]] || die "Expected a regular file at $path."
    fi
    read -r uid mode links < <(/usr/bin/stat -f '%u %OLp %l' -- "$path") \
        || die "Cannot stat $path."
    [[ $uid == "$owner" && $mode =~ ^[0-7]+$ ]] \
        || die "Unexpected owner or mode at $path."
    (( (8#$mode & 022) == 0 )) || die "Group or other users can write $path."
    # Darwin ls -e lists extended ACL entries on subsequent lines.
    listing=$(/bin/ls -lde -- "$path") || die "Cannot read ACL for $path."
    [[ $listing != *$'\n'* ]] || die "Extended ACL at $path."
    if [[ $type == executable ]]; then
        [[ $links == 1 ]] || die "Multiple hard links at $path."
        (( (8#$mode & 07100) == 0100 )) || die "Unexpected executable mode at $path."
    fi
}

cleanup() {
    if [[ -n $stage ]]; then
        /bin/rm -f -- "$stage/watchdog"
        /bin/rmdir -- "$stage"
    fi
    if [[ -n $lock ]]; then
        /bin/rmdir -- "$lock"
    fi
}

lock_deployment() {
    local directory="$helper_dir/.cvbbmso-egress-deploy.lock"
    /bin/mkdir -- "$directory" || die "Cannot acquire $directory. Another deployment may be running."
    lock=$directory
    trap cleanup EXIT
    trap 'exit 1' HUP INT TERM
}

validate_digest() {
    [[ $1 =~ ^[0-9a-f]{64}$ ]] || die 'Expected a lowercase SHA-256 digest from a trusted build.'
}

digest() {
    local result
    result=$(/usr/bin/env -i /usr/bin/shasum -a 256 < "$1") || return 1
    printf '%s\n' "${result%% *}"
}

require_digest() {
    local actual
    actual=$(digest "$1") || die "Cannot hash $1."
    [[ $actual == "$2" ]] || die "Unexpected checksum for $1."
}

check_architecture() {
    local architecture format
    architecture=$(/usr/sbin/sysctl -n hw.optional.arm64 2>/dev/null) || architecture=0
    if [[ $architecture == 1 ]]; then
        architecture=arm64
    else
        architecture=x86_64
    fi
    format=$(/usr/bin/env -i LC_ALL=C /usr/bin/file -b -- "$1") || die "Cannot inspect $1."
    [[ $format == "Mach-O 64-bit executable $architecture" ||
        $format == "Mach-O 64-bit executable $architecture "* ]] \
        || die "Expected a native Mach-O executable for $architecture."
}

pfctl() {
    /sbin/pfctl "$@"
}

launchctl() {
    /bin/launchctl "$@"
}

read_pf() {
    pfctl "$@" || die "Cannot read PF ($*)."
}

check_idle() {
    local name job contents section
    if exists "$support"; then
        protected "$support" directory
    fi
    if exists "$state_dir"; then
        protected "$state_dir" directory
        for name in egress.state pf.rules pf.enable; do
            if exists "$state_dir/$name"; then
                die "Guard state at $state_dir/$name. Complete recovery first."
            fi
        done
    fi
    if exists "$launch_dir"; then
        protected "$launch_dir" directory
    fi
    if exists "$launch_dir/$cleanup_label.plist"; then
        die 'A guard recovery definition is present. Complete recovery first.'
    fi
    if job=$(launchctl print "system/$cleanup_label" 2>&1); then
        die 'A guard recovery job is loaded. Complete recovery first.'
    fi
    if [[ $job != *"Could not find service \"$cleanup_label\" in domain for system"* ]]; then
        printf '%s\n' "$job" >&2
        die 'Cannot establish that the guard recovery job is absent.'
    fi
    # Darwin's scoped queries can fail with EINVAL for a nonexistent anchor.
    contents=$(read_pf -s Anchors) || exit 1
    if /usr/bin/awk -v anchor="$anchor" '$1 == anchor {seen=1} END {exit !seen}' <<< "$contents"; then
        for section in rules nat Tables Anchors; do
            contents=$(read_pf -a "$anchor" -s "$section") || exit 1
            [[ ! $contents =~ [^[:space:]] ]] \
                || die "Reserved PF anchor has $section. Complete recovery or resolve the conflict first."
        done
    fi
}

check_hook() {
    local expected=$1 rules
    rules=$(read_pf -s rules) || exit 1
    /usr/bin/awk -v anchor="$anchor" -v expected="$expected" '
        {
            sub(/^[[:space:]]+/, "")
            sub(/[[:space:]]+$/, "")
            if ($0 == "" || $0 == "FILTER RULES:" ||
                $0 ~ /^(scrub(-anchor)?|no[[:space:]]+scrub)[[:space:]]+[^[:space:]]/) next
            if (!seen++) first = $0
            if ($1 == "anchor" &&
                (index($2, "\"" anchor "\"") == 1 || index($2, "\"" anchor "/") == 1)) count++
        }
        END {
            if (expected == "absent") exit (count != 0)
            exit (count != 1 || first != "anchor \"" anchor "\" out all")
        }
    ' <<< "$rules" || if [[ $expected == absent ]]; then
        die 'The reserved PF entry is still loaded. Remove it through the host PF configuration first.'
    else
        die 'Expected the reserved entry as the first live PF filter rule, without quick or duplicates.'
    fi
}

check_host_pf() {
    local contents
    check_hook present
    contents=$(read_pf -vv -s Interfaces) || exit 1
    if /usr/bin/awk '$1 != "lo0" && /(^|[[:space:]])\(skip\)($|[[:space:]])/ {found=1} END {exit !found}' <<< "$contents"; then
        die 'PF skips a non-loopback interface.'
    fi
    contents=$(read_pf -a '*' -s nat) || exit 1
    if /usr/bin/awk '$1 ~ /^(nat|rdr|binat)$/ && $2 == "pass" {found=1} END {exit !found}' <<< "$contents"; then
        die 'A PF translation rule bypasses filtering (nat/rdr/binat pass).'
    fi
}

lint_config() {
    [[ -f $1 && -r $1 ]] || die "Cannot read PF configuration $1."
    pfctl -n -o none -f "$1" || die 'PF configuration did not parse.'
}

install_helper() {
    local source=$1 checksum=$2 previous=${3-} current
    validate_digest "$checksum"
    if [[ -n $previous ]]; then validate_digest "$previous"; fi
    [[ -f $source && ! -L $source && -r $source ]] || die 'Expected a readable watchdog file.'
    if ! exists "$helper_dir"; then
        /bin/mkdir -m 0755 -- "$helper_dir"
    fi
    protected "$helper_dir" directory
    lock_deployment
    check_idle
    stage=$(/usr/bin/mktemp -d "$helper_dir/.cvbbmso-egress.XXXXXXXX")
    /bin/cat -- "$source" > "$stage/watchdog"
    require_digest "$stage/watchdog" "$checksum"
    check_architecture "$stage/watchdog"
    if exists "$helper"; then
        protected "$helper" executable
        current=$(digest "$helper") || die 'Cannot hash the existing watchdog.'
        if [[ $current == "$checksum" ]]; then
            printf '%s\n' 'The approved watchdog is already installed.'
            return
        fi
        [[ $current == "$previous" ]] || die 'Existing watchdog does not match the approved previous checksum.'
    fi
    /usr/sbin/chown "$owner:$group" "$stage/watchdog"
    /bin/chmod 0755 "$stage/watchdog"
    protected "$stage/watchdog" executable
    /bin/mv -fh -- "$stage/watchdog" "$helper"
    printf 'Installed %s\n' "$helper"
}

check_installation() {
    validate_digest "$1"
    protected "$helper_dir" directory
    protected "$helper" executable
    require_digest "$helper" "$1"
    check_architecture "$helper"
    check_idle
    check_host_pf
    if [[ $# == 2 ]]; then lint_config "$2"; fi
    printf '%s\n' 'The approved watchdog and live PF entry are ready.'
}

remove_helper() {
    validate_digest "$1"
    if ! exists "$helper_dir"; then
        printf '%s\n' 'The watchdog is already absent.'
        return
    fi
    protected "$helper_dir" directory
    lock_deployment
    if ! exists "$helper"; then
        printf '%s\n' 'The watchdog is already absent.'
        return
    fi
    protected "$helper" executable
    require_digest "$helper" "$1"
    check_idle
    check_hook absent
    /bin/rm -- "$helper"
    printf 'Removed %s\n' "$helper"
}

usage() {
    cat >&2 <<'EOF'
Usage:
  deploy.sh lint PF_CONF
  sudo deploy.sh install WATCHDOG SHA256 [PREVIOUS_SHA256]
  sudo deploy.sh check SHA256 [PF_CONF]
  sudo deploy.sh remove SHA256
EOF
    exit 2
}

main() {
    local command=${1-}
    case $command in
        lint|remove) [[ $# == 2 ]] || usage ;;
        check) [[ $# == 2 || $# == 3 ]] || usage ;;
        install) [[ $# == 3 || $# == 4 ]] || usage ;;
        *) usage ;;
    esac
    shift
    [[ $(/usr/bin/uname -s) == Darwin ]] || die 'This tool requires macOS.'
    if [[ $command == lint ]]; then
        lint_config "$1"
        return
    fi
    [[ $EUID == 0 ]] || die 'Run this operation as root.'
    set_paths /
    protected "$system_root" directory
    protected "$library" directory
    case $command in
        install) install_helper "$@" ;;
        check) check_installation "$@" ;;
        remove) remove_helper "$@" ;;
    esac
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    main "$@"
fi
