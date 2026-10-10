// aish-vm: 開発のマシン (Linux) で aios の VM を相手にする道具 (Claude は MCP のツール)
//   vm      VM を動かす: start (カーネルを選んで起こす) stop status run (シリアルの端末でコマンド) put shot
//           (画面を png に) keys (画面のキーボード: @alt-b など) log (シリアルの終わり、grep) mon (QEMU のモニタ)。
//           test/vm.py を呼ぶ
//   try     C (か C++) のコードを aarch64 の静的なプログラムにして (zig cc)、VM の中で動かして結果を返す
//   ab      2 つのカーネルで同じコマンドを n 回ずつ動かし、時間と大きなロックの使われ方をくらべる
//   bisect  test が通る (good) コミットと通らない (bad) コミットのあいだを git bisect で探す。
//           コミットごとにカーネルを作って VM で test を動かす (変わるのはカーネルだけ。ディスクはいまのもの)
// カーネルの選び方 (kernel、a、b): ファイルのパス / "." (作業中のソースから作る) / git のコミット (別の作業場所
//   build/aish-vm/wt で作る)。どれも AIOS_INITRD=none (ルートはディスク) で build/aish-vm/target に作り、
//   build/aish-vm/kernels/ にとっておく (同じコミットは作りなおさない)
// start、ab、bisect は長いので、うしろで動かす (自分を aish-vm start / ab / bisect で起こす)。もういちど呼ぶと様子か結果。
// ab と bisect は、動いている VM を止めてから、-snapshot (ディスクに書き残さない) で起こしなおす。2 つの VM が同じ
// disk.img を使うと壊れるので、同時には動かさない
// リポジトリは dir か、いまのディレクトリから上にたどって test/vm.py があるところ、なければ $AIOS_SRC
use aish_plugin::{Spec, Tool, Value, error, json, s};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const VM: &str = r#"{"type":"object","properties":{"op":{"type":"string","description":"start / stop / status / run / put / shot / keys / log / mon"},"kernel":{"type":"string","description":"start: カーネル (ファイルのパス / . で作業中のソースから作る / git のコミット)。なければ disk.img のもの"},"www":{"type":"string","description":"start: このディレクトリを http://10.0.2.2:8000/ に出す (中から fetch で取れる)"},"snapshot":{"type":"boolean","description":"start: ディスクに書き残さない (QEMU の -snapshot)"},"cmd":{"type":"string","description":"run: シリアルの端末で動かすコマンド。mon: QEMU のモニタのコマンド"},"timeout":{"type":"number","description":"run: 秒 (既定 50、50 まで)"},"local":{"type":"string","description":"put: 送るファイル"},"remote":{"type":"string","description":"put: 中の置き場 (既定 /tmp/ファイル名)"},"file":{"type":"string","description":"shot: png の置き場 (既定 build/aish-vm/shot.png)"},"keys":{"type":"array","items":{"type":"string"},"description":"keys: @ で始まるものは QEMU のキーの名前 (@alt-b @ctrl-l @ret @f5)、ほかは文字として打つ"},"lines":{"type":"integer","description":"log: 終わりから何行 (既定 40)"},"grep":{"type":"string","description":"log: この文字をふくむ行だけ"},"wait_ms":{"type":"integer","description":"start: 待つ長さ (既定 30000、50000 まで。起動しきっていなければ running。もういちど呼ぶと続きを待つ)"},"dir":{"type":"string"}},"required":["op"]}"#;
const TRY: &str = r#"{"type":"object","properties":{"code":{"type":"string","description":"C のソース (main のあるもの)"},"file":{"type":"string","description":"code のかわりにソースのファイル"},"lang":{"type":"string","description":"c (既定) か c++"},"cflags":{"type":"array","items":{"type":"string"},"description":"zig cc に足すもの (-lpthread、-DX=1 など)"},"args":{"type":"string","description":"中で動かすときの引数 (シェルの書き方で)"},"timeout":{"type":"number","description":"秒 (既定 30、50 まで)"},"dir":{"type":"string"}}}"#;
const AB: &str = r#"{"type":"object","properties":{"a":{"type":"string","description":"くらべる 1 つめのカーネル (パス / . / git のコミット。既定 HEAD)"},"b":{"type":"string","description":"2 つめ (既定 . = 作業中のソース)"},"cmd":{"type":"string","description":"中で動かして時間を測るコマンド"},"setup":{"type":"string","description":"起こしたあとに 1 度だけ動かすもの (測らない)"},"www":{"type":"string","description":"このディレクトリを http://10.0.2.2:8000/ に出す (setup で fetch して使う)"},"n":{"type":"integer","description":"カーネルごとに何回 (既定 5)"},"timeout":{"type":"number","description":"1 回の上限 (秒、既定 300)"},"wait_ms":{"type":"integer","description":"待つ長さ (既定 30000、50000 まで)"},"dir":{"type":"string"}},"required":["cmd"]}"#;
const BISECT: &str = r#"{"type":"object","properties":{"good":{"type":"string","description":"test が通るコミット"},"bad":{"type":"string","description":"test が通らないコミット (既定 HEAD)"},"test":{"type":"string","description":"中で動かすコマンド。終了コード 0 で good、125 で skip、ほかは bad (起動しない、時間切れも bad)"},"timeout":{"type":"number","description":"1 回の test の上限 (秒、既定 300)"},"wait_ms":{"type":"integer","description":"待つ長さ (既定 30000、50000 まで)"},"dir":{"type":"string"}},"required":["good","test"]}"#;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let (Some(cmd), Some(j)) = (args.first(), args.get(1)) {
        let a = aish_plugin::parse(j).unwrap_or(json!({}));
        let root = PathBuf::from(std::env::var("AIOS_SRC").unwrap_or_else(|_| ".".into()));
        let r = match cmd.as_str() {
            "start" => job_start(&root, &a),
            "ab" => job_ab(&root, &a),
            "bisect" => job_bisect(&root, &a),
            _ => Err(format!("{}: unknown", cmd)),
        };
        match r {
            Ok(v) => println!("{}", v),
            Err(e) => {
                println!("{}", json!({ "error": e }));
                std::process::exit(1);
            }
        }
        return;
    }
    let spec = Spec {
        name: "vm",
        hooks: &[],
        keys: &[],
        tools: &[
            Tool { name: "vm", desc: "開発のマシンで aios の VM を動かす (test/vm.py): start (kernel: パス / . / git のコミット) stop status run (シリアルの端末でコマンド) put (ファイルを中へ) shot (画面を png に。Read で見る) keys (画面のキーボード: @alt-b @ctrl-l @ret、ほかは文字) log (シリアルの終わり、grep) mon (QEMU のモニタ)", input: VM },
            Tool { name: "try", desc: "C (か C++) のコードを aarch64-linux-musl の静的なプログラムにして (zig cc -O2 -static -s)、動いている VM の中で動かし、出力と終了コードを返す。カーネルのふるまいをすぐ確かめるため (VM は vm の start で先に)", input: TRY },
            Tool { name: "ab", desc: "2 つのカーネル (a、b: パス / . / git のコミット) で同じ cmd を n 回ずつ動かし、時間 (ms のまんなか、いちばん短い、長い) と大きなロックを持っていた割合をくらべる。動いている VM を止めて -snapshot で起こしなおす。うしろで動くので、終わっていなければ running。もういちど呼ぶと続きを待つ", input: AB },
            Tool { name: "bisect", desc: "test が通る good と通らない bad のあいだのコミットを git bisect で探す (別の作業場所 build/aish-vm/wt で)。コミットごとにカーネルを作り、VM (-snapshot) で test を動かす: 0 で good、125 で skip、ほか (起動しない、時間切れも) bad。変わるのはカーネルだけ。うしろで動くので、もういちど呼ぶと様子か結果 (first_bad)", input: BISECT },
        ],
    };
    let mut jobs: HashMap<String, Job> = HashMap::new();
    aish_plugin::run(spec, |ev, v| match ev {
        "tool" => {
            let a = &v["args"];
            let Some(root) = root(a["dir"].as_str(), s(v, "pwd")) else { return error("no aios repository (test/vm.py) here or above; give dir or set AIOS_SRC") };
            match s(v, "name") {
                "vm" => vm(&mut jobs, &root, a),
                "try" => try_(&root, a),
                "ab" => job(&mut jobs, &root, "ab", a),
                "bisect" => job(&mut jobs, &root, "bisect", a),
                n => error(format!("{}: no such tool", n)),
            }
        }
        _ => json!({}),
    });
}

