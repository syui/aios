// しくじったわけ: ためて見せるほう (/proc/ai/why。aish の why)。わけを書くのは why.rs (core) で、ここはそれを輪にためて読ませる。
//   出口 (syscall::dispatch) で errno を返すとき、done がわけ (Proc の why) を輪 (N こ。古いものから消える) に入れる。
//   わけの errno と返す errno がちがえば (しくじってもよい確かめの残り)、わけはつけずに errno だけ。
//   よくあって、わけのいらないもの (EAGAIN、EINTR、ETIMEDOUT、待ちを割りこまれたもの) は入れない。
//   さがしもの (stat のなかまの ENOENT と ENOTDIR: PATH をさがすシェル、aish の色づけ、locale のファイル、端末かどうかの
//   ioctl の ENOTTY) は、別の輪 (PROBE) に。たくさんあって、ほんとうのしくじり (MAIN) を押しださないように
//   /proc/ai/why: 新しい順に 1 行 1 つの JSON (pid、syscall、errno、わけ。終わったプロセスのものも、輪にあれば)。
//   だれでも読めるが、wheel でない人には自分 (実 uid) のものだけ (ほかの人が開こうとしたパスは見せない)。
//   aish はコマンドが 0 でなく終わったとき、その pid のものを出す
// aiproc (/proc/ai) を切ったカーネルでは残さない
use crate::cell::Bkl;
use crate::spinlock::SpinLock;
use crate::why::{copy, Code, Note, CODES, NONE, PATH};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// 輪に入れるしくじりの数
const N: usize = 256;

/// LANDLOCK_ACCESS_FS_* と LANDLOCK_ACCESS_NET_* のビットの名前
fn landlock_names(access: u64, net: bool) -> String {
    const FS: [&str; 17] = ["execute", "write_file", "read_file", "read_dir", "remove_dir", "remove_file", "make_char", "make_dir", "make_reg", "make_sock", "make_fifo", "make_block", "make_sym", "refer", "truncate", "ioctl_dev", "resolve_unix"];
    const NET: [&str; 4] = ["bind_tcp", "connect_tcp", "bind_udp", "connect_send_udp"];
    let names: &[&str] = if net { &NET } else { &FS };
    let v: Vec<&str> = names.iter().enumerate().filter(|(i, _)| access & (1 << i) != 0).map(|(_, n)| *n).collect();
    if v.is_empty() { format!("{:#x}", access) } else { v.join(" ") }
}

#[derive(Clone, Copy)]
struct Ent {
    ns: u64,
    tgid: u32,
    tid: u32,
    /// 呼んだ人の実 uid (wheel でない人には自分のものだけ見せる)
    uid: u32,
    nr: u16,
    errno: u16,
    comm: [u8; 16],
    note: Note,
}

struct Ring {
    ev: [Ent; N],
    head: usize,
    len: usize,
    /// 入れた数 (起きてから)
    total: u64,
}

const EMPTY: Ent = Ent { ns: 0, tgid: 0, tid: 0, uid: 0, nr: 0, errno: 0, comm: [0; 16], note: NONE };
static MAIN: SpinLock<Ring> = SpinLock::new(Ring { ev: [EMPTY; N], head: 0, len: 0, total: 0 });
static PROBE: SpinLock<Ring> = SpinLock::new(Ring { ev: [EMPTY; N], head: 0, len: 0, total: 0 });

/// さがしもの (PROBE の輪に入れるもの)
fn probe(nr: u64, e: i64) -> bool {
    // newfstatat statx faccessat faccessat2 readlinkat の ENOENT ENOTDIR、ioctl の ENOTTY、readlinkat の EINVAL (リンクで
    // ない)、fcntl と close の EBADF (開いている fd をさがす: closefrom や、cp が fd を見てまわるもの)、getxattr の
    // ENODATA (その属性がない: cp や tar がファイルごとに見る)
    (matches!(nr, 79 | 291 | 48 | 439 | 78) && matches!(-e, 2 | 20)) || (nr == 29 && -e == 25) || (nr == 78 && -e == 22) || (matches!(nr, 25 | 57) && -e == 9) || (matches!(nr, 8..=10) && -e == 61)
}

/// システムコールの出口で errno (負の数) を返すとき (syscall::dispatch。a はその引数)
#[inline]
pub fn done(nr: u64, e: i64, a: &[u64]) {
    // EAGAIN、EINTR、ETIMEDOUT と、割りこまれてやりなおすもの (ERESTARTSYS など、512 から)
    if matches!(-e, 11 | 4 | 110) || -e >= 512 {
        return;
    }
    done_slow(nr, e, a);
}

