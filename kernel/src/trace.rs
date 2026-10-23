// カーネルの中の出来事の記録 (/proc/ai/trace。aish の trace): システムコールの入りと出、スレッドの切りかえ、眠りと
// 起こし、割り込み、ディスクの読み書き、ページフォルト、exec と exit を、時刻 (起動からの ns) と CPU とスレッドつきで、
// 決まった数 (N) の輪に入れる (古いものから消える)。/proc/ai/ctl の trace on [NAME] で始め (NAME があれば、その名前で
// 始まるプロセスのシステムコールとフォルトだけ。切りかえ・眠り・起こし・割り込み・ディスクはみな)、trace off で止め、
// trace clear で空にする。止まっているあいだは、それぞれの場所で ON を 1 回見るだけ。
// 読むと 1 行 1 つの JSON: はじめの行が様子 (on、filter、events、dropped、names: tid → 名前)、あとは出来事 (t、c、p、k ...)。
// 読みはじめ (位置 0) で 1 度作り、続きはそれを切って返す (読むたびに作りなおさない)
use crate::cell::Bkl;
use crate::spinlock::SpinLock;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering::Relaxed};

/// 覚える出来事の数 (1 つ 24 バイト: 8192 で 192 KiB)
const N: usize = 8192;

pub static ON: AtomicBool = AtomicBool::new(false);

/// 出来事の種類
#[derive(Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum Kind {
    /// システムコールに入る (flag = 番号、x = 1 つめの引数)
    Sys = 1,
    /// 出る (flag = 番号、x = 答え)
    Ret = 2,
    /// この CPU がスレッドを選んだ (tid = 選んだもの、x = その前にこの CPU で走っていたもの)
    Switch = 3,
    /// 眠る (x = chan)
    Sleep = 4,
    /// 起こした (tid = 起こされたもの、x = chan。cpu と flag は起こしたほうの CPU と tid の下 16 ビット)
    Wake = 5,
    /// 割り込み (flag = 番号)
    Irq = 6,
    /// ディスクの読み書き (x = セクタ、flag = セクタの数 (65535 まで)。書くなら flag の上のビット... は使わず Kind で分ける)
    BlkRead = 7,
    BlkWrite = 8,
    /// 読み書きが終わった (x = かかった ns)
    BlkDone = 9,
    /// ページフォルト (x = 番地、flag = 1 なら書き)
    Fault = 10,
    /// exec した (名前は names に)
    Exec = 11,
    /// 終わった (x = 終わりの番号)
    Exit = 12,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct Ev {
    ns: u64,
    x: u64,
    tid: u32,
    cpu: u8,
    kind: u8,
    flag: u16,
}

const EMPTY: Ev = Ev { ns: 0, x: 0, tid: 0, cpu: 0, kind: 0, flag: 0 };

struct Ring {
    ev: [Ev; N],
    /// 次に書くところ
    head: usize,
    len: usize,
    /// いっぱいで消した数
    dropped: u64,
    /// 名前で絞る (空ならみな)
    filter: [u8; 16],
    /// CPU ごとに、前に走らせたスレッド (Switch の x)
    last: [u32; 8],
}

static RING: SpinLock<Ring> = SpinLock::new(Ring { ev: [EMPTY; N], head: 0, len: 0, dropped: 0, filter: [0; 16], last: [0; 8] });

/// tid → 名前 (exec と fork で覚える。終わったプロセスの名前も出せるように)。1024 まで
static NAMES: SpinLock<BTreeMap<u32, [u8; 16]>> = SpinLock::new(BTreeMap::new());

fn put(kind: Kind, tid: u32, x: u64, flag: u16) {
    let ns = crate::timer::uptime_ns();
    let mut r = RING.lock();
    let i = r.head;
    r.ev[i] = Ev { ns, x, tid, cpu: crate::smp::id() as u8, kind: kind as u8, flag };
    r.head = (i + 1) % N;
    if r.len < N {
        r.len += 1;
    } else {
        r.dropped += 1;
    }
}

/// いまのプロセスが、名前の絞りに合うか (絞りがなければみな)
fn wanted() -> bool {
    let r = RING.lock();
    let n = r.filter.iter().position(|&c| c == 0).unwrap_or(16);
    if n == 0 {
        return true;
    }
    let f = r.filter;
    drop(r);
    let p = crate::proc::current_leader();
    f[..n].split(|&c| c == b',').any(|x| !x.is_empty() && p.comm.starts_with(x))
}

// ---- それぞれの場所から (ON でなければすぐ戻る) ----