/// aios のリポジトリ (test/vm.py のあるところ)
fn root(dir: Option<&str>, pwd: &str) -> Option<PathBuf> {
    let has = |p: &Path| p.join("test/vm.py").is_file();
    if let Some(d) = dir.filter(|d| !d.is_empty()) {
        return has(Path::new(d)).then(|| PathBuf::from(d));
    }
    let mut p = PathBuf::from(if pwd.is_empty() { "." } else { pwd });
    loop {
        if has(&p) {
            return Some(p);
        }
        if !p.pop() {
            break;
        }
    }
    std::env::var("AIOS_SRC").ok().map(PathBuf::from).filter(|p| has(p))
}

/// コマンドを動かす (時間切れなら止める)。(終了コード, 出力, エラー出力, ms, 時間切れ)
fn sh(root: &Path, prog: &str, args: &[&str], env: &[(&str, String)], timeout: Duration) -> (i32, String, String, u64, bool) {
    let t0 = Instant::now();
    let mut c = Command::new(prog);
    c.args(args).current_dir(root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env {
        c.env(k, v);
    }
    let Ok(mut child) = c.spawn() else { return (-1, String::new(), format!("{}: cannot run", prog), 0, false) };
    // 出力はうしろで読む (パイプがいっぱいになって止まらないように)
    let (mut so, mut se) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let to = std::thread::spawn(move || {
        let mut b = String::new();
        let _ = std::io::Read::read_to_string(&mut so, &mut b);
        b
    });
    let te = std::thread::spawn(move || {
        let mut b = String::new();
        let _ = std::io::Read::read_to_string(&mut se, &mut b);
        b
    });
    let mut timed_out = false;
    let st = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code().unwrap_or(-1),
            Ok(None) if t0.elapsed() < timeout => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                timed_out = true;
                break child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
            }
        }
    };
    (st, to.join().unwrap_or_default(), te.join().unwrap_or_default(), t0.elapsed().as_millis() as u64, timed_out)
}

