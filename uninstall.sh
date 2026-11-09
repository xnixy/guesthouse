#!/bin/sh
# Removes what install.sh installed, and nothing else.
#
# It works from ~/.local/lib/guesthouse/manifest, so any copy of this file does the same job, and it
# needs no network. A file goes only if it's still the one that was installed: it's left alone if its
# hash has changed, a folder goes only once it's empty, and anything outside your own folders is refused
# whatever the manifest says. The one exception is QEMU's bridge helper, which root put in the folder
# named below, and root takes away. Your machines and their disks stay, unless you pass
# --delete-machines and then type the folder's name.
set -eu

MANIFEST_FORMAT=1
TAB=$(printf '\t')
# The same as install.sh's. This path is never read from the manifest, which you can write.
BRIDGE_HELPER_FOLDER=/usr/local/libexec/guesthouse
BRIDGE_HELPER=$BRIDGE_HELPER_FOLDER/qemu-bridge-helper

DRY_RUN=no
DELETE_MACHINES=no
OLD_VERSIONS=no

say() {
    printf '%s\n' "$*"
}

warn() {
    printf 'uninstall.sh: %s\n' "$*" >&2
}

die() {
    warn "$*"
    exit 1
}

usage() {
    cat <<'EOF'
Usage: uninstall.sh [OPTION]

  --dry-run          Show what would be removed, and stop
  --delete-machines  Also delete your machines' folder, once you've typed its name
  --old-versions     Remove only versions that `current` doesn't point to, as an update does
  --help             Show this
EOF
}

parse_arguments() {
    for argument in "$@"; do
        case $argument in
            --dry-run) DRY_RUN=yes ;;
            --delete-machines) DELETE_MACHINES=yes ;;
            --old-versions) OLD_VERSIONS=yes ;;
            --help)
                usage
                exit 0
                ;;
            *)
                usage >&2
                exit 2
                ;;
        esac
    done
}

refuse_root() {
    [ "$(id -u)" -ne 0 ] || die "don't run this as root. Everything it removes is yours."
}

xdg_folder() {
    case ${1:-} in
        /*) printf '%s' "$1" ;;
        *) printf '%s' "$HOME/$2" ;;
    esac
}

set_paths() {
    [ -n "${HOME:-}" ] && [ -d "$HOME" ] || die "HOME isn't set to a folder."
    LIB_ROOT=$HOME/.local/lib/guesthouse
    CURRENT_LINK=$LIB_ROOT/current
    MANIFEST=$LIB_ROOT/manifest
    CONFIG=$(xdg_folder "${XDG_CONFIG_HOME:-}" .config)/guesthouse
    STATE=$(xdg_folder "${XDG_STATE_HOME:-}" .local/state)/guesthouse
    DATA=$(xdg_folder "${XDG_DATA_HOME:-}" .local/share)/guesthouse
}

# The same order as the app: the variable, then the pointer, then the default. See paths.rs.
find_store() {
    store=${GUESTHOUSE_STORE:-}
    if [ -z "$store" ] && [ -f "$CONFIG/store" ]; then
        store=$(sed -n '1s/^[[:space:]]*//; 1s/[[:space:]]*$//; 1p' "$CONFIG/store")
        case $store in
            "~/"*) store=$HOME/${store#"~/"} ;;
        esac
    fi
    STORE=${store:-$DATA}
    MACHINES=${GUESTHOUSE_MACHINES:-$STORE/machines}
}

manifest_value() {
    awk -F "$TAB" -v key="$1" '$1 == key { print $3; exit }' "$MANIFEST"
}

read_manifest() {
    if [ ! -f "$MANIFEST" ]; then
        say "There's no $MANIFEST, so install.sh hasn't installed anything for this user. Nothing to do."
        exit 0
    fi
    format=$(awk -F "$TAB" '$1 == "format" { print $2; exit }' "$MANIFEST")
    [ "$format" = "$MANIFEST_FORMAT" ] ||
        die "$MANIFEST isn't a format this script knows, so I won't act on it. A newer uninstall.sh will."
    # Anything else here, like nothing at all, would widen what path_allowed lets through.
    DATA_HOME=$(manifest_value data-home)
    case $DATA_HOME in
        /?*) ;;
        *) DATA_HOME=$HOME ;;
    esac
}

real_folder() {
    (cd "$1" 2>/dev/null && pwd -P) || printf '%s' "$1"
}

# Processes started from this install: their program is in its folder, which is also how the app tells
# its own QEMU from another's. Every install of this user shares the runtime folder, so a socket there
# doesn't say whose VM it is. A process that won't say what it runs is judged by its first word.
list_running() {
    real_root=$(real_folder "$LIB_ROOT")
    for folder in /proc/[0-9]*; do
        program=$(readlink "$folder/exe" 2>/dev/null) ||
            program=$(tr '\0' '\n' 2>/dev/null <"$folder/cmdline" | head -n 1)
        case $program in
            "$real_root"/* | "$LIB_ROOT"/*) say "${folder#/proc/} ${program##*/}" ;;
        esac
    done
}

