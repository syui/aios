// sysdiff: Linux と答えあわせ (test/sysdiff.c)。種から決まるシステムコールの並びを、手本の Linux と VM の aios で
// 動かし、case ごとに最初にちがう行をくらべる。手本は既定で kernel.org の最新の安定版 (test/linux-vm.sh が arm64 で
// 作り、QEMU で起こした中で動かす)。base: host なら、このマシンのカーネル。ちがいは (op、Linux の答え、aios の答え) で
// まとめ (syzbot がクラッシュをまとめるように)、多いものから、op を減らしてもちがいが残るいちばん小さな並び
// (SEED:I,J,...) にする (C-Reduce や syzkaller の minimize と同じ: 縮めたものも、同じちがいが同じ op で出ること)。
// 縮めるのは、まとめたものをみな 1 回の Linux と 1 回の VM でためす (VM は 1 回の行き来が重いので)。
// aios がえらんだちがい (sysdiff -l の chosen) は、Linux でもそのふるまいをまねて答えにするので、ちがいには
// ならない。それを通った op の行には [chosen NAME] がつくので、いくつの case で使ったかを chosen に出す
use super::{boot, compiler, kernel_for, note, sh, sub, vmpy};
use aish_plugin::{Value, json, s};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn mtime(p: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// test/sysdiff.c を、このマシンのもの (cc) と aios のもの (aarch64 の静的なもの) に。ソースが新しいときだけ
fn build(root: &Path, dir: &Path) -> Result<(PathBuf, PathBuf), String> {
    let src = root.join("test/sysdiff.c");
    let newer = |out: &Path| mtime(out).is_none() || mtime(&src) > mtime(out);
    let srcs = src.display().to_string();
    let host = dir.join("sysdiff-host");
    if newer(&host) {
        let (st, _, err, _, _) = sh(root, "cc", &["-O2", "-o", &host.display().to_string(), &srcs], &[], Duration::from_secs(120));
        if st != 0 {
            return Err(format!("cc: {}", err.trim()));
        }
    }
    let aios = dir.join("sysdiff");
    if newer(&aios) {
        let Some((cc, head)) = compiler(root, false) else { return Err("no compiler for aios: zig (PATH, $ZIG or build/zig/*/zig) or aarch64-linux-gnu-gcc".into()) };
        let mut args = head;
        args.extend(["-O2".into(), "-static".into(), "-s".into(), "-o".into(), aios.display().to_string(), srcs]);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let (st, _, err, _, _) = sh(root, &cc, &argv, &[], Duration::from_secs(120));
        if st != 0 {
            return Err(format!("{}: {}", cc, err.trim()));
        }
    }
    Ok((host, aios))
}

/// aios がえらんだちがい (kind "chosen") か手本の Linux のまちがい (kind "linuxbug") の一覧 (sysdiff -l の
/// 「KIND NAME わけ」の行): (名前, わけ)
fn marked_list(root: &Path, host: &Path, kind: &str) -> Vec<(String, String)> {
    let (st, out, _, _, _) = sh(root, &host.display().to_string(), &["-l"], &[], Duration::from_secs(10));
    if st != 0 {
        return Vec::new();
    }
    let prefix = format!("{} ", kind);
    out.lines().filter_map(|l| l.strip_prefix(prefix.as_str())?.split_once(' ')).map(|(n, w)| (n.to_string(), w.to_string())).collect()
}

/// えらんだちがい (kind "chosen") や Linux のまちがい (kind "linuxbug") の name を通った case の数 (どちらかの
/// 出力に [KIND NAME] で終わる行があるもの)
fn marked_cases(kind: &str, name: &str, l: &[(String, Vec<String>)], a: &[(String, Vec<String>)]) -> usize {
    let mark = format!("[{} {}]", kind, name);
    let used: std::collections::BTreeSet<&str> = l.iter().chain(a).filter(|(_, v)| v.iter().any(|x| x.ends_with(&mark))).map(|(c, _)| c.as_str()).collect();
    used.len()
}

/// 手本の Linux: このマシンのカーネル (host) か、test/linux-vm.sh で作った版を QEMU で起こしたもの (Image のパスと版)
enum Base {
    Host,
    Vm(String, String),
}

impl Base {
    /// base の名前 (latest、X.Y.Z、host) から。版のものは、なければ作る (一度だけ。数分)
    fn get(root: &Path, spec: &str) -> Result<Base, String> {
        if spec == "host" {
            return Ok(Base::Host);
        }
        let script = root.join("test/linux-vm.sh").display().to_string();
        let (st, v, err, _, _) = sh(root, &script, &["version", spec], &[], Duration::from_secs(60));
        if st != 0 {
            return Err(format!("test/linux-vm.sh version {}: {}", spec, err.trim()));
        }
        let v = v.trim().to_string();
        note(json!({ "step": "linux kernel", "version": v }));
        let (st, out, err, _, to) = sh(root, &script, &["build", &v], &[], Duration::from_secs(3600));
        let image = out.lines().last().unwrap_or_default().trim().to_string();
        if st != 0 || to || image.is_empty() {
            return Err(format!("test/linux-vm.sh build {}{}: {}", v, if to { " (timed out)" } else { "" }, err.trim()));
        }
        Ok(Base::Vm(image, v))
    }

    fn name(&self) -> String {
        match self {
            Base::Host => "host".into(),
            Base::Vm(_, v) => format!("Linux {} (built by test/linux-vm.sh, run in QEMU)", v),
        }
    }
}

/// 手本の Linux で動かす。nobody で (aios の中は ai なので、許可のしらべが同じになるように)。
/// QEMU の Linux なら、そこの ext4 (home) か tmpfs (tmp) で、aios と同じ aarch64 の sysdiff を。
/// このマシンなら root のとき setpriv で、home は build/ の下 (ext4 などそのマシンのもの)、tmp は /dev/shm (tmpfs)
fn linux(root: &Path, base: &Base, bins: &(PathBuf, PathBuf), ops: u64, tmp: bool, cases: &Path) -> Result<String, String> {
    if let Base::Vm(image, _) = base {
        let out = root.join("build/aish-vm/sysdiff/linux.txt");
        let (b, c, n, o) = (bins.1.display().to_string(), cases.display().to_string(), ops.to_string(), out.display().to_string());
        let script = root.join("test/linux-vm.sh").display().to_string();
        let (st, _, err, _, to) = sh(root, &script, &["run", image, &b, &c, &n, if tmp { "tmp" } else { "home" }, &o], &[], Duration::from_secs(1900));
        if st != 0 || to {
            return Err(format!("linux (QEMU): sysdiff failed (status {}{}): {}", st, if to { ", timed out" } else { "" }, err.trim()));
        }
        return std::fs::read_to_string(&out).map_err(|e| e.to_string());
    }
    let bin = &bins.0;
    let work = if tmp { PathBuf::from("/dev/shm/aish-sysdiff") } else { root.join("build/aish-vm/sysdiff/work") };
    std::fs::create_dir_all(&work).map_err(|e| format!("{}: {}", work.display(), e))?;
    let rootuser = sh(root, "id", &["-u"], &[], Duration::from_secs(10)).1.trim() == "0";
    if rootuser {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o777));
        let _ = std::fs::set_permissions(cases, std::fs::Permissions::from_mode(0o644));
    }
    let (b, w, n, c) = (bin.display().to_string(), work.display().to_string(), ops.to_string(), format!("@{}", cases.display()));
    let mut args: Vec<&str> = Vec::new();
    let prog = if rootuser {
        args.extend(["--reuid=65534", "--regid=65534", "--clear-groups", "--", b.as_str()]);
        "setpriv"
    } else {
        b.as_str()
    };
    args.extend(["-n", n.as_str(), w.as_str(), c.as_str()]);
    let (st, out, err, _, to) = sh(root, prog, &args, &[], Duration::from_secs(900));
    if st != 0 || to {
        return Err(format!("linux: sysdiff failed (status {}{}): {}", st, if to { ", timed out" } else { "" }, err.trim()));
    }
    Ok(out)
}

