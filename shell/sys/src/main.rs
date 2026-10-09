// aish-sys: aios の様子を見る (aios を作るための、aish の基本のプラグイン。doc/aios.md の Claude の入口)
//   get     状態の木 (aios get --json。host kernel mem disk proc service pkg net user boot log)
//   do      aiosd に頼んで変える (サービス、パッケージ、再起動、apply / rollback。aios do と同じ)
//   diff    /etc/aios.json といまのちがい (aios diff)
//   sys     まとめ: カーネル、起きてからの時間、CPU、メモリ、スワップ、ディスク、重いプロセス、BKL、カーネルのメッセージ
//   procs   プロセスの一覧 (CPU の時間かメモリの順)
//   kmsg    カーネルのメッセージ (/proc/kmsg。シリアルの画面にしか出なかった println! のもの)
//   log     サービスのログ (/var/log/UNIT.log。journalctl -u と同じもの)
//   bkl     コマンドを 1 つ動かして、そのあいだの大きなロック (/proc/bkl) を測る
//   strace  コマンドを 1 つ動かして、そのシステムコールを kmsg から取る (/proc/strace)
//   threads スレッドの一覧 (/proc/ai/threads): 眠っていれば何を (futex / 見張っている fd とその中身) どれだけ待っているか
//   hang    プロセスがなぜ止まっているか: 長く眠っているスレッドと待っているもの、読まれずに残っているデータ、
//           futex をみな起こしても同じところで眠りなおすか (起こしが消えたのか、だれも起こしていないのか)
//   stack   スレッドの呼び出しの並び (/proc/ai/stack と、写しているファイルの .eh_frame。unwind.rs)
//   watch   aiwatch (カーネルの見張り) がいま見つけているものと、最近の記録 (/run/aiwatch.json、/var/log/aiwatch.service.log)
//   tune    sysctl の値を順に変えてコマンドの時間をくらべる (aios tune。--apply でいちばん速い値を /etc/aios.json に)
//   M-s     まとめを画面に出す (キーを押すと消える)
// /proc/bkl、/proc/strace を 0 からにするのは root だけなので、root でなければ sudo -n tee で書く。
// 開発の Linux でも動く (aios にしかないものは「ない」と答える)
mod unwind;

use aish_plugin::{Spec, Tool, Tty, Value, error, json, s};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const GET: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"点でつなぐ: host kernel mem disk proc service pkg net user boot log の下 (kernel.cpus、service.sshd、pkg.installed.cargo、proc.123)。なければぜんぶ"}}}"#;
const DO: &str = r#"{"type":"object","properties":{"op":{"type":"string","description":"service / pkg / power / apply (/etc/aios.json のとおりにそろえる) / rollback (ひとつ前の apply に戻す) / ping"},"action":{"type":"string","description":"service: start stop restart enable disable。pkg: install remove upgrade refresh。power: reboot poweroff"},"name":{"type":"string","description":"service の名前 (sshd など)"},"names":{"type":"array","items":{"type":"string"},"description":"pkg install / remove のパッケージ"}},"required":["op"]}"#;
const DIFF: &str = r#"{"type":"object","properties":{}}"#;
const TUNE: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"sysctl の名前 (kernel.sched_timeslice_ms など。aios get kernel.sysctl で一覧)"},"values":{"type":"array","items":{"type":"string"},"description":"くらべる値"},"cmd":{"type":"string","description":"測る仕事 (sh -c)。値ごとに n 回動かす"},"n":{"type":"integer","description":"値ごとに動かす回数 (既定 3)"},"apply":{"type":"boolean","description":"いちばん速かった値を /etc/aios.json の sysctl に書いて aios apply する"}},"required":["name","values","cmd"]}"#;
const NONE: &str = r#"{"type":"object","properties":{}}"#;
const PROCS: &str = r#"{"type":"object","properties":{"sort":{"type":"string","description":"cpu (既定。使った CPU の時間) か mem (メモリ)"},"limit":{"type":"integer","description":"いくつまで (既定 20)"},"name":{"type":"string","description":"名前にこれをふくむものだけ"}}}"#;
const KMSG: &str = r#"{"type":"object","properties":{"grep":{"type":"string","description":"この文字をふくむ行だけ"},"lines":{"type":"integer","description":"終わりから何行 (既定 50)"}}}"#;
const LOG: &str = r#"{"type":"object","properties":{"unit":{"type":"string","description":"サービスの名前 (sshd、aiwm.service など)。なければログのある一覧"},"grep":{"type":"string"},"lines":{"type":"integer","description":"終わりから何行 (既定 50)"}}}"#;
const RUN: &str = r#"{"type":"object","properties":{"cmd":{"type":"string","description":"動かすコマンド (sh -c)。なければ今の値を読むだけ (bkl)"},"timeout_ms":{"type":"integer","description":"これを過ぎたら止める (既定 50000)"}}}"#;
const THREADS: &str = r#"{"type":"object","properties":{"pid":{"type":"integer","description":"このプロセスのスレッドだけ"},"name":{"type":"string","description":"名前 (スレッドかプロセスの) にこれをふくむものだけ"},"state":{"type":"string","description":"sleep / run / ready / zombie"},"min_slept_s":{"type":"number","description":"これだけ眠りつづけているものだけ"},"limit":{"type":"integer","description":"いくつまで (既定 50)"}}}"#;
const HANG: &str = r#"{"type":"object","properties":{"pid":{"type":"integer","description":"見るプロセス"},"name":{"type":"string","description":"pid のかわりに名前 (プロセスの代表スレッドの名前にふくむもの。いくつもあればみな)"},"kick":{"type":"boolean","description":"futex で眠っているスレッドをみな起こして、同じところで眠りなおすかを見る (既定 true。わけもなく起こしても壊れない)"},"stack":{"type":"boolean","description":"長く (1 秒より) 眠っているスレッドの呼び出しの並び (stack ツールと同じ。長い順に 4 つまで)"}}}"#;
const STACK: &str = r#"{"type":"object","properties":{"tid":{"type":"integer","description":"スレッドの番号"},"file":{"type":"string","description":"tid のかわりに、残しておいた /proc/ai/stack の中身 (aiwatch の stack_file)"},"pid":{"type":"integer","description":"tid のかわりに: そのプロセスのスレッドみな (眠っているものの、長く眠っている順)"},"max":{"type":"integer","description":"何段まで (既定 40)"},"limit":{"type":"integer","description":"pid のときいくつのスレッドまで (既定 8)"}}}"#;
const WATCH: &str = r#"{"type":"object","properties":{"lines":{"type":"integer","description":"記録の終わりから何行 (既定 20)"}}}"#;
const STRACE: &str = r#"{"type":"object","properties":{"cmd":{"type":"string","description":"動かすコマンド (sh -c)"},"name":{"type":"string","description":"見るプロセスの名前 (既定: cmd の最初の語。子のプロセスを見るときに)"},"all":{"type":"boolean","description":"うまくいったものも (既定 true。false ならしくじったものだけ)"},"limit":{"type":"integer","description":"何行まで (既定 300)"},"timeout_ms":{"type":"integer"}},"required":["cmd"]}"#;