refuse_while_running() {
    running=$(list_running)
    [ -z "$running" ] || die "Guesthouse is running, so I've changed nothing. Shut its VMs down and close it first:
$running"
}

# Whatever the manifest says, nothing outside these folders is ever touched.
path_allowed() {
    case $1 in
        */../* | */.. | */./*) return 1 ;;
        "$HOME"/* | "$DATA_HOME"/*) return 0 ;;
    esac
    return 1
}

entries_of_kind() {
    awk -F "$TAB" -v kind="$1" '$1 == kind' "$2"
}

remove_file() {
    wanted=$1
    path=$2
    [ -e "$path" ] || [ -L "$path" ] || return 0
    if [ -L "$path" ] || [ ! -f "$path" ] || [ "$(sha256sum "$path" | cut -d ' ' -f 1)" != "$wanted" ]; then
        say "$path" >>"$KEPT"
        return 0
    fi
    rm -f -- "$path"
}

remove_link() {
    target=$1
    path=$2
    [ -L "$path" ] || return 0
    if [ "$(readlink "$path")" = "$target" ]; then
        rm -f -- "$path"
    else
        say "$path" >>"$KEPT"
    fi
}

# A path can be listed twice, with an old hash and a new one, after an update that was cut short. Then
# one of them is what's there. Links before files, then the folders, deepest first.
remove_entries() {
    list=$1
    KEPT=$LIB_ROOT/.incoming.kept.$$
    : >"$KEPT"
    entries_of_kind link "$list" | while IFS="$TAB" read -r _ target path; do
        ! path_allowed "$path" || remove_link "$target" "$path"
    done
    entries_of_kind file "$list" | while IFS="$TAB" read -r _ hash path; do
        ! path_allowed "$path" || remove_file "$hash" "$path"
    done
    entries_of_kind dir "$list" | cut -f 3 | sort -r | while IFS= read -r path; do
        ! path_allowed "$path" || rmdir -- "$path" 2>/dev/null || true
    done
}

# What's left after a stale hash line for a path that a newer line removed isn't really kept.
print_kept() {
    sort -u "$KEPT" | while IFS= read -r path; do
        [ ! -e "$path" ] || say "  left alone, as it changed since the install: $path"
    done
    rm -f -- "$KEPT"
}

# Only for what's in the manifest. An update ran this, so a VM can't be up, and this is checked anyway.
remove_old_versions() {
    keep=$LIB_ROOT/$(readlink "$CURRENT_LINK")/
    old=$LIB_ROOT/.incoming.old.$$
    awk -F "$TAB" -v root="$LIB_ROOT/" -v keep="$keep" '
        ($1 == "file" || $1 == "dir" || $1 == "link") && index($3, root) == 1 &&
        index($3 "/", keep) != 1 && $3 != root "current"' "$MANIFEST" >"$old"
    remove_entries "$old"
    print_kept
    grep -vxFf "$old" "$MANIFEST" >"$MANIFEST.new"
    mv -f "$MANIFEST.new" "$MANIFEST"
    rm -f -- "$old"
}

print_machines() {
    if [ ! -d "$MACHINES" ]; then
        say "You have no machines in $MACHINES."
        return
    fi
    say "Your machines are in $MACHINES:"
    for machine in "$MACHINES"/*; do
        [ -d "$machine" ] || continue
        say "  ${machine##*/}: $(du -sh "$machine" 2>/dev/null | cut -f 1)"
    done
}

print_plan() {
    say "This removes Guesthouse $(manifest_value version):"
    say "  $(awk -F "$TAB" '$1 == "file"' "$MANIFEST" | wc -l) files listed in $MANIFEST"
    say "  and the folders made for them, if they end up empty"
    print_machines
    if [ "$DELETE_MACHINES" = yes ]; then
        say "--delete-machines was given, so that folder goes too, once you type its name. Disks kept outside it stay."
    else
        say "They all stay. Settings in $CONFIG and the log in $STATE stay too."
    fi
    [ ! -d "$STORE/setup" ] ||
        say "NVIDIA's driver files in $STORE/setup stay too. They're copied from your installer."
    packages=$(awk -F "$TAB" '$1 == "package" { printf "%s ", $3 }' "$MANIFEST")
    [ -z "$packages" ] || say "Packages stay, as something else may use them. The install added: $packages"
    if group_was_added; then
        say "The install added you to the kvm group. This takes you out again, as root:"
        say "  sudo gpasswd -d $(id -un) kvm"
    fi
    if helper_is_ours; then
        say "The install put QEMU's bridge helper in $BRIDGE_HELPER_FOLDER. This removes it, as root:"
        say "  sudo sh -c '$(helper_command)'"
    fi
}