/// test/vm.py を動かす
fn vmpy(root: &Path, args: &[&str], env: &[(&str, String)], timeout: Duration) -> (i32, String, String, u64, bool) {
    let mut a = vec!["test/vm.py"];
    a.extend_from_slice(args);
    sh(root, "python3", &a, env, timeout)
}

fn ready(root: &Path) -> bool {
    let (_, out, _, _, _) = vmpy(root, &["status"], &[], Duration::from_secs(15));
    aish_plugin::parse(out.trim()).is_some_and(|v| v["ready"] == true)
}

fn secs(a: &Value, k: &str, default: f64, max: f64) -> f64 {
    a[k].as_f64().unwrap_or(default).clamp(1.0, max)
}

fn vm(jobs: &mut HashMap<String, Job>, root: &Path, a: &Value) -> Value {
    let op = s(a, "op");
    let short = Duration::from_secs(55);
    let res = |(st, out, err, ms, to): (i32, String, String, u64, bool)| {
        let mut r = json!({ "status": st, "out": out, "ms": ms });
        if !err.trim().is_empty() {
            r["err"] = json!(err.trim());
        }
        if to {
            r["timeout"] = json!(true);
        }
        r
    };
    match op {
        "start" => job(jobs, root, "start", a),
        "stop" | "status" => res(vmpy(root, &[op], &[], short)),
        "run" => {
            let t = secs(a, "timeout", 50.0, 50.0);
            res(vmpy(root, &["run", s(a, "cmd"), "-t", &format!("{}", t)], &[], Duration::from_secs_f64(t + 5.0)))
        }
        "put" => {
            let local = s(a, "local");
            let name = Path::new(local).file_name().and_then(|n| n.to_str()).unwrap_or("file");
            let remote = a["remote"].as_str().filter(|r| !r.is_empty()).map(String::from).unwrap_or(format!("/tmp/{}", name));
            let mut r = res(vmpy(root, &["put", local, &remote], &[], short));
            r["remote"] = json!(remote);
            r
        }
        "shot" => {
            let f = a["file"].as_str().filter(|f| !f.is_empty()).map(PathBuf::from).unwrap_or_else(|| root.join("build/aish-vm/shot.png"));
            let _ = std::fs::create_dir_all(f.parent().unwrap_or(Path::new(".")));
            let (st, out, err, ms, _) = vmpy(root, &["shot", &f.display().to_string()], &[], short);
            json!({ "status": st, "file": out.trim(), "err": err.trim(), "ms": ms })
        }
        "keys" => {
            let keys: Vec<String> = a["keys"].as_array().map(|k| k.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            let mut args = vec!["keys"];
            args.extend(keys.iter().map(String::as_str));
            res(vmpy(root, &args, &[], short))
        }
        "log" => {
            let n = a["lines"].as_u64().unwrap_or(40);
            let g = s(a, "grep");
            // grep するなら多めに読んでから絞る
            let (st, out, err, ms, to) = vmpy(root, &["log", &format!("{}", if g.is_empty() { n } else { 20000 })], &[], short);
            let text = if g.is_empty() { out } else { out.lines().filter(|l| l.contains(g)).collect::<Vec<_>>().iter().rev().take(n as usize).rev().cloned().collect::<Vec<_>>().join("\n") };
            res((st, text, err, ms, to))
        }
        "mon" => res(vmpy(root, &["mon", s(a, "cmd")], &[], short)),
        _ => error("op: start stop status run put shot keys log mon"),
    }
}

// ---- C を中で試す ----

/// zig (PATH か $ZIG か build/zig/*/zig)
fn zig(root: &Path) -> Option<String> {
    if let Ok(z) = std::env::var("ZIG") {
        return Some(z);
    }
    if sh(root, "zig", &["version"], &[], Duration::from_secs(10)).0 == 0 {
        return Some("zig".into());
    }
    let d = root.join("build/zig");
    std::fs::read_dir(&d).ok()?.filter_map(|e| e.ok()).map(|e| e.path().join("zig")).find(|p| p.is_file()).map(|p| p.display().to_string())
}

fn try_(root: &Path, a: &Value) -> Value {
    let Some(z) = zig(root) else { return error("no zig (PATH, $ZIG or build/zig/*/zig)") };
    let dir = root.join("build/aish-vm/try");
    let _ = std::fs::create_dir_all(&dir);
    let cxx = s(a, "lang") == "c++";
    let src = match a["file"].as_str().filter(|f| !f.is_empty()) {
        Some(f) => PathBuf::from(f),
        None => {
            let p = dir.join(if cxx { "try.cc" } else { "try.c" });
            if std::fs::write(&p, s(a, "code")).is_err() {
                return error("cannot write the source");
            }
            p
        }
    };
    let bin = dir.join("try");
    let (srcs, bins) = (src.display().to_string(), bin.display().to_string());
    let mut args: Vec<String> = vec![if cxx { "c++" } else { "cc" }.into(), "-target".into(), "aarch64-linux-musl".into(), "-O2".into(), "-static".into(), "-s".into(), "-o".into(), bins.clone(), srcs];
    if let Some(f) = a["cflags"].as_array() {
        args.extend(f.iter().filter_map(|x| x.as_str().map(String::from)));
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let (st, _, cerr, cms, _) = sh(root, &z, &argv, &[], Duration::from_secs(120));
    if st != 0 {
        return json!({ "error": "compile failed", "compile": cerr.trim(), "compile_ms": cms });
    }
    if !ready(root) {
        return error("the VM is not running (vm start first)");
    }
    let (pst, _, perr, _, _) = vmpy(root, &["put", &bins, "/tmp/.try"], &[], Duration::from_secs(55));
    if pst != 0 {
        return json!({ "error": "put failed", "err": perr.trim() });
    }
    let t = secs(a, "timeout", 30.0, 50.0);
    let cmd = format!("chmod +x /tmp/.try && /tmp/.try {}", s(a, "args"));
    let (st, out, err, ms, to) = vmpy(root, &["run", &cmd, "-t", &format!("{}", t)], &[], Duration::from_secs_f64(t + 5.0));
    let mut r = json!({ "status": st, "out": out, "ms": ms, "compile_ms": cms });
    if !err.trim().is_empty() {
        r["err"] = json!(err.trim());
    }
    if to {
        r["timeout"] = json!(true);
    }
    // 警告
    if !cerr.trim().is_empty() {
        r["compile"] = json!(cerr.trim());
    }
    r
}

// ---- うしろで動かすもの ----

/// うしろで動いているもの (自分を aish-vm start / ab / bisect で起こしたもの)。出力は build/aish-vm/KEY.log
struct Job {
    child: std::process::Child,
    log: PathBuf,
    t0: Instant,
}

/// key のものがなければ起こし、wait_ms まで待つ。終わったら最後の行の JSON (と途中の行)
fn job(jobs: &mut HashMap<String, Job>, root: &Path, key: &str, a: &Value) -> Value {
    if !jobs.contains_key(key) {
        let dir = root.join("build/aish-vm");
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join(format!("{}.log", key));
        let (Ok(out), Ok(exe)) = (std::fs::File::create(&log), std::env::current_exe()) else { return error(format!("{}: cannot start", log.display())) };
        let Ok(err) = out.try_clone() else { return error("cannot start") };
        match Command::new(exe).args([key, &a.to_string()]).current_dir(root).env("AIOS_SRC", root).stdin(Stdio::null()).stdout(out).stderr(err).spawn() {
            Ok(c) => jobs.insert(key.to_string(), Job { child: c, log, t0: Instant::now() }),
            Err(e) => return error(e),
        };
    }
    let wait = Duration::from_millis(a["wait_ms"].as_u64().unwrap_or(30_000).min(50_000));
    let end = Instant::now() + wait;
    let j = jobs.get_mut(key).unwrap();
    let status = loop {
        match j.child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(200)),
            _ => break None,
        }
    };
    let text = std::fs::read_to_string(&j.log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(12)..].join("\n");
    let ms = j.t0.elapsed().as_millis() as u64;
    let Some(st) = status else {
        return json!({ "running": true, "ms": ms, "log": tail, "hint": "call again to wait more" });
    };
    let log = j.log.display().to_string();
    jobs.remove(key);
    let mut r = lines.iter().rev().find_map(|l| aish_plugin::parse(l)).unwrap_or_else(|| json!({}));
    if !st.success() && r.get("error").is_none() {
        r["error"] = json!(format!("exit {}", st));
    }
    r["ms"] = json!(ms);
    r["log"] = json!(log);
    r
}

