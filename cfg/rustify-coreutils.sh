#!/usr/bin/env bash
#
# rustify-coreutils.sh — aios の coreutils / sudo を Rust 版へ置き換える。
#
# 非破壊オーバーレイ方式:
#   GNU 版は /usr/bin にそのまま残す。/usr/local/bin に uu-* / sudo-rs への
#   シンボリックリンクを張り、PATH と sudo の secure_path が /usr/local/bin を
#   /usr/bin より先に見ることで Rust 版を既定にする。
#   `revert` で symlink を撤去すれば GNU に戻る(GNU は消していないので安全)。
#
#   build.zsh が image ビルドの最後に `apply` する。手動なら:
#     sudo rustify-coreutils apply    # Rust 版へ
#     sudo rustify-coreutils revert   # GNU へ戻す
#
set -euo pipefail

LOCAL_BIN="/usr/local/bin"

# GNU に残すコマンド名。aios は全置換なので空。
# (cp/mv/rm/df/install/true 等も含め uutils 0.9.0 + pacman/makepkg で実機検証済み)
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
    echo ">> coreutils -> uutils  (KEEP_GNU: ${KEEP_GNU[*]:-なし})"
    mkdir -p "$LOCAL_BIN"
    local bin name
    for bin in /usr/bin/uu-*; do
        [[ -e "$bin" ]] || continue
        name="${bin##*/uu-}"
        [[ "$name" == "coreutils" ]] && continue       # マルチコール本体は除外
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
    echo ">> done. 確認: ls --version (uutils) / sudo --version (sudo-rs)"
}

revert() {
    echo ">> Rust 版リンクを撤去し GNU へ戻す"
    local bin name link n
    for bin in /usr/bin/uu-*; do
        [[ -e "$bin" ]] || continue
        name="${bin##*/uu-}"
        link="$LOCAL_BIN/$name"
        if [[ -L "$link" && "$(readlink -f "$link")" == "$(readlink -f "$bin")" ]]; then
            rm -f "$link"
        fi
    done
    for n in sudo su visudo; do
        link="$LOCAL_BIN/$n"
        [[ -L "$link" && "$(readlink -f "$link")" == /usr/bin/*-rs ]] && rm -f "$link"
    done
    echo ">> done. GNU (/usr/bin) に戻りました。"
}

case "${1:-apply}" in
    apply)  apply  ;;
    revert) revert ;;
    *) echo "usage: ${0##*/} [apply|revert]"; exit 1 ;;
esac
