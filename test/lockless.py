#!/usr/bin/env python3
# 大きなロック (BKL) なしで動く道の見はり: kernel/src の関数のうち、BKL を持たずに動くものには、fn の上の注釈か属性の
# 並びに [lockless] の印をつける (「どこがロックなしか」の地図)。印のある関数の中では:
#   1. Rc と Weak の数を変えるもの (.clone()、Rc::new、Rc::clone、Rc::downgrade、.upgrade()、Weak::new) を書かない。
#      Rust の Rc の数は atomic でないので、ほかの CPU が BKL の中で同じ数を変えると片方が消え、まだ持っている人のいる
#      ものが返る (2026-11-09 の mem-corrupt-stress: fast_unmap の split が Vma を写して落としていた)。数を変えないもの
#      (u64 や配列の clone、自分だけの新しいもの、Arc) は、その行か上の行に「lockless-ok: わけ」を書く
#   2. カーネルの関数を呼ぶなら、その名前の関数のどれかに [lockless] の印がある (印のないものを呼ぶなら、その行か上の
#      行に「lockless-ok: わけ」)。名前だけで引くので、同じ名前の関数がほかにあれば、それも通る (おおまか)
#   3. 入口 (ROOTS: trap.rs が BKL を取る前と放したあとに呼ぶもの) には印がある
#   4. 印のあるものは、入口から印のあるものを呼んでたどりつく (古い印をためない)
# 見ないもの: かくれた drop (Option に None を入れる、Vec を落とすなど)。それは、印のある関数を書くときに見ること。
#   python3 test/lockless.py        しくじった数を終了コードで (CI の kernel.json のところ)
import os
import re
import sys

ROOT = os.environ.get("LOCKLESS_SRC") or os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "kernel", "src")
# BKL を取る前 (trap.rs の trap_handler の速い道) と、放したあとに呼ぶもの (ファイル, 名前)。ここから印がたどれること。
# 関数のポインタや dyn の向こう (名前ではたどれない) で BKL なしに呼ばれるものも、ここに
ROOTS = [
    ("syscall.rs", "fast"), ("vm.rs", "fast_fault"), ("uart.rs", "intr_fast"), ("smp.rs", "fs_check_released"),
    ("proc.rs", "kernel_enter"), ("proc.rs", "kernel_leave"), ("vm.rs", "free_deferred"),
    # syscall::register_fast_splice、file::register_fast_rw_sock、fastdir::FastFs (dyn)、debug::Tracer (dyn) の向こう
    ("splice.rs", "fast"), ("unix.rs", "fast_rw"),
]
# 名前がカーネルの関数とかさなるが、呼ぶのは core や alloc のもの (Option、Vec、イテレータ、RefCell、atomic) である
# ことが多い名前。印のある関数の中でこれを呼ぶときは、カーネルの関数とはみない
EXTERNAL = {
    "new", "len", "is_empty", "get", "get_mut", "iter", "iter_mut", "map", "and_then", "filter", "find", "position", "min",
    "max", "take", "insert", "remove", "push", "pop", "contains", "load", "store", "fetch_add", "fetch_sub", "swap",
    "as_ref", "as_mut", "as_ptr", "is_some", "is_none", "unwrap", "unwrap_or", "ok", "ok_or", "copied", "cloned", "next",
    "range", "first", "last", "split", "starts_with", "ends_with", "as_bytes", "to_le_bytes", "from_le_bytes", "lock",
    "read", "write", "fill", "any", "all", "count", "sum", "rev", "zip", "chain", "flatten", "enumerate", "skip", "step_by",
    "from", "into", "try_into", "unwrap_or_else", "map_or", "is_some_and", "is_none_or", "then", "then_some", "extend",
    "retain", "truncate", "clear", "drain", "append", "entry", "or_insert", "values", "keys", "get_or_insert_with",
    "saturating_sub", "saturating_add", "checked_add", "checked_sub", "wrapping_add", "wrapping_sub", "min_by_key",
    "max_by_key", "compare_exchange", "fetch_or", "fetch_and", "swap_remove", "copy_from_slice", "split_at", "chunks",
    "borrow", "borrow_mut", "try_borrow", "try_borrow_mut", "deref", "drop", "with_capacity", "resize", "rsplit_once",
    "split_once", "trim", "parse", "to_string", "eq", "cmp", "partial_cmp", "hash", "fmt", "default", "clone_from",
    "div_ceil", "trailing_zeros", "leading_zeros", "count_ones", "rotate_left", "wrapping_mul", "pow", "abs", "signum",
    "add", "sub", "set", "replace", "or_else", "or", "and", "xor", "not", "neg", "index", "index_mut", "write_bytes",
    "read_volatile", "write_volatile", "spin_loop", "fence", "from_raw_parts", "from_raw_parts_mut", "get_unchecked",
    "is_null", "null_mut", "null", "offset", "byte_add", "collect", "sort", "sort_by", "sort_by_key", "dedup", "split_off", "clone", "uninit", "zeroed", "into_iter", "extend_from_slice", "upgrade", "downgrade",
}
# Rc と Weak の数を変えるもの
RC_OPS = re.compile(r"\.clone\(\)|\bRc::new\(|\bRc::clone\(|\bRc::downgrade\(|\.upgrade\(\)|\bWeak::new\(")
KEYWORDS = {"if", "while", "for", "match", "return", "loop", "in", "as", "let", "fn", "move", "unsafe", "else", "break",
            "continue", "where", "impl", "mut", "ref", "Some", "None", "Ok", "Err", "Box", "Vec", "String", "self", "Self",
            "super", "crate", "true", "false", "async", "await", "dyn", "type", "struct", "enum", "trait", "use", "pub",
            "const", "static", "extern"}


