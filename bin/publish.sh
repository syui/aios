#!/bin/sh
set -e
cd "$(dirname "$0")/.."
dry=
[ "$1" = -n ] && dry=1
sha=$(git rev-parse HEAD)
[ "$(git rev-parse origin/main 2>/dev/null)" = "$sha" ] || git fetch -q origin main
[ "$(git rev-parse origin/main)" = "$sha" ] || { echo "publish: HEAD ($(git rev-parse --short HEAD)) is not origin/main; push it first" >&2; exit 1; }
api=https://api.github.com/repos/syui/aios
json() { python3 -c 'import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1], {"d": d}))' "$1"; }

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

run=
for i in $(seq 1 12); do
  run=$(curl -fsS "$api/actions/workflows/unix.yml/runs?head_sha=$sha&per_page=1" | json 'd["workflow_runs"][0]["id"] if d["workflow_runs"] else ""')
  [ -n "$run" ] && break
  sleep 5
done
rsha=$sha
if [ -z "$run" ]; then
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
# CI のいまの step と、前に通った run でのその step の長さ (1 分に 1 度、変わったら 1 行: ship が publish のログの
# 終わりを見せるので、どこで待っているか、いつもより長いか (止まっていないか) がわかる。2026-11-22 に VM test が
# panic で止まり、GitHub を見にいくまでわからなかった)
step_times() {
  python3 -c '
import json, sys
from datetime import datetime
t = lambda s: datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()
for j in json.load(sys.stdin).get("jobs", []):
    for st in j.get("steps", []):
        if st.get("started_at") and st.get("completed_at"):
            print("%s\t%d" % (st["name"], t(st["completed_at"]) - t(st["started_at"])))'
}
step_now() {
  python3 -c '
import json, sys, time
from datetime import datetime
usual = dict(l.rsplit("\t", 1) for l in sys.argv[1].splitlines() if "\t" in l)
m = lambda s: "%dm" % (s // 60) if s >= 60 else "%ds" % s
for j in json.load(sys.stdin).get("jobs", []):
    for st in j.get("steps", []):
        if st.get("status") == "in_progress" and st.get("started_at"):
            took = time.time() - datetime.fromisoformat(st["started_at"].replace("Z", "+00:00")).timestamp()
            u = int(usual[st["name"]]) if st["name"] in usual else None
            slow = u is not None and took > 1.5 * u + 120
            print("ci: step \"%s\" for %s%s%s" % (st["name"], m(int(took)), " (usually %s)" % m(u) if u is not None else "", "; slow: stuck? look at the run" if slow else ""))
            sys.exit(0)' "$1"
}
if [ -n "$run" ]; then
  prev=$(curl -fsS "$api/actions/workflows/unix.yml/runs?branch=main&status=success&per_page=1" 2>/dev/null | json 'd["workflow_runs"][0]["id"] if d["workflow_runs"] else ""' 2>/dev/null) || prev=
  usual=
  [ -z "$prev" ] || usual=$(curl -fsS "$api/actions/runs/$prev/jobs?per_page=50" 2>/dev/null | step_times 2>/dev/null) || usual=
  n=0 last=
  while :; do
    st=$(curl -fsS "$api/actions/runs/$run" | json 'd["status"] + " " + str(d["conclusion"])')
    case "$st" in completed*) break ;; esac
    [ -n "$dry" ] && break
    if [ $((n % 4)) = 0 ]; then
      line=$(curl -fsS "$api/actions/runs/$run/jobs?per_page=50" 2>/dev/null | step_now "$usual" 2>/dev/null) || line=
      [ -z "$line" ] || [ "$line" = "$last" ] || { echo "$line"; last=$line; }
    fi
    n=$((n + 1))
    sleep 15
  done
  echo "ci: $st (https://github.com/syui/aios/actions/runs/$run)"
  if [ "$st" = "completed success" ]; then
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
  if [ -n "$old" ] && git cat-file -e "$old^{commit}" 2>/dev/null && git diff --quiet "$old" HEAD -- $(paths "$p"); then
    echo "pkg: $p ${v:-?} is up to date"
  elif [ -n "$old" ] && ! git cat-file -e "$old^{commit}" 2>/dev/null && [ "$(rev "$v")" = "$(rev "$(now_ver "$p")")" ]; then
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
clean() {
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