/// 途中の様子 (ログに 1 行。最後の行だけが答え)
fn note(v: Value) {
    println!("{}", v);
}

// ---- カーネルを選ぶ ----

/// dir (ワークスペース) のカーネルを作る。できたもののパス
fn build_kernel(root: &Path, dir: &Path) -> Result<PathBuf, String> {
    let target = root.join("build/aish-vm/target");
    let env = [("AIOS_INITRD", "none".to_string()), ("CARGO_TARGET_DIR", target.display().to_string())];
    let (st, _, err, _, _) = sh(dir, "cargo", &["build", "--release", "-p", "aios"], &env, Duration::from_secs(1800));
    if st != 0 {
        let tail: Vec<&str> = err.lines().filter(|l| l.starts_with("error")).take(5).collect();
        return Err(format!("kernel build failed: {}", tail.join(" / ")));
    }
    Ok(target.join("aarch64-unknown-none-softfloat/release/aios"))
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let (st, out, err, _, _) = sh(dir, "git", args, &[], Duration::from_secs(300));
    if st == 0 { Ok(out.trim().to_string()) } else { Err(format!("git {}: {}", args.join(" "), err.trim())) }
}

/// 別の作業場所 (build/aish-vm/wt。なければ作る)
fn worktree(root: &Path) -> Result<PathBuf, String> {
    let wt = root.join("build/aish-vm/wt");
    if !wt.join(".git").exists() {
        let _ = std::fs::create_dir_all(root.join("build/aish-vm"));
        git(root, &["worktree", "add", "--detach", &wt.display().to_string(), "HEAD"])?;
    }
    Ok(wt)
}

