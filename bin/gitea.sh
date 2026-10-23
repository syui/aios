#!/bin/sh
# git.syui.ai (gitea) へ push する
#   bin/gitea.sh id      gpg の鍵の束にある秘密鍵で、このリポジトリのコミットの名前・メール・署名を決める
#   bin/gitea.sh os      このリポジトリの main ブランチを ai/os へ (main ブランチにいなければ GitHub の main を)
#   bin/gitea.sh repo    repo/aarch64 (rust/ c/ shell/ desktop/ のパッケージと aios.db) を ai/repo の main の aarch64/ へ
#                        (署名つきのコミット。歴史は残さず、いつも 1 コミットにして force push する。ai/repo の写し
#                        build/airepo から: 変わったものだけ取ってきて、変わったものだけ送る)
#   bin/gitea.sh release ディスクのイメージ (aios-aarch64.img.zst) を作り、ai/os のリリース latest (タイトル aios) に
#                        置きかえる (AIOS_RELEASE_REMOTE=1 でパッケージを ai/repo から取って作る)
#   bin/gitea.sh mirror  GitHub のリリース latest (CI が作って起動をたしかめたもの) をそのまま ai/os の latest に
#                        (作らないので速い。版とコミットも GitHub のもの)
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
  sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
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

# push の前のやりとり (push.negotiate) はしない: この環境の git の設定 (~/.gitconfig) では true で、git.syui.ai はその
# 答えに packfile を返すので、毎回 "fatal: expected 'acknowledgments', received 'packfile'" となり、あきらめてから送っていた
git_push() {
  git -c "$(auth)" -c push.negotiate=false push "$@"
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

# その鍵の持ち主: name と mail (と find_key の fpr)
identity() {
  find_key
  name=${uid% <*}
  mail=${uid##*<}
  mail=${mail%>}
}

# 引数のリポジトリのコミットを、その鍵の持ち主の名前と署名にする
use_identity() {
  identity
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
    # main ブランチにいればそれを、ほかのブランチ (Claude の作業ブランチなど) にいれば GitHub の main (先に
    # git push origin HEAD:main したもの) を送る。手元の古い main ブランチを送らないように
    if [ "$(git rev-parse --abbrev-ref HEAD)" = main ]; then
      ref=main
    else
      git fetch -q origin main
      ref=origin/main
    fi
    git_push "$host/ai/os.git" "$ref:refs/heads/main"
    ;;
  repo)
    [ -f repo/aarch64/rust/aios.db ] || { echo "repo/aarch64/rust/aios.db がありません (bin/mkrepo.sh で作る)" >&2; exit 1; }
    identity
    echo "commits by $name <$mail>, signed with $fpr" >&2
    # ai/repo の写し (build/airepo。aish-pkg の push が ai/repo とくらべるのにも使う) から送る。あれば変わったところだけを
    # 取る (前は送るたびに ai/repo をまるごと (いま 540 MB) clone していて何分もかかり、プロキシに切られることもあった)。
    # 送るのも、写しにないもの (作りなおしたパッケージ) だけ
    dir=$PWD/build/airepo
    if [ -d "$dir/.git" ]; then
      git -C "$dir" -c "$(auth)" fetch -q --depth 1 "$host/ai/repo.git" main
      base=$(git -C "$dir" rev-parse FETCH_HEAD)
    else
      mkdir -p build
      git -c "$(auth)" clone -q --depth 1 -b main "$host/ai/repo.git" "$dir"
      base=$(git -C "$dir" rev-parse HEAD)
    fi
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    # 写しのファイルにはさわらず、別の index で: ai/repo の木 (x86_64/ や aios.gpg もそのまま) の aarch64/ を、この
    # リポジトリの repo/aarch64 (rust/ c/ shell/ desktop/) にする。前の置き場所 aarch64-c/ は消す
    g() { GIT_DIR="$dir/.git" GIT_INDEX_FILE="$tmp/index" git "$@"; }
    g read-tree "$(g ls-tree "$base" | awk -F '\t' '$2 != "aarch64" && $2 != "aarch64-c"' | g mktree)"
    for d in rust c shell desktop; do
      [ -f "repo/aarch64/$d/aios.db" ] || continue
      (cd repo && GIT_DIR="$dir/.git" GIT_INDEX_FILE="$tmp/index" GIT_WORK_TREE=. git add -f -- aarch64/$d/*.pkg.tar.zst "aarch64/$d/aios.db" "aarch64/$d/aios.db.tar.gz")
    done
    tree=$(g write-tree)
    # 中身が同じでも、前の歴史が残っていれば (親のあるコミット) まとめる
    if [ "$tree" = "$(g rev-parse "$base^{tree}")" ] && ! g cat-file -p "$base" | grep -q '^parent '; then
      echo "ai/repo: no change"
      exit 0
    fi
    changes=$(g diff-index --cached --name-status "$base" | sed 's/^/  /')
    # 歴史は残さない: いまのファイルだけの 1 コミット (親なし) で main を置きかえる。パッケージは圧縮したバイナリで
    # 差分がとれず、作りなおすたびに積もるので (配る場所なので、要るのはいまのパッケージだけ。aipkg からは何も変わらない)
    new=$(g -c gpg.format=openpgp -c user.name="$name" -c user.email="$mail" commit-tree -S"$fpr" -m "aarch64: update packages" -m "$changes" "$tree")
    (cd "$dir" && git_push -q --force "$host/ai/repo.git" "$new:refs/heads/main")
    # 写しも送ったものに (書きかわるのは変わったファイルだけ)。前のコミットのものは、ふくらまないように捨てる
    # (ばらばらのものだけ。clone したときの pack はそのまま。gc で詰めなおすと、このディスクでは何分もかかる)
    git -C "$dir" reset -q --hard "$new"
    git -C "$dir" reflog expire --expire=now --all
    git -C "$dir" prune --expire=now
    echo "ai/repo: pushed $(git -C "$dir" rev-parse --short "$new") (history squashed)"
    ;;
  release | mirror)
    # ディスクのイメージ (GitHub の .github/workflows/unix.yml と同じ作り方) を ai/os のリリース latest に
    # 置きかえる。rootfs はこのリポジトリと repo/aarch64 のパッケージから作る (AIOS_SERVER から取るなら -r)。
    # mirror は作らずに GitHub の latest をそのまま。ノートの 1 行目が版 ( - vVER)
    api=$host/api/v1/repos/ai/os
    tag=latest img=aios-aarch64.img.zst
    hdr=
    [ -n "$GITEA_TOKEN" ] && hdr="Authorization: token $GITEA_TOKEN"
    call() { curl -fsS ${hdr:+-H "$hdr"} "$@"; }
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    if [ "$1" = mirror ]; then
      gh=https://github.com/syui/aios/releases/download/$tag/$img
      info=$(curl -fsS https://api.github.com/repos/syui/aios/releases/tags/$tag)
      # 版はノートの 1 行目 " - vVER"
      ver=$(printf '%s' "$info" | python3 -c 'import json,re,sys; m=re.search(r"^\s*-\s*v(\S+)", json.load(sys.stdin)["body"], re.M); print(m.group(1) if m else "")')
      [ -n "$ver" ] || { echo "ai/os: no version ( - vVER) in the notes of the GitHub release $tag" >&2; exit 1; }
      sha=$(printf '%s' "$info" | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_commitish"])')
      curl -fsSL -o "$tmp/$img" "$gh"
      zstd -t -q "$tmp/$img"
    else
      bin/mkrootfs.sh ${AIOS_RELEASE_REMOTE:+-r} all > "$tmp/rootfs.log" 2>&1 || { tail "$tmp/rootfs.log" >&2; exit 1; }
      # 8G (空きはゼロなので .zst は小さいまま。zstd -d は sparse に書くので、ホストのディスクは使ったぶんだけ)。
      # スワップ 1G は cargo のビルドのため
      SWAP=${AIOS_RELEASE_SWAP:-1G} bin/mkdisk.sh "${AIOS_RELEASE_SIZE:-8G}"
      zstd -q -19 -T0 -f aios.img -o "$tmp/$img"
      ver=$(ls repo/aarch64/rust/aikernel-*.pkg.tar.zst | sed 's|.*/aikernel-||; s|-aarch64.pkg.tar.zst||')
      sha=$(git rev-parse HEAD)
    fi
    # 前の latest (リリースとタグ) を消す。前の名前の unix-latest がのこっていれば、それも
    for t in $tag unix-latest; do
      old=$(call "$api/releases/tags/$t" 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])' 2>/dev/null) || true
      [ -n "$old" ] && call -X DELETE "$api/releases/$old" > /dev/null
      call -X DELETE "$api/tags/$t" > /dev/null 2>&1 || true
    done
    notes=$(printf '%s\n' " - v$ver" '' '```sh' '# brew install qemu zstd' 'git clone https://git.syui.ai/ai/os aios && cd aios' \
      "curl -fLO $host/ai/os/releases/download/$tag/$img" './bin/run.sh' '```' '' 'exit: `sudo poweroff`')
    body=$(python3 -c 'import json,sys; print(json.dumps({"tag_name": sys.argv[1], "target_commitish": sys.argv[2], "name": "aios", "body": sys.argv[3], "prerelease": False}))' "$tag" "$sha" "$notes")
    id=$(call -X POST -H 'Content-Type: application/json' -d "$body" "$api/releases" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
    [ -n "$id" ] || { echo "ai/os: could not create the release" >&2; exit 1; }
    call -X POST -F "attachment=@$tmp/$img" "$api/releases/$id/assets?name=$img" > /dev/null
    echo "ai/os: released $tag ($ver, $(du -h "$tmp/$img" | cut -f1))"
    echo "  $host/ai/os/releases/download/$tag/$img"
    ;;
  *)
    usage
    ;;
esac
