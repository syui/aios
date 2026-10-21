// bench: 速さを測って、記録し、くらべる (aish-vm の bench)
//   kind fs   ファイルまわり (test/fsbench.c)。kind net  ループバックの TCP / UDP / AF_UNIX とパイプ (test/netbench.c)
//   どれも VM (-snapshot) で n 回動かして op ごとのまんなかをとり、prof をつけた回で /proc/ai/prof の part (kernel.json の
//   名前) ごとの CPU の時間の割合 (どこがボトルネックか) と、時間の多い区間を出す (net は op ごとに)。lock は lockbench
//   against  2 つの aios のカーネルを op ごとにくらべる
//   linux    同じプログラムを Linux (test/linux-vm.sh の latest) の VM でも動かし、op ごとに aios の時間が Linux の何倍か。
//            VM はどちらも QEMU の cortex-a72 (TCG)、smp 個の CPU、2 GiB
//   record   結果を bench/KIND.jsonl (1 行 1 回の記録。リポジトリに入れる) に足し、bench/KIND.md (表) を作りなおす。
//            いつも、前の記録 (同じ VM のもの) から op ごとにどれだけ変わったかも出す
//   show     測らずに、記録のいちばん新しいものと、その前からの変わりかた
use super::{boot_env, compiler, git, kernel_for, lockbench, note, sh, spread, strings, sub, switchable, vmpy};
use aish_plugin::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// net の op と、表に書く中身
const NET_OPS: &[(&str, &str)] = &[
    ("pipe_rr", "パイプで 1 バイトの往復 (2 つのプロセス。くらべるための土台: プロセスの切りかえと起こし)"),
    ("futex_rr", "2 つのスレッドが futex で交互に起こしあう往復 (スレッドの切りかえと futex の起こしだけ)"),
    ("unix_rr", "AF_UNIX の stream で 1 バイトの往復 (2 つのプロセス)"),
    ("tcp_rr", "TCP (127.0.0.1、TCP_NODELAY) で 1 バイトの往復 (2 つのプロセス)"),
    ("udp_rr", "UDP (つないだもの) で 1 バイトの往復 (2 つのプロセス)"),
    ("tcp_rr_epoll", "TCP の往復で、読む前に epoll_wait で待つ (サーバーのよくある形)"),
    ("unix_local", "AF_UNIX の往復を 1 つのプロセスで (切りかえなし: ソケットの道だけ)"),
    ("tcp_local", "TCP の往復を 1 つのプロセスで"),
    ("udp_local", "UDP の往復を 1 つのプロセスで"),
    ("unix_stream", "AF_UNIX に 64 KiB ずつ書いて、別のプロセスが読む"),
    ("tcp_stream", "TCP に 64 KiB ずつ書いて、別のプロセスが読む"),
    ("tcp_stream_4k", "TCP に 4 KiB ずつ書いて、別のプロセスが読む"),
    ("tcp_connect", "connect、accept、両方を閉じる (1 つのつながり)"),
];

/// fs の op と、表に書く中身 (n は 1000、ディレクトリは ext4 の上)
const FS_OPS: &[(&str, &str)] = &[
    ("create", "空のファイルを O_CREAT|O_EXCL で作って閉じる"),
    ("stat", "作ったばかりのファイルを名前で stat"),
    ("stat-missing", "ないファイルを stat (ENOENT)"),
    ("stat-missing-again", "同じないものをもう一度 (PATH や #include を探すときのように)"),
    ("open-close", "あるファイルを開いて閉じる"),
    ("write-4k", "小さなファイルに 4 KiB 書く"),
    ("read-4k", "4 KiB 読む (ページキャッシュにある)"),
    ("readdir", "1000 こ入ったディレクトリを読む"),
    ("deep-stat", "10 段の深いパスの先を stat"),
    ("stat-at", "開いているディレクトリから名前で stat (fstatat の dirfd。find や rm -r、ls -l)"),
    ("rename", "名前を変える"),
    ("unlink", "消す"),
    ("mkdir-rmdir", "ディレクトリを作って消す"),
    ("write-seq", "16 MiB を 64 KiB ずつ書いて fsync"),
    ("read-seq", "16 MiB を 64 KiB ずつ読む (ページキャッシュにある)"),
    ("write-fsync", "4 KiB 書いて fsync (ジャーナルの commit) を 50 回"),
];