/// カーネルの選び方 (パス / . / git のコミット) から、とっておいたカーネルのパス
fn kernel_for(root: &Path, spec: &str) -> Result<PathBuf, String> {
    let kdir = root.join("build/aish-vm/kernels");
    let _ = std::fs::create_dir_all(&kdir);
    if spec.is_empty() {
        return Err("no kernel".into());
    }
    let p = if Path::new(spec).is_absolute() { PathBuf::from(spec) } else { root.join(spec) };
    if spec != "." && p.is_file() {
        return Ok(p);
    }
    if spec == "." {
        let k = build_kernel(root, root)?;
        let dst = kdir.join("work");
        std::fs::copy(&k, &dst).map_err(|e| e.to_string())?;
        return Ok(dst);
    }
    let sha = git(root, &["rev-parse", "--verify", &format!("{}^{{commit}}", spec)])?;
    let dst = kdir.join(&sha);
    if dst.is_file() {
        return Ok(dst);
    }
    let wt = worktree(root)?;
    git(&wt, &["checkout", "--detach", "-f", &sha])?;
    let k = build_kernel(root, &wt)?;
    std::fs::copy(&k, &dst).map_err(|e| e.to_string())?;
    Ok(dst)
}

/// VM を止めて、kernel で起こす (snapshot ならディスクに書き残さない)。起動に失敗したら Err
fn boot(root: &Path, kernel: Option<&Path>, snapshot: bool, www: &str) -> Result<Value, String> {
    vmpy(root, &["stop"], &[], Duration::from_secs(90));
    let mut env: Vec<(&str, String)> = Vec::new();
    if let Some(k) = kernel {
        env.push(("AIOS_KERNEL", k.display().to_string()));
    }
    if snapshot {
        env.push(("AIOS_QEMU_ARGS", "-snapshot".into()));
    }
    let mut args = vec!["start"];
    if !www.is_empty() {
        args.extend(["--www", www]);
    }
    let (st, out, err, ms, _) = vmpy(root, &args, &env, Duration::from_secs(1000));
    let r = aish_plugin::parse(out.trim()).unwrap_or(json!({}));
    if st != 0 || r["ready"] != true {
        return Err(format!("the VM did not come up ({} ms): {}", ms, err.lines().rev().take(3).collect::<Vec<_>>().join(" / ")));
    }
    Ok(r)
}

