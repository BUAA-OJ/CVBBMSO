set -euo pipefail
umask 022

source_app=$1
expected_cdhash=$2
install_dir='/Library/Application Support/com.cvbbmso-client'
installed_app="$install_dir/CVBBMSO.app"
helper_path='Contents/Helpers/cvbbmso-pf'
helper="$installed_app/$helper_path"
lock="$install_dir/session.lock"

fail() {
    printf '%s\n' "$*" >&2
    exit 1
}

protected() {
    local entry=$1 metadata mode listing
    [[ ! -L $entry ]] || fail "Unexpected symbolic link: $entry."
    metadata=$(/usr/bin/stat -f '%u:%OLp' "$entry") || fail "Cannot read $entry."
    mode=${metadata#*:}
    listing=$(/bin/ls -lde "$entry")
    if [[ $metadata != 0:* || ! $mode =~ ^[0-7]+$ || $listing == *$'\n'* ]] \
        || (( (8#$mode & 022) != 0 )); then
        fail "Regular users must not be able to change $entry."
    fi
}

cdhash() {
    /usr/bin/codesign --display --verbose=4 -- "$1" 2>&1 | /usr/bin/sed -n 's/^CDHash=//p'
}

[[ $EUID == 0 && $source_app == /*.app && $expected_cdhash =~ ^[a-f0-9]{40}$ ]] \
    || fail 'Invalid application installation request.'
protected /Library
protected '/Library/Application Support'
/bin/mkdir -p -m 0755 "$install_dir"
protected "$install_dir"

previous_cdhash=
if [[ -e $installed_app || -L $installed_app ]]; then
    protected "$installed_app"
    protected "$installed_app/Contents"
    protected "$installed_app/Contents/Helpers"
    protected "$helper"
    previous_cdhash=$(cdhash "$installed_app")
    [[ $previous_cdhash =~ ^[a-f0-9]{40}$ ]] || fail 'The installed application has no valid code signature.'
    if [[ $previous_cdhash == "$expected_cdhash" ]]; then
        exec "$helper" launch
    fi
    "$helper" recover
fi

# The installed helper uses the same lock for launches and recovery.
[[ ! -L $lock ]] || fail 'Unexpected session lock.'
if [[ ! -e $lock ]]; then
    (umask 077; : >> "$lock")
fi
protected "$lock"
[[ -f $lock ]] || fail 'Invalid session lock.'
exec 9<>"$lock"
/usr/bin/perl -MFcntl=:flock -e '
    open(my $lock, "+<&=9") or die "Cannot open the session lock: $!\n";
    exit(!flock($lock, LOCK_EX | LOCK_NB));
' || fail 'CVBBMSO is active. Close it before updating.'
if [[ -n $previous_cdhash ]]; then
    [[ $(cdhash "$installed_app") == "$previous_cdhash" ]] || fail 'CVBBMSO was updated. Open the app again.'
elif [[ -e $installed_app || -L $installed_app ]]; then
    fail 'CVBBMSO was installed. Open the app again.'
fi

scratch=$(/usr/bin/mktemp -d "$install_dir/.install.XXXXXXXX")
replacing=false
cleanup() {
    if $replacing && [[ -d $scratch/previous.app ]]; then
        /bin/rm -rf -- "$installed_app"
        /bin/mv -- "$scratch/previous.app" "$installed_app" || return
    elif $replacing && [[ -z $previous_cdhash ]]; then
        /bin/rm -rf -- "$installed_app"
    fi
    /bin/rm -rf -- "$scratch"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

staged="$scratch/CVBBMSO.app"
/usr/bin/ditto --noacl "$source_app" "$staged"
[[ -z $(/usr/bin/find "$staged" ! -type f ! -type d -print -quit) && -x $staged/$helper_path ]] \
    || fail 'Unexpected files in the application bundle.'
/usr/bin/codesign --verify --strict --deep --all-architectures -- "$staged"
[[ $(cdhash "$staged") == "$expected_cdhash" ]] || fail 'The application changed after launch.'
/usr/sbin/chown -R root:wheel "$staged"
/bin/chmod -R go-w "$staged"

replacing=true
if [[ -n $previous_cdhash ]]; then
    /bin/mv -- "$installed_app" "$scratch/previous.app"
fi
/bin/mv -- "$staged" "$installed_app"
"$helper" status
replacing=false
cleanup
trap - EXIT HUP INT TERM
exec 9>&-
exec "$helper" launch
