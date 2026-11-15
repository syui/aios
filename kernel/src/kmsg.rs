// カーネルのメッセージを覚えておく (Linux の dmesg と同じ)。println! はシリアルの画面にしか出ないので、
// ssh や aiwm の端末、Claude からも読めるように、出したものをここにもためる。/proc/kmsg で読む。
// 行の頭に起動してからの時間 ([    12.345678]) をつける (シリアルの画面にはつけない)
use crate::spinlock::SpinLock;
use alloc::string::String;
use alloc::vec::Vec;

/// ためる大きさ (古いものから消える)
const SIZE: usize = 256 * 1024;

struct Ring {
    buf: [u8; SIZE],
    /// いままでに書いたバイトの数 (buf の位置は head % SIZE)
    head: usize,
    /// 行の途中か (次に書くのが行の頭でない。はじめは頭: みな 0 なので、256 KiB が .data でなく .bss に入る)
    mid: bool,
}

/// 書く (println! の uart::Uart、strace) のも読む (/proc/kmsg) のもこのロックを取って
static RING: SpinLock<Ring> = SpinLock::new(Ring { buf: [0; SIZE], head: 0, mid: false });

fn put(r: &mut Ring, b: u8) {
    r.buf[r.head % SIZE] = b;
    r.head += 1;
}

/// 出したものを足す (uart::Uart と strace から)
pub fn push(s: &str) {
    let mut g = RING.lock();
    let r = &mut *g;
    for b in s.bytes() {
        if !r.mid {
            r.mid = true;
            let ns = crate::timer::uptime_ns();
            let mut t = [0u8; 32];
            let n = fmt_time(&mut t, ns);
            for &c in &t[..n] {
                put(r, c);
            }
        }
        put(r, b);
        if b == b'\n' {
            r.mid = false;
        }
    }
}