#[cold]
fn done_slow(nr: u64, e: i64, a: &[u64]) {
    if crate::parts::off(crate::parts::Part::Aiproc) {
        return;
    }
    let Some(p) = crate::proc::try_current() else { return };
    let mut note = p.why;
    // ファイアウォールのわけ (errno 0) は、返す errno がなんでも
    if note.code != Code::None && note.errno as i64 != -e && !(note.code == Code::Firewall && note.errno == 0) {
        note = NONE;
    }
    if note.code == Code::None || note.errno == 0 {
        note.errno = (-e) as u16;
    }
    // わけを書いたところがパスを知らなければ (許可を見るところなど)、プログラムがたのんだパスを
    if note.plen == 0
        && (note.nlen == 0 || note.code == Code::Where)
        && let Some(i) = crate::syscall::path_arg(nr)
        && let Some(&va) = a.get(i)
    {
        let asked = crate::syscall::user_str(va as usize, PATH);
        if asked != "?" {
            note.plen = copy(&mut note.path, &asked);
            note.asked = true;
        }
    }
    let ent = Ent { ns: crate::timer::uptime_ns(), tgid: p.tgid as u32, tid: p.pid as u32, uid: p.cred.uid, nr: nr as u16, errno: (-e) as u16, comm: p.comm, note };
    push(probe(nr, e), ent);
}

/// 大きなロックなしの stat (statc.rs) が「ない」と答えた: ふつうの道の missing_cached と同じわけを残す (取るのは輪のロック
/// だけ)。path は畳んだパス (先頭 / なし)
#[cold]
pub fn fast_missing(p: &crate::proc::Proc, nr: u64, path: &str) {
    if crate::parts::off(crate::parts::Part::Aiproc) {
        return;
    }
    let mut note = Note { code: Code::MissingCached, errno: 2, ..NONE };
    note.plen = copy(&mut note.path, path);
    let ent = Ent { ns: crate::timer::uptime_ns(), tgid: p.tgid as u32, tid: p.pid as u32, uid: p.cred.uid, nr: nr as u16, errno: 2, comm: p.comm, note };
    push(probe(nr, -2), ent);
}

/// 大きなロックなしの stat や open が、ディレクトリ dir (畳んだパス) で name を引いて、なかった: ふつうの道が
/// たどって見つけなかったとき (at) と同じわけ
pub fn fast_missing_at(p: &crate::proc::Proc, nr: u64, dir: &str, name: &str) {
    if crate::parts::off(crate::parts::Part::Aiproc) {
        return;
    }
    let mut note = Note { code: Code::Missing, errno: 2, ..NONE };
    note.plen = copy(&mut note.path, dir);
    note.nlen = copy(&mut note.name, name);
    let ent = Ent { ns: crate::timer::uptime_ns(), tgid: p.tgid as u32, tid: p.pid as u32, uid: p.cred.uid, nr: nr as u16, errno: 2, comm: p.comm, note };
    push(probe(nr, -2), ent);
}

fn push(probe: bool, ent: Ent) {
    let mut r = if probe { PROBE.lock() } else { MAIN.lock() };
    let h = r.head;
    r.ev[h] = ent;
    r.head = (h + 1) % N;
    r.len = (r.len + 1).min(N);
    r.total += 1;
}

/// errno の名前 (よく出るもの。ほかは数のまま)
pub fn errno_name(e: u16) -> Option<&'static str> {
    const T: [(u16, &str); 72] = [
        (1, "EPERM"), (2, "ENOENT"), (3, "ESRCH"), (4, "EINTR"), (5, "EIO"), (6, "ENXIO"), (7, "E2BIG"), (8, "ENOEXEC"), (9, "EBADF"), (10, "ECHILD"),
        (11, "EAGAIN"), (12, "ENOMEM"), (13, "EACCES"), (14, "EFAULT"), (15, "ENOTBLK"), (16, "EBUSY"), (17, "EEXIST"), (18, "EXDEV"), (19, "ENODEV"), (20, "ENOTDIR"),
        (21, "EISDIR"), (22, "EINVAL"), (23, "ENFILE"), (24, "EMFILE"), (25, "ENOTTY"), (26, "ETXTBSY"), (27, "EFBIG"), (28, "ENOSPC"), (29, "ESPIPE"), (30, "EROFS"),
        (31, "EMLINK"), (32, "EPIPE"), (33, "EDOM"), (34, "ERANGE"), (35, "EDEADLK"), (36, "ENAMETOOLONG"), (37, "ENOLCK"), (38, "ENOSYS"), (39, "ENOTEMPTY"), (40, "ELOOP"),
        (42, "ENOMSG"), (43, "EIDRM"), (61, "ENODATA"), (62, "ETIME"), (71, "EPROTO"), (74, "EBADMSG"), (75, "EOVERFLOW"), (84, "EILSEQ"), (88, "ENOTSOCK"), (89, "EDESTADDRREQ"),
        (90, "EMSGSIZE"), (91, "EPROTOTYPE"), (92, "ENOPROTOOPT"), (93, "EPROTONOSUPPORT"), (94, "ESOCKTNOSUPPORT"), (95, "EOPNOTSUPP"), (97, "EAFNOSUPPORT"), (98, "EADDRINUSE"), (99, "EADDRNOTAVAIL"), (101, "ENETUNREACH"),
        (103, "ECONNABORTED"), (104, "ECONNRESET"), (105, "ENOBUFS"), (106, "EISCONN"), (107, "ENOTCONN"), (110, "ETIMEDOUT"), (111, "ECONNREFUSED"), (113, "EHOSTUNREACH"), (114, "EALREADY"), (115, "EINPROGRESS"),
        (116, "ESTALE"), (125, "ECANCELED"),
    ];
    T.iter().find(|t| t.0 == e).map(|t| t.1)
}