/// op の中身 (表に書く)
fn what(op: &str) -> &'static str {
    NET_OPS.iter().chain(FS_OPS).find(|o| o.0 == op).map_or("", |o| o.1)
}

const BYTES: &str = "us: microseconds per operation (smaller is faster); mb_s: megabytes per second (bigger is faster)";

pub fn job(root: &Path, a: &Value) -> Result<Value, String> {
    let kind = a["kind"].as_str().filter(|k| !k.is_empty()).unwrap_or("fs");
    if kind != "fs" && kind != "net" {
        return Err(format!("kind: {} (fs or net)", kind));
    }
    if a["show"].as_bool() == Some(true) {
        return show(root, kind);
    }
    let n = a["n"].as_u64().unwrap_or(3).clamp(1, 20) as usize;
    let scale = a["scale"].as_u64().unwrap_or(1).clamp(1, 100);
    let smp = a["smp"].as_u64().unwrap_or(1).clamp(1, 8);
    let only = strings(&a["ops"]);
    let off = strings(&a["off"]);
    if !off.is_empty() {
        let names = switchable(root)?;
        if let Some(x) = off.iter().find(|x| !names.contains(x)) {
            return Err(format!("off: {} cannot be turned off (kernel.json: {})", x, names.join(" ")));
        }
    }
    let accel = std::env::var("AIOS_ACCEL").ok().filter(|x| !x.is_empty()).unwrap_or_else(|| "tcg".into());
    let want_linux = a["linux"].as_bool() == Some(true);
    if want_linux && accel != "tcg" {
        return Err(format!("linux: the Linux VM runs on TCG only, but AIOS_ACCEL is {} (the times would not compare)", accel));
    }
    let prog = if kind == "fs" { "fsbench" } else { "netbench" };
    note(json!({ "step": prog }));
    let bin = build_prog(root, prog)?;
    let cfg = Cfg { kind: kind.to_string(), bin: bin.clone(), n, scale, smp, only: only.clone(), off: off.clone() };
    let spec = a["kernel"].as_str().filter(|x| !x.is_empty()).unwrap_or(".");
    let against = a["against"].as_str().filter(|x| !x.is_empty());
    let base = against.map(|k| measure_aios(root, &cfg, k)).transpose()?;
    let mine = measure_aios(root, &cfg, spec)?;
    vmpy(root, &["stop"], &[], Duration::from_secs(90));
    let linux = if want_linux { Some(measure_linux(root, &cfg)?) } else { None };
    let vm = json!({ "cpu": "cortex-a72", "smp": smp, "mem": "2G", "accel": accel });
    let mut out = json!({ "bench": kind, "n": n, "scale": scale, "vm": vm, "result": mine.clone(), "note": format!("{}. Each kernel ran on a fresh -snapshot VM; the first run is thrown away; parts come from runs with prof on (CPU time only: time asleep is wait_us); lock: nanoseconds per lockbench step (median of 5)", BYTES) });
    if !off.is_empty() {
        out["off"] = json!(off);
    }
    if !only.is_empty() {
        out["ops"] = json!(only);
    }
    if let Some(b) = &base {
        out["against"] = b.clone();
        out["compare"] = json!(compare(&mine["ops"], &b["ops"], against.unwrap_or("")).0);
        out["compare"].as_array_mut().unwrap().extend(compare_lock(&mine, b, against.unwrap_or("")).into_iter().map(Value::from));
    }
    if let Some(l) = &linux {
        let (lines, ratio) = compare(&mine["ops"], &l["ops"], &format!("Linux {}", s(l, "version")));
        out["linux"] = l.clone();
        out["vs_linux"] = json!(lines);
        out["ratio_linux"] = ratio;
    }
    // 記録: 前の (同じ VM、同じ量の) ものからの変わりかた。record なら足す
    let rec = record_of(root, &cfg, spec, &mine, linux.as_ref(), &vm);
    let file = root.join("bench").join(format!("{}.jsonl", kind));
    let history = read_records(&file);
    if let Some(last) = history.iter().rev().find(|r| same_setup(r, &rec)) {
        let (lines, _) = compare(&mine["ops"], &last["aios"], &format!("the last record ({} {})", s(last, "date"), s(last, "commit")));
        out["since_last"] = json!(lines);
    }
    if a["record"].as_bool() == Some(true) {
        if !only.is_empty() || !off.is_empty() || against.is_some() {
            return Err("record: measure the whole set (no ops, off or against) so the records compare".into());
        }
        let _ = std::fs::create_dir_all(root.join("bench"));
        let mut all = history.clone();
        all.push(rec.clone());
        let mut text = String::new();
        for r in &all {
            text.push_str(&format!("{}\n", r));
        }
        std::fs::write(&file, text).map_err(|e| format!("{}: {}", file.display(), e))?;
        let md = root.join("bench").join(format!("{}.md", kind));
        std::fs::write(&md, render_md(kind, &all)).map_err(|e| format!("{}: {}", md.display(), e))?;
        out["recorded"] = json!([file.display().to_string(), md.display().to_string()]);
    }
    // 残す: build/aish-vm/bench/UNIX秒.json
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let saved = root.join("build/aish-vm/bench").join(format!("{}.json", secs));
    if std::fs::write(&saved, format!("{}\n", out)).is_ok() {
        out["saved"] = json!(saved.display().to_string());
    }
    Ok(out)
}

