#!/usr/bin/env python3
# kernel/kernel.json (カーネルの地図) とソースがそろっているか。CI と手元で: python3 test/kernel-json.py
#   core   起動して /init が動き、シリアルに書けるのに要るファイル。いつも小さく
#   extra  Linux の口 (syscall、/proc、ioctl、ファイルシステム)、aios の機能、ドライバ。なくてもカーネルは動く
#   quirk  ある機械やプログラムのつごうで入れた手当て (Linux の quirk と同じ考え)。原因 (cause) と、外せる条件 (until)
# core と extra は 1 つのファイルに 1 行で、kernel/src の .rs がみなちょうど 1 度ずつ。quirk は 1 つの手当てに 1 行で、
# src のファイルには札 [quirk NAME] があり、ソースの札はみな一覧の quirk の src のファイルにある。.aios.json の kernel の
# core / extra / quirk は、kernel.json の名前をその順に並べたもの。off (起動の引数 aios.off= で切れるものと、切るとどうなるか) の
# あるものは src/parts.rs の NAMES (と enum Part の順) と同じ。bench (速さを測るもの) はあるファイルで、prof の区間の名前は
# part:what (part は kernel.json の名前。/proc/ai/prof が part ごとに足す)
# aios がえらんだちがい (Linux とちがうふるまいを、えらんでとったもの): test/sysdiff.c の chosen の名前は、doc/aios.md の
# 表と、カーネルのソースの「aios がえらんだちがい (名前)」にもあること (Linux に合わせようとして消さないように)
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

# 切れるもの (起動の引数 aios.off=): src/parts.rs の NAMES が kernel.json の off のあるものと同じで、Part がその順か
parts_rs = text.get("src/parts.rs", "")
m = re.search(r"const NAMES: \[&str; \d+\] = \[(.*?)\];", parts_rs, re.S)
switch = re.findall(r'"([^"]+)"', m.group(1)) if m else []
m = re.search(r"pub enum Part \{(.*?)\}", parts_rs, re.S)
variants = re.findall(r"(\w+),", m.group(1)) if m else []
camel = ["".join(w[:1].upper() + w[1:] for w in re.split(r"[-_]", n)) for n in switch]
if variants != camel:
    err("src/parts.rs: enum Part should be NAMES in the same order (%s)" % ", ".join(camel))
with_off = [e["name"] for e in entries if "off" in e]
if sorted(switch) != sorted(with_off):
    err("src/parts.rs NAMES and kernel.json off differ: only in parts.rs %s, only in kernel.json %s" % (sorted(set(switch) - set(with_off)), sorted(set(with_off) - set(switch))))
for e in entries:
    if "off" in e and not e["off"]:
        err("%s: off is empty" % e.get("name"))

# bench (速さを測るもの) のファイルがあるか。prof の区間 (prof::span("part:what")) の part が kernel.json の名前か
repo = os.path.join(root, "..")
for e in entries:
    b = e.get("bench")
    if b is not None and not os.path.isfile(os.path.join(repo, b.split()[0])):
        err("%s: bench %s is not a file" % (e.get("name"), b.split()[0]))
spans = set()
for s in sources:
    # 注釈 (// から) はのぞく。syscall.rs の sys_span は、システムコールの番号ごとの区間の名前の表
    code = "\n".join(l.split("//")[0] for l in text[s].split("\n"))
    spans.update(re.findall(r'prof::span\("([^"]+)"\)', code))
    m = re.search(r"fn sys_span\(.*?\n\}\n", code, re.S)
    if m:
        spans.update(re.findall(r'=> "([^"]+)"', m.group(0)))
for sp in sorted(spans):
    part = sp.split(":")[0]
    if ":" not in sp:
        err("prof span %r has no part (name it part:what, part a kernel.json name)" % sp)
    elif part not in names:
        err("prof span %r: %s is not a name in kernel.json" % (sp, part))

# .aios.json の kernel (名前だけの一覧) が kernel.json とそろっているか
with open(os.path.join(root, "..", ".aios.json"), encoding="utf-8") as f:
    summary = json.load(f).get("kernel", {})
for t in ("core", "extra", "quirk"):
    want = [e.get("name") for e in entries if e.get("type") == t]
    got = summary.get(t)
    if got != want:
        got = got if isinstance(got, list) else []
        lost, more = [n for n in want if n not in got], [n for n in got if n not in want]
        err(".aios.json kernel.%s is not the %s names of kernel.json in order (missing %s, not in kernel.json %s)" % (t, t, lost, more))

# aios がえらんだちがい: ロボット (test/sysdiff.c の chosen)、doc/aios.md の表、カーネルのソースの印がそろっているか
top = os.path.join(root, "..")
with open(os.path.join(top, "test", "sysdiff.c"), encoding="utf-8") as f:
    robot = f.read()
m = re.search(r"\} chosen\[\] = \{(.*?)\n\};", robot, re.S)
chosen = re.findall(r'\{"([a-z0-9-]+)", "', m.group(1)) if m else []
if not chosen:
    err("test/sysdiff.c: no chosen list")
m = re.search(r"\} linux_bugs\[\] = \{(.*?)\n\};", robot, re.S)
linux_bugs = re.findall(r'\{"([a-z0-9-]+)", "', m.group(1)) if m else []
with open(os.path.join(top, "doc", "aios.md"), encoding="utf-8") as f:
    doc = f.read()