fn main() {
    let spec = Spec {
        name: "sys",
        hooks: &["key"],
        keys: &[("M-s", "sys")],
        tools: &[
            Tool { name: "get", desc: "aios の状態の木 (aios get --json と同じ)。host kernel mem disk proc service pkg net user boot log。path で一部だけ", input: GET },
            Tool { name: "do", desc: "aiosd (root) に頼んで aios を変える: サービスの start/stop/restart/enable/disable、パッケージの install/remove/upgrade/refresh、reboot/poweroff、apply / rollback (/etc/aios.json)。root と wheel の人だけ。したことは /var/log/aiosd.log に残る", input: DO },
            Tool { name: "diff", desc: "/etc/aios.json (望む状態) といまのちがいと、そろえる手順 (aios diff。動かさない)。そろえるのは do の apply", input: DIFF },
            Tool { name: "tune", desc: "sysctl の値を順に変えて cmd を n 回ずつ動かし、かかった時間のまんなか (ms) でくらべる (aios tune。root で。終わったら元の値に戻す)。apply でいちばん速かった値を /etc/aios.json に書いて apply。50 秒をこえそうなら run の bg で sudo aios tune NAME=V1,V2 -- CMD", input: TUNE },
            Tool { name: "sys", desc: "aios のまとめ: カーネル、起きてからの時間、CPU、メモリ、スワップ、ディスク、CPU を使っているプロセス、BKL、カーネルの新しいメッセージ", input: NONE },
            Tool { name: "procs", desc: "プロセスの一覧 (pid ppid 状態 スレッド CPU 秒 メモリ 名前)。sort: cpu / mem", input: PROCS },
            Tool { name: "kmsg", desc: "カーネルのメッセージ (dmesg。[起動からの秒] つき)。grep で絞れる", input: KMSG },
            Tool { name: "log", desc: "サービスのログ (/var/log/UNIT.log)。unit がなければログのある一覧", input: LOG },
            Tool { name: "bkl", desc: "cmd を動かして、そのあいだの大きなロック (BKL) の統計 (CPU ごとの待ち・持ち、長く持ったシステムコール)。cmd がなければ今の値", input: RUN },
            Tool { name: "threads", desc: "スレッドの一覧 (/proc/ai/threads): tid pid 名前 状態 最後のシステムコール 走った回数。眠っていれば slept_s (眠りつづけている秒) と wait: futex (アドレス、待つ値、いまの値) / poll (見張っている fd と、それが何か・読めるか・残っているバイト) / chan", input: THREADS },
            Tool { name: "hang", desc: "プロセスがなぜ止まっているかを調べる: 長く眠っているスレッドと待っているもの、読まれずに残っているデータのある fd、futex をみな起こしても同じところで眠りなおすか (起こしが消えたのか、だれも起こしていないのか)、見立て (hints)", input: HANG },
            Tool { name: "stack", desc: "スレッドの呼び出しの並び (いちばん上から)。ファイルの .eh_frame でたどり、CFI のないところ (musl の libc) はスタックから戻り先を探す (how: pc / cfi / lr / scan)。名前は .symtab か .dynsym、名前のないもの (strip した libxul など) は、その関数のあたりで使っている文字列 (strings_near) を手がかりに", input: STACK },
            Tool { name: "watch", desc: "aiwatch (カーネルの見張りのサービス) がいま見つけているもの (active) と最近の記録 (log)。kind: lost_wakeup_poll / lost_wakeup_futex (起こしが消えた: カーネルのまちがいかも) / wake_storm (起こしすぎ) / bkl_busy (大きなロックの混雑) / resolved。stack_file はそのときのスタック (stack ツールの file で)", input: WATCH },
            Tool { name: "strace", desc: "cmd を動かして、そのプロセス (name) のシステムコールを返す (名前(引数 3 つ) = 答え)", input: STRACE },
        ],
    };
    aish_plugin::run(spec, |ev, v| match ev {
        "tool" => {
            let a = &v["args"];
            let pwd = s(v, "pwd");
            match s(v, "name") {
                "get" => get(a),
                "do" => do_(a),
                "diff" => match Command::new("aios").arg("diff").output() {
                    Ok(o) => json!({ "status": o.status.code().unwrap_or(-1), "text": String::from_utf8_lossy(&o.stdout), "err": String::from_utf8_lossy(&o.stderr) }),
                    Err(e) => error(format!("aios: {}", e)),
                },
                "tune" => tune(a),
                "sys" => summary(),
                "procs" => procs(a),
                "kmsg" => kmsg(a),
                "log" => log(a),
                "bkl" => bkl(a, pwd),
                "strace" => strace(a, pwd),
                "threads" => threads(a),
                "hang" => hang(a),
                "stack" => stack(a),
                "watch" => watch(a),
                n => error(format!("{}: no such tool", n)),
            }
        }
        "key" => key(),
        _ => json!({}),
    });
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// aios のカーネルか (/proc/bkl は aios だけ)。Linux の /proc/kmsg は読むと待ちつづけるので、aios でなければ読まない
fn aios() -> bool {
    Path::new("/proc/bkl").exists()
}

/// カーネルのメッセージ (aios の /proc/kmsg)
fn kmsg_text() -> Option<String> {
    if aios() { read("/proc/kmsg") } else { None }
}

/// 終わりから n 行 (pat があればそれをふくむ行だけ)
fn tail(text: &str, pat: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| pat.is_empty() || l.contains(pat)).collect();
    let from = lines.len().saturating_sub(n);
    let mut s = lines[from..].join("\n");
    s.push('\n');
    s
}