/// VM の aios で動かす (ai で。home ならホームの ext4、tmp なら /tmp)。case は www の cases.txt から取り、
/// 出力はファイルにして取り出す (シリアルに流すと遅い)
fn aios(root: &Path, ops: u64, tmp: bool) -> Result<String, String> {
    let work = if tmp { "/tmp/sd" } else { "~/sd" };
    let cmd = format!(
        "fetch http://10.0.2.2:8000/cases.txt -o /tmp/sd.cases >/dev/null && mkdir -p {w} && /tmp/sysdiff -n {n} {w} @/tmp/sd.cases > /tmp/sd.out 2>/tmp/sd.err; echo \"sysdiff status $?\"; tail -3 /tmp/sd.err",
        w = work,
        n = ops
    );
    let (_, out, _, _, to) = vmpy(root, &["run", &sub(&cmd), "-t", "900"], &[], Duration::from_secs(920));
    if to || !out.contains("sysdiff status 0") {
        let (_, log, _, _, _) = vmpy(root, &["log", "15"], &[], Duration::from_secs(30));
        return Err(format!("aios: sysdiff did not finish{}: {} / serial: {}", if to { " (timed out)" } else { "" }, out.trim(), log.trim()));
    }
    let local = root.join("build/aish-vm/sysdiff/aios.txt");
    let (st, gout, gerr, _, _) = vmpy(root, &["get", "/tmp/sd.out", &local.display().to_string()], &[], Duration::from_secs(600));
    if st != 0 {
        return Err(format!("aios: cannot get the output: {} {}", gout.trim(), gerr.trim()));
    }
    std::fs::read_to_string(&local).map_err(|e| e.to_string())
}