struct Cfg {
    kind: String,
    bin: PathBuf,
    n: usize,
    scale: u64,
    smp: u64,
    only: Vec<String>,
    off: Vec<String>,
}

impl Cfg {
    /// netbench の引数 (runs 回くりかえす。-o は ops があれば)
    fn net_args(&self, runs: usize, ops: Option<&str>) -> Vec<String> {
        let mut v = vec!["-r".to_string(), runs.to_string()];
        let o = ops.map(str::to_string).or_else(|| (!self.only.is_empty()).then(|| self.only.join(",")));
        if let Some(o) = o {
            v.extend(["-o".to_string(), o]);
        }
        v.push(self.scale.to_string());
        v
    }

    /// prof をつけて動かす op (net は 1 つずつ。fs は全部を 1 回)
    fn prof_ops(&self) -> Vec<Option<String>> {
        if self.kind == "fs" {
            return vec![None];
        }
        NET_OPS.iter().map(|o| o.0).filter(|o| self.only.is_empty() || self.only.iter().any(|x| x == o)).map(|o| Some(o.to_string())).collect()
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

/// test/NAME.c を中で動かす静的なプログラムに (build/aish-vm/bench/NAME。ソースが新しいときだけ作りなおす)。
/// aios と Linux の VM で同じものを動かす (同じ libc)
fn build_prog(root: &Path, name: &str) -> Result<PathBuf, String> {
    let dir = root.join("build/aish-vm/bench");
    let _ = std::fs::create_dir_all(&dir);
    let (src, out) = (root.join(format!("test/{}.c", name)), dir.join(name));
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    if let (Some(o), Some(s)) = (mtime(&out), mtime(&src))
        && o >= s
    {
        return Ok(out);
    }
    let Some((cc, head)) = compiler(root, false) else { return Err("no compiler: zig or aarch64-linux-gnu-gcc".into()) };
    let mut args = head;
    args.extend(["-O2".into(), "-static".into(), "-s".into(), "-o".into(), out.display().to_string(), src.display().to_string()]);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let (st, _, err, _, _) = sh(root, &cc, &argv, &[], Duration::from_secs(120));
    if st != 0 {
        return Err(format!("{}: {}", name, err.trim()));
    }
    Ok(out)
}

/// 出力の 1 行 1 つの JSON から op ごとの値 (µs か MB/s)。skip0 なら run 0 (起動したばかりのキャッシュ) を捨てる
fn collect(out: &str, skip0: bool, vals: &mut Vec<(String, &'static str, Vec<f64>)>, errors: &mut Vec<String>) {
    for v in out.lines().filter_map(aish_plugin::parse) {
        let Some(op) = v["op"].as_str() else { continue };
        if let Some(e) = v["error"].as_str() {
            errors.push(format!("{}: {}", op, e));
            continue;
        }
        if skip0 && v["run"].as_u64() == Some(0) {
            continue;
        }
        let (unit, x) = match (v["us"].as_f64(), v["mb_s"].as_f64()) {
            (Some(us), _) => ("us", us),
            (None, Some(mb)) => ("mb_s", mb),
            _ => continue,
        };
        match vals.iter_mut().find(|e| e.0 == op) {
            Some(e) => e.2.push(x),
            None => vals.push((op.to_string(), unit, vec![x])),
        }
    }
}

/// op ごとのまんなか、いちばん小さい、大きい
fn summarize(vals: &[(String, &'static str, Vec<f64>)]) -> Value {
    let mut ops = json!({});
    for (op, unit, v) in vals {
        let (med, lo, hi) = spread(v);
        ops[op.as_str()] = json!({ unit.to_string(): (med * 100.0).round() / 100.0, "min": lo, "max": hi });
    }
    ops
}

/// aios のカーネル spec を VM で測る
fn measure_aios(root: &Path, cfg: &Cfg, spec: &str) -> Result<Value, String> {
    let t = Duration::from_secs(900);
    let www = cfg.bin.parent().unwrap_or(root).display().to_string();
    let prog = cfg.bin.file_name().and_then(|f| f.to_str()).unwrap_or("bench").to_string();
    note(json!({ "step": "kernel", "spec": spec }));
    let k = kernel_for(root, spec)?;
    note(json!({ "step": "boot", "spec": spec, "smp": cfg.smp }));
    boot_env(root, Some(&k), true, &www, &cfg.off, &[("AIOS_SMP", cfg.smp.to_string())])?;
    let fetch = format!("fetch http://10.0.2.2:8000/{0} -o /tmp/{0} >/dev/null && chmod +x /tmp/{0} && echo fetched", prog);
    let (st, out, _, _, _) = vmpy(root, &["run", &fetch, "-t", "60"], &[], Duration::from_secs(70));
    if st != 0 || !out.contains("fetched") {
        return Err(format!("{}: cannot fetch {} into the VM: {}", spec, prog, out.trim()));
    }
    let mut vals = Vec::new();
    let mut errors = Vec::new();
    let mut total = 0.0;
    if cfg.kind == "net" {
        // 1 回で n + 1 回くりかえし、はじめの回を捨てる
        let cmd = format!("cd /tmp && /tmp/netbench {}", cfg.net_args(cfg.n + 1, None).join(" "));
        let (st, out, _, ms, to) = vmpy(root, &["run", &sub(&cmd), "-t", &t.as_secs().to_string()], &[], t + Duration::from_secs(10));
        if to || out.trim().is_empty() {
            return Err(format!("{}: netbench did not finish (status {}): {}", spec, st, out.lines().rev().take(3).collect::<Vec<_>>().join(" / ")));
        }
        collect(&out, true, &mut vals, &mut errors);
        total = ms as f64;
        note(json!({ "step": "run", "spec": spec, "ms": ms }));
    } else {
        // ext4 の上 (ホーム) で動かす。/tmp は tmpfs のこともある。1 回目は捨てる (起動したばかりのキャッシュ)
        let run_cmd = format!("cd ~ && /tmp/fsbench . {}", cfg.scale);
        vmpy(root, &["run", &sub(&format!("{} > /dev/null", run_cmd)), "-t", &t.as_secs().to_string()], &[], t + Duration::from_secs(10));
        let mut totals = Vec::new();
        for i in 0..cfg.n {
            let (st, out, _, ms, to) = vmpy(root, &["run", &sub(&run_cmd), "-t", &t.as_secs().to_string()], &[], t + Duration::from_secs(10));
            if st != 0 || to {
                return Err(format!("{}: fsbench failed (status {}): {}", spec, st, out.lines().rev().take(3).collect::<Vec<_>>().join(" / ")));
            }
            collect(&out, false, &mut vals, &mut errors);
            for v in out.lines().filter_map(aish_plugin::parse) {
                if let Some(t) = v["total_ms"].as_f64() {
                    totals.push(t);
                }
            }
            note(json!({ "step": "run", "spec": spec, "i": i, "ms": ms }));
        }
        total = spread(&totals).0.max(total);
    }
    // ロックなどの 1 回の時間 (lockbench を 5 回。kmsg の答えに LB をつけて、シリアルに出たカーネルの行と分ける)
    let lb = "for i in 1 2 3 4 5; do echo lockbench > /proc/ai/ctl; done 2>/dev/null; grep 'lockbench:' /proc/kmsg | tail -5 | sed 's/^/LB /'";
    let (_, lout, _, _, _) = vmpy(root, &["run", &sub(lb), "-t", "60"], &[], Duration::from_secs(70));
    let lock = lockbench(&lout);
    // prof をつけて (数えるぶん少し遅くなるので、時間は上の回のものを使う)。net は op ごとに
    let mut parts = json!({});
    let mut prof_error = None;
    for op in cfg.prof_ops() {
        let body = match (&op, cfg.kind.as_str()) {
            (Some(o), _) => format!("cd /tmp && /tmp/netbench {}", cfg.net_args(1, Some(o)).join(" ")),
            (None, _) => format!("cd ~ && /tmp/fsbench . {}", cfg.scale),
        };
        let cmd = format!("echo prof on > /proc/ai/ctl && ({} > /dev/null); echo prof off > /proc/ai/ctl; cat /proc/ai/prof", body);
        let (_, pout, _, _, _) = vmpy(root, &["run", &sub(&cmd), "-t", &t.as_secs().to_string()], &[], t + Duration::from_secs(10));
        let prof: Vec<Value> = pout.lines().filter_map(aish_plugin::parse).collect();
        let p: Vec<Value> = prof.iter().filter(|v| v.get("part").is_some()).take(if op.is_some() { 4 } else { 6 }).cloned().collect();
        let mut spans: Vec<Value> = prof.iter().filter(|v| v.get("name").is_some()).cloned().collect();
        spans.sort_by(|a, b| b["self_us"].as_f64().unwrap_or(0.0).partial_cmp(&a["self_us"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));
        spans.truncate(if op.is_some() { 5 } else { 20 });
        if p.is_empty() {
            // 数えられなかった (/proc/ai/ctl に書けない、aiproc を切った、など)
            prof_error = Some(pout.lines().filter(|l| !l.trim().is_empty()).take(3).collect::<Vec<_>>().join(" / "));
            break;
        }
        parts[op.as_deref().unwrap_or("all")] = json!({ "parts": p, "spans": spans });
    }
    let mut r = json!({ "kernel": spec, "path": k.display().to_string(), "total_ms": total, "ops": summarize(&vals), "prof": parts });
    if lock.as_object().is_some_and(|o| !o.is_empty()) {
        r["lock"] = lock;
    }
    if let Some(e) = prof_error {
        r["prof_error"] = json!(e);
    }
    if !errors.is_empty() {
        r["errors"] = json!(errors);
    }
    Ok(r)
}

/// 同じプログラムを Linux の VM で (test/linux-vm.sh exec latest。cortex-a72、smp 個の CPU、2 GiB)
fn measure_linux(root: &Path, cfg: &Cfg) -> Result<Value, String> {
    let script = root.join("test/linux-vm.sh").display().to_string();
    let (st, v, err, _, _) = sh(root, &script, &["version", "latest"], &[], Duration::from_secs(60));
    if st != 0 {
        return Err(format!("test/linux-vm.sh version latest: {}", err.trim()));
    }
    let version = v.trim().to_string();
    note(json!({ "step": "linux", "version": version }));
    let env = [("LINUX_CPU", "cortex-a72".to_string()), ("LINUX_SMP", cfg.smp.to_string())];
    let bin = cfg.bin.display().to_string();
    let run = |args: Vec<String>| -> Result<String, String> {
        let mut argv: Vec<&str> = vec!["exec", version.as_str(), bin.as_str()];
        argv.extend(args.iter().map(String::as_str));
        let (st, out, err, ms, to) = sh(root, &script, &argv, &env, Duration::from_secs(1800));
        if to || out.trim().is_empty() {
            return Err(format!("Linux {}: {} did not finish (status {}): {}", version, cfg.kind, st, err.lines().rev().take(3).collect::<Vec<_>>().join(" / ")));
        }
        note(json!({ "step": "linux run", "ms": ms }));
        Ok(out)
    };
    let mut vals = Vec::new();
    let mut errors = Vec::new();
    if cfg.kind == "net" {
        collect(&run(cfg.net_args(cfg.n + 1, None))?, true, &mut vals, &mut errors);
    } else {
        // 1 回ずつ起こす (はじめの 1 回は捨てる)。/work は ext4 (4 KiB のブロック)
        for i in 0..=cfg.n {
            let out = run(vec!["/work".into(), cfg.scale.to_string()])?;
            if i > 0 {
                collect(&out, false, &mut vals, &mut errors);
            }
        }
    }
    let mut r = json!({ "version": version, "ops": summarize(&vals) });
    if !errors.is_empty() {
        r["errors"] = json!(errors);
    }
    Ok(r)
}

/// op ごとに、mine の時間が other の何倍か (1 より小さければ速い)。範囲が重ならなければ faster / slower。
/// (文の一覧, op → 時間の比)
fn compare(mine: &Value, other: &Value, label: &str) -> (Vec<String>, Value) {
    let mut lines = Vec::new();
    let mut ratio = json!({});
    let Some(ops) = mine.as_object() else { return (lines, ratio) };
    for (op, m) in ops {
        let o = &other[op];
        let (unit, mv, ov) = match m["us"].as_f64() {
            Some(us) => ("us", us, o["us"].as_f64().unwrap_or(0.0)),
            None => ("mb_s", m["mb_s"].as_f64().unwrap_or(0.0), o["mb_s"].as_f64().unwrap_or(0.0)),
        };
        if mv <= 0.0 || ov <= 0.0 {
            continue;
        }
        let r = if unit == "us" { mv / ov } else { ov / mv };
        let (mlo, mhi, olo, ohi) = (m["min"].as_f64().unwrap_or(mv), m["max"].as_f64().unwrap_or(mv), o["min"].as_f64().unwrap_or(ov), o["max"].as_f64().unwrap_or(ov));
        let verdict = if mhi < olo || ohi < mlo {
            if r < 1.0 { "faster" } else { "slower" }
        } else {
            "no clear difference"
        };
        lines.push(format!("{}: {:.2}x the time of {} ({})", op, r, label, verdict));
        ratio[op.as_str()] = json!((r * 1000.0).round() / 1000.0);
    }
    (lines, ratio)
}

/// lockbench (どちらのカーネルにもあるもの)
fn compare_lock(mine: &Value, other: &Value, label: &str) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(lk) = mine["lock"].as_object() {
        for (name, m) in lk {
            let o = &other["lock"][name];
            let (Some(mv), Some(ov)) = (m["ns"].as_f64(), o["ns"].as_f64()) else { continue };
            if mv > 0.0 && ov > 0.0 {
                let clear = m["max"].as_f64().unwrap_or(mv) < o["min"].as_f64().unwrap_or(ov) || o["max"].as_f64().unwrap_or(ov) < m["min"].as_f64().unwrap_or(mv);
                v.push(format!("lockbench {}: {:.2}x the time of {} ({})", name, mv / ov, label, if !clear { "no clear difference" } else if mv < ov { "faster" } else { "slower" }));
            }
        }
    }
    v
}

/// 記録の 1 行: いつ、どのコミット (作業中のものなら dirty)、どの VM とホスト、aios と Linux の op ごとの値、
/// aios の op ごとの part (CPU の時間の多いもの 3 つ)
fn record_of(root: &Path, cfg: &Cfg, spec: &str, mine: &Value, linux: Option<&Value>, vm: &Value) -> Value {
    let commit = git(root, &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
    let rev = git(root, &["rev-list", "--count", "HEAD"]).unwrap_or_default();
    let dirty = spec == "." && !git(root, &["status", "--porcelain", "--", "kernel", "test/netbench.c", "test/fsbench.c"]).unwrap_or_default().is_empty();
    let date = sh(root, "date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"], &[], Duration::from_secs(10)).1.trim().to_string();
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let model = cpuinfo.lines().find_map(|l| l.strip_prefix("model name").map(|r| r.trim_start_matches([' ', '\t', ':']).to_string())).unwrap_or_default();
    let cpus = cpuinfo.lines().filter(|l| l.starts_with("processor")).count();
    // part (CPU の時間の多いもの 3 つ) と、CPU の時間の多い区間 3 つの 1 回あたりの µs (どこを直すか)
    let mut parts = json!({});
    let mut hot = json!({});
    if let Some(p) = mine["prof"].as_object() {
        for (op, v) in p {
            // fs は全部を 1 回 (all) なので多めに
            let k = if op == "all" { 8 } else { 3 };
            let top: Vec<Value> = v["parts"].as_array().map(|a| a.iter().take(k).map(|x| json!({ "part": x["part"], "pct": x["pct"] })).collect()).unwrap_or_default();
            parts[op.as_str()] = json!(top);
            let spans: Vec<Value> = v["spans"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .take(k)
                        .map(|x| {
                            let c = x["count"].as_f64().unwrap_or(0.0).max(1.0);
                            json!({ "name": x["name"], "us": (x["self_us"].as_f64().unwrap_or(0.0) / c * 100.0).round() / 100.0, "count": x["count"] })
                        })
                        .collect()
                })
                .unwrap_or_default();
            hot[op.as_str()] = json!(spans);
        }
    }
    let mut r = json!({ "date": date, "commit": if spec == "." { commit } else { spec.to_string() }, "rev": rev, "dirty": dirty, "kind": cfg.kind, "vm": vm, "host": { "cpu": model, "cpus": cpus }, "n": cfg.n, "scale": cfg.scale, "aios": mine["ops"], "parts": parts, "hot": hot });
    if let Some(l) = linux {
        r["linux"] = json!({ "version": l["version"], "ops": l["ops"] });
        r["ratio_linux"] = compare(&mine["ops"], &l["ops"], "").1;
    }
    r
}

fn read_records(file: &Path) -> Vec<Value> {
    std::fs::read_to_string(file).unwrap_or_default().lines().filter_map(aish_plugin::parse).collect()
}

/// くらべてよい記録か (同じ種類、同じ VM、同じ量)
fn same_setup(a: &Value, b: &Value) -> bool {
    a["kind"] == b["kind"] && a["vm"] == b["vm"] && a["scale"] == b["scale"]
}

/// 値を表の文字に (µs か MB/s)
fn cell(v: &Value) -> String {
    match (v["us"].as_f64(), v["mb_s"].as_f64()) {
        (Some(us), _) if us >= 100.0 => format!("{:.0} µs", us),
        (Some(us), _) => format!("{:.1} µs", us),
        (None, Some(mb)) => format!("{:.0} MB/s", mb),
        _ => "-".into(),
    }
}

/// bench/KIND.md: いちばん新しい記録の表 (aios、Linux、比、前の記録から、part) と、これまでの比の並び
fn render_md(kind: &str, all: &[Value]) -> String {
    let title = if kind == "net" { "ループバックの速さ (TCP、UDP、AF_UNIX、パイプ)" } else { "ファイルまわりの速さ" };
    let mut md = format!("# {}\n\n", title);
    md.push_str(&format!(
        "aish の `bench` (kind {k}、linux、record) が作る。もとは bench/{k}.jsonl (1 行 1 回の記録)。測るのは test/{p}.c で、aios と\n\
         Linux の VM で同じプログラム (同じ libc) を動かす。時間の比は aios / Linux (1 より小さければ aios が速い)。part は aios の\n\
         /proc/ai/prof で CPU を使った時間の多いもの (眠って待った時間はのぞく)。VM は QEMU の TCG (x86 の上で aarch64 をまねる)\n\
         なので、本物の機械とは比がちがうことがある\n\n",
        k = kind,
        p = if kind == "net" { "netbench" } else { "fsbench" }
    ));
    let Some(last) = all.last() else { return md };
    let prev = all[..all.len() - 1].iter().rev().find(|r| same_setup(r, last));
    let lv = s(&last["linux"], "version");
    md.push_str(&format!(
        "いちばん新しい記録: {} の {}{} (r{})。VM: {} × {}、{}、{} (ホスト: {} × {})\n\n",
        s(last, "date"),
        s(last, "commit"),
        if last["dirty"] == true { " (作業中)" } else { "" },
        s(last, "rev"),
        s(&last["vm"], "cpu"),
        last["vm"]["smp"],
        s(&last["vm"], "mem"),
        s(&last["vm"], "accel"),
        s(&last["host"], "cpu"),
        last["host"]["cpus"]
    ));
    md.push_str(&format!("| op | aios | Linux {} | aios / Linux | 前の記録から | part (aios) | 中身 |\n|---|---|---|---|---|---|---|\n", if lv.is_empty() { "-" } else { lv }));
    let ops: Vec<String> = last["aios"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    let mut order: Vec<String> = NET_OPS.iter().chain(FS_OPS).map(|o| o.0.to_string()).filter(|o| ops.contains(o)).collect();
    order.extend(ops.iter().filter(|o| !order.contains(o)).cloned().collect::<Vec<_>>());
    for op in &order {
        let a = &last["aios"][op];
        let l = &last["linux"]["ops"][op];
        let ratio = last["ratio_linux"][op].as_f64().map_or("-".into(), |r| format!("{:.2}", r));
        let since = prev.map(|p| compare(&json!({ op.clone(): a.clone() }), &json!({ op.clone(): p["aios"][op].clone() }), "").1).and_then(|r| r[op.as_str()].as_f64()).map_or("-".into(), |r| format!("{:.2}", r));
        let parts = last["parts"][op].as_array().map(|v| v.iter().map(|x| format!("{} {}%", s(x, "part"), x["pct"].as_f64().unwrap_or(0.0).round())).collect::<Vec<_>>().join(", ")).unwrap_or_default();
        md.push_str(&format!("| {} | {} | {} | {} | {} | {} | {} |\n", op, cell(a), cell(l), ratio, since, parts, what(op)));
    }
    // どこで CPU を使っているか (aios の区間の 1 回あたり)。fs は全部を 1 回 (all)
    let spans_of = |h: &Value| -> String { h.as_array().map(|h| h.iter().map(|x| format!("`{}` {:.1} µs × {}", s(x, "name"), x["us"].as_f64().unwrap_or(0.0), x["count"])).collect::<Vec<_>>().join("、")).unwrap_or_default() };
    if last["hot"]["all"].is_array() {
        let parts = last["parts"]["all"].as_array().map(|v| v.iter().map(|x| format!("{} {}%", s(x, "part"), x["pct"].as_f64().unwrap_or(0.0).round())).collect::<Vec<_>>().join(", ")).unwrap_or_default();
        md.push_str(&format!("\n## part と CPU を使っている区間 (aios、全部の op を 1 回、1 回あたり)\n\n- part: {}\n- 区間: {}\n", parts, spans_of(&last["hot"]["all"])));
    } else if last["hot"].as_object().is_some_and(|h| !h.is_empty()) {
        md.push_str("\n## CPU を使っている区間 (aios、op ごとに多いもの 3 つ、1 回あたり)\n\n");
        for op in &order {
            if last["hot"][op].as_array().is_some_and(|h| !h.is_empty()) {
                md.push_str(&format!("- {}: {}\n", op, spans_of(&last["hot"][op])));
            }
        }
    }
    // 直すところの候補: Linux より 1 割より遅いもの、前の記録より 1 割より遅くなったもの
    let mut todo = Vec::new();
    for op in &order {
        if let Some(r) = last["ratio_linux"][op].as_f64().filter(|r| *r > 1.1) {
            todo.push(format!("- {}: Linux の {:.2} 倍の時間", op, r));
        }
        if let Some(p) = prev {
            let r = compare(&json!({ op.clone(): last["aios"][op].clone() }), &json!({ op.clone(): p["aios"][op].clone() }), "").1;
            if let Some(x) = r[op.as_str()].as_f64().filter(|x| *x > 1.1) {
                todo.push(format!("- {}: 前の記録 ({}) の {:.2} 倍の時間", op, s(p, "commit"), x));
            }
        }
    }
    if !todo.is_empty() {
        md.push_str(&format!("\n## 直すところの候補\n\n{}\n", todo.join("\n")));
    }
    // これまで (同じ VM のもの): aios / Linux の比と、aios の値
    let same: Vec<&Value> = all.iter().filter(|r| same_setup(r, last)).collect();
    if same.len() > 1 {
        md.push_str("\n## これまで (同じ VM の記録。aios の値、かっこは aios / Linux)\n\n| 記録 |");
        for op in &order {
            md.push_str(&format!(" {} |", op));
        }
        md.push_str("\n|---|");
        md.push_str(&"---|".repeat(order.len()));
        md.push('\n');
        for r in same.iter().rev().take(20) {
            md.push_str(&format!("| {} {}{} |", s(r, "date").get(..10).unwrap_or(""), s(r, "commit"), if r["dirty"] == true { "+" } else { "" }));
            for op in &order {
                let ratio = r["ratio_linux"][op].as_f64().map_or(String::new(), |x| format!(" ({:.2})", x));
                md.push_str(&format!(" {}{} |", cell(&r["aios"][op]), ratio));
            }
            md.push('\n');
        }
    }
    md
}

/// 測らずに、いちばん新しい記録と、その前 (同じ VM) からの変わりかた (bench/KIND.md も記録から作りなおす)
fn show(root: &Path, kind: &str) -> Result<Value, String> {
    let file = root.join("bench").join(format!("{}.jsonl", kind));
    let all = read_records(&file);
    let Some(last) = all.last() else { return Err(format!("{}: no records yet (bench with record: true)", file.display())) };
    let _ = std::fs::write(root.join("bench").join(format!("{}.md", kind)), render_md(kind, &all));
    let mut out = json!({ "records": all.len(), "file": file.display().to_string(), "last": last });
    if let Some(prev) = all[..all.len() - 1].iter().rev().find(|r| same_setup(r, last)) {
        out["since_previous"] = json!(compare(&last["aios"], &prev["aios"], &format!("{} {}", s(prev, "date"), s(prev, "commit"))).0);
    }
    if last.get("linux").is_some() {
        out["vs_linux"] = json!(compare(&last["aios"], &last["linux"]["ops"], &format!("Linux {}", s(&last["linux"], "version"))).0);
    }
    Ok(out)
}