fn kib(kb: u64) -> String {
    if kb >= 1024 * 1024 {
        format!("{:.1} GiB", kb as f64 / 1024.0 / 1024.0)
    } else {
        format!("{} MiB", kb / 1024)
    }
}

// ---- 状態の木 (aios get) ----

/// tune: aios tune NAME=V1,V2 [-n N] [--apply] --json -- CMD。root でなければ sudo -n で
fn tune(a: &Value) -> Value {
    let vals: Vec<&str> = a["values"].as_array().map(|v| v.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
    let (name, cmd) = (s(a, "name"), s(a, "cmd"));
    if name.is_empty() || vals.is_empty() || cmd.is_empty() {
        return error("give name, values and cmd");
    }
    let mut args = vec!["aios".to_string(), "tune".into(), format!("{}={}", name, vals.join(",")), "-n".into(), a["n"].as_u64().unwrap_or(3).to_string(), "--json".into()];
    if a["apply"] == true {
        args.push("--apply".into());
    }
    args.push("--".into());
    args.push(cmd.to_string());
    let root = unsafe { libc::geteuid() } == 0;
    let mut c = if root { Command::new(&args[0]) } else { Command::new("sudo") };
    if root {
        c.args(&args[1..]);
    } else {
        c.arg("-n").args(&args);
    }
    match c.stdin(Stdio::null()).output() {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout);
            let mut r = out.lines().find_map(|l| serde_json::from_str::<Value>(l).ok().filter(|v| v.is_object())).unwrap_or_else(|| json!({}));
            if !o.status.success() {
                r["error"] = json!(String::from_utf8_lossy(&o.stderr).trim().to_string());
            }
            // --apply のときは apply の結果の行も
            let rest: Vec<&str> = out.lines().filter(|l| !l.starts_with('{')).collect();
            if !rest.is_empty() {
                r["text"] = json!(rest.join("\n"));
            }
            r
        }
        Err(e) => error(format!("aios: {}", e)),
    }
}

