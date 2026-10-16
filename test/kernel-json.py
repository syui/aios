#!/usr/bin/env python3
# kernel/kernel.json (カーネルの地図) とソースがそろっているか。CI と手元で: python3 test/kernel-json.py
#   core   起動して /init が動き、シリアルに書けるのに要るファイル。いつも小さく
#   extra  Linux の口 (syscall、/proc、ioctl、ファイルシステム)、aios の機能、ドライバ。なくてもカーネルは動く
#   quirk  ある機械やプログラムのつごうで入れた手当て (Linux の quirk と同じ考え)。原因 (cause) と、外せる条件 (until)
# core と extra は 1 つのファイルに 1 行で、kernel/src の .rs がみなちょうど 1 度ずつ。quirk は 1 つの手当てに 1 行で、
# src のファイルには札 [quirk NAME] があり、ソースの札はみな一覧の quirk の src のファイルにある
# 終わりの番号はくいちがいの数。おしまいに数と、core が extra を使っているところ (いまはまだ多い。減らしていく)
import json
import os
import re
import sys

root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "kernel")
errors = []


def err(msg):
    errors.append(msg)
    print("FAIL " + msg)


with open(os.path.join(root, "kernel.json"), encoding="utf-8") as f:
    entries = json.load(f)

sources = sorted(os.path.relpath(os.path.join(d, n), root) for d, _, ns in os.walk(os.path.join(root, "src")) for n in ns if n.endswith(".rs"))
text = {s: open(os.path.join(root, s), encoding="utf-8").read() for s in sources}
names, files, quirks = set(), {}, {}
for e in entries:
    n, t = e.get("name", ""), e.get("type", "")
    if not n or n in names:
        err("name %r is empty or listed twice" % n)
    names.add(n)
    if t in ("core", "extra"):
        src = e.get("src")
        if not isinstance(src, str) or src not in text:
            err("%s: src %r is not a file in kernel/src" % (n, src))
        elif src in files:
            err("%s: %s is already listed as %s" % (n, src, files[src][0]))
        else:
            files[src] = (n, t)
        if not e.get("what"):
            err("%s: no what" % n)
    elif t == "quirk":
        src = e.get("src")
        if not isinstance(src, list) or not src:
            err("%s: src should be a list of files" % n)
            continue
        for s in src:
            if s not in text:
                err("%s: src %s is not a file in kernel/src" % (n, s))
            elif "[quirk %s]" % n not in text[s]:
                err("%s: %s has no [quirk %s] in it" % (n, s, n))
        for k in ("cause", "what", "until"):
            if not e.get(k):
                err("%s: no %s" % (n, k))
        quirks[n] = set(src)
    else:
        err("%s: type %r (core, extra or quirk)" % (n, t))

for s in sources:
    if s not in files:
        err("%s is not in kernel.json (core or extra)" % s)
    for m in re.finditer(r"\[quirk ([^\]\s]+)\]", text[s]):
        q = m.group(1)
        if q not in quirks:
            err("%s: [quirk %s] is not in kernel.json" % (s, q))
        elif s not in quirks[q]:
            err("%s: [quirk %s], but %s is not in its src" % (s, q, s))

# 数と、core が extra を使っているところ (crate::NAME)
lines = {t: sum(text[s].count("\n") for s, (_, k) in files.items() if k == t) for t in ("core", "extra")}
count = {t: sum(1 for _, k in files.values() if k == t) for t in ("core", "extra")}
extra = {n for n, k in files.values() if k == "extra"}
uses = {}
for s, (n, k) in sorted(files.items()):
    if k == "core":
        used = sorted({u for u in re.findall(r"crate::(\w+)", text[s]) if u in extra})
        if used:
            uses[n] = used
print("kernel.json: core %d files (%d lines), extra %d files (%d lines), quirk %d" % (count["core"], lines["core"], count["extra"], lines["extra"], len(quirks)))
print("core uses extra in %d files: %s" % (len(uses), "; ".join("%s → %s" % (n, " ".join(u)) for n, u in uses.items())))
print("kernel-json: %d failed" % len(errors))
sys.exit(min(len(errors), 125))
