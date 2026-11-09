#!/bin/sh
set -e
here=$(pwd)
cd "$(dirname "$0")/.."
host=https://git.syui.ai
user=${GITEA_USER:-ai.syui.ai}

usage() {
  echo "usage: bin/gitea.sh id | os | repo | release | mirror | model FILE..." >&2
  echo "  env: GITEA_TOKEN GITEA_USER GITEA_URL GPG_SIGNER AIOS_RELEASE_REMOTE AIOS_RELEASE_SWAP AIOS_RELEASE_SIZE" >&2
  exit 1
}

auth() {
  if [ -n "$GITEA_TOKEN" ]; then
    printf 'http.%s/.extraheader=Authorization: Basic %s' "$host" "$(printf '%s:%s' "$user" "$GITEA_TOKEN" | base64 | tr -d '\n')"
  else
    printf 'gitea.noauth=1'
  fi
}

git_push() {
  git -c "$(auth)" -c push.negotiate=false push "$@"
}

find_key() {
  list=$(gpg --batch --with-colons --list-secret-keys ${GPG_SIGNER:+"$GPG_SIGNER"} 2>/dev/null) || true
  fpr=$(printf '%s\n' "$list" | awk -F: '/^fpr:/ {print $10; exit}')
  uid=$(printf '%s\n' "$list" | awk -F: '/^uid:/ {print $10; exit}')
  if [ -z "$fpr" ] || [ -z "$uid" ]; then
    echo "gpg の鍵の束に秘密鍵がありません (gpg --import してください)" >&2
    return 1
  fi
}