fn get(a: &Value) -> Value {
    let path = a["path"].as_str().unwrap_or("");
    let mut c = Command::new("aios");
    c.arg("get");
    if !path.is_empty() {
        c.arg(path);
    }
    match c.arg("--json").output() {
        Ok(o) if o.status.success() => match serde_json::from_slice::<Value>(&o.stdout) {
            Ok(v) if v.is_object() => v,
            Ok(v) => json!({ "value": v }),
            Err(e) => error(format!("aios get: {}", e)),
        },
        Ok(o) => error(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => error(format!("aios: {} (aios の base パッケージのコマンド)", e)),
    }
}

/// aiosd に 1 つ頼む (/run/aiosd.sock に 1 行の JSON)
fn do_(a: &Value) -> Value {
    use std::io::BufRead;
    let mut c = match std::os::unix::net::UnixStream::connect("/run/aiosd.sock") {
        Ok(c) => c,
        Err(e) => return error(format!("/run/aiosd.sock: {} (aiosd is not running: sudo systemctl enable --now aiosd)", e)),
    };
    let mut line = String::new();
    if writeln!(c, "{}", a).is_err() || std::io::BufReader::new(&c).read_line(&mut line).is_err() {
        return error("aiosd did not answer");
    }
    serde_json::from_str(&line).unwrap_or_else(|_| error("aiosd: bad answer"))
}

// ---- まとめ ----

fn meminfo() -> std::collections::HashMap<String, u64> {
    read("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.to_string(), v.split_whitespace().next()?.parse().ok()?))
        })
        .collect()
}

fn disk(path: &str) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let bs = st.f_frsize as u64;
    Some(((st.f_blocks - st.f_bfree) as u64 * bs / 1024, st.f_blocks as u64 * bs / 1024))
}

fn uname_r() -> String {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 {
        return "?".into();
    }
    let f = |a: &[libc::c_char]| unsafe { std::ffi::CStr::from_ptr(a.as_ptr()) }.to_string_lossy().into_owned();
    format!("{} {} ({})", f(&u.sysname), f(&u.release), f(&u.machine))
}

fn summary() -> Value {
    let mut t = String::new();
    t.push_str(&format!("kernel   {}\n", uname_r()));
    if let Some(up) = read("/proc/uptime").and_then(|u| u.split_whitespace().next()?.parse::<f64>().ok()) {
        let s = up as u64;
        t.push_str(&format!("uptime   {}h {}m {}s\n", s / 3600, s / 60 % 60, s % 60));
    }
    let cpus = read("/proc/cpuinfo").map_or(0, |c| c.lines().filter(|l| l.starts_with("processor")).count());
    t.push_str(&format!("cpu      {}\n", cpus));
    let m = meminfo();
    let g = |k: &str| m.get(k).copied().unwrap_or(0);
    let avail = if m.contains_key("MemAvailable") { g("MemAvailable") } else { g("MemFree") };
    t.push_str(&format!("memory   {} / {} used\n", kib(g("MemTotal").saturating_sub(avail)), kib(g("MemTotal"))));
    t.push_str(&format!("swap     {} / {} used\n", kib(g("SwapTotal").saturating_sub(g("SwapFree"))), kib(g("SwapTotal"))));
    if let Some((used, total)) = disk("/") {
        t.push_str(&format!("disk /   {} / {} used ({}%)\n", kib(used), kib(total), used * 100 / total.max(1)));
    }
    let ps = list_procs();
    t.push_str(&format!("procs    {}\n", ps.len()));
    let mut top: Vec<&P> = ps.iter().collect();
    top.sort_by_key(|p| std::cmp::Reverse(p.ticks));
    for p in top.iter().take(5) {
        t.push_str(&format!("  {:>6} {:>8.1}s {:>8} {}\n", p.pid, p.ticks as f64 / 100.0, kib(p.rss_kb), p.name));
    }
    if let Some(b) = read("/proc/bkl")
        && let Some(all) = b.lines().find(|l| l.starts_with("all"))
    {
        t.push_str(&format!("bkl      {}\n", all.trim_start_matches("all").trim()));
    }
    if let Some(k) = kmsg_text() {
        t.push_str("kmsg (終わりの 5 行)\n");
        for l in tail(&k, "", 5).lines() {
            t.push_str(&format!("  {}\n", l));
        }
    }
    json!({ "text": t })
}

// ---- プロセス ----

struct P {
    pid: u32,
    ppid: u32,
    state: String,
    threads: u32,
    /// 使った CPU の時間 (1/100 秒)
    ticks: u64,
    rss_kb: u64,
    name: String,
    cmdline: String,
}

fn list_procs() -> Vec<P> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else { return out };
    for e in dir.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Some(stat) = read(&format!("/proc/{}/stat", pid)) else { continue };
        // pid (comm) state ppid ... utime(14) stime(15) ... threads(20)
        let (Some(l), Some(r)) = (stat.find('('), stat.rfind(')')) else { continue };
        let name = stat[l + 1..r].to_string();
        let f: Vec<&str> = stat[r + 1..].split_whitespace().collect();
        let num = |i: usize| f.get(i - 3).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        let rss_kb = read(&format!("/proc/{}/status", pid))
            .and_then(|st| st.lines().find(|l| l.starts_with("VmRSS:"))?.split_whitespace().nth(1)?.parse().ok())
            .unwrap_or(0);
        let cmdline = std::fs::read(format!("/proc/{}/cmdline", pid))
            .map(|b| String::from_utf8_lossy(&b).replace('\0', " ").trim().to_string())
            .unwrap_or_default();
        out.push(P {
            pid,
            ppid: num(4) as u32,
            state: f.first().unwrap_or(&"?").to_string(),
            threads: num(20) as u32,
            ticks: num(14) + num(15),
            rss_kb,
            name,
            cmdline,
        });
    }
    out
}