def strip(src):
    """注釈と文字列の中身を空白にする (行と桁はそのまま)。注釈は別に返す (行ごと)"""
    out = []
    comments = {}
    i, n, line = 0, len(src), 1
    while i < n:
        c = src[i]
        if c == "\n":
            out.append(c)
            line += 1
            i += 1
        elif src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            comments.setdefault(line, "")
            comments[line] += src[i:j]
            out.append(" " * (j - i))
            i = j
        elif src.startswith("/*", i):
            j = src.find("*/", i + 2)
            j = n if j < 0 else j + 2
            seg = src[i:j]
            out.append(re.sub(r"[^\n]", " ", seg))
            line += seg.count("\n")
            i = j
        elif c == "r" and re.match(r'r#*"', src[i:i + 8]) and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
            m = re.match(r'r(#*)"', src[i:])
            end = '"' + m.group(1)
            j = src.find(end, i + len(m.group(0)))
            j = n if j < 0 else j + len(end)
            seg = src[i:j]
            out.append(re.sub(r"[^\n]", " ", seg))
            line += seg.count("\n")
            i = j
        elif c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            j = min(j + 1, n)
            seg = src[i:j]
            out.append('"' + re.sub(r"[^\n]", " ", seg[1:-1]) + '"' if len(seg) >= 2 else seg)
            line += seg.count("\n")
            i = j
        elif c == "'":
            # 文字 ('a'、'\n'、'\u{..}') か、ライフタイム ('a)
            m = re.match(r"'(\\u\{[0-9a-fA-F]+\}|\\.|[^\\'])'", src[i:])
            if m:
                out.append(" " * len(m.group(0)))
                i += len(m.group(0))
            else:
                out.append(c)
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out), comments


FN = re.compile(r"\bfn\s+([a-z_][a-z0-9_]*)\s*(<[^{;]*?>)?\s*\(")


def functions(path):
    src = open(path, encoding="utf-8").read()
    code, comments = strip(src)
    lines = src.split("\n")
    out = []
    for m in FN.finditer(code):
        name = m.group(1)
        start_line = code.count("\n", 0, m.start()) + 1
        # 本体: 引数の ( の外で、最初の { (; が先なら本体なし: trait の宣言)。数えるのは ( ) と [ ] だけ (-> の > や
        # 戻り値の [T; N] の ; にまどわされないように)
        k = m.end()
        depth = 1
        brace = semi = None
        while k < len(code):
            ch = code[k]
            if ch in "([":
                depth += 1
            elif ch in ")]":
                depth -= 1
            elif ch == "{" and depth == 0:
                brace = k
                break
            elif ch == ";" and depth == 0:
                semi = k
                break
            k += 1
        if brace is None:
            continue
        d, k = 0, brace
        while k < len(code):
            if code[k] == "{":
                d += 1
            elif code[k] == "}":
                d -= 1
                if d == 0:
                    break
            k += 1
        end_line = code.count("\n", 0, k) + 1
        # 印: fn の上の、注釈か属性か空でない doc の続くところ
        marked = False
        j = start_line - 1
        while j >= 1:
            t = lines[j - 1].strip()
            if t.startswith("//") or t.startswith("#[") or t.startswith("#!"):
                if "[lockless]" in t:
                    marked = True
                j -= 1
                continue
            break
        body = code[brace:k + 1]
        out.append({"name": name, "file": os.path.basename(path), "line": start_line, "end": end_line, "marked": marked,
                    "body": body, "body_line": code.count("\n", 0, brace) + 1, "comments": comments, "src": lines})
    return out