identity() {
  find_key
  name=${uid% <*}
  mail=${uid##*<}
  mail=${mail%>}
}

use_identity() {
  identity
  git -C "$1" config user.name "$name"
  git -C "$1" config user.email "$mail"
  git -C "$1" config gpg.format openpgp
  git -C "$1" config user.signingkey "$fpr"
  git -C "$1" config commit.gpgsign true
  echo "commits by $name <$mail>, signed with $fpr" >&2
}

case "$1" in
  id)
    use_identity .
    git config aish.mirror "$host/ai/os.git"
    ;;
  os)
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
    g() { GIT_DIR="$dir/.git" GIT_INDEX_FILE="$tmp/index" git "$@"; }
    g read-tree "$(g ls-tree "$base" | awk -F '\t' '$2 != "aarch64" && $2 != "aarch64-c"' | g mktree)"
    for d in rust c shell desktop; do
      [ -f "repo/aarch64/$d/aios.db" ] || continue
      (cd repo && GIT_DIR="$dir/.git" GIT_INDEX_FILE="$tmp/index" GIT_WORK_TREE=. git add -f -- aarch64/$d/*.pkg.tar.zst "aarch64/$d/aios.db" "aarch64/$d/aios.db.tar.gz")
    done
    tree=$(g write-tree)
    if [ "$tree" = "$(g rev-parse "$base^{tree}")" ] && ! g cat-file -p "$base" | grep -q '^parent '; then
      echo "ai/repo: no change"
      exit 0
    fi
    changes=$(g diff-index --cached --name-status "$base" | sed 's/^/  /')
    commit() { g -c gpg.format=openpgp -c user.name="$name" -c user.email="$mail" commit-tree -S"$fpr" "$@"; }
    limit=$((80 * 1024 * 1024))
    batch= size=0 total=0 n=0
    for p in $(g diff-index --cached --diff-filter=AM --name-only "$base" -- '*.pkg.tar.zst'); do
      total=$((total + $(g cat-file -s ":$p")))
    done
    if [ "$total" -gt "$limit" ]; then
      gs() { GIT_DIR="$dir/.git" GIT_INDEX_FILE="$tmp/step" git "$@"; }
      step() {
        n=$((n + 1))
        gs read-tree "$base"
        for q in $batch; do
          gs update-index --add --cacheinfo "100644,$(g rev-parse ":$q"),$q"
        done
        c=$(commit -m "aarch64: add new packages ($n)" "$(gs write-tree)")
        echo "ai/repo: sending new packages, part $n ($((size / 1048576)) MB)" >&2
        (cd "$dir" && git_push -q --force "$host/ai/repo.git" "$c:refs/heads/main")
        size=0
      }
      for p in $(g diff-index --cached --diff-filter=AM --name-only "$base" -- '*.pkg.tar.zst'); do
        s=$(g cat-file -s ":$p")
        [ "$size" -gt 0 ] && [ $((size + s)) -gt "$limit" ] && step
        batch="$batch $p" size=$((size + s))
      done
      [ "$size" -gt 0 ] && step
    fi
    new=$(commit -m "aarch64: update packages" -m "$changes" "$tree")
    (cd "$dir" && git_push -q --force "$host/ai/repo.git" "$new:refs/heads/main")
    git -C "$dir" reset -q --hard "$new"
    git -C "$dir" reflog expire --expire=now --all
    git -C "$dir" prune --expire=now
    echo "ai/repo: pushed $(git -C "$dir" rev-parse --short "$new") (history squashed)"
    ;;
  release | mirror)
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
      ver=$(printf '%s' "$info" | python3 -c 'import json,re,sys; m=re.search(r"^\s*-\s*v(\S+)", json.load(sys.stdin)["body"], re.M); print(m.group(1) if m else "")')
      [ -n "$ver" ] || { echo "ai/os: no version ( - vVER) in the notes of the GitHub release $tag" >&2; exit 1; }
      sha=$(printf '%s' "$info" | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_commitish"])')
      curl -fsSL -o "$tmp/$img" "$gh"
      zstd -t -q "$tmp/$img"
    else
      bin/mkrootfs.sh ${AIOS_RELEASE_REMOTE:+-r} all > "$tmp/rootfs.log" 2>&1 || { tail "$tmp/rootfs.log" >&2; exit 1; }
      SWAP=${AIOS_RELEASE_SWAP:-1G} bin/mkdisk.sh "${AIOS_RELEASE_SIZE:-8G}"
      zstd -q -19 -T0 -f aios.img -o "$tmp/$img"
      ver=$(ls repo/aarch64/rust/aikernel-*.pkg.tar.zst | sed 's|.*/aikernel-||; s|-aarch64.pkg.tar.zst||')
      sha=$(git rev-parse HEAD)
    fi
    call "$api/git/commits/$sha" > /dev/null 2>&1 || { echo "ai/os: $sha is not in ai/os (push main first: bin/gitea.sh os); the release $tag is left as is" >&2; exit 1; }
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
  model)
    shift
    [ $# -gt 0 ] || usage
    for f in "$@"; do
      case $f in /*) ;; *) f=$here/$f ;; esac
      [ -f "$f" ] || { echo "ai/os: $f: no such file" >&2; exit 1; }
    done
    api=${GITEA_URL:-$host}/api/v1/repos/ai/os
    tag=model
    hdr=
    [ -n "$GITEA_TOKEN" ] && hdr="Authorization: token $GITEA_TOKEN"
    call() { curl -fsS ${hdr:+-H "$hdr"} "$@"; }
    jid() { python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])'; }
    asset() {
      call "$api/releases/$id/assets" | python3 -c 'import json,sys
print(next((str(a["id"]) for a in json.load(sys.stdin) if a["name"] == sys.argv[1]), ""))' "$1"
    }
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    id=$(call "$api/releases/tags/$tag" 2>/dev/null | jid 2>/dev/null) || id=
    if [ -z "$id" ]; then
      notes=$(printf '%s\n' 'aios のローカルの AI のモデル (GGUF)。aios では `sudo ap -S aimodel` で入り、`sudo aimodel pull` で新しくなる。' '' \
        '- ai.gguf: llama-server と llama cli が使うもの' '- NAME.sha256: aimodel が確かめる sha256')
      body=$(python3 -c 'import json,sys; print(json.dumps({"tag_name": sys.argv[1], "target_commitish": "main", "name": "model", "body": sys.argv[2], "prerelease": False}))' "$tag" "$notes")
      id=$(call -X POST -H 'Content-Type: application/json' -d "$body" "$api/releases" | jid)
      [ -n "$id" ] || { echo "ai/os: could not create the release $tag" >&2; exit 1; }
    fi
    for f in "$@"; do
      case $f in /*) ;; *) f=$here/$f ;; esac
      name=${f##*/}
      sum=$( (sha256sum "$f" 2>/dev/null || shasum -a 256 "$f") | cut -d ' ' -f 1)
      printf '%s  %s\n' "$sum" "$name" > "$tmp/$name.sha256"
      echo "ai/os: $name ($(du -h "$f" | cut -f1), sha256 $sum)"
      old=$(asset "new-$name")
      [ -z "$old" ] || call -X DELETE "$api/releases/$id/assets/$old" > /dev/null
      curl -fS --progress-bar ${hdr:+-H "$hdr"} -X POST -F "attachment=@$f" -o "$tmp/up.json" "$api/releases/$id/assets?name=new-$name"
      new=$(jid < "$tmp/up.json")
      for n in "$name.sha256" "$name"; do
        old=$(asset "$n")
        [ -z "$old" ] || call -X DELETE "$api/releases/$id/assets/$old" > /dev/null
      done
      call -X PATCH -H 'Content-Type: application/json' -d "{\"name\": \"$name\"}" "$api/releases/$id/assets/$new" > /dev/null
      call -X POST -F "attachment=@$tmp/$name.sha256" "$api/releases/$id/assets?name=$name.sha256" > /dev/null
      echo "  $host/ai/os/releases/download/$tag/$name"
    done
    ;;
  *)
    usage
    ;;
esac