fn procs(a: &Value) -> Value {
    let mut ps = list_procs();
    if let Some(n) = a["name"].as_str().filter(|n| !n.is_empty()) {
        ps.retain(|p| p.name.contains(n) || p.cmdline.contains(n));
    }
    match a["sort"].as_str().unwrap_or("cpu") {
        "mem" => ps.sort_by_key(|p| std::cmp::Reverse(p.rss_kb)),
        _ => ps.sort_by_key(|p| std::cmp::Reverse(p.ticks)),
    }
    let limit = a["limit"].as_u64().unwrap_or(20) as usize;
    let mut t = String::from("   pid   ppid s thr    cpu s      rss  cmd\n");
    for p in ps.iter().take(limit) {
        let cmd = if p.cmdline.is_empty() { format!("[{}]", p.name) } else { p.cmdline.chars().take(100).collect() };
        t.push_str(&format!("{:>6} {:>6} {} {:>3} {:>8.1} {:>8}  {}\n", p.pid, p.ppid, p.state, p.threads, p.ticks as f64 / 100.0, kib(p.rss_kb), cmd));
    }
    json!({ "text": t, "count": ps.len() })
}

// ---- ログ ----

fn kmsg(a: &Value) -> Value {
    let Some(k) = kmsg_text() else { return error("no /proc/kmsg (not aios, or an older kernel)") };
    json!({ "text": tail(&k, a["grep"].as_str().unwrap_or(""), a["lines"].as_u64().unwrap_or(50) as usize) })
}

fn log(a: &Value) -> Value {
    let unit = a["unit"].as_str().unwrap_or("");
    if unit.is_empty() {
        let mut names: Vec<String> = std::fs::read_dir("/var/log")
            .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".log")).collect())
            .unwrap_or_default();
        names.sort();
        return json!({ "text": names.join("\n") + "\n" });
    }
    let full = if unit.contains('.') { unit.to_string() } else { format!("{}.service", unit) };
    let path = format!("/var/log/{}.log", full);
    match read(&path) {
        Some(t) => json!({ "path": path, "text": tail(&t, a["grep"].as_str().unwrap_or(""), a["lines"].as_u64().unwrap_or(50) as usize) }),
        None => error(format!("{}: no log", path)),
    }
}

// ---- コマンドを動かして測る ----

/// root だけが書ける /proc のファイルに書く (root でなければ sudo -n tee)
fn write_root(path: &str, data: &str) -> Result<(), String> {
    if unsafe { libc::geteuid() } == 0 {
        return std::fs::write(path, data).map_err(|e| format!("{}: {}", path, e));
    }
    let mut c = Command::new("sudo")
        .args(["-n", "tee", path])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("sudo: {}", e))?;
    let _ = c.stdin.take().unwrap().write_all(data.as_bytes());
    let o = c.wait_with_output().map_err(|e| e.to_string())?;
    if o.status.success() { Ok(()) } else { Err(format!("sudo -n tee {}: {}", path, String::from_utf8_lossy(&o.stderr).trim())) }
}

/// sh -c cmd を pwd で動かす。時間切れなら止める。(status, out, err, ms, timeout)
fn run(cmd: &str, pwd: &str, timeout: Duration) -> Result<(i32, String, String, u64, bool), String> {
    use std::os::unix::process::CommandExt;
    let start = Instant::now();
    let mut c = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(if pwd.is_empty() { "." } else { pwd })
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut o = c.stdout.take().unwrap();
    let mut e = c.stderr.take().unwrap();
    let ro = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = o.read_to_end(&mut b);
        b
    });
    let re = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = e.read_to_end(&mut b);
        b
    });
    let mut timed_out = false;
    let status = loop {
        match c.try_wait() {
            Ok(Some(st)) => break st.code().unwrap_or(-1),
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                timed_out = true;
                unsafe { libc::kill(-(c.id() as i32), libc::SIGKILL) };
                let _ = c.wait();
                break 124;
            }
        }
    };
    let cut = |b: Vec<u8>| {
        let s = String::from_utf8_lossy(&b).into_owned();
        if s.len() > 4000 { format!("...{}", &s[s.floor_char_boundary(s.len() - 4000)..]) } else { s }
    };
    let (out, err) = (cut(ro.join().unwrap_or_default()), cut(re.join().unwrap_or_default()));
    Ok((status, out, err, start.elapsed().as_millis() as u64, timed_out))
}

fn timeout_of(a: &Value) -> Duration {
    // Claude Code はツールの答えを 60 秒しか待たないので、長くても 50 秒
    Duration::from_millis(a["timeout_ms"].as_u64().unwrap_or(50_000).min(50_000))
}

