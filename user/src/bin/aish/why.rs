// しくじったわけ: aios のカーネルは、システムコールがしくじったわけを /proc/ai/why に残す (kernel/src/why.rs。errno の
// ほかに、パスのどこで止まったか、どの許しが足りなかったか、どの決まりが止めたか)。前のコマンドが 0 でなく終わったら
// (対話のとき。シグナルで終わったものと、エラーを 2>/dev/null などでよそへ向けたものはのぞく)、そのジョブのプロセスの
// いちばん新しいわけを 1 行出す:
//   $ cat /root/secret
//   cat: /root/secret: Permission denied
//   aish: why: openat EACCES: search (x) on /root is not allowed for uid 1000 gid 1000: mode 0700, owner 0:0, ...
// why で、前にしくじったジョブのわけをみな (新しい順。-a は自分のものみな)。/proc/ai/why がなければ (Linux) 何もしない
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

/// 前にしくじったジョブ: (プロセス, 始まった時刻)
static mut LAST: Option<(Vec<i32>, Instant)> = None;
/// aish 自身 (fork した子でなく)
static SHELL: AtomicI32 = AtomicI32::new(0);

/// aish が始まった (Shell::new)
pub fn set_shell(pid: i32) {
    SHELL.store(pid, Ordering::Relaxed);
}

/// /proc/ai/why の、pids のもの (pids が空ならみな) で、since からあとのもの。新しい順
fn read(pids: &[i32], since: Option<Instant>) -> Option<Vec<serde_json::Value>> {
    let text = std::fs::read_to_string("/proc/ai/why").ok()?;
    let max_ago = since.map(|t| t.elapsed().as_millis() as u64);
    Some(
        text.lines()
            .skip(1)
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|e| pids.is_empty() || e["pid"].as_i64().is_some_and(|p| pids.contains(&(p as i32))))
            // pid は使いまわされるので、ジョブが始まってからのものだけ
            .filter(|e| max_ago.is_none_or(|m| e["ago_ms"].as_u64().is_some_and(|a| a <= m)))
            .collect(),
    )
}

fn line(e: &serde_json::Value) -> String {
    let s = |k: &str| e[k].as_str().unwrap_or("").to_string();
    let head = format!("{} {} (pid {}, {})", s("syscall"), s("errno"), e["pid"], s("comm"));
    match e["why"].as_str() {
        Some(w) => format!("{}: {}", head, w),
        None => head,
    }
}

fn dim(text: &str) {
    if unsafe { libc::isatty(2) } == 1 {
        eprintln!("\x1b[90maish: why: {}\x1b[0m", text);
    } else {
        eprintln!("aish: why: {}", text);
    }
}

/// わけのパス (たどったところと名前、たのんだパス) が、コマンドの言葉 (引数) のどれかと重なるか
fn about(e: &serde_json::Value, words: &[&str]) -> bool {
    let path = e["path"].as_str().unwrap_or("").trim_start_matches('/');
    if path.is_empty() {
        return false;
    }
    let full = match e["name"].as_str() {
        Some(n) => format!("{}/{}", path, n),
        None => path.to_string(),
    };
    words.iter().map(|w| w.trim_start_matches("./").trim_start_matches('/')).filter(|w| w.len() > 1).any(|w| w.contains(path) || full.contains(w))
}

/// 出すわけを選ぶ (新しい順の v から): コマンドに書いたパスにかかわるもの、なければ、さがしもの (probe) でないもの、
/// なければいちばん新しいもの。しくじったあとに、エラーの文を訳すために locale のファイルをさがすもの (uutils) がある
fn pick<'a>(v: &'a [serde_json::Value], cmd: &str) -> Option<&'a serde_json::Value> {
    let words: Vec<&str> = cmd.split_whitespace().skip(1).collect();
    let with: Vec<&serde_json::Value> = v.iter().filter(|e| e.get("why").is_some()).collect();
    with.iter().find(|e| about(e, &words)).or_else(|| with.iter().find(|e| e.get("probe").is_none())).or_else(|| with.first()).copied()
}

/// コマンドが標準エラーをよそへ向けている (2>/dev/null、2>FILE、&>FILE。2>&1 はのぞく)
fn quiet(cmd: &str) -> bool {
    cmd.split_whitespace().any(|w| (w.starts_with("2>") && !w.starts_with("2>&")) || w.starts_with("&>"))
}

/// 前のジョブ (cmd) が終わった (jobs::foreground)。0 でなければ覚えて、対話ならわけを 1 つ出す
pub fn after_job(pids: &[i32], start: Instant, status: i32, cmd: &str) {
    if status == 0 || status >= 128 {
        return;
    }
    unsafe { *(&raw mut LAST) = Some((pids.to_vec(), start)) };
    // エラーをよそへ向けたもの (2>/dev/null など) は、わけも出さない (why では見られる)
    if !crate::jobs::interactive() || quiet(cmd) {
        return;
    }
    if let Some(v) = read(pids, Some(start))
        && let Some(e) = pick(&v, cmd)
    {
        dim(&line(e));
    }
}

/// aish 自身がしくじった (リダイレクトのファイルが開けない ...。what はそのパス): 対話なら、aish のこの 2 秒のわけを
/// 1 つ出す。fork した子の中なら出さない (そのジョブが終わったときに after_job が出す)
pub fn after_self(what: &str) {
    let me = unsafe { libc::getpid() };
    if !crate::jobs::interactive() || me != SHELL.load(Ordering::Relaxed) {
        return;
    }
    let start = Instant::now().checked_sub(Duration::from_secs(2)).unwrap_or_else(Instant::now);
    unsafe { *(&raw mut LAST) = Some((vec![me], start)) };
    if let Some(v) = read(&[me], Some(start))
        && let Some(e) = pick(&v, &format!("- {}", what))
    {
        dim(&line(e));
    }
}

/// why [-a]: 前にしくじったジョブのわけをみな (新しい順)。-a は見られるものみな
pub fn builtin(args: &[String]) -> i32 {
    let all = args.iter().any(|a| a == "-a");
    let (pids, since) = match unsafe { &*(&raw const LAST) } {
        _ if all => (Vec::new(), None),
        Some((p, t)) => (p.clone(), Some(*t)),
        None => {
            eprintln!("why: no failed command yet (why -a shows everything kept in /proc/ai/why)");
            return 1;
        }
    };
    let Some(v) = read(&pids, since) else {
        eprintln!("why: /proc/ai/why is not here (aios keeps it; Linux has only the errno)");
        return 1;
    };
    if v.is_empty() {
        println!("why: no failed system call was kept for it");
    }
    for e in &v {
        println!("{}", line(e));
    }
    0
}
