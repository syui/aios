// わざとしくじらせる (フォルト注入。/proc/ai/ctl の fail、/proc/ai/fail。aish の fail): プログラムが、ないはずのない
// しくじり (ENOMEM、EIO、EAGAIN ...) にどう耐えるかを、カーネルの側から確かめる。決まり (RULE) は 1 つ:
//   fail pid N|name NAME sys NR|all ERRNO [TIMES]   そのプロセスのシステムコール (NR か、みな) を、動かさずに -ERRNO に
//   fail pid N|name NAME heap [TIMES]                そのプロセスが走っているあいだのカーネルのヒープの alloc を、取れないことに
//                                                    (耐えるところ (heap::fallible) だけ: そのシステムコールが ENOMEM になる)
//   fail pid N|name NAME oom [TIMES]                 同じく、耐えないところだけ: 予備から渡り、次のシステムコールで oom.rs が
//                                                    いちばん大きなプロセスを殺す (-snapshot の VM で)
//   fail pid N|name NAME page [TIMES]                同じく物理ページ (kalloc) を
//   fail off                                        やめる
// TIMES (既定 1) 回しくじらせたら、決まりはそのまま残って効かなくなる (hits で何回効いたかがわかる)。
// heap、oom、page は、カーネルの中の「取れなかった」道を確かめるもの。page (kalloc) は Option で返るので、そのプロセスが
// out of memory で殺されてカーネルは続く。heap は、耐えるところ (read の入れ物など) が ENOMEM を返して続くのを、
// oom は、耐えないところ (Vec::push など) が予備から渡されて oom.rs が空けるのを確かめる (heap.rs の「足りないとき」)
use crate::spinlock::SpinLock;
use core::sync::atomic::{AtomicBool, Ordering::Relaxed};

/// 決まりがあるか (それぞれの場所は、まずこれだけ見る)
pub static ON: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, PartialEq)]
enum What {
    Sys,
    Heap,
    Oom,
    Page,
}

#[derive(Clone, Copy)]
struct Rule {
    what: What,
    /// pid で選ぶなら Some
    pid: Option<u32>,
    /// 名前で選ぶなら (comm の頭)。空なら pid
    name: [u8; 16],
    /// Sys: 番号 (u64::MAX はみな)
    nr: u64,
    /// Sys: 返す errno (正の数)
    errno: i64,
    /// あと何回効かせるか
    left: u32,
    /// 効いた数
    hits: u32,
}

static RULE: SpinLock<Option<Rule>> = SpinLock::new(None);

/// いまのプロセスが決まりの相手か
fn matches(r: &Rule) -> bool {
    match r.pid {
        Some(pid) => crate::proc::current_pid_or_zero() != 0 && crate::proc::current_tgid_or_zero() == pid,
        None => {
            let n = r.name.iter().position(|&c| c == 0).unwrap_or(16);
            n > 0 && crate::proc::current_comm_starts_with(&r.name[..n])
        }
    }
}

/// 決まりに合い、まだ回数が残っていれば 1 回使う
fn take(what: What) -> Option<Rule> {
    let mut g = RULE.lock();
    let r = g.as_mut()?;
    if r.what != what || r.left == 0 {
        return None;
    }
    // 相手かどうかはロックの外で見たいが、決まりは 1 つなので、ここで写してから見る
    let copy = *r;
    drop(g);
    if !matches(&copy) {
        return None;
    }
    let mut g = RULE.lock();
    let r = g.as_mut()?;
    if r.left == 0 {
        return None;
    }
    r.left -= 1;
    r.hits += 1;
    Some(*r)
}

/// システムコール nr をしくじらせるなら、その errno (正の数)
pub fn syscall(nr: u64) -> Option<i64> {
    if !ON.load(Relaxed) {
        return None;
    }
    // 番号が合わなければ使わない (take は番号を見ない)
    if !RULE.lock().is_some_and(|r| r.what == What::Sys && (r.nr == u64::MAX || r.nr == nr)) {
        return None;
    }
    take(What::Sys).map(|r| r.errno)
}