fn bkl(a: &Value, pwd: &str) -> Value {
    if !Path::new("/proc/bkl").exists() {
        return error("no /proc/bkl (not aios, or an older kernel)");
    }
    let cmd = a["cmd"].as_str().unwrap_or("");
    if cmd.is_empty() {
        return json!({ "text": read("/proc/bkl").unwrap_or_default() });
    }
    if let Err(e) = write_root("/proc/bkl", "\n") {
        return error(e);
    }
    match run(cmd, pwd, timeout_of(a)) {
        Ok((status, out, err, ms, to)) => {
            let mut r = json!({ "status": status, "ms": ms, "text": read("/proc/bkl").unwrap_or_default() });
            if to {
                r["timeout"] = json!(true);
            }
            if status != 0 {
                r["out"] = json!(out);
                r["err"] = json!(err);
            }
            r
        }
        Err(e) => error(e),
    }
}

fn strace(a: &Value, pwd: &str) -> Value {
    if !aios() || !Path::new("/proc/strace").exists() {
        return error("no /proc/strace or /proc/kmsg (not aios, or an older kernel)");
    }
    let cmd = s(a, "cmd");
    let name: String = match a["name"].as_str().filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => cmd.split_whitespace().next().unwrap_or("").rsplit('/').next().unwrap_or("").to_string(),
    };
    if name.is_empty() {
        return error("no command");
    }
    // カーネルの comm は 15 文字まで
    let name: String = name.chars().take(15).collect();
    let all = a["all"].as_bool().unwrap_or(true);
    let before = kmsg_text().map_or(0, |k| k.lines().count());
    if let Err(e) = write_root("/proc/strace", &format!("{}{}", if all { "+" } else { "" }, name)) {
        return error(e);
    }
    let res = run(cmd, pwd, timeout_of(a));
    let _ = write_root("/proc/strace", "");
    let k = kmsg_text().unwrap_or_default();
    let limit = a["limit"].as_u64().unwrap_or(300) as usize;
    // 前に読んだ行より後ろの strace の行 (kmsg があふれていれば、あるものぜんぶ)
    let lines: Vec<&str> = k.lines().skip(before.min(k.lines().count())).filter(|l| l.contains("strace [")).collect();
    let n = lines.len();
    let mut text = lines.iter().take(limit).map(|l| l.split_once("strace ").map_or(*l, |(_, r)| r)).collect::<Vec<_>>().join("\n");
    text.push('\n');
    match res {
        Ok((status, out, err, ms, to)) => {
            let mut r = json!({ "status": status, "ms": ms, "calls": n, "text": text });
            if n > limit {
                r["more"] = json!(n - limit);
            }
            if to {
                r["timeout"] = json!(true);
            }
            r["out"] = json!(out);
            r["err"] = json!(err);
            r
        }
        Err(e) => error(e),
    }
}

// ---- M-s ----

fn key() -> Value {
    let Ok(mut tty) = Tty::open() else { return json!({}) };
    let t = summary()["text"].as_str().unwrap_or("").to_string();
    let lines: Vec<&str> = t.lines().collect();
    for l in &lines {
        tty.write(&format!("\x1b[2m{}\x1b[0m\r\n", aish_plugin::clip(l, tty.cols().saturating_sub(1))));
    }
    tty.write("\x1b[2m(何かキーで消す)\x1b[0m");
    let _ = tty.key();
    // 出したものを消して、始めの場所 (打ちかけの行の下の行の頭) へ戻る
    tty.write(&format!("\r\x1b[{}A\x1b[J", lines.len()));
    json!({})
}

// ---- /proc/ai: threads と hang ----

/// root (と /proc/ai は wheel の人) だけが読めるもの。読めなければ sudo -n cat
fn read_root(path: &str) -> Result<String, String> {
    if let Ok(t) = std::fs::read_to_string(path) {
        return Ok(t);
    }
    let o = Command::new("sudo").args(["-n", "cat", path]).output().map_err(|e| format!("sudo: {}", e))?;
    if o.status.success() { Ok(String::from_utf8_lossy(&o.stdout).into_owned()) } else { Err(format!("sudo -n cat {}: {}", path, String::from_utf8_lossy(&o.stderr).trim())) }
}