/// 出力を case ごとの行に。sys の行 (そのカーネル) は別に
fn split(text: &str) -> (String, Vec<(String, Vec<String>)>) {
    let mut sys = String::new();
    let mut v: Vec<(String, Vec<String>)> = Vec::new();
    for l in text.lines() {
        if let Some(x) = l.strip_prefix("sys ") {
            sys = x.to_string();
        } else if let Some(c) = l.strip_prefix("case ") {
            v.push((c.to_string(), Vec::new()));
        } else if let Some(last) = v.last_mut() {
            last.1.push(l.to_string());
        }
    }
    (sys, v)
}

/// 最初にちがう行 (何行め, Linux の行, aios の行)
fn first_diff(a: &[String], b: &[String]) -> Option<(usize, String, String)> {
    let none = || "(none)".to_string();
    (0..a.len().max(b.len())).find(|&i| a.get(i) != b.get(i)).map(|i| (i, a.get(i).cloned().unwrap_or_else(none), b.get(i).cloned().unwrap_or_else(none)))
}

/// op の行なら (番号, 名前, = のあと)
fn op_line(l: &str) -> Option<(usize, &str, &str)> {
    let (num, rest) = l.split_once(' ')?;
    let i = num.parse().ok()?;
    let name = rest.split('(').next()?;
    let ans = rest.rsplit_once(") = ").map_or("", |(_, x)| x);
    Some((i, name, ans))
}