fn job_start(root: &Path, a: &Value) -> Result<Value, String> {
    let k = match a["kernel"].as_str().filter(|k| !k.is_empty()) {
        Some(spec) => {
            note(json!({ "step": "kernel", "spec": spec }));
            Some(kernel_for(root, spec)?)
        }
        None => None,
    };
    let www = a["www"].as_str().unwrap_or("");
    let r = boot(root, k.as_deref(), a["snapshot"] == true, www)?;
    Ok(json!({ "ready": true, "boot_s": r["boot_s"], "kernel": k.map(|p| p.display().to_string()) }))
}

// ---- くらべる ----

fn median(v: &[u64]) -> u64 {
    let mut s = v.to_vec();
    s.sort();
    s.get(s.len() / 2).copied().unwrap_or(0)
}

/// 大きなロックを持っていた割合 (/proc/ai/bkl、なければ /proc/bkl の all の行)
fn bkl_hold(root: &Path) -> Option<f64> {
    let (_, out, _, _, _) = vmpy(root, &["run", "sudo cat /proc/ai/bkl 2>/dev/null || grep '^all' /proc/bkl", "-t", "20"], &[], Duration::from_secs(30));
    if let Some(v) = out.lines().find_map(aish_plugin::parse) {
        return v["hold_pct"].as_f64();
    }
    // all  待った ms 待った%  持っていた ms 持っていた%
    let l = out.lines().find(|l| l.starts_with("all"))?;
    let pcts: Vec<f64> = l.split_whitespace().filter_map(|w| w.strip_suffix('%')?.parse().ok()).collect();
    pcts.get(1).copied()
}

