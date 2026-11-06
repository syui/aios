#!/usr/bin/env python3
# .aios.json (リポジトリの見取り図: aigpt import os で PDS の ai.syui.gpt.os にも) が、本物とそろっているか。
# CI と手元で: python3 test/aios-json.py。1 つのことは 1 か所 (本物) にあり、.aios.json はそれを写したもの:
#   pkg       パッケージの版は pkg/KIND/NAME/PKGBUILD の pkgver、種類は KIND (rust c shell desktop)、組は自作なら
#             "aios" (pkg/upstream.json の skip が "aios")、ほかは "linux"。どの PKGBUILD もちょうど 1 度。npm と uv の
#             下は人が書く (PKGBUILD のない、npm / uv で入れるもの)
#   aish      plugins は base のパッケージが入れる aish-* (pkg/rust/base/PKGBUILD の install の並び) と、pkg/shell の
#             aish-* のパッケージ
#   kernel    core / extra / quirk は kernel/kernel.json の名前 (test/kernel-json.py が見る)。ほかのキーは置かない
#   ほか      上の段のキーは決まったものだけ (課題は todo.json に、のぞむ設定は OS の中の /etc/aios.json に)
# aish-pkg の pkg_edit / pkg_build (aios_sync) が pkg を書きかえる。終わりの番号はくいちがいの数
import json
import os
import re
import sys

top = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
errors = []


def err(msg):
    errors.append(msg)
    print("FAIL " + msg)


with open(os.path.join(top, ".aios.json"), encoding="utf-8") as f:
    aios = json.load(f)

KEYS = ["branch", "arch", "updated", "repo", "toolchain", "graphics", "aish", "kernel", "pkg"]
for k in aios:
    if k not in KEYS:
        err("unknown top-level key %r (put todos in todo.json, settings in /etc/aios.json)" % k)
for k in aios.get("kernel", {}):
    if k not in ("core", "extra", "quirk"):
        err("unknown kernel key %r (kernel.json has the parts; todos go to todo.json)" % k)
if not re.fullmatch(r"\d{4}-\d{2}-\d{2}", str(aios.get("updated", ""))):
    err("updated %r is not YYYY-MM-DD" % aios.get("updated"))

KINDS = ["rust", "c", "shell", "desktop"]


def pkgver(path):
    """PKGBUILD の pkgver (同じファイルの変数 $x、${x}、${x:0:N} はひらく)"""
    text = open(path, encoding="utf-8").read()
    vars_ = dict(re.findall(r"^(\w+)=['\"]?([^'\"\s]*)['\"]?\s*$", text, re.M))
    v = vars_.get("pkgver", "")

    def sub(m):
        name, a, n = m.group(1) or m.group(4), m.group(2), m.group(3)
        val = vars_.get(name, "")
        return val[int(a):int(a) + int(n)] if a is not None else val

    return re.sub(r"\$\{(\w+)(?::(\d+):(\d+))?\}|\$(\w+)", sub, v)


with open(os.path.join(top, "pkg", "upstream.json"), encoding="utf-8") as f:
    own = {e["name"] for e in json.load(f) if e.get("skip") == "aios"}

builds = {}
for kind in KINDS:
    d = os.path.join(top, "pkg", kind)
    for name in sorted(os.listdir(d)):
        p = os.path.join(d, name, "PKGBUILD")
        if os.path.isfile(p):
            builds[name] = (kind, pkgver(p))

seen = {}
pkg = aios.get("pkg", {})
for group in pkg:
    if group not in ("aios", "linux"):
        err("unknown pkg group %r (aios or linux)" % group)
for group in ("aios", "linux"):
    for kind, items in pkg.get(group, {}).items():
        if kind in ("npm", "uv"):
            continue
        if kind not in KINDS:
            err("pkg.%s.%s: unknown kind" % (group, kind))
            continue
        for name, ver in items.items():
            seen.setdefault(name, []).append((group, kind, ver))
for name, (kind, ver) in builds.items():
    where = seen.get(name, [])
    if len(where) != 1:
        err("pkg/%s/%s is listed %d times in .aios.json pkg (want once)" % (kind, name, len(where)))
        continue
    group, k, v = where[0]
    want = "aios" if name in own else "linux"
    if k != kind:
        err("%s: listed as %s, but its PKGBUILD is in pkg/%s" % (name, k, kind))
    if group != want:
        err("%s: listed in pkg.%s, but it belongs in pkg.%s (upstream.json skip %s)" % (name, group, want, "aios" if want == "aios" else "is not aios"))
    if v != ver:
        err("%s: .aios.json has %s, PKGBUILD pkgver is %s" % (name, v, ver))
for name in seen:
    if name not in builds:
        err("%s is in .aios.json pkg but has no pkg/*/%s/PKGBUILD" % (name, name))

# aish のプラグイン: base が入れるもの (install の並び) と pkg/shell の aish-*
base = open(os.path.join(top, "pkg", "rust", "base", "PKGBUILD"), encoding="utf-8").read()
m = re.search(r"for p in ((?:aish-\S+ ?)+); do\n\s+install [^\n]*/usr/lib/aish/plugins/", base)
want = [p[len("aish-"):] for p in m.group(1).split()] if m else []
if not m:
    err("pkg/rust/base/PKGBUILD: cannot find the loop that installs aish-* into /usr/lib/aish/plugins")
want += [n[len("aish-"):] for n in sorted(os.listdir(os.path.join(top, "pkg", "shell"))) if n.startswith("aish-")]
got = aios.get("aish", {}).get("plugins", [])
if got != want:
    err("aish.plugins is %s; the packages install %s" % (got, want))

print("aios-json: %d failed (%d packages, %d plugins)" % (len(errors), len(builds), len(want)))
sys.exit(1 if errors else 0)