/// 1 行 1 つの JSON を読む
fn json_lines(path: &str) -> Result<Vec<Value>, String> {
    Ok(read_root(path)?.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
}

fn ai_threads() -> Result<Vec<Value>, String> {
    if !Path::new("/proc/ai/threads").exists() {
        return Err("no /proc/ai (not aios, or an older kernel)".into());
    }
    json_lines("/proc/ai/threads")
}

/// プロセス pid の fd (/proc/ai/fd/PID): fd 番号 → その行
fn ai_fds(pid: u64) -> std::collections::BTreeMap<u64, Value> {
    json_lines(&format!("/proc/ai/fd/{}", pid)).unwrap_or_default().into_iter().filter_map(|v| Some((v["fd"].as_u64()?, v))).collect()
}

/// wait の poll の fd 番号を、その fd の様子に置きかえる
fn annotate(t: &mut Value, fds: &std::collections::BTreeMap<u64, Value>) {
    if let Some(list) = t["wait"]["poll"].as_array() {
        let v: Vec<Value> = list.iter().map(|n| n.as_u64().and_then(|n| fds.get(&n).cloned()).unwrap_or(n.clone())).collect();
        t["wait"]["poll"] = json!(v);
    }
}

fn threads(a: &Value) -> Value {
    let ts = match ai_threads() {
        Ok(t) => t,
        Err(e) => return error(e),
    };
    // プロセスの名前 (代表スレッドの名前) も名前で絞るときに見る
    let leaders: std::collections::BTreeMap<u64, String> = ts.iter().filter(|t| t["tid"] == t["pid"]).map(|t| (t["pid"].as_u64().unwrap_or(0), s(t, "name").to_string())).collect();
    let name = s(a, "name");
    let state = s(a, "state");
    let min = a["min_slept_s"].as_f64().unwrap_or(0.0);
    let limit = a["limit"].as_u64().unwrap_or(50) as usize;
    let mut sel: Vec<Value> = ts
        .into_iter()
        .filter(|t| a["pid"].as_u64().is_none_or(|p| t["pid"].as_u64() == Some(p)))
        .filter(|t| name.is_empty() || s(t, "name").contains(name) || leaders.get(&t["pid"].as_u64().unwrap_or(0)).is_some_and(|l| l.contains(name)))
        .filter(|t| state.is_empty() || s(t, "state") == state)
        .filter(|t| min <= 0.0 || t["slept_s"].as_f64().is_some_and(|x| x >= min))
        .collect();
    let total = sel.len();
    sel.truncate(limit);
    let mut cache = std::collections::BTreeMap::new();
    for t in sel.iter_mut() {
        if t["wait"]["poll"].is_array() {
            let pid = t["pid"].as_u64().unwrap_or(0);
            let fds = cache.entry(pid).or_insert_with(|| ai_fds(pid));
            annotate(t, fds);
        }
    }
    json!({ "count": total, "threads": sel })
}

fn hang(a: &Value) -> Value {
    let ts = match ai_threads() {
        Ok(t) => t,
        Err(e) => return error(e),
    };
    let pids: Vec<u64> = match (a["pid"].as_u64(), s(a, "name")) {
        (Some(p), _) => vec![p],
        (None, n) if !n.is_empty() => ts.iter().filter(|t| t["tid"] == t["pid"] && s(t, "name").contains(n)).filter_map(|t| t["pid"].as_u64()).collect(),
        _ => return error("pid か name を"),
    };
    if pids.is_empty() {
        return error("no such process");
    }
    // 起こしてみる (futex で眠っているもの)。少し待って読みなおし、同じ futex で眠りなおしたかを見る
    let kick = a["kick"].as_bool().unwrap_or(true);
    let mut after: Vec<Value> = Vec::new();
    if kick {
        for p in &pids {
            if ts.iter().any(|t| t["pid"].as_u64() == Some(*p) && t["wait"]["futex"].is_u64()) {
                let cmd = format!("kick {}", p);
                if std::fs::write("/proc/ai/ctl", &cmd).is_err() {
                    let _ = write_root("/proc/ai/ctl", &cmd);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
        after = ai_threads().unwrap_or_default();
    }
    let procs: Vec<Value> = pids.iter().map(|&pid| hang_one(pid, &ts, &after, kick, a["stack"].as_bool().unwrap_or(false))).collect();
    json!({ "procs": procs })
}

fn hang_one(pid: u64, ts: &[Value], after: &[Value], kick: bool, stack: bool) -> Value {
    let fds = ai_fds(pid);
    let mut mine: Vec<Value> = ts.iter().filter(|t| t["pid"].as_u64() == Some(pid)).cloned().collect();
    let name = mine.iter().find(|t| t["tid"].as_u64() == Some(pid)).map_or(String::new(), |t| s(t, "name").to_string());
    let mut hints: Vec<String> = Vec::new();
    // 読まれずに残っているもの (眠っている人がそれを見張っていないなら、起こしが消えたかもしれない)
    let pending: Vec<Value> = fds.values().filter(|f| f["pending"].as_u64().is_some_and(|n| n > 0)).cloned().collect();
    let watched: std::collections::BTreeSet<u64> = mine.iter().filter_map(|t| t["wait"]["poll"].as_array()).flatten().filter_map(|n| n.as_u64()).collect();
    for f in &pending {
        let n = f["fd"].as_u64().unwrap_or(0);
        if watched.contains(&n) {
            hints.push(format!("fd {} ({}) に読まれていないデータがあるのに、それを見張って眠っているスレッドがいる: 起こしが消えたかも", n, s(f, "what")));
        }
    }
    // 長く眠っているものから
    mine.sort_by(|x, y| y["slept_s"].as_f64().unwrap_or(-1.0).partial_cmp(&x["slept_s"].as_f64().unwrap_or(-1.0)).unwrap_or(std::cmp::Ordering::Equal));
    for t in mine.iter_mut() {
        annotate(t, &fds);
        let tid = t["tid"].as_u64().unwrap_or(0);
        if let Some(addr) = t["wait"]["futex"].as_u64() {
            let unchanged = t["wait"]["val"] == t["wait"]["now"];
            if kick {
                let again = after.iter().find(|x| x["tid"].as_u64() == Some(tid));
                let same = again.is_some_and(|x| x["wait"]["futex"].as_u64() == Some(addr) && x["state"] == "sleep");
                t["kick"] = json!(if same { "眠りなおした (同じ futex)" } else { "起きて進んだ" });
                if !same && t["slept_s"].as_f64().is_some_and(|x| x > 1.0) {
                    hints.push(format!("tid {} ({}) は起こすと進んだ: futex の起こしが消えていたかも", tid, s(t, "name")));
                }
            }
            if unchanged && t["slept_s"].as_f64().is_some_and(|x| x > 5.0) && tid == pid {
                hints.push(format!("代表スレッド {} が futex {:#x} で {:.0} 秒眠っていて、値は変わっていない: だれも知らせていない (ほかのスレッドかプロセスの返事を待っている。stack: true でどこで待っているか)", tid, addr, t["slept_s"].as_f64().unwrap_or(0.0)));
            }
        }
    }
    let busy: Vec<String> = mine.iter().filter(|t| t["state"] == "run" || t["state"] == "ready").map(|t| format!("{} {}", t["tid"], s(t, "name"))).collect();
    if !busy.is_empty() && mine.iter().any(|t| t["slept_s"].as_f64().is_some_and(|x| x > 5.0)) {
        hints.push(format!("走っているもの: {}。CPU が足りないか、大きなロックが混んでいるかもしれない (bkl で)", busy.join(", ")));
    }
    let mut r = json!({ "pid": pid, "name": name, "threads": mine, "pending_fds": pending, "hints": hints });
    if stack {
        // 長く眠っているものから 4 つ (mine はもう長い順)
        let tids: Vec<u64> = mine.iter().filter(|t| t["slept_s"].as_f64().is_some_and(|x| x > 1.0)).filter_map(|t| t["tid"].as_u64()).take(4).collect();
        r["stacks"] = json!(tids.iter().map(|&t| json!({ "tid": t, "frames": thread_frames(t, 30) })).collect::<Vec<_>>());
    }
    r
}

/// tid の呼び出しの並び (/proc/ai/stack/TID から)
fn thread_frames(tid: u64, max: usize) -> Value {
    match read_root(&format!("/proc/ai/stack/{}", tid)).map(|t| serde_json::from_str::<Value>(t.trim())) {
        Ok(Ok(st)) => json!(unwind::frames(&st, max)),
        Ok(Err(e)) => json!(format!("/proc/ai/stack/{}: {}", tid, e)),
        Err(e) => json!(e),
    }
}

fn stack(a: &Value) -> Value {
    let max = a["max"].as_u64().unwrap_or(40) as usize;
    if let Some(f) = a["file"].as_str().filter(|f| !f.is_empty()) {
        // 残しておいたもの: 写していたファイルが同じなら、プロセスが終わっていてもたどれる
        return match std::fs::read_to_string(f).map(|t| serde_json::from_str::<Value>(t.trim())) {
            Ok(Ok(st)) => json!({ "tid": st["tid"], "name": st["name"], "frames": unwind::frames(&st, max) }),
            Ok(Err(e)) => error(format!("{}: {}", f, e)),
            Err(e) => error(format!("{}: {}", f, e)),
        };
    }
    if let Some(t) = a["tid"].as_u64() {
        return json!({ "tid": t, "frames": thread_frames(t, max) });
    }
    let Some(pid) = a["pid"].as_u64() else { return error("tid か pid を") };
    let ts = match ai_threads() {
        Ok(t) => t,
        Err(e) => return error(e),
    };
    let mut mine: Vec<&Value> = ts.iter().filter(|t| t["pid"].as_u64() == Some(pid)).collect();
    mine.sort_by(|x, y| y["slept_s"].as_f64().unwrap_or(-1.0).partial_cmp(&x["slept_s"].as_f64().unwrap_or(-1.0)).unwrap_or(std::cmp::Ordering::Equal));
    let limit = a["limit"].as_u64().unwrap_or(8) as usize;
    let out: Vec<Value> = mine.iter().take(limit).map(|t| json!({ "tid": t["tid"], "name": t["name"], "state": t["state"], "wait": t["wait"], "frames": thread_frames(t["tid"].as_u64().unwrap_or(0), max) })).collect();
    json!({ "pid": pid, "threads": out })
}

fn watch(a: &Value) -> Value {
    let n = a["lines"].as_u64().unwrap_or(20) as usize;
    let state: Value = read("/run/aiwatch.json").and_then(|t| serde_json::from_str(t.trim()).ok()).unwrap_or(Value::Null);
    if state.is_null() {
        return error("no /run/aiwatch.json (aiwatch is not running: systemctl start aiwatch)");
    }
    let log = read("/var/log/aiwatch.service.log").unwrap_or_default();
    let all: Vec<Value> = log.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let recent: Vec<Value> = all.iter().skip(all.len().saturating_sub(n)).cloned().collect();
    json!({ "active": state["active"], "checked_at": state["t"], "interval_s": state["interval_s"], "log": recent })
}