// システムコールごとに通るものは、ON を見るところだけ呼ぶ側に入れる (関数を呼ぶのも、システムコールの重さのうち)
#[inline]
pub fn sys_enter(nr: u64, a0: u64) {
    if ON.load(Relaxed) {
        sys_on(Kind::Sys, nr, a0);
    }
}

#[inline]
pub fn sys_exit(nr: u64, r: i64) {
    if ON.load(Relaxed) {
        sys_on(Kind::Ret, nr, r as u64);
    }
}

#[cold]
fn sys_on(kind: Kind, nr: u64, v: u64) {
    if wanted() {
        put(kind, crate::proc::current().pid, v, nr as u16);
    }
}

/// 大きなロックなしの道 (syscall::fast) で終わったもの: 入りと出をいっしょに
#[inline]
pub fn sys_fast(nr: u64, a0: u64, r: i64) {
    if ON.load(Relaxed) {
        sys_fast_on(nr, a0, r);
    }
}

#[cold]
fn sys_fast_on(nr: u64, a0: u64, r: i64) {
    if wanted() {
        let tid = crate::proc::current().pid;
        put(Kind::Sys, tid, a0, nr as u16);
        put(Kind::Ret, tid, r as u64, nr as u16);
    }
}

/// この CPU が tid を走らせる (名前をまだ知らなければ覚える: exec していないもの (fork した子など) の名前も出るように)
pub fn switch(tid: u32, comm: &[u8; 16]) {
    if ON.load(Relaxed) {
        {
            let mut n = NAMES.lock();
            if !n.contains_key(&tid) && n.len() < 1024 {
                n.insert(tid, *comm);
            }
        }
        let cpu = crate::smp::id();
        let prev = {
            let mut r = RING.lock();
            let p = r.last.get(cpu).copied().unwrap_or(0);
            if let Some(l) = r.last.get_mut(cpu) {
                *l = tid;
            }
            p
        };
        put(Kind::Switch, tid, prev as u64, 0);
    }
}

pub fn sleep(chan: usize) {
    if ON.load(Relaxed) {
        put(Kind::Sleep, crate::proc::current().pid, chan as u64, 0);
    }
}

/// tid を chan から起こした (起こしたのはいまのスレッド)
pub fn wake(tid: u32, chan: usize) {
    if ON.load(Relaxed) {
        put(Kind::Wake, tid, chan as u64, crate::proc::current_pid_or_zero() as u16);
    }
}

pub fn irq(n: u32) {
    if ON.load(Relaxed) {
        put(Kind::Irq, crate::proc::current_pid_or_zero(), 0, n as u16);
    }
}

pub fn blk(write: bool, sector: u64, sectors: u64) {
    if ON.load(Relaxed) {
        put(if write { Kind::BlkWrite } else { Kind::BlkRead }, crate::proc::current_pid_or_zero(), sector, sectors.min(65535) as u16);
    }
}

pub fn blk_done(t0: u64) {
    if ON.load(Relaxed) {
        put(Kind::BlkDone, crate::proc::current_pid_or_zero(), crate::timer::uptime_ns().saturating_sub(t0), 0);
    }
}

pub fn fault(va: usize, write: bool) {
    if ON.load(Relaxed) && wanted() {
        put(Kind::Fault, crate::proc::current().pid, va as u64, write as u16);
    }
}

/// exec と fork で: 名前を覚える (ON でなくても。ON にしたときに、もう走っているものの名前もわかるように)
pub fn name(tid: u32, comm: &[u8; 16]) {
    let mut n = NAMES.lock();
    if n.len() >= 1024 && !n.contains_key(&tid) {
        n.clear();
    }
    n.insert(tid, *comm);
    drop(n);
    if ON.load(Relaxed) {
        put(Kind::Exec, tid, 0, 0);
    }
}

pub fn exit(tid: u32, code: i64) {
    if ON.load(Relaxed) {
        put(Kind::Exit, tid, code as u64, 0);
    }
}

// ---- ctl と /proc/ai/trace ----

/// trace on [NAME] / off / clear
pub fn ctl(arg: &str) -> Result<(), i64> {
    let (cmd, name) = arg.split_once(' ').map_or((arg, ""), |(c, n)| (c, n.trim()));
    match cmd {
        "on" => {
            let mut r = RING.lock();
            r.filter = [0; 16];
            let n = name.len().min(15);
            r.filter[..n].copy_from_slice(&name.as_bytes()[..n]);
            drop(r);
            ON.store(true, Relaxed);
        }
        "off" => ON.store(false, Relaxed),
        "clear" => {
            let mut r = RING.lock();
            r.head = 0;
            r.len = 0;
            r.dropped = 0;
        }
        _ => return Err(crate::why::inval()),
    }
    Ok(())
}

