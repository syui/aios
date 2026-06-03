#!/usr/bin/env bash

# gnu-coreutils -> uutils-coreutils

set -euo pipefail

LOCAL_BIN="/usr/local/bin"

KEEP_GNU=()

is_kept() {
    local name="$1" k
    (( ${#KEEP_GNU[@]} == 0 )) && return 1
    for k in "${KEEP_GNU[@]}"; do
        [[ "$name" == "$k" ]] && return 0
    done
    return 1
}

link_coreutils() {
    echo ">> coreutils -> uutils  (KEEP_GNU: ${KEEP_GNU[*]:-none})"
    mkdir -p "$LOCAL_BIN"
    local bin name
    for bin in /usr/bin/uu-*; do
        [[ -e "$bin" ]] || continue
        name="${bin##*/uu-}"
        [[ "$name" == "coreutils" ]] && continue
        is_kept "$name" && { echo "   skip(GNU): $name"; continue; }
        ln -sf "$bin" "$LOCAL_BIN/$name"
    done
}

link_sudo() {
    echo ">> sudo -> sudo-rs"
    [[ -e /usr/bin/sudo-rs   ]] && ln -sf /usr/bin/sudo-rs   "$LOCAL_BIN/sudo"
    [[ -e /usr/bin/su-rs     ]] && ln -sf /usr/bin/su-rs     "$LOCAL_BIN/su"
    [[ -e /usr/bin/visudo-rs ]] && ln -sf /usr/bin/visudo-rs "$LOCAL_BIN/visudo"
}

apply() {
    link_coreutils
    link_sudo
    echo ">> done. check: ls --version (uutils) / sudo --version (sudo-rs)"
}

case "${1:-apply}" in
    apply)  apply  ;;
    *) echo "usage: ${0##*/} [apply|revert]"; exit 1 ;;
esac
