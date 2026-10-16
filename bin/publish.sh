#!/bin/sh
# unix に push したあとに、配るところをそろえる (push するたびに。Claude もこれを動かす):
#   1. GitHub の CI (.github/workflows/unix.yml) がこのコミットのディスクを作って起動を確かめ、リリース unix-latest
#      に置くのを待ち、git.syui.ai の ai/os の unix-latest にも同じものを置く (bin/gitea.sh mirror)。
#      CI が動かないコミット (paths-ignore: ドキュメントなど) なら、ここはとばす
#   2. base / aikernel / aiboot のうち、ai/repo にある版 (0.0.1.rN.gSHA) のあとでソースが変わったものを作って確かめ、
#      ai/repo に送る (aish-pkg build / test / push)。CI はこの 3 つをディスクに入れるだけで ai/repo には送らないので、
#      これをしないと、入っている aios の ap -Syu に来ない。ドキュメントだけの変更では送らない
#   bin/publish.sh [-n]   -n: 何をするかを出すだけ (CI も待たず、いまの様子を)
# CI がしくじったら、2 はしない (ディスクの起動を確かめていないものは配らない)。
# 3 つの PKGBUILD は作業場所から作るので、そのソースにコミットしていない変更があれば (作る前も、作ったあとも)
# 送らずに止まる (待っているあいだに編集しても、それが HEAD の版として配られないように)
set -e
cd "$(dirname "$0")/.."
dry=
[ "$1" = -n ] && dry=1
sha=$(git rev-parse HEAD)
[ "$(git rev-parse origin/unix 2>/dev/null)" = "$sha" ] || git fetch -q origin unix
[ "$(git rev-parse origin/unix)" = "$sha" ] || { echo "publish: HEAD ($(git rev-parse --short HEAD)) is not origin/unix; push it first" >&2; exit 1; }
api=https://api.github.com/repos/syui/aios
json() { python3 -c 'import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1], {"d": d}))' "$1"; }

# 1. CI とリリース
run=
for i in $(seq 1 12); do
  run=$(curl -fsS "$api/actions/workflows/unix.yml/runs?head_sha=$sha&per_page=1" | json 'd["workflow_runs"][0]["id"] if d["workflow_runs"] else ""')
  [ -n "$run" ] && break
  sleep 5
done
if [ -z "$run" ]; then
  echo "ci: no unix.yml run for $(git rev-parse --short HEAD) (paths-ignore); the release stays"
else
  while :; do
    st=$(curl -fsS "$api/actions/runs/$run" | json 'd["status"] + " " + str(d["conclusion"])')
    case "$st" in completed*) break ;; esac
    [ -n "$dry" ] && break
    sleep 15
  done
  echo "ci: $st (https://github.com/syui/aios/actions/runs/$run)"
  if [ "$st" = "completed success" ]; then
    # リリースは CI のおしまいに作りなおされる
    for i in $(seq 1 20); do
      t=$(curl -fsS "$api/releases/tags/unix-latest" | json 'd["target_commitish"]' 2>/dev/null) || t=
      [ "$t" = "$sha" ] && break
      sleep 6
    done
    [ "$t" = "$sha" ] || { echo "publish: the release unix-latest is not $sha" >&2; exit 1; }
    if [ -n "$dry" ]; then echo "would: bin/gitea.sh mirror"; else sh bin/gitea.sh mirror; fi
  elif [ -n "$dry" ]; then
    case "$st" in completed*) echo "would: stop (CI did not pass)" ;; *) echo "would: wait for CI, then bin/gitea.sh mirror" ;; esac
  else
    echo "publish: CI did not pass; not publishing" >&2
    exit 1
  fi
fi

# 2. パッケージ: ai/repo の版の gSHA から、そのパッケージのソースが変わったか
repo_ver() {
  curl -fsS "https://git.syui.ai/api/v1/repos/ai/repo/contents/aarch64/rust?ref=main" |
    json '" ".join(x["name"] for x in d)' | tr ' ' '\n' | sed -n "s/^$1-\(0\.0\.1\.r[0-9]*\.g[0-9a-f]*-[0-9]*\)-aarch64\.pkg\.tar\.zst$/\1/p"
}
paths() {
  case $1 in
    base) echo "user shell etc pkg/rust/base" ;;
    aikernel) echo "kernel Cargo.toml Cargo.lock pkg/rust/aikernel" ;;
    aiboot) echo "boot pkg/rust/aiboot" ;;
  esac
}
todo=
for p in base aikernel aiboot; do
  v=$(repo_ver "$p")
  old=$(echo "$v" | sed -n 's/.*\.g\([0-9a-f]*\)-[0-9]*$/\1/p')
  # shellcheck disable=SC2046
  if [ -n "$old" ] && git cat-file -e "$old^{commit}" 2>/dev/null && git diff --quiet "$old" HEAD -- $(paths "$p"); then
    echo "pkg: $p ${v:-?} is up to date"
  else
    echo "pkg: $p ${v:-none} -> this commit"
    todo="$todo $p"
  fi
done
[ -n "$todo" ] || exit 0
[ -n "$dry" ] && { echo "would: aish-pkg build / test$todo, then push"; exit 0; }
host=$(rustc -vV | sed -n 's/^host: //p')
pkg=shell/target/$host/release/aish-pkg
[ -x "$pkg" ] || (cd shell && cargo build -q --release --target "$host" -p aish-pkg)
# そのパッケージのソースが HEAD のままか (コミットしていない変更、追いかけていないファイルがないか)
clean() {
  # shellcheck disable=SC2046
  dirty=$(git status --porcelain -- $(paths "$1"))
  [ -z "$dirty" ] && return
  echo "publish: $1 would not be $(git rev-parse --short HEAD): uncommitted changes in its sources ($(echo "$dirty" | awk '{print $2}' | head -5 | tr '\n' ' ')); commit or stash them and run bin/publish.sh again" >&2
  exit 1
}
for p in $todo; do
  clean "$p"
  "$pkg" build "$p" >/dev/null || { echo "publish: aish-pkg build $p failed (build/aish-pkg/build-$p.log)" >&2; exit 1; }
  clean "$p"
  "$pkg" test "$p" | sed -n '2,5p'
done
"$pkg" push
