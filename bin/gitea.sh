#!/bin/sh
# git.syui.ai (gitea) へ push する
#   bin/gitea.sh id      gpg の鍵の束にある秘密鍵で、このリポジトリのコミットの名前・メール・署名を決める
#   bin/gitea.sh os      このリポジトリの unix ブランチを ai/os へ
#   bin/gitea.sh repo    repo/aarch64 (rust/ と c/ のパッケージと aios.db) を ai/repo の main の aarch64/ へ
#                        (署名つきのコミット)
#
# 秘密鍵は環境の setup script で鍵の束 (/root/.gnupg) に取りこんでおく:
#   gpg --batch --import <<'EOF'
#   -----BEGIN PGP PRIVATE KEY BLOCK-----
#   ...
#   EOF
# 名前とメールはその鍵の uid から。鍵がいくつかあれば GPG_SIGNER (指紋か uid のメール) で選ぶ。
# 認証は git.syui.ai への通信に環境の API 認証情報がつくのにまかせる
# (GITEA_TOKEN があればそれを使う。ユーザーは GITEA_USER、既定 ai.syui.ai)。トークンはディスクに書かない
set -e
cd "$(dirname "$0")/.."
host=https://git.syui.ai
user=${GITEA_USER:-ai.syui.ai}

usage() {
  sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
  exit 1
}

# git に認証のヘッダを渡す (コマンドラインの -c なので、設定ファイルにもログにも残らない)。
# トークンがなければ何もしない設定 (プロキシが認証をつける)
auth() {
  if [ -n "$GITEA_TOKEN" ]; then
    printf 'http.%s/.extraheader=Authorization: Basic %s' "$host" "$(printf '%s:%s' "$user" "$GITEA_TOKEN" | base64 | tr -d '\n')"
  else
    printf 'gitea.noauth=1'
  fi
}

# 鍵の束から署名に使う秘密鍵を選ぶ: fpr (指紋) と uid ("名前 <メール>")。なければ失敗
find_key() {
  list=$(gpg --batch --with-colons --list-secret-keys ${GPG_SIGNER:+"$GPG_SIGNER"} 2>/dev/null) || true
  fpr=$(printf '%s\n' "$list" | awk -F: '/^fpr:/ {print $10; exit}')
  uid=$(printf '%s\n' "$list" | awk -F: '/^uid:/ {print $10; exit}')
  if [ -z "$fpr" ] || [ -z "$uid" ]; then
    echo "gpg の鍵の束に秘密鍵がありません (環境の setup script で gpg --import してください)" >&2
    return 1
  fi
}

# 引数のリポジトリのコミットを、その鍵の持ち主の名前と署名にする
use_identity() {
  find_key
  name=${uid% <*}
  mail=${uid##*<}
  mail=${mail%>}
  git -C "$1" config user.name "$name"
  git -C "$1" config user.email "$mail"
  # この環境の git は SSH の鍵で署名する設定 (gpg.format=ssh) なので、GPG に戻す
  git -C "$1" config gpg.format openpgp
  git -C "$1" config user.signingkey "$fpr"
  git -C "$1" config commit.gpgsign true
  echo "commits by $name <$mail>, signed with $fpr" >&2
}

case "$1" in
  id)
    use_identity .
    ;;
  os)
    git -c "$(auth)" push "$host/ai/os.git" unix:unix
    ;;
  repo)
    [ -f repo/aarch64/rust/aios.db ] || { echo "repo/aarch64/rust/aios.db がありません (bin/mkrepo.sh、または pkg ブランチの aarch64/ を repo/aarch64 へ)" >&2; exit 1; }
    find_key
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    git -c "$(auth)" clone -q --depth 1 -b main "$host/ai/repo.git" "$tmp/repo"
    use_identity "$tmp/repo"
    # aarch64/ をこのリポジトリの repo/aarch64 (rust/ と c/) と同じにする
    rm -rf "$tmp/repo/aarch64"
    for d in rust c; do
      [ -f "repo/aarch64/$d/aios.db" ] || continue
      mkdir -p "$tmp/repo/aarch64/$d"
      cp repo/aarch64/$d/*.pkg.tar.zst repo/aarch64/$d/aios.db repo/aarch64/$d/aios.db.tar.gz "$tmp/repo/aarch64/$d/"
    done
    # 前の置き場所 (aarch64-c/) は消す
    rm -rf "$tmp/repo/aarch64-c"
    cd "$tmp/repo"
    git add -A .
    if git diff --cached --quiet; then
      echo "ai/repo: no change"
      exit 0
    fi
    git commit -q -m "aarch64: update packages" -m "$(git diff --cached --name-status | sed 's/^/  /')"
    git -c "$(auth)" push -q origin main
    echo "ai/repo: pushed $(git rev-parse --short HEAD)"
    ;;
  *)
    usage
    ;;
esac