# Only while it's still the copy the install put there. Another install of Guesthouse on this PC may
# have replaced it since, and then it's that one's.
helper_is_ours() {
    hash=$(awk -F "$TAB" -v path="$BRIDGE_HELPER" '$1 == "root-file" && $3 == path { found = $2 }
        END { print found }' "$MANIFEST")
    [ -n "$hash" ] && [ -f "$BRIDGE_HELPER" ] && [ "$(sha256sum "$BRIDGE_HELPER" | cut -d ' ' -f 1)" = "$hash" ]
}

helper_command() {
    printf 'rm -f -- "%s" && { rmdir -- "%s" 2>/dev/null || true; }' "$BRIDGE_HELPER" "$BRIDGE_HELPER_FOLDER"
}

group_was_added() {
    awk -F "$TAB" '$1 == "group" && $3 == "kvm" { found = 1 } END { exit !found }' "$MANIFEST"
}

need_terminal() {
    (: </dev/tty) 2>/dev/null || die "I need a terminal to ask you, so nothing was removed."
}

ask() {
    printf '%s' "$1" >/dev/tty
    IFS= read -r ANSWER </dev/tty || ANSWER=""
}

confirm() {
    need_terminal
    ask "Go ahead? Type yes: "
    case $ANSWER in
        y | Y | yes | Yes | YES) ;;
        *) die "stopped. Nothing was removed." ;;
    esac
}

# A link would send rm somewhere else, and the app's store can be pointed at anything, so the folder
# must be a real one, and can't hold the home folder or be it.
machines_safe_to_delete() {
    [ -d "$MACHINES" ] && [ ! -L "$MACHINES" ] || return 1
    real=$(real_folder "$MACHINES")
    home=$(real_folder "$HOME")
    case $home/ in
        "$real"/*) return 1 ;;
    esac
    [ "$real" != / ]
}

confirm_machines() {
    DELETE_CONFIRMED=no
    [ "$DELETE_MACHINES" = yes ] || return 0
    [ -e "$MACHINES" ] || return 0
    machines_safe_to_delete || die "I won't delete $MACHINES: it's a link, or it holds your home folder. Nothing was removed."
    need_terminal
    say "This deletes every machine and disk in that folder, for good."
    ask "To do that, type the folder's full name: "
    [ "$ANSWER" = "$MACHINES" ] || die "that isn't its name, so nothing was removed."
    DELETE_CONFIRMED=yes
}

# The same as install.sh does once the icons are in, so that they're gone from the menus too. The cache
# isn't in the manifest, as the install never made it, so it stays.
refresh_icons() {
    theme=$DATA_HOME/icons/hicolor
    awk -F "$TAB" -v prefix="$theme/" '$1 == "file" && index($3, prefix) == 1 { found = 1 }
        END { exit !found }' "$MANIFEST" || return 0
    [ -d "$theme" ] || return 0
    touch "$theme" 2>/dev/null || true
    [ -f "$theme/icon-theme.cache" ] || return 0
    for tool in gtk-update-icon-cache gtk4-update-icon-cache; do
        if command -v "$tool" >/dev/null 2>&1; then
            "$tool" -q -t -f "$theme" >/dev/null 2>&1 || true
            return 0
        fi
    done
}

remove_program() {
    remove_entries "$MANIFEST"
    refresh_icons
    # A run killed outright can leave these behind, and only these scripts make them.
    for stale in "$LIB_ROOT"/.incoming.*; do
        [ "$stale" = "$KEPT" ] || [ ! -e "$stale" ] || rm -rf -- "${stale:?}"
    done
    print_kept
}

delete_machines() {
    [ "$DELETE_CONFIRMED" = yes ] || return 0
    rm -rf -- "${MACHINES:?}"
    say "Deleted $MACHINES"
}

leave_group() {
    group_was_added || return 0
    sudo gpasswd -d "$(id -un)" kvm </dev/tty || warn "couldn't take you out of the kvm group."
}

remove_helper() {
    helper_is_ours || return 0
    sudo sh -c "$(helper_command)" </dev/tty || warn "couldn't remove $BRIDGE_HELPER."
}

# The folders go after the manifest, as it's what lists them, and it sits in one of them.
remove_manifest() {
    folders=$(entries_of_kind dir "$MANIFEST" | cut -f 3 | sort -r)
    rm -f -- "$MANIFEST"
    printf '%s\n' "$folders" | while IFS= read -r path; do
        ! path_allowed "$path" || rmdir -- "$path" 2>/dev/null || true
    done
}

main() {
    parse_arguments "$@"
    refuse_root
    set_paths
    read_manifest
    find_store
    refuse_while_running
    if [ "$OLD_VERSIONS" = yes ]; then
        remove_old_versions
        return
    fi
    print_plan
    if [ "$DRY_RUN" = yes ]; then
        say "That was a dry run, so nothing was removed."
        return
    fi
    confirm
    confirm_machines
    leave_group
    remove_helper
    remove_program
    delete_machines
    remove_manifest
    say "Guesthouse is gone. To put it back, run install.sh again."
}

# In braces, so that a download cut off anywhere in this line, even right after `main`, is a syntax error.
{ main "$@"; }