fn job_ab(root: &Path, a: &Value) -> Result<Value, String> {
    let n = a["n"].as_u64().unwrap_or(5).clamp(1, 100) as usize;
    let cmd = s(a, "cmd");
    let t = Duration::from_secs_f64(secs(a, "timeout", 300.0, 3600.0));
    let specs = [a["a"].as_str().filter(|x| !x.is_empty()).unwrap_or("HEAD"), a["b"].as_str().filter(|x| !x.is_empty()).unwrap_or(".")];
    let mut res = Vec::new();
    for spec in specs {
        note(json!({ "step": "kernel", "spec": spec }));
        let k = kernel_for(root, spec)?;
        note(json!({ "step": "boot", "spec": spec }));
        boot(root, Some(&k), true, a["www"].as_str().unwrap_or(""))?;
        let setup = s(a, "setup");
        if !setup.is_empty() {
            vmpy(root, &["run", setup, "-t", &format!("{}", t.as_secs())], &[], t + Duration::from_secs(10));
        }
        vmpy(root, &["run", "sudo sh -c 'echo > /proc/bkl'", "-t", "20"], &[], Duration::from_secs(30));
        let (mut ms, mut fails) = (Vec::new(), 0);
        for i in 0..n {
            let (st, _, _, m, to) = vmpy(root, &["run", cmd, "-t", &format!("{}", t.as_secs())], &[], t + Duration::from_secs(10));
            if st != 0 || to {
                fails += 1;
            }
            ms.push(m);
            note(json!({ "step": "run", "spec": spec, "i": i, "ms": m, "status": st }));
        }
        let hold = bkl_hold(root);
        res.push(json!({ "kernel": spec, "path": k.display().to_string(), "ms": ms, "median_ms": median(&ms), "min_ms": ms.iter().min(), "max_ms": ms.iter().max(), "failures": fails, "bkl_hold_pct": hold }));
    }
    vmpy(root, &["stop"], &[], Duration::from_secs(90));
    let (m0, m1) = (res[0]["median_ms"].as_u64().unwrap_or(0) as f64, res[1]["median_ms"].as_u64().unwrap_or(0) as f64);
    let change = if m0 > 0.0 { (m1 - m0) / m0 * 100.0 } else { 0.0 };
    // はっきりした差か: いちばん短い〜長いの範囲が重ならない (QEMU の上の時間は 10% ほどぶれる)
    let range = |r: &Value| (r["min_ms"].as_u64().unwrap_or(0), r["max_ms"].as_u64().unwrap_or(0));
    let ((a0, a1), (b0, b1)) = (range(&res[0]), range(&res[1]));
    let clear = b1 < a0 || a1 < b0;
    let verdict = match (clear, m1 < m0) {
        (false, _) => "no clear difference (the ranges overlap; try a larger n or a longer cmd)",
        (true, true) => "b is faster",
        (true, false) => "b is slower",
    };
    Ok(json!({ "a": res[0], "b": res[1], "b_vs_a_pct": (change * 10.0).round() / 10.0, "clear": clear, "verdict": verdict, "note": "ms includes the serial round trip (about the same for both); the VM ran with -snapshot" }))
}