fn rwx(bits: u32) -> [u8; 3] {
    [if bits & 4 != 0 { b'r' } else { b'-' }, if bits & 2 != 0 { b'w' } else { b'-' }, if bits & 1 != 0 { b'x' } else { b'-' }]
}

/// JSON の文字列の中身に (", \、制御文字を逃がす)
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

/// わけを人と AI が読む 1 文に
fn sentence(n: &Note, syscall: &str) -> String {
    let path = core::str::from_utf8(&n.path[..n.plen as usize]).unwrap_or("?");
    let name = core::str::from_utf8(&n.name[..n.nlen as usize]).unwrap_or("?");
    // たのんだパスは、そのまま (相対のこともある)。たどったところは先頭 / なしなので / をつける
    let p = if n.asked { String::from(path) } else if path.is_empty() { String::from("/") } else if path.starts_with('/') { String::from(path) } else { format!("/{}", path) };
    let joined = if name.is_empty() { p.clone() } else if p.ends_with('/') { format!("{}{}", p, name) } else { format!("{}/{}", p, name) };
    match n.code {
        Code::Perm => {
            let what = match n.want {
                1 => "search (x)",
                2 => "write (w)",
                4 => "read (r)",
                6 => "read and write (rw)",
                3 => "making or removing a name, which needs write and search (wx),",
                _ => "access",
            };
            let on = if n.plen == 0 {
                String::from("the file")
            } else if n.asked && n.want == 3 {
                format!("the directory of {}", joined)
            } else {
                joined.clone()
            };
            let on = on.as_str();
            let class = match n.num {
                0 => ("its owner", (n.mode >> 6) & 7),
                1 => ("its group", (n.mode >> 3) & 7),
                2 => ("others", n.mode & 7),
                _ => return format!("even root needs an x bit somewhere to execute {}: mode {:04o}", on, n.mode & 0o7777),
            };
            let has = rwx(class.1);
            format!(
                "{} on {} is not allowed for uid {} gid {}: mode {:04o}, owner {}:{}, so the caller counts as {}, which have {}",
                what,
                on,
                n.cuid,
                n.cgid,
                n.mode & 0o7777,
                n.uid,
                n.gid,
                class.0,
                core::str::from_utf8(&has).unwrap_or("?")
            )
        }
        Code::Missing => format!("{} exists but has no entry named \"{}\"", p, name),
        Code::MissingCached => format!("{} does not exist (remembered from an earlier lookup)", joined),
        Code::NotDir => format!("{} is not a directory, so the path cannot go through it", p),
        Code::Loop => format!("too many symbolic links (more than 40) or a link where links are not allowed, at {}", joined),
        Code::NameTooLong => format!("the name \"{}...\" is longer than 255 bytes", name.get(..16).unwrap_or(name)),
        Code::Seccomp => format!("a seccomp filter of this process returns this errno for {}", syscall),
        Code::Landlock if n.want == 2 => format!("a landlock ruleset of this process does not allow {}", path),
        Code::Landlock if n.want != 0 => format!("a landlock ruleset of this process does not allow {} on {}", landlock_names(n.num, true), path),
        Code::Landlock => format!("a landlock ruleset of this process does not allow {} under {}", landlock_names(n.num, false), p),
        Code::Injected => String::from("injected on purpose by the fail rule in /proc/ai/ctl"),
        Code::PartOff => format!("{} belongs to a kernel part that is turned off (aios.off= or ctl part)", syscall),
        Code::NoSys => format!("aios does not have {} (number {})", syscall, n.num),
        Code::NotLink => format!("{} is not a symbolic link", p),
        Code::BadFd => format!("fd {} is not open in this process", n.num),
        Code::Where if path.is_empty() => format!("refused at {}:{} (the check there says why)", name, n.num),
        Code::Where => format!("refused at {}:{} (the check there says why), for {}", name, n.num, p),
        Code::Firewall => format!("the firewall dropped the packet: {}", path),
        Code::None => String::new(),
    }
}