/// いまの名前の絞り
fn filter_str() -> String {
    let r = RING.lock();
    let n = r.filter.iter().position(|&c| c == 0).unwrap_or(16);
    String::from(core::str::from_utf8(&r.filter[..n]).unwrap_or(""))
}

/// 読みはじめで作ったものと、作ったときの位置 (続きはこれを切って返す)
static CACHE: Bkl<Option<String>> = Bkl::new(None);

/// /proc/ai/trace の中身 (off から)。off が 0 なら作りなおす (大きなロックを持って呼ぶ)
pub fn read_at(off: usize, buf: &mut [u8]) -> usize {
    let cache = CACHE.get();
    if off == 0 || cache.is_none() {
        *cache = Some(json());
    }
    let s = cache.as_ref().unwrap();
    if off >= s.len() {
        return 0;
    }
    let n = (s.len() - off).min(buf.len());
    buf[..n].copy_from_slice(&s.as_bytes()[off..off + n]);
    n
}

pub fn json() -> String {
    use alloc::format;
    let mut evs: Vec<Ev> = Vec::with_capacity(N);
    let (dropped, len) = {
        let r = RING.lock();
        let start = (r.head + N - r.len) % N;
        for i in 0..r.len {
            evs.push(r.ev[(start + i) % N]);
        }
        (r.dropped, r.len)
    };
    let names: Vec<String> = {
        let n = NAMES.lock();
        n.iter().map(|(t, c)| format!("\"{}\":\"{}\"", t, crate::aiproc::esc(comm_str(c)))).collect()
    };
    let mut s = String::with_capacity(evs.len() * 72 + 1024);
    s.push_str(&format!(
        "{{\"on\":{},\"filter\":\"{}\",\"events\":{},\"dropped\":{},\"names\":{{{}}}}}\n",
        ON.load(Relaxed),
        crate::aiproc::esc(&filter_str()),
        len,
        dropped,
        names.join(",")
    ));
    for e in &evs {
        s.push_str(&format!("{{\"t\":{},\"c\":{},\"p\":{},", e.ns, e.cpu, e.tid));
        match e.kind {
            k if k == Kind::Sys as u8 => s.push_str(&format!("\"k\":\"sys\",\"n\":\"{}\",\"a\":\"{:#x}\"}}\n", sysname(e.flag), e.x)),
            k if k == Kind::Ret as u8 => s.push_str(&format!("\"k\":\"ret\",\"n\":\"{}\",\"r\":{}}}\n", sysname(e.flag), e.x as i64)),
            k if k == Kind::Switch as u8 => s.push_str(&format!("\"k\":\"switch\",\"from\":{}}}\n", e.x)),
            k if k == Kind::Sleep as u8 => s.push_str(&format!("\"k\":\"sleep\",\"chan\":\"{:#x}\"}}\n", e.x)),
            k if k == Kind::Wake as u8 => s.push_str(&format!("\"k\":\"wake\",\"chan\":\"{:#x}\",\"by\":{}}}\n", e.x, e.flag)),
            k if k == Kind::Irq as u8 => s.push_str(&format!("\"k\":\"irq\",\"irq\":{}}}\n", e.flag)),
            k if k == Kind::BlkRead as u8 => s.push_str(&format!("\"k\":\"blk\",\"op\":\"read\",\"sector\":{},\"n\":{}}}\n", e.x, e.flag)),
            k if k == Kind::BlkWrite as u8 => s.push_str(&format!("\"k\":\"blk\",\"op\":\"write\",\"sector\":{},\"n\":{}}}\n", e.x, e.flag)),
            k if k == Kind::BlkDone as u8 => s.push_str(&format!("\"k\":\"blkdone\",\"ns\":{}}}\n", e.x)),
            k if k == Kind::Fault as u8 => s.push_str(&format!("\"k\":\"fault\",\"va\":\"{:#x}\",\"w\":{}}}\n", e.x, e.flag != 0)),
            k if k == Kind::Exec as u8 => s.push_str("\"k\":\"exec\"}\n"),
            k if k == Kind::Exit as u8 => s.push_str(&format!("\"k\":\"exit\",\"code\":{}}}\n", e.x as i64)),
            _ => s.push_str("\"k\":\"?\"}\n"),
        }
    }
    s
}

fn comm_str(c: &[u8; 16]) -> &str {
    let n = c.iter().position(|&b| b == 0).unwrap_or(16);
    core::str::from_utf8(&c[..n]).unwrap_or("?")
}

fn sysname(nr: u16) -> String {
    crate::syscall::name(nr as u64).map_or_else(|| alloc::format!("{}", nr), |n| n.to_ascii_lowercase())
}