/// "[    12.345678] " を書いて長さを返す (format! はヒープを使うので、ここでは使わない)
fn fmt_time(out: &mut [u8; 32], ns: u64) -> usize {
    let (sec, us) = (ns / 1_000_000_000, ns / 1000 % 1_000_000);
    let mut digits = [0u8; 20];
    let mut n = 0;
    let mut v = sec;
    loop {
        digits[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let mut i = 0;
    out[i] = b'[';
    i += 1;
    for _ in n..5 {
        out[i] = b' ';
        i += 1;
    }
    for k in (0..n).rev() {
        out[i] = digits[k];
        i += 1;
    }
    out[i] = b'.';
    i += 1;
    let mut d = 100_000;
    while d > 0 {
        out[i] = b'0' + (us / d % 10) as u8;
        i += 1;
        d /= 10;
    }
    out[i] = b']';
    out[i + 1] = b' ';
    i + 2
}

// ---- syslog (Linux の kernel/printk/printk.c の do_syslog: dmesg や klogd が使う) ----

/// syslog の状態: READ (2) が読んだところ (書いたバイトの数え方) とその行の中で渡したバイト数 (Linux の syslog_seq と
/// syslog_partial)、READ_CLEAR と CLEAR で消したところ (clear_seq)、端末のレベル (console_loglevel と、CONSOLE_OFF で
/// しまったもの)、dmesg_restrict
struct Syslog {
    read: usize,
    partial: usize,
    clear: usize,
    console: u8,
    saved: u8,
    restrict: bool,
}

static SYSLOG: SpinLock<Syslog> = SpinLock::new(Syslog { read: 0, partial: 0, clear: 0, console: 7, saved: 0, restrict: false });

/// from (書いたバイトの数え方。消えたところなら残っているはじめ) から先の、終わっている行: (その行のはじめ, "<6>" を
/// つけた行)。aios のメッセージはレベルを持たないので、みな KERN_INFO (6)
fn records(from: usize) -> Vec<(usize, Vec<u8>)> {
    let r = RING.lock();
    let oldest = r.head.saturating_sub(SIZE);
    let mut at = from.max(oldest);
    // 消えたところから始まるなら、行のはじめまで飛ばす
    if from < oldest {
        while at < r.head && r.buf[(at - 1) % SIZE] != b'\n' {
            at += 1;
        }
    }
    let mut out = Vec::new();
    let mut line = b"<6>".to_vec();
    let mut start = at;
    while at < r.head {
        let c = r.buf[at % SIZE];
        line.push(c);
        at += 1;
        if c == b'\n' {
            out.push((start, core::mem::replace(&mut line, b"<6>".to_vec())));
            start = at;
        }
    }
    out
}

/// いま書いたところ
fn head() -> usize {
    RING.lock().head
}

/// /proc/sys/kernel/dmesg_restrict
pub fn dmesg_restrict() -> bool {
    SYSLOG.lock().restrict
}

pub fn set_dmesg_restrict(on: bool) {
    SYSLOG.lock().restrict = on;
}

/// syslog (Linux の do_syslog と同じ順): dmesg_restrict が 0 なら READ_ALL (3) と SIZE_BUFFER (10) はだれでも、ほかは
/// CAP_SYSLOG がなければ EPERM (知らない番号も)。READ と READ_ALL は buf がないか len が負なら EINVAL、len 0 は 0。
/// READ_ALL は入る分だけ終わりの行から (READ_CLEAR はそのあと消す)、READ は読んだところから先を (なければ待つ)、
/// CONSOLE_LEVEL は 1〜8 (でなければ EINVAL)。答えは書いたバイト数か大きさ
pub fn syslog(ty: i32, buf: usize, len: i32) -> Result<i64, i64> {
    const EINVAL: i64 = 22;
    const EPERM: i64 = 1;
    const EFAULT: i64 = 14;
    const CAP_SYSLOG: u32 = 34;
    let restricted = dmesg_restrict() || (ty != 3 && ty != 10);
    if restricted && !crate::cred::current().capable(CAP_SYSLOG) {
        return Err(-EPERM);
    }
    let out = |at: usize, b: &[u8]| crate::proc::current().pt().copy_out(at, b).ok_or(-EFAULT);
    match ty {
        0 | 1 => Ok(0),
        2 => {
            if buf == 0 || len < 0 {
                return Err(-EINVAL);
            }
            if len == 0 {
                return Ok(0);
            }
            if buf.checked_add(len as usize).is_none_or(|e| e > crate::vm::MAXVA) {
                return Err(-EFAULT);
            }
            // 読んでいないものが来るまで待つ (書く側は割り込みの中からも来るので、起こしてはもらわず 50 ms ずつ見る)
            loop {
                let (read, partial) = {
                    let s = SYSLOG.lock();
                    (s.read, s.partial)
                };
                let recs = records(read);
                if !recs.is_empty() {
                    let mut done = 0usize;
                    let mut next = (read, partial);
                    for (i, (start, line)) in recs.iter().enumerate() {
                        let skip = if i == 0 && *start == read { partial } else { 0 };
                        let rest = &line[skip..];
                        let room = len as usize - done;
                        if rest.len() > room {
                            // はじめの行だけは切って渡す (Linux の syslog_partial)
                            if done == 0 {
                                out(buf, &rest[..room])?;
                                done = room;
                                next = (*start, skip + room);
                            }
                            break;
                        }
                        out(buf + done, rest)?;
                        done += rest.len();
                        next = (start + line.len() - 3, 0);
                    }
                    let mut s = SYSLOG.lock();
                    s.read = next.0;
                    s.partial = next.1;
                    return Ok(done as i64);
                }
                let deadline = crate::timer::ticks() + crate::timer::HZ / 20 + 1;
                crate::proc::sleep_until(&SYSLOG as *const _ as usize, deadline)?;
            }
        }
        3 | 4 => {
            if buf == 0 || len < 0 {
                return Err(-EINVAL);
            }
            if len == 0 {
                return Ok(0);
            }
            if buf.checked_add(len as usize).is_none_or(|e| e > crate::vm::MAXVA) {
                return Err(-EFAULT);
            }
            let clear = SYSLOG.lock().clear;
            let recs = records(clear);
            // 終わりの行から、入るところまで
            let mut total = 0usize;
            let mut first = recs.len();
            for (i, (_, l)) in recs.iter().enumerate().rev() {
                if total + l.len() > len as usize {
                    break;
                }
                total += l.len();
                first = i;
            }
            let mut done = 0usize;
            for (_, l) in &recs[first..] {
                out(buf + done, l)?;
                done += l.len();
            }
            if ty == 4 {
                SYSLOG.lock().clear = recs.last().map_or(clear, |(s, l)| s + l.len() - 3);
            }
            Ok(done as i64)
        }
        5 => {
            SYSLOG.lock().clear = head();
            Ok(0)
        }
        6 => {
            let mut s = SYSLOG.lock();
            if s.saved == 0 {
                s.saved = s.console;
            }
            s.console = 1;
            Ok(0)
        }
        7 => {
            let mut s = SYSLOG.lock();
            if s.saved != 0 {
                s.console = s.saved;
                s.saved = 0;
            }
            Ok(0)
        }
        8 => {
            if !(1..=8).contains(&len) {
                return Err(-EINVAL);
            }
            let mut s = SYSLOG.lock();
            s.console = len as u8;
            s.saved = 0;
            Ok(0)
        }
        9 => {
            let (read, partial) = {
                let s = SYSLOG.lock();
                (s.read, s.partial)
            };
            Ok(records(read).iter().map(|(_, l)| l.len()).sum::<usize>().saturating_sub(partial) as i64)
        }
        10 => Ok(SIZE as i64),
        _ => Err(-EINVAL),
    }
}

/// 端末のレベル (/proc/sys/kernel/printk の 1 つ目)
pub fn console_loglevel() -> u8 {
    SYSLOG.lock().console
}

/// ためてあるもの (古いものが消えて途中から始まるなら、最初の行の残りは捨てる)
pub fn text() -> String {
    let r = RING.lock();
    let bytes: Vec<u8> = if r.head <= SIZE {
        r.buf[..r.head].to_vec()
    } else {
        let at = r.head % SIZE;
        let mut v = r.buf[at..].to_vec();
        v.extend_from_slice(&r.buf[..at]);
        match v.iter().position(|&c| c == b'\n') {
            Some(i) => v.split_off(i + 1),
            None => v,
        }
    };
    String::from_utf8_lossy(&bytes).into_owned()
}