def main():
    fns = []
    for f in sorted(os.listdir(ROOT)):
        if f.endswith(".rs"):
            fns += functions(os.path.join(ROOT, f))
    by_name = {}
    for f in fns:
        by_name.setdefault(f["name"], []).append(f)
    errs = []

    def ok_at(f, line):
        # その行の注釈か、すぐ上につづく注釈だけの行に「lockless-ok」
        if "lockless-ok" in f["comments"].get(line, ""):
            return True
        j = line - 1
        while j >= 1 and f["src"][j - 1].strip().startswith("//"):
            if "lockless-ok" in f["src"][j - 1]:
                return True
            j -= 1
        return False

    for file, r in ROOTS:
        if not any(f["marked"] and f["file"] == file for f in by_name.get(r, [])):
            errs.append("root %s in %s is not marked [lockless]" % (r, file))
    marked = [f for f in fns if f["marked"]]
    for f in marked:
        # 中の関数 (fn の中の fn) は、それぞれの印で見る: ここでは外の本体だけを行で見る
        inner = [(g["line"], g["end"]) for g in fns if g["file"] == f["file"] and f["line"] < g["line"] <= f["end"]]
        # 本体の中のクロージャ (let NAME = |..| や move |..|) は、外の関数とまちがえない
        local = set(re.findall(r"\blet\s+(?:mut\s+)?([a-z_][a-z0-9_]*)\s*(?::[^=]*)?=\s*(?:move\s*)?\|", f["body"]))
        for i, text in enumerate(f["body"].split("\n")):
            line = f["body_line"] + i
            if any(a <= line <= b for a, b in inner):
                continue
            for m in RC_OPS.finditer(text):
                if not ok_at(f, line):
                    errs.append("%s:%d: %s (lockless): %s changes an Rc count without the BKL (or write lockless-ok: why)" % (f["file"], line, f["name"], m.group(0)))
            for m in re.finditer(r"(\.|::|\b)([a-z_][a-z0-9_]*)\s*(::<[^>]*>)?\(", text):
                sep, name = m.group(1), m.group(2)
                if name in KEYWORDS or name not in by_name or name in local:
                    continue
                # drop(x) は core の drop (かくれた drop と同じく見ない)
                if sep == "" and name == "drop":
                    continue
                # self.NAME( はいつもカーネルのもの。ほかの .NAME( は、よくある名前なら core や alloc のものとみる
                if sep == "." and name in EXTERNAL and not text[:m.start()].endswith("self"):
                    continue
                if sep == "::" and name in EXTERNAL and not any(d["marked"] for d in by_name[name]):
                    # Vec::new、AtomicU64::new など
                    continue
                if text[:m.start()].rstrip().endswith("fn"):
                    continue
                if any(d["marked"] for d in by_name[name]):
                    continue
                if ok_at(f, line):
                    continue
                defs = ", ".join("%s:%d" % (d["file"], d["line"]) for d in by_name[name][:3])
                errs.append("%s:%d: %s (lockless) calls %s (%s), not marked [lockless] (mark it or write lockless-ok: why)" % (f["file"], line, f["name"], name, defs))
    # 4. 印のあるものは、入口 (ROOTS) から印のあるものを呼んでたどりつく (古い印をためない)。名前で引くので、よくある
    #    名前 (EXTERNAL) の呼び出しも、その名前の印のあるものへの矢印にする (おおまか)
    reach = set()
    stack = [f for f in marked for file, r in ROOTS if f["file"] == file and f["name"] == r]
    while stack:
        f = stack.pop()
        key = (f["file"], f["line"])
        if key in reach:
            continue
        reach.add(key)
        local = set(re.findall(r"\blet\s+(?:mut\s+)?([a-z_][a-z0-9_]*)\s*(?::[^=]*)?=\s*(?:move\s*)?\|", f["body"]))
        for m in re.finditer(r"(\.|::|\b)([a-z_][a-z0-9_]*)\s*(::<[^>]*>)?\(", f["body"]):
            name = m.group(2)
            if name in KEYWORDS or name in local:
                continue
            stack += [d for d in by_name.get(name, []) if d["marked"]]
    for f in marked:
        if (f["file"], f["line"]) not in reach:
            errs.append("%s:%d: %s is marked [lockless] but no root reaches it (remove the mark)" % (f["file"], f["line"], f["name"]))
    for e in errs:
        print("FAIL " + e)
    print("lockless: %d marked functions, %d failed" % (len(marked), len(errs)))
    return 1 if errs else 0


if __name__ == "__main__":
    sys.exit(main())