# 見出しからつぎの見出しまでの表の名前
def names(heading):
    table = doc.split(heading, 1)[-1].split("\n#", 1)[0]
    return set(re.findall(r"^\| ([a-z0-9-]+) \|", table, re.M))
in_doc = names("#### aios がえらんだちがい")
marked = {n for s in text.values() for n in re.findall(r"aios がえらんだちがい \(([a-z0-9-]+)\)", s)}
for n in chosen:
    if n not in in_doc:
        err("chosen %s (test/sysdiff.c) is not in the table of doc/aios.md (aios がえらんだちがい)" % n)
    if n not in marked:
        err("chosen %s (test/sysdiff.c) has no 「aios がえらんだちがい (%s)」 in kernel/src" % (n, n))
for n in sorted((in_doc | marked) - set(chosen)):
    err("chosen %s is in doc/aios.md or kernel/src but not in test/sysdiff.c (the robot would not check it)" % n)
# 手本の Linux のまちがい (test/sysdiff.c の linux_bugs) も、doc/aios.md の表とそろっているか
bugs_doc = names("#### 手本の Linux のまちがい")
for n in linux_bugs:
    if n not in bugs_doc:
        err("linux bug %s (test/sysdiff.c) is not in the table of doc/aios.md (手本の Linux のまちがい)" % n)
for n in sorted(bugs_doc - set(linux_bugs)):
    err("linux bug %s is in doc/aios.md but not in test/sysdiff.c (the robot would not correct it)" % n)

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
print("aios.off= can turn off %d: %s" % (len(switch), " ".join(switch)))
print("prof spans in %d parts: %s; bench for %s" % (len({s.split(':')[0] for s in spans}), " ".join(sorted({s.split(':')[0] for s in spans})), " ".join(e["name"] for e in entries if "bench" in e)))
# static mut (だれが守るのかを書いていない。BKL を外した道が触っても、Rust のコンパイラは何も言わない) は増やさない:
# kernel/src/cell.rs の Bkl (大きなロック)、Once (1 度だけ入れる)、Early (起動のときだけ書く)、PerCpu (CPU ごと)、
# Raw (番地を渡すだけ)、atomic、SpinLock にする。残りの 1 つは boot.rs のアセンブリが持つページ表 (memlayout.rs の
# boot_l1_hi: extern の中なので型を変えられない)。減ったら STATIC_MUT_MAX も下げる (ちがえば落ちる: 戻らないように)
STATIC_MUT_MAX = 1
static_mut = [(s, i + 1) for s in sources for i, l in enumerate(text[s].split("\n")) if re.match(r"\s*(pub(\(\w+\))?\s+)?static mut ", l)]
if len(static_mut) > STATIC_MUT_MAX:
    err("static mut: %d, more than %d: use cell::Bkl / cell::Once, an atomic or a SpinLock (%s)" % (len(static_mut), STATIC_MUT_MAX, " ".join("%s:%d" % x for x in static_mut)))
elif len(static_mut) < STATIC_MUT_MAX:
    err("static mut: %d, fewer than %d: lower STATIC_MUT_MAX in test/kernel-json.py" % (len(static_mut), STATIC_MUT_MAX))
cells = {c: sum(1 for t in text.values() for l in t.split("\n") if re.match(r"\s*(pub(\(\w+\))?\s+)?static \w+: (crate::cell::)?%s<" % c, l)) for c in ("Bkl", "Once", "Early", "PerCpu", "Raw")}
print("static mut %d (cell: %s)" % (len(static_mut), ", ".join("%s %d" % c for c in cells.items())))
# cell::PerCpu は割り込みを止めたまま触る (それを実行のたびには見ない: DAIF を読むのは QEMU で重い)。カーネルが割り込みを
# 開けるのは、スケジューラが眠るところ (proc.rs が trap::intr_on を 1 度) と、開いていたものを戻す SpinLock (intr_restore)
# だけであること。ほかで開けるなら、そこで触る PerCpu を考えなおして、ここを変える
opens = [s for s in sources for l in text[s].split("\n") if "daifclr" in l.split("//")[0]]
if sorted(opens) != ["src/spinlock.rs", "src/trap.rs"]:
    err("IRQs should be opened (msr daifclr) only by trap::intr_on and the SpinLock restore, once each: %s (cell::PerCpu needs IRQs off)" % " ".join(opens))
callers = ["%s:%d" % (s, i + 1) for s in sources for i, l in enumerate(text[s].split("\n")) if "intr_on()" in l.split("//")[0] and "fn intr_on" not in l]
if callers != [c for c in callers if c.startswith("src/proc.rs:")] or len(callers) != 1:
    err("trap::intr_on() should be called once, by the scheduler in proc.rs (cell::PerCpu needs IRQs off): %s" % " ".join(callers))
print("chosen (aios がえらんだちがい) %d: %s" % (len(chosen), " ".join(chosen)))
print("linux bugs (手本の Linux のまちがい) %d: %s" % (len(linux_bugs), " ".join(linux_bugs)))
print("kernel-json: %d failed" % len(errors))
sys.exit(min(len(errors), 125))
