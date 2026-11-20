#!/bin/bash
# shellcheck source-path=SCRIPTDIR
set -euo pipefail
watchdog=${1-}
cd -- "$(dirname -- "$0")"
script="$PWD/deploy.sh"
fixture="$(/usr/bin/mktemp -d)"
trap '/bin/rm -rf -- "$fixture"' EXIT

mkdir -p "$fixture/root/Library/Application Support/com.cvbbmso" "$fixture/root/Library/LaunchDaemons"
printf 'watchdog v1\n' > "$fixture/v1"
printf 'watchdog v2\n' > "$fixture/v2"
sha1="$(/usr/bin/shasum -a 256 < "$fixture/v1")"
sha1=${sha1%% *}
sha2="$(/usr/bin/shasum -a 256 < "$fixture/v2")"
sha2=${sha2%% *}

valid_rules=$'FILTER RULES:\n\n scrub-anchor "com.apple/*" all\n scrub out all\n no scrub out all\n anchor "com.cvbbmso.egress" out all\n block drop out all'
fixture_rules=$valid_rules
interfaces=$'lo0 (skip)\nen0'
translation='nat on en0 from 192.0.2.0/24 to any -> 198.51.100.1'
rules_inside=
tables_inside=
nested=
anchors='  com.apple'
launch_status=absent

run() (
    # Exercise deployment logic with the real Darwin filesystem tools. Only
    # this test process changes the paths and replaces network queries.
    # shellcheck source=deploy.sh
    source "$script"
    owner=$EUID
    group=$(/usr/bin/id -g)
    set_paths "$fixture/root"
    protected "$system_root" directory
    protected "$library" directory

    # shellcheck disable=SC2329
    check_architecture() {
        grep -q '^watchdog v[12]$' "$1"
    }
    # shellcheck disable=SC2329
    pfctl() {
        case "$*" in
            '-s rules') printf '%s\n' "$fixture_rules" ;;
            '-s Anchors') printf '%s\n' "$anchors" ;;
            '-vv -s Interfaces') printf '%s\n' "$interfaces" ;;
            '-a * -s nat') printf '%s\n' "$translation" ;;
            '-a com.cvbbmso.egress -s rules') printf '%s\n' "$rules_inside" ;;
            '-a com.cvbbmso.egress -s nat') : ;;
            '-a com.cvbbmso.egress -s Tables') printf '%s\n' "$tables_inside" ;;
            '-a com.cvbbmso.egress -s Anchors') printf '%s\n' "$nested" ;;
            "-n -o none -f $fixture/pf.conf") : ;;
            *) printf 'Unexpected PF command: %s\n' "$*" >> "$fixture/unexpected"; exit 97 ;;
        esac
    }
    # shellcheck disable=SC2329
    launchctl() {
        [[ $* == 'print system/com.cvbbmso.egress-cleanup' ]] || {
            printf 'Unexpected launchctl command: %s\n' "$*" >> "$fixture/unexpected"; exit 97;
        }
        case $launch_status in
            absent) echo 'Could not find service "com.cvbbmso.egress-cleanup" in domain for system' >&2; return 113 ;;
            loaded) return 0 ;;
            error) echo 'Permission denied' >&2; return 1 ;;
        esac
    }
    "$@"
)

reject() {
    local message=$1 output
    shift
    if output=$(run "$@" 2>&1) || [[ $output != *"$message"* ]]; then
        printf 'Expected rejection containing %s, got: %s\n' "$message" "$output" >&2
        exit 1
    fi
}

helper_dir="$fixture/root/Library/PrivilegedHelperTools"
helper="$helper_dir/com.cvbbmso.egress-watchdog"
state="$fixture/root/Library/Application Support/com.cvbbmso"
cp pf-entry.conf "$fixture/pf.conf"

run install_helper "$fixture/v1" "$sha1"
[[ $(stat -f '%OLp %l' "$helper") == '755 1' ]]
installed_inode=$(stat -f '%i' "$helper")
anchors=$'  com.apple\n  com.cvbbmso.egress'
run install_helper "$fixture/v1" "$sha1"
[[ $(stat -f '%i' "$helper") == "$installed_inode" ]]
run check_installation "$sha1" "$fixture/pf.conf"
reject 'Unexpected checksum' install_helper "$fixture/v1" "$sha2" "$sha1"
reject 'approved previous checksum' install_helper "$fixture/v2" "$sha2"
reject 'The reserved PF entry is still loaded' remove_helper "$sha1"

