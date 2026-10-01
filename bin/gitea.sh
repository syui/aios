#!/bin/sh
# git.syui.ai (gitea) へ push する
#   bin/gitea.sh os      このリポジトリの unix ブランチを ai/os へ
#   bin/gitea.sh repo    repo/aarch64 (パッケージと aios.db) を ai/repo の main の aarch64/ へ
#   bin/gitea.sh id      GPG_KEY を取りこんで、このリポジトリのコミットをそのキーの持ち主の名前と署名にする
# 環境変数:
#   GITEA_TOKEN  gitea のアクセストークン (ディスクには書かない)。なければ、通信に認証をつけてくれる
#                プロキシ (Claude Code の環境の API 認証情報) にまかせる
#   GITEA_USER   トークンの持ち主 (既定 ai.syui.ai)
#   GPG_KEY      ASCII armor の GPG 秘密鍵 (AI_GPG_KEY でも。あれば: コミットの名前とメールはキーの uid から、署名つき)。
#                なければ gpg の鍵の束 (setup script で取りこんだもの) の最初の秘密鍵を使う
set -e
cd "$(dirname "$0")/.."
host=https://git.syui.ai
user=${GITEA_USER:-ai.syui.ai}
AI_GPG_KEY=${AI_GPG_KEY:-$GPG_KEY}

# git に認証のヘッダを渡す (コマンドラインの -c なので、設定ファイルにもログにも残らない)。
# トークンがなければ何もしない設定 (プロキシが認証をつける)
auth() {
  if [ -n "$GITEA_TOKEN" ]; then
    printf 'http.%s/.extraheader=Authorization: Basic %s' "$host" "$(printf '%s:%s' "$user" "$GITEA_TOKEN" | base64 | tr -d '\n')"
  else
    printf 'gitea.noauth=1'
  fi
}

# GPG_KEY (AI_GPG_KEY) を取りこみ、そのキーの uid と指紋を返す ("名前 <メール>" と指紋)
import_key() {
  if [ -n "$AI_GPG_KEY" ]; then
    printf '%s\n' "$AI_GPG_KEY" | gpg --batch --quiet --import 2>/dev/null
    list=$(printf '%s\n' "$AI_GPG_KEY" | gpg --batch --with-colons --import-options show-only --import 2>/dev/null)
  else
    # 鍵の束にある秘密鍵 (setup script などで取りこんだもの)
    list=$(gpg --batch --with-colons --list-secret-keys 2>/dev/null)
  fi
  fpr=$(printf '%s\n' "$list" | awk -F: '/^fpr:/ {print $10; exit}')
  uid=$(printf '%s\n' "$list" | awk -F: '/^uid:/ {print $10; exit}')
  [ -n "$fpr" ] && [ -n "$uid" ]
}

# git の名前・メール・署名をキーに合わせる (引数のディレクトリのリポジトリ)
use_identity() {
  import_key || return 0
  name=${uid% <*}
  mail=${uid##*<}
  mail=${mail%>}
  git -C "$1" config user.name "$name"
  git -C "$1" config user.email "$mail"
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
    [ -f repo/aarch64/aios.db ] || { echo "repo/aarch64/aios.db がありません (bin/mkrepo.sh)" >&2; exit 1; }
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    git -c "$(auth)" clone -q --depth 1 -b main "$host/ai/repo.git" "$tmp/repo"
    use_identity "$tmp/repo"
    # aarch64/ をこのリポジトリの repo/aarch64 と同じにする
    rm -rf "$tmp/repo/aarch64"
    mkdir -p "$tmp/repo/aarch64"
    cp repo/aarch64/*.pkg.tar.zst repo/aarch64/aios.db repo/aarch64/aios.db.tar.gz "$tmp/repo/aarch64/"
    cd "$tmp/repo"
    git add -A aarch64
    if git diff --cached --quiet; then
      echo "ai/repo: no change"
      exit 0
    fi
    git commit -q -m "aarch64: update packages" -m "$(git diff --cached --name-status | sed 's/^/  /')"
    git -c "$(auth)" push -q origin main
    echo "ai/repo: pushed $(git rev-parse --short HEAD)"
    ;;
  *)
    sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
    ;;
esac
