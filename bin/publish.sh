#!/bin/sh
# main に push したあとに、配るところをそろえる (push するたびに。Claude もこれを動かす):
#   1. GitHub の CI (.github/workflows/unix.yml) がこのコミットのディスクを作って起動を確かめ、リリース latest
#      (タイトル aios) に置くのを待ち、git.syui.ai の ai/os の latest にも同じものを置く (bin/gitea.sh mirror)。
#      CI が動かないコミット (paths-ignore: ドキュメント、pkg.json など) なら、CI の走った一番新しいコミットのうち、
#      そこから HEAD までに CI が見るファイルが変わっていないものの run を待って使う (まだ走っていても)。
#      ai/os の latest がもうそのコミットのものなら置きなおさない
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
[ "$(git rev-parse origin/main 2>/dev/null)" = "$sha" ] || git fetch -q origin main
[ "$(git rev-parse origin/main)" = "$sha" ] || { echo "publish: HEAD ($(git rev-parse --short HEAD)) is not origin/main; push it first" >&2; exit 1; }
api=https://api.github.com/repos/syui/aios
json() { python3 -c 'import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1], {"d": d}))' "$1"; }

# CI が最後に通ったコミット ($1) から、CI が見るファイル (unix.yml の paths-ignore の外) が変わっていれば 1 つ出す
# (変わっていなければ何も出さない。$1 がここになくて分からなければ "?")
ci_changed() {
  git cat-file -e "$1^{commit}" 2>/dev/null || { echo "?"; return 0; }
  git diff --name-only "$1" HEAD | python3 -c '
import fnmatch, re, sys
y = open(".github/workflows/unix.yml").read()
m = re.search(r"paths-ignore:\n((?:\s+(?:#.*|- .*)\n)+)", y)
pats = re.findall(r"- \x27([^\x27]+)\x27", m.group(1)) if m else []
for f in sys.stdin.read().split():
    if not any(fnmatch.fnmatch(f, p) for p in pats):
        print(f)
        break'
}

# 1. CI とリリース
run=
for i in $(seq 1 12); do
  run=$(curl -fsS "$api/actions/workflows/unix.yml/runs?head_sha=$sha&per_page=1" | json 'd["workflow_runs"][0]["id"] if d["workflow_runs"] else ""')
  [ -n "$run" ] && break
  sleep 5
done
# リリースになるコミット (このコミットか、run を借りるコミット)
rsha=$sha
if [ -z "$run" ]; then
  # paths-ignore で動かないのか、GitHub が動かさなかった (遅れている) だけか。main の run を新しい順に見て、HEAD の祖先で
  # そこから CI が見るものが変わっていない一番新しいものを使う (pkg.json だけのコミットの前のコミットの CI がまだ走って
  # いれば、それを待つ)。なければ、ディスクの起動を確かめていないので、パッケージも送らずに止まる
  runs=$(curl -fsS "$api/actions/workflows/unix.yml/runs?branch=main&per_page=30" | json '"\n".join(str(r["id"]) + " " + r["head_sha"] for r in d["workflow_runs"])')
  newest= changed=
  while read -r id s; do
    [ -n "$id" ] && git merge-base --is-ancestor "$s" HEAD 2>/dev/null || continue
    c=$(ci_changed "$s")
    [ -n "$newest" ] || { newest=$s changed=$c; }
    [ -z "$c" ] && { run=$id rsha=$s; break; }
  done <<RUNS
$runs
RUNS
  if [ -z "$run" ]; then
    echo "publish: no unix.yml run for $(git rev-parse --short HEAD), but it changes what CI builds since the newest run (${newest:+$(git rev-parse --short "$newest")}: ${changed:-?}): GitHub did not start it (yet). Run unix.yml by hand (Actions: unix, Run workflow on main; or workflow_dispatch) or wait, then bin/publish.sh again" >&2
    [ -n "$dry" ] || exit 1
  else
    echo "ci: no unix.yml run for $(git rev-parse --short HEAD) (paths-ignore); using the run of $(git rev-parse --short "$rsha") (nothing CI builds changed since)"
  fi
fi
if [ -n "$run" ]; then
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
      t=$(curl -fsS "$api/releases/tags/latest" | json 'd["target_commitish"]' 2>/dev/null) || t=
      [ "$t" = "$rsha" ] && break
      sleep 6
    done
    [ "$t" = "$rsha" ] || { echo "publish: the release latest is not $rsha" >&2; exit 1; }
    g=$(curl -fsS "https://git.syui.ai/api/v1/repos/ai/os/releases/tags/latest" | json 'd["target_commitish"]' 2>/dev/null) || g=
    if [ "$g" = "$rsha" ]; then
      echo "ai/os: latest is already $(git rev-parse --short "$rsha")"
    elif [ -n "$dry" ]; then
      echo "would: bin/gitea.sh mirror"
    else
      sh bin/gitea.sh mirror
    fi
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
# いまのソースでその PKGBUILD の pkgver() が出す版 (0.0.1.rN.gSHA)
now_ver() {
  bash -c 'startdir=$1; . "$1/PKGBUILD"; pkgver' _ "$PWD/pkg/rust/$1"
}
rev() {
  echo "$1" | sed -n 's/^0\.0\.1\.r\([0-9]*\)\.g.*/\1/p'
}
todo=
for p in base aikernel aiboot; do
  v=$(repo_ver "$p")
  old=$(echo "$v" | sed -n 's/.*\.g\([0-9a-f]*\)-[0-9]*$/\1/p')
  # shellcheck disable=SC2046
  if [ -n "$old" ] && git cat-file -e "$old^{commit}" 2>/dev/null && git diff --quiet "$old" HEAD -- $(paths "$p"); then
    echo "pkg: $p ${v:-?} is up to date"
  elif [ -n "$old" ] && ! git cat-file -e "$old^{commit}" 2>/dev/null && [ "$(rev "$v")" = "$(rev "$(now_ver "$p")")" ]; then
    # その版のコミットがこの歴史にない (署名しなおしなどで書きかえた) が、rN はいまと同じ: 同じところの書きかえとみる。
    # 作りなおすと gSHA だけがちがう版になり、版のくらべ方しだいで ai/repo のほうが新しいとされて送れない
    echo "pkg: $p $v is up to date (g$old is not in this history; same r$(rev "$v"), taken as rewritten)"
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