/// ことばの形: 数字は # に (3 と 4 の fd のちがいより、成功と EISDIR のちがいでまとめる)
fn shape(t: &str) -> String {
    // 中身のハッシュ (h と 16 進の 8 けた) も、数と同じく 1 つの印に (でないと、ハッシュごとにちがう印になる)
    if t.len() == 9 && t.starts_with('h') && t[1..].bytes().all(|b| b.is_ascii_hexdigit()) {
        return "h#".into();
    }
    let mut out = String::new();
    for c in t.chars() {
        if c.is_ascii_digit() {
            if !out.ends_with('#') {
                out.push('#');
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// ちがいの印。op なら名前と、答えの最初にちがうことば (どちらも数なら、それがどこの数か)。
/// world や end の行なら、その最初にちがうことば
fn signature(l: &str, a: &str) -> String {
    let (head, x, y) = match (op_line(l), op_line(a)) {
        (Some((_, name, x)), Some((_, _, y))) => (name.to_string(), x, y),
        _ => (if l.starts_with("end") || a.starts_with("end") { "end" } else { "world" }.to_string(), l.trim(), a.trim()),
    };
    let (xs, ys): (Vec<&str>, Vec<&str>) = (x.split(' ').collect(), y.split(' ').collect());
    let k = (0..xs.len().max(ys.len())).find(|&k| xs.get(k) != ys.get(k)).unwrap_or(0);
    let (p, q) = (shape(xs.get(k).copied().unwrap_or("(none)")), shape(ys.get(k).copied().unwrap_or("(none)")));
    if p == q { format!("{}: {} differs", head, p) } else { format!("{}: {} vs {}", head, p, q) }
}

/// 同じ印のちがい: case ごとの (case, ちがいの出た行の番号, その op の番号) と、最初に見つけた case の 2 つの行
struct Group {
    sig: String,
    cases: Vec<(String, usize, Option<usize>)>,
    linux: String,
    aios: String,
}

/// 縮めているもの: 種、残す op、ちがいの出る op (world や end なら None)、印、いまの切れはしの大きさ
struct Shrink {
    seed: String,
    keep: Vec<usize>,
    at: Option<usize>,
    sig: String,
    chunk: usize,
    linux: Vec<String>,
    aios: Vec<String>,
}

fn case_of(seed: &str, keep: &[usize]) -> String {
    format!("{}:{}", seed, keep.iter().map(|k| k.to_string()).collect::<Vec<_>>().join(","))
}

/// この case の出力で、同じちがいが同じ op で出るか
fn same_diff(sh_: &Shrink, l: &[String], a: &[String]) -> bool {
    let Some((_, x, y)) = first_diff(l, a) else { return false };
    signature(&x, &y) == sh_.sig && sh_.at.is_none_or(|at| op_line(&x).is_some_and(|(i, _, _)| i == at))
}

pub fn job(root: &Path, a: &Value) -> Result<Value, String> {
    let seeds = a["seeds"].as_u64().unwrap_or(300).clamp(1, 20_000);
    let from = a["from"].as_u64().unwrap_or(1);
    let ops = a["ops"].as_u64().unwrap_or(30).clamp(1, 200);
    let nshrink = a["shrink"].as_u64().unwrap_or(12).min(40) as usize;
    let tmp = s(a, "fs") == "tmp";
    let dir = root.join("build/aish-vm/sysdiff");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    note(json!({ "step": "build" }));
    let bins = build(root, &dir)?;
    let chosen = marked_list(root, &bins.0, "chosen");
    let bugs = marked_list(root, &bins.0, "linuxbug");
    let base = Base::get(root, a["base"].as_str().filter(|x| !x.is_empty()).unwrap_or("latest"))?;
    let cases = dir.join("cases.txt");
    std::fs::write(&cases, format!("{}-{}\n", from, from + seeds - 1)).map_err(|e| e.to_string())?;
    // Linux の答え: ほかの Linux で作ったもの (linux) か、このマシンで
    let ltext = match a["linux"].as_str().filter(|x| !x.is_empty()) {
        Some(f) => std::fs::read_to_string(f).map_err(|e| format!("{}: {}", f, e))?,
        None => {
            note(json!({ "step": "linux", "seeds": seeds }));
            linux(root, &base, &bins, ops, tmp, &cases)?
        }
    };
    let spec = a["kernel"].as_str().filter(|x| !x.is_empty()).unwrap_or(".");
    note(json!({ "step": "kernel", "spec": spec }));
    let k = kernel_for(root, spec)?;
    note(json!({ "step": "boot" }));
    boot(root, Some(&k), true, &dir.display().to_string(), &[])?;
    let (st, out, _, _, _) = vmpy(root, &["run", "fetch http://10.0.2.2:8000/sysdiff -o /tmp/sysdiff >/dev/null && chmod +x /tmp/sysdiff && echo fetched", "-t", "60"], &[], Duration::from_secs(70));
    if st != 0 || !out.contains("fetched") {
        return Err(format!("cannot fetch sysdiff into the VM: {}", out.trim()));
    }
    note(json!({ "step": "aios", "seeds": seeds }));
    let atext = aios(root, ops, tmp)?;
    let (lsys, lcases) = split(&ltext);
    let (asys, acases) = split(&atext);
    let amap: std::collections::HashMap<&str, &Vec<String>> = acases.iter().map(|(c, v)| (c.as_str(), v)).collect();
    // まとめる
    let mut groups: Vec<Group> = Vec::new();
    let (mut same, mut missing) = (0, 0);
    for (c, l) in &lcases {
        let Some(av) = amap.get(c.as_str()) else {
            missing += 1;
            continue;
        };
        let Some((i, x, y)) = first_diff(l, av) else {
            same += 1;
            continue;
        };
        let sig = signature(&x, &y);
        let at = op_line(&x).map(|(k, _, _)| k);
        match groups.iter_mut().find(|g| g.sig == sig) {
            Some(g) => g.cases.push((c.clone(), i, at)),
            None => groups.push(Group { sig, cases: vec![(c.clone(), i, at)], linux: x, aios: y }),
        }
    }
    groups.sort_by_key(|g| std::cmp::Reverse(g.cases.len()));
    // 多いものから縮める。もとは、ちがいがいちばん早く出る case
    let mut work: Vec<Shrink> = groups
        .iter()
        .take(nshrink)
        .map(|g| {
            let (c, _, at) = g.cases.iter().min_by_key(|(c, i, _)| (*i, c.len(), c.clone())).cloned().unwrap();
            let keep: Vec<usize> = match at {
                Some(at) => (0..=at).collect(),
                None => (0..ops as usize).collect(),
            };
            let chunk = (keep.len() / 2).max(1);
            Shrink { seed: c, keep, at, sig: g.sig.clone(), chunk, linux: Vec::new(), aios: Vec::new() }
        })
        .collect();
    let mut rounds = 0;
    while rounds < 30 && work.iter().any(|w| w.chunk > 0) {
        rounds += 1;
        // 縮めているものごとに、切れはしを 1 つ抜いた並びをみな
        let mut tries: Vec<(usize, Vec<usize>)> = Vec::new();
        for (wi, w) in work.iter().enumerate() {
            if w.chunk == 0 {
                continue;
            }
            let rest: Vec<usize> = w.keep.iter().copied().filter(|&k| Some(k) != w.at).collect();
            for part in rest.chunks(w.chunk) {
                tries.push((wi, w.keep.iter().copied().filter(|k| !part.contains(k)).collect()));
            }
        }
        if tries.is_empty() {
            break;
        }
        note(json!({ "step": "shrink", "round": rounds, "tries": tries.len() }));
        let list: Vec<String> = tries.iter().map(|(wi, keep)| case_of(&work[*wi].seed, keep)).collect();
        std::fs::write(&cases, list.join("\n") + "\n").map_err(|e| e.to_string())?;
        let lt = linux(root, &base, &bins, ops, tmp, &cases)?;
        let at = aios(root, ops, tmp)?;
        let (_, lc) = split(&lt);
        let (_, ac) = split(&at);
        let am: std::collections::HashMap<&str, &Vec<String>> = ac.iter().map(|(c, v)| (c.as_str(), v)).collect();
        let lm: std::collections::HashMap<&str, &Vec<String>> = lc.iter().map(|(c, v)| (c.as_str(), v)).collect();
        let mut moved = vec![false; work.len()];
        for ((wi, keep), name) in tries.iter().zip(&list) {
            if moved[*wi] {
                continue;
            }
            let (Some(l), Some(a)) = (lm.get(name.as_str()), am.get(name.as_str())) else { continue };
            if same_diff(&work[*wi], l, a) {
                let w = &mut work[*wi];
                w.keep = keep.clone();
                w.chunk = w.chunk.min((w.keep.len() / 2).max(1));
                w.linux = l.to_vec();
                w.aios = a.to_vec();
                moved[*wi] = true;
            }
        }
        for (wi, w) in work.iter_mut().enumerate() {
            if !moved[wi] && w.chunk > 0 {
                w.chunk /= 2;
            }
        }
    }
    // 縮めたものをもう 1 度動かして、答えの並びを残す
    if !work.is_empty() {
        let list: Vec<String> = work.iter().map(|w| case_of(&w.seed, &w.keep)).collect();
        std::fs::write(&cases, list.join("\n") + "\n").map_err(|e| e.to_string())?;
        let lt = linux(root, &base, &bins, ops, tmp, &cases)?;
        let at = aios(root, ops, tmp)?;
        let (_, lc) = split(&lt);
        let (_, ac) = split(&at);
        for (w, name) in work.iter_mut().zip(&list) {
            w.linux = lc.iter().find(|(c, _)| c == name).map(|(_, v)| v.clone()).unwrap_or_default();
            w.aios = ac.iter().find(|(c, _)| c == name).map(|(_, v)| v.clone()).unwrap_or_default();
        }
    }
    vmpy(root, &["stop"], &[], Duration::from_secs(90));
    // 縮めた並びの op の行 (と、ちがいの行まで)
    let trace = |v: &[String], other: &[String]| -> Vec<String> {
        let end = first_diff(v, other).map_or(v.len(), |(i, _, _)| i + 1);
        v[..end.min(v.len())].iter().filter(|x| op_line(x).is_some() || x.starts_with("  ") || x.starts_with("end")).cloned().collect()
    };
    let out_groups: Vec<Value> = groups
        .iter()
        .map(|g| {
            let mut v = json!({
                "sig": g.sig,
                "cases": g.cases.len(),
                "seeds": g.cases.iter().take(5).map(|x| x.0.clone()).collect::<Vec<_>>(),
                "linux": [g.linux],
                "aios": [g.aios],
            });
            if let Some(w) = work.iter().find(|w| w.sig == g.sig) {
                v["small"] = json!(case_of(&w.seed, &w.keep));
                if !w.linux.is_empty() && !w.aios.is_empty() {
                    v["linux"] = json!(trace(&w.linux, &w.aios));
                    v["aios"] = json!(trace(&w.aios, &w.linux));
                }
            }
            v
        })
        .collect();
    let mut r = json!({
        "linux": lsys,
        "base": base.name(),
        "aios": asys,
        "fs": if tmp { "tmpfs" } else { "home (ext4)" },
        "ops": ops,
        "cases": lcases.len(),
        "same": same,
        "differ": lcases.len() - same - missing,
        "kinds": groups.len(),
        "groups": out_groups,
        "chosen": chosen.iter().map(|(n, w)| json!({ "name": n, "why": w, "cases": marked_cases("chosen", n, &lcases, &acases) })).collect::<Vec<_>>(),
        "linux_bugs": bugs.iter().map(|(n, w)| json!({ "name": n, "why": w, "cases": marked_cases("linuxbug", n, &lcases, &acases) })).collect::<Vec<_>>(),
        "note": "groups are sorted by how many cases show them. small is the shrunk case SEED:I,J,... (only those ops); its linux and aios traces end with the first difference. Run one again with test/sysdiff.c: sysdiff -n OPS DIR CASE on Linux and in the VM. chosen: differences aios chose over Linux (test/sysdiff.c, doc/aios.md); the Linux run imitates them, so they are not in differ; cases is how many cases went through each. linux_bugs: bugs of the Linux used as the answer key (test/sysdiff.c linux_bugs); the Linux run corrects them the same way",
    });
    if missing > 0 {
        r["missing"] = json!(missing);
    }
    if a["linux"].as_str().is_some_and(|x| !x.is_empty()) {
        r["linux_file"] = json!(s(a, "linux"));
        r["note"] = json!(format!("Linux answers come from linux_file; shrinking ran on {}", base.name()));
    }
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let file = dir.join(format!("{}.json", secs));
    if std::fs::write(&file, format!("{}\n", r)).is_ok() {
        r["saved"] = json!(file.display().to_string());
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        assert_eq!(signature("5 pwrite64(-1, 100, 0) = EBADF", "5 pwrite64(-1, 100, 0) = ESPIPE"), "pwrite64: EBADF vs ESPIPE");
        assert_eq!(signature("13 openat(cwd, \"b\", O_RDONLY|O_CREAT, 0444) = EISDIR", "13 openat(cwd, \"b\", O_RDONLY|O_CREAT, 0444) = 3"), "openat: EISDIR vs #");
        assert_eq!(signature("7 fstat(4) = 0 fifo 0600 n1 me", "7 fstat(4) = 0 fifo 0600 n1 other"), "fstat: me vs other");
        assert_eq!(signature("7 newfstatat(cwd, \"d/c\", 0) = 0 reg 0600 n1 s5001 me", "7 newfstatat(cwd, \"d/c\", 0) = 0 reg 0600 n1 s100001 me"), "newfstatat: s# differs");
        assert_eq!(signature("  \"r/a\" reg 0644 n1 s6 me h1", "  \"r/a\" reg 0644 n2 s6 me h1"), "world: n# differs");
        assert_eq!(signature("  \"r/a\" reg 0644 n1 s1 me hed0c3757", "  \"r/a\" reg 0644 n1 s1 me hab0bcf71"), "world: h# differs");
        assert_eq!(signature("17 read(4, 16384) = 100 h60ddf891", "17 read(4, 16384) = 100 hEACCES"), "read: h# vs hEACCES");
        assert_eq!(signature("end exit 0", "end signal 14"), "end: exit vs signal");
        assert_eq!(op_line("12 read(3, 0) = 0"), Some((12, "read", "0")));
    }

    #[test]
    fn diffs() {
        let v = |x: &[&str]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(first_diff(&v(&["a", "b"]), &v(&["a", "b"])), None);
        assert_eq!(first_diff(&v(&["a", "b"]), &v(&["a", "c"])), Some((1, "b".into(), "c".into())));
        assert_eq!(first_diff(&v(&["a"]), &v(&["a", "c"])), Some((1, "(none)".into(), "c".into())));
        let (sys, c) = split("sys Linux 6.18 x86_64\ncase 1\n0 close(-1) = EBADF\nend exit 0\ncase 2:0\nend exit 0\n");
        assert_eq!(sys, "Linux 6.18 x86_64");
        assert_eq!(c.len(), 2);
        assert_eq!(c[1].0, "2:0");
        assert_eq!(case_of("7", &[0, 3, 9]), "7:0,3,9");
    }

    #[test]
    fn chosen() {
        let c = |n: &str, l: &[&str]| (n.to_string(), l.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let l = vec![c("1", &["3 pwrite64(3, 7, 0) = 7 [chosen pwrite-append]"]), c("2", &["3 pwrite64(3, 7, 0) = 7"])];
        let a = vec![c("1", &["3 pwrite64(3, 7, 0) = 7 [chosen pwrite-append]"]), c("3", &["0 pwrite64(4, 1, 3) = 1 [chosen pwrite-append]"])];
        assert_eq!(marked_cases("chosen", "pwrite-append", &l, &a), 2);
        assert_eq!(marked_cases("chosen", "pwrite", &l, &a), 0);
        assert_eq!(marked_cases("linuxbug", "pwrite-append", &l, &a), 0);
    }
}