fixture_rules=$'pass out quick all\nanchor "com.cvbbmso.egress" out all'
reject 'first live PF filter rule' check_installation "$sha1"
fixture_rules='anchor "com.cvbbmso.egress" out quick all'
reject 'first live PF filter rule' check_installation "$sha1"
fixture_rules="$valid_rules"$'\nanchor "com.cvbbmso.egress" out all'
reject 'without quick or duplicates' check_installation "$sha1"
fixture_rules=$'scrub out all\nblock drop out all'
reject 'first live PF filter rule' check_installation "$sha1"
fixture_rules=$valid_rules
interfaces=$'lo0 (skip)\nen0 (skip)'
reject 'non-loopback' check_installation "$sha1"
interfaces='lo0 (skip)'
translation='rdr pass on en0 inet from any to any -> 192.0.2.4'
reject 'bypasses filtering' check_installation "$sha1"
translation=
tables_inside='<reserved>'
reject 'Reserved PF anchor has Tables' install_helper "$fixture/v2" "$sha2" "$sha1"
tables_inside=
nested='com.cvbbmso.egress/sub'
reject 'Reserved PF anchor has Anchors' check_installation "$sha1"
nested=
launch_status=loaded
reject 'recovery job is loaded' install_helper "$fixture/v2" "$sha2" "$sha1"
launch_status=error
reject 'Cannot establish' check_installation "$sha1"
launch_status=absent
touch "$state/pf.enable"
reject 'Complete recovery first' install_helper "$fixture/v2" "$sha2" "$sha1"
rm "$state/pf.enable"

chmod g+w "$helper_dir"
reject 'Group or other users can write' check_installation "$sha1"
chmod g-w "$helper_dir"
chmod +a 'everyone allow read' "$helper"
reject 'Extended ACL' check_installation "$sha1"
chmod -N "$helper"
chmod +a 'everyone allow list' "$helper_dir"
reject 'Extended ACL' check_installation "$sha1"
chmod -N "$helper_dir"
ln "$helper" "$fixture/linked"
reject 'Multiple hard links' install_helper "$fixture/v2" "$sha2" "$sha1"
rm "$fixture/linked"
mv "$helper" "$fixture/displaced"
ln -s "$fixture/displaced" "$helper"
reject 'Symbolic link' remove_helper "$sha1"
rm "$helper"
mv "$fixture/displaced" "$helper"

mkdir "$helper_dir/.cvbbmso-egress-deploy.lock"
reject 'Cannot acquire' install_helper "$fixture/v2" "$sha2" "$sha1"
rmdir "$helper_dir/.cvbbmso-egress-deploy.lock"
[[ $(stat -f '%i' "$helper") == "$installed_inode" ]]
run install_helper "$fixture/v2" "$sha2" "$sha1"
run check_installation "$sha2"
fixture_rules='block drop out all'
anchors='  com.apple'
reject 'Unexpected checksum' remove_helper "$sha1"
run remove_helper "$sha2"
run remove_helper "$sha2"
[[ ! -e $helper && ! -e $fixture/unexpected ]]
[[ -z $(ls -A "$helper_dir") ]]
cmp pf-entry.conf "$fixture/pf.conf"
printf '%s\n' 'macOS deployment filesystem and PF/launchd fixture checks passed.'

{
    cat <<'EOF'
set skip on lo0
scrub-anchor "com.apple/*"
nat-anchor "com.apple/*"
rdr-anchor "com.apple/*"
dummynet-anchor "com.apple/*"
EOF
    cat pf-entry.conf
    printf '%s\n' 'anchor "com.apple/*"' 'block drop out proto tcp to 203.0.113.1 port 443'
} > "$fixture/host.conf"
./deploy.sh lint "$fixture/host.conf"
/sbin/pfctl -n -o none -v -f "$fixture/host.conf" > "$fixture/parsed"
grep -Fx 'anchor "com.cvbbmso.egress" out all' "$fixture/parsed"
grep -Fx 'block drop out inet proto tcp from any to 203.0.113.1 port = 443' "$fixture/parsed"
printf '%s\n' 'anchor "com.cvbbmso.egress" nonsense' > "$fixture/invalid.conf"
if ./deploy.sh lint "$fixture/invalid.conf" > "$fixture/invalid.log" 2>&1; then
    echo 'Invalid configuration unexpectedly parsed.' >&2
    exit 1
fi
grep -q 'PF configuration did not parse' "$fixture/invalid.log"

if [[ -n $watchdog ]]; then
    (
        # shellcheck source=deploy.sh
        source "$script"
        check_architecture "$watchdog"
    )
fi
printf '%s\n' 'macOS pfctl dry-run checks passed.'
