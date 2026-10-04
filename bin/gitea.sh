#!/bin/sh
# git.syui.ai (gitea) へ push する
#   bin/gitea.sh id      gpg の鍵の束にある秘密鍵で、このリポジトリのコミットの名前・メール・署名を決める
#   bin/gitea.sh os      このリポジトリの unix ブランチを ai/os へ
#   bin/gitea.sh repo    repo/aarch64 (rust/ c/ shell/ desktop/ のパッケージと aios.db) を ai/repo の main の aarch64/ へ
#                        (署名つきのコミット。歴史は残さず、いつも 1 コミットにして force push する)
#   bin/gitea.sh release ディスクのイメージ (aios-unix-aarch64.img.zst) を作り、ai/os のリリース unix-latest に置きかえる
#                        (AIOS_RELEASE_REMOTE=1 でパッケージを ai/repo から取って作る)
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
  sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
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
    [ -f repo/aarch64/rust/aios.db ] || { echo "repo/aarch64/rust/aios.db がありません (bin/mkrepo.sh で作る)" >&2; exit 1; }
    find_key
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    git -c "$(auth)" clone -q --depth 1 -b main "$host/ai/repo.git" "$tmp/repo"
    use_identity "$tmp/repo"
    # aarch64/ をこのリポジトリの repo/aarch64 (rust/ c/ shell/ desktop/) と同じにする
    rm -rf "$tmp/repo/aarch64"
    for d in rust c shell desktop; do
      [ -f "repo/aarch64/$d/aios.db" ] || continue
      mkdir -p "$tmp/repo/aarch64/$d"
      cp repo/aarch64/$d/*.pkg.tar.zst repo/aarch64/$d/aios.db repo/aarch64/$d/aios.db.tar.gz "$tmp/repo/aarch64/$d/"
    done
    # 前の置き場所 (aarch64-c/) は消す
    rm -rf "$tmp/repo/aarch64-c"
    cd "$tmp/repo"
    git add -A .
    # 中身が同じでも、前の歴史が残っていれば (親のあるコミット) まとめる
    if git diff --cached --quiet && ! git cat-file -p HEAD | grep -q '^parent '; then
      echo "ai/repo: no change"
      exit 0
    fi
    changes=$(git diff --cached --name-status | sed 's/^/  /')
    # 歴史は残さない: いまのファイル (x86_64/ や aios.gpg もそのまま) だけの 1 コミットで main を置きかえる。
    # パッケージは圧縮したバイナリで差分がとれず、作りなおすたびに積もるので
    # (配る場所なので、要るのはいまのパッケージだけ。aipkg からは何も変わらない)
    git checkout -q --orphan new
    git commit -q -m "aarch64: update packages" -m "$changes"
    git -c "$(auth)" push -q --force origin new:main
    echo "ai/repo: pushed $(git rev-parse --short HEAD) (history squashed)"
    ;;
  release)
    # ディスクのイメージ (GitHub の .github/workflows/unix.yml と同じ作り方) を ai/os のリリース unix-latest に
    # 置きかえる。rootfs はこのリポジトリと repo/aarch64 のパッケージから作る (AIOS_SERVER から取るなら -r)
    api=$host/api/v1/repos/ai/os
    hdr=
    [ -n "$GITEA_TOKEN" ] && hdr="Authorization: token $GITEA_TOKEN"
    call() { curl -fsS ${hdr:+-H "$hdr"} "$@"; }
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    bin/mkrootfs.sh ${AIOS_RELEASE_REMOTE:+-r} all > "$tmp/rootfs.log" 2>&1 || { tail "$tmp/rootfs.log" >&2; exit 1; }
    # 8G (空きはゼロなので .zst は小さいまま。zstd -d は sparse に書くので、ホストのディスクは使ったぶんだけ)。
    # スワップ 1G は cargo のビルドのため
    SWAP=${AIOS_RELEASE_SWAP:-1G} bin/mkdisk.sh "${AIOS_RELEASE_SIZE:-8G}"
    zstd -q -19 -T0 -f disk.img -o "$tmp/aios-unix-aarch64.img.zst"
    ver=$(ls repo/aarch64/rust/aikernel-*.pkg.tar.zst | sed 's|.*/aikernel-||; s|-aarch64.pkg.tar.zst||')
    sha=$(git rev-parse HEAD)
    # 前の unix-latest (リリースとタグ) を消す
    old=$(call "$api/releases/tags/unix-latest" 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])' 2>/dev/null) || true
    [ -n "$old" ] && call -X DELETE "$api/releases/$old" > /dev/null
    call -X DELETE "$api/tags/unix-latest" > /dev/null 2>&1 || true
    notes=$(printf '%s\n' '```sh' '# brew install qemu zstd' 'git clone -b unix https://git.syui.ai/ai/os aios && cd aios' \
      'curl -fLO https://git.syui.ai/ai/os/releases/download/unix-latest/aios-unix-aarch64.img.zst' './bin/run.sh' '```' '' 'exit: `sudo poweroff`')
    body=$(python3 -c 'import json,sys; print(json.dumps({"tag_name": "unix-latest", "target_commitish": sys.argv[1], "name": "unix-latest (" + sys.argv[2] + ")", "body": sys.argv[3], "prerelease": True}))' "$sha" "$ver" "$notes")
    id=$(call -X POST -H 'Content-Type: application/json' -d "$body" "$api/releases" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
    [ -n "$id" ] || { echo "ai/os: could not create the release" >&2; exit 1; }
    call -X POST -F "attachment=@$tmp/aios-unix-aarch64.img.zst" "$api/releases/$id/assets?name=aios-unix-aarch64.img.zst" > /dev/null
    echo "ai/os: released unix-latest ($ver, $(du -h "$tmp/aios-unix-aarch64.img.zst" | cut -f1))"
    echo "  $host/ai/os/releases/download/unix-latest/aios-unix-aarch64.img.zst"
    ;;
  *)
    usage
    ;;
esac
