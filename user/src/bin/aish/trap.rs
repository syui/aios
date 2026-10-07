// trap (POSIX): シグナルとシェルの終わり (EXIT) に動かすもの
//   trap 'CMD' SIG...   SIG が来たら (コマンドのあいだで) CMD を動かす。EXIT (0) はシェルが終わるとき
//   trap '' SIG...      無視する
//   trap - SIG...       もとに戻す (trap N SIG... と最初が数でも)
//   trap                一覧
// シグナルのハンドラは印をつけるだけで、動かすのは run_list のコマンドのあいだ。
// fork したサブシェルでは動かさない (trap を置いたプロセスだけ。POSIX ではサブシェルは trap を受けつがない)
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

static PENDING: AtomicU64 = AtomicU64::new(0);
static OWNER: AtomicI32 = AtomicI32::new(0);
static mut TRAPS: BTreeMap<i32, String> = BTreeMap::new();
/// EXIT の trap を動かすシェル (exit_shell から呼ぶため)
static mut SHELL: *mut super::Shell = std::ptr::null_mut();

const SIGS: &[(&str, i32)] = &[
    ("EXIT", 0),
    // bash: コマンドが失敗したとき (set -e で止まるのと同じところ)
    ("ERR", -1),
    ("HUP", 1),
    ("INT", 2),
    ("QUIT", 3),
    ("ILL", 4),
    ("TRAP", 5),
    ("ABRT", 6),
    ("BUS", 7),
    ("FPE", 8),
    ("USR1", 10),
    ("SEGV", 11),
    ("USR2", 12),
    ("PIPE", 13),
    ("ALRM", 14),
    ("TERM", 15),
    ("CHLD", 17),
    ("CONT", 18),
    ("TSTP", 20),
    ("TTIN", 21),
    ("TTOU", 22),
    ("WINCH", 28),
];

fn traps() -> &'static mut BTreeMap<i32, String> {
    unsafe { &mut *(&raw mut TRAPS) }
}

/// trap を置いたシェル (EXIT を動かすのに使う)
pub fn set_shell(sh: *mut super::Shell) {
    unsafe { SHELL = sh };
}

fn mine() -> bool {
    OWNER.load(Ordering::Relaxed) == unsafe { libc::getpid() }
}

fn number(s: &str) -> Option<i32> {
    if let Ok(n) = s.parse::<i32>() {
        return (0..64).contains(&n).then_some(n);
    }
    let up = s.to_uppercase();
    let name = up.strip_prefix("SIG").unwrap_or(&up);
    SIGS.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
}

fn name(sig: i32) -> String {
    match SIGS.iter().find(|(_, v)| *v == sig) {
        Some((n, v)) if *v <= 0 => n.to_string(),
        Some((n, _)) => format!("SIG{}", n),
        None => sig.to_string(),
    }
}

extern "C" fn on_signal(sig: libc::c_int) {
    PENDING.fetch_or(1 << sig, Ordering::SeqCst);
}

/// trap の組み込みコマンド
pub fn builtin(a: &[String]) -> i32 {
    let a: Vec<&str> = a.iter().map(|s| s.as_str()).filter(|s| *s != "--").collect();
    if a.is_empty() || a == ["-p"] {
        for (sig, cmd) in traps().iter() {
            println!("trap -- {} {}", super::quote(cmd), name(*sig));
        }
        return 0;
    }
    if a == ["-l"] {
        for (n, v) in SIGS.iter().filter(|(_, v)| *v > 0) {
            println!("{}) SIG{}", v, n);
        }
        return 0;
    }
    // 最初が数なら、みなもとに戻す (POSIX)
    let (action, sigs) = if a.len() == 1 || a[0].parse::<u32>().is_ok() { ("-", &a[..]) } else { (a[0], &a[1..]) };
    OWNER.store(unsafe { libc::getpid() }, Ordering::Relaxed);
    let mut st = 0;
    for s in sigs {
        let Some(sig) = number(s) else {
            eprintln!("trap: {}: invalid signal specification", s);
            st = 1;
            continue;
        };
        if sig == libc::SIGKILL || sig == libc::SIGSTOP {
            eprintln!("trap: {}: cannot be trapped", s);
            st = 1;
            continue;
        }
        match action {
            "-" => {
                traps().remove(&sig);
                if sig > 0 {
                    unsafe { libc::signal(sig, libc::SIG_DFL) };
                }
            }
            "" => {
                traps().insert(sig, String::new());
                if sig > 0 {
                    unsafe { libc::signal(sig, libc::SIG_IGN) };
                }
            }
            cmd => {
                traps().insert(sig, cmd.to_string());
                if sig > 0 {
                    unsafe {
                        let mut sa: libc::sigaction = std::mem::zeroed();
                        sa.sa_sigaction = on_signal as *const () as usize;
                        sa.sa_flags = libc::SA_RESTART;
                        libc::sigaction(sig, &sa, std::ptr::null_mut());
                    }
                }
            }
        }
    }
    st
}

/// 来たシグナルの trap (コマンドのあいだで動かすもの)。来ていなければ空
pub fn take_pending() -> Vec<String> {
    let p = PENDING.swap(0, Ordering::SeqCst);
    if p == 0 || !mine() {
        return vec![];
    }
    (1..64).filter(|s| p & (1 << s) != 0).filter_map(|s| traps().get(&s).filter(|c| !c.is_empty()).cloned()).collect()
}

/// 失敗したコマンドのあと: ERR の trap (なければ None。trap の中の失敗では動かさない)
pub fn take_err() -> Option<String> {
    static IN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !mine() || IN.load(Ordering::Relaxed) {
        return None;
    }
    let cmd = traps().get(&-1).filter(|c| !c.is_empty()).cloned()?;
    IN.store(true, Ordering::Relaxed);
    let sh = unsafe { SHELL };
    if !sh.is_null() {
        let sh = unsafe { &mut *sh };
        let st = sh.status;
        sh.run_source(&cmd, "trap");
        sh.status = st;
    }
    IN.store(false, Ordering::Relaxed);
    Some(cmd)
}

/// シェルが終わるとき: EXIT の trap を 1 回だけ動かす
pub fn run_exit() {
    if !mine() {
        return;
    }
    let Some(cmd) = traps().remove(&0).filter(|c| !c.is_empty()) else { return };
    let sh = unsafe { SHELL };
    if !sh.is_null() {
        let sh = unsafe { &mut *sh };
        let st = sh.status;
        sh.run_source(&cmd, "trap");
        sh.status = st;
    }
}