/// 読みはじめで作ったもの (だれ向けか、中身)。/proc/ai/why は大きく (512 行ほど) いくつかに分けて読まれるので、続きは
/// これを切って返す (読むたびに作りなおすと、そのあいだに増えたものと ago_ms で行がずれる)
static CACHE: Bkl<Option<(Option<u32>, String)>> = Bkl::new(None);

/// /proc/ai/why の中身 (off から)。off が 0 なら作りなおす (大きなロックを持って呼ぶ)
pub fn read_at(off: usize, buf: &mut [u8], uid: Option<u32>) -> usize {
    let cache = CACHE.get();
    if off == 0 || cache.as_ref().is_none_or(|c| c.0 != uid) {
        *cache = Some((uid, text(uid)));
    }
    let s = &cache.as_ref().unwrap().1;
    if off >= s.len() {
        return 0;
    }
    let n = (s.len() - off).min(buf.len());
    buf[..n].copy_from_slice(&s.as_bytes()[off..off + n]);
    n
}

/// /proc/ai/why (uid があればその人のものだけ): 新しい順に 1 行 1 つの JSON。はじめの行は様子
pub fn text(uid: Option<u32>) -> String {
    let now = crate::timer::uptime_ns();
    let mut ents: Vec<(bool, Ent)> = Vec::new();
    let mut total = [0u64; 2];
    for (k, ring) in [&MAIN, &PROBE].into_iter().enumerate() {
        let r = ring.lock();
        for i in 0..r.len {
            let e = r.ev[(r.head + N - 1 - i) % N];
            if uid.is_none_or(|u| u == e.uid) {
                ents.push((k == 1, e));
            }
        }
        total[k] = r.total;
    }
    // 2 つの輪をあわせて新しい順
    ents.sort_by(|a, b| b.1.ns.cmp(&a.1.ns));
    let mut s = format!("{{\"why\":\"aios\",\"kept\":{},\"total\":{},\"probes\":{},\"shown\":{}}}\n", N, total[0], total[1], ents.len());
    for (probe, e) in &ents {
        let sys = crate::syscall::name(e.nr as u64).map_or(format!("sys{}", e.nr), |x| x.to_ascii_lowercase());
        let comm = core::str::from_utf8(&e.comm[..e.comm.iter().position(|&b| b == 0).unwrap_or(16)]).unwrap_or("?");
        let en = errno_name(e.errno).map_or(format!("{}", e.errno), String::from);
        s.push_str(&format!("{{\"ago_ms\":{},\"pid\":{},\"tid\":{},\"comm\":\"{}\",\"syscall\":\"{}\",\"errno\":\"{}\"", (now.saturating_sub(e.ns)) / 1_000_000, e.tgid, e.tid, esc(comm), sys, en));
        if *probe {
            s.push_str(",\"probe\":true");
        }
        let n = &e.note;
        // わけのないものも、たのんだパスがあれば (asked)
        if n.code == Code::None && n.plen > 0 {
            s.push_str(&format!(",\"path\":\"{}\",\"asked\":true", esc(core::str::from_utf8(&n.path[..n.plen as usize]).unwrap_or("?"))));
        }
        if n.code != Code::None {
            s.push_str(&format!(",\"code\":\"{}\"", CODES[n.code as usize]));
            if n.plen > 0 {
                s.push_str(&format!(",\"path\":\"{}\"", esc(core::str::from_utf8(&n.path[..n.plen as usize]).unwrap_or("?"))));
            }
            if n.code == Code::Where {
                // EINVAL を返したところ (src: カーネルのソースのファイルと行)
                s.push_str(&format!(",\"src\":\"{}:{}\"", esc(core::str::from_utf8(&n.name[..n.nlen as usize]).unwrap_or("?")), n.num));
            } else if n.nlen > 0 {
                s.push_str(&format!(",\"name\":\"{}\"", esc(core::str::from_utf8(&n.name[..n.nlen as usize]).unwrap_or("?"))));
            }
            if n.asked {
                s.push_str(",\"asked\":true");
            }
            if n.code == Code::Perm {
                s.push_str(&format!(",\"want\":\"{}\",\"mode\":\"{:04o}\",\"owner\":\"{}:{}\",\"cred\":\"{}:{}\"", core::str::from_utf8(&rwx(n.want as u32)).unwrap_or("?"), n.mode & 0o7777, n.uid, n.gid, n.cuid, n.cgid));
            }
            s.push_str(&format!(",\"why\":\"{}\"", esc(&sentence(n, &sys))));
        }
        s.push_str("}\n");
    }
    s
}