/// カーネルのヒープの alloc を、ふつうの道も遅い道もなしに取れなかったことにするか。heap は耐える区間 (fallible) の
/// alloc だけ、oom は耐えない alloc だけ (heap.rs の alloc がそのあとを分ける)
pub fn heap(fallible: bool) -> bool {
    if !ON.load(Relaxed) {
        return false;
    }
    take(if fallible { What::Heap } else { What::Oom }).is_some()
}

/// 物理ページの alloc をしくじらせるか
pub fn page() -> bool {
    ON.load(Relaxed) && take(What::Page).is_some()
}

/// /proc/ai/ctl の fail ...
pub fn ctl(arg: &str) -> Result<(), i64> {
    const EINVAL: i64 = 22;
    if arg == "off" {
        *RULE.lock() = None;
        ON.store(false, Relaxed);
        return Ok(());
    }
    let mut w = arg.split_whitespace();
    let (pid, name) = match (w.next(), w.next()) {
        (Some("pid"), Some(n)) => (Some(n.parse::<u32>().map_err(|_| -EINVAL)?), [0u8; 16]),
        (Some("name"), Some(n)) if !n.is_empty() => {
            let mut c = [0u8; 16];
            let k = n.len().min(15);
            c[..k].copy_from_slice(&n.as_bytes()[..k]);
            (None, c)
        }
        _ => return Err(-EINVAL),
    };
    let what = match w.next() {
        Some("sys") => What::Sys,
        Some("heap") => What::Heap,
        Some("oom") => What::Oom,
        Some("page") => What::Page,
        _ => return Err(-EINVAL),
    };
    let (mut nr, mut errno) = (u64::MAX, 0);
    if what == What::Sys {
        let which = w.next().ok_or(-EINVAL)?;
        nr = if which == "all" { u64::MAX } else { which.parse::<u64>().or_else(|_| crate::syscall::number(which).ok_or(-EINVAL))? };
        errno = w.next().ok_or(-EINVAL)?.parse::<i64>().map_err(|_| -EINVAL)?;
        if !(1..=4095).contains(&errno) {
            return Err(-EINVAL);
        }
    }
    let times = match w.next() {
        Some(t) => t.parse::<u32>().map_err(|_| -EINVAL)?,
        None => 1,
    };
    *RULE.lock() = Some(Rule { what, pid, name, nr, errno, left: times, hits: 0 });
    ON.store(true, Relaxed);
    Ok(())
}

/// /proc/ai/fail: 1 行の JSON (決まりがなければ {"rule":null})
pub fn json() -> alloc::string::String {
    use alloc::format;
    let Some(r) = *RULE.lock() else { return alloc::string::String::from("{\"rule\":null}\n") };
    let n = r.name.iter().position(|&c| c == 0).unwrap_or(16);
    let target = match r.pid {
        Some(p) => format!("\"pid\":{}", p),
        None => format!("\"name\":\"{}\"", crate::aiproc::esc(core::str::from_utf8(&r.name[..n]).unwrap_or("?"))),
    };
    let what = match r.what {
        What::Sys => format!(
            "\"what\":\"sys\",\"nr\":{},\"errno\":{}",
            if r.nr == u64::MAX { alloc::string::String::from("\"all\"") } else { format!("\"{}\"", crate::syscall::name(r.nr).map_or_else(|| format!("{}", r.nr), |s| s.to_ascii_lowercase())) },
            r.errno
        ),
        What::Heap => alloc::string::String::from("\"what\":\"heap\""),
        What::Oom => alloc::string::String::from("\"what\":\"oom\""),
        What::Page => alloc::string::String::from("\"what\":\"page\""),
    };
    format!("{{\"rule\":{{{},{},\"left\":{},\"hits\":{}}}}}\n", target, what, r.left, r.hits)
}