// ---- さがす ----

fn job_bisect(root: &Path, a: &Value) -> Result<Value, String> {
    let good = s(a, "good");
    let bad = a["bad"].as_str().filter(|x| !x.is_empty()).unwrap_or("HEAD");
    let test = s(a, "test");
    let t = Duration::from_secs_f64(secs(a, "timeout", 300.0, 3600.0));
    let (good, bad) = (git(root, &["rev-parse", "--verify", &format!("{}^{{commit}}", good)])?, git(root, &["rev-parse", "--verify", &format!("{}^{{commit}}", bad)])?);
    let wt = worktree(root)?;
    let _ = git(&wt, &["bisect", "reset"]);
    git(&wt, &["checkout", "--detach", "-f", &bad])?;
    let mut out = git(&wt, &["bisect", "start", &bad, &good])?;
    let mut steps = Vec::new();
    let result = loop {
        if let Some(first) = out.lines().find(|l| l.contains("is the first bad commit")) {
            let sha = first.split_whitespace().next().unwrap_or("").to_string();
            let subject = git(root, &["log", "-1", "--format=%s", &sha]).unwrap_or_default();
            break Ok(json!({ "first_bad": sha, "subject": subject }));
        }
        if steps.len() >= 40 {
            break Err("too many steps".to_string());
        }
        let sha = git(&wt, &["rev-parse", "HEAD"])?;
        let t0 = Instant::now();
        // カーネルを作る (作れなければ skip)、起こす (起きなければ bad)、test (0 good、125 skip、ほか bad)
        let verdict = match kernel_for(root, &sha) {
            Err(e) => ("skip", e),
            Ok(k) => match boot(root, Some(&k), true, "") {
                Err(e) => ("bad", e),
                Ok(_) => {
                    let (st, o, _, _, to) = vmpy(root, &["run", test, "-t", &format!("{}", t.as_secs())], &[], t + Duration::from_secs(10));
                    let last = o.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" / ");
                    match (st, to) {
                        (_, true) => ("bad", format!("timeout: {}", last)),
                        (0, _) => ("good", last),
                        (125, _) => ("skip", last),
                        _ => ("bad", format!("exit {}: {}", st, last)),
                    }
                }
            },
        };
        let subject = git(root, &["log", "-1", "--format=%s", &sha]).unwrap_or_default();
        let step = json!({ "sha": &sha[..12.min(sha.len())], "subject": subject, "result": verdict.0, "why": verdict.1, "ms": t0.elapsed().as_millis() as u64 });
        note(step.clone());
        steps.push(step);
        out = match git(&wt, &["bisect", verdict.0]) {
            Ok(o) => o,
            Err(e) => break Err(e),
        };
    };
    let _ = git(&wt, &["bisect", "reset"]);
    vmpy(root, &["stop"], &[], Duration::from_secs(90));
    let mut r = result?;
    r["steps"] = json!(steps);
    r["note"] = json!("only the kernel changes between steps; the disk (userland) is the current disk.img");
    Ok(r)
}
