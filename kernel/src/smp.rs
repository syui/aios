// 複数 CPU
//
// カーネルは大きなロック (BKL) 1 つで守る: カーネルの中を走れるのはいつも 1 つの CPU だけで、
// ユーザーのプログラムはすべての CPU で同時に動く (Linux 2.0 のころと同じ)。
//   EL0 から例外で入ったらロックを取り、EL0 へ戻る直前に放す (trap.rs、forkret)。
//   スケジューラはロックを持ったまま回し、することがなければ放して割り込みを待つ。
// これでいままでの 1 CPU 向けのコード (static mut、RefCell) はそのまま使える。
// ロックなしで通すもの: 自分のことを読むだけのシステムコール (syscall::fast) と、自分だけの無名の
// 領域のページフォルト (vm.rs fast_fault。表を変える側は mutating() でそれを止めて待つ)、コンソールの
// 送信の割り込み (uart.rs intr_fast。輪から次の文字を出すだけ)。
//
// 2 つめからの CPU は DTB の cpu ノードの enable-method で起こす:
//   psci        PSCI の CPU_ON (QEMU virt、UEFI)
//   spin-table  cpu-release-addr に入口を書いて sev (ラズパイのファームウェア)
// 起きた CPU は boot.rs と同じページ表で MMU を入れ、自分のスタックでスケジューラに入る。
// TPIDR_EL1 に CPU の番号を入れておく。
use crate::memlayout::{p2v, v2p};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub const MAXCPU: usize = 8;
const STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
struct Stacks([[u8; STACK_SIZE]; MAXCPU]);

#[unsafe(no_mangle)]
static secondary_stacks: crate::cell::Raw<Stacks> = crate::cell::Raw::new(Stacks([[0; STACK_SIZE]; MAXCPU]));

/// 動いている CPU の数
static ONLINE: AtomicUsize = AtomicUsize::new(1);
/// CPU の番号ごとの MPIDR の Aff0 (CPU 間の割り込みの宛先)
static TARGET: [AtomicUsize; MAXCPU] = [const { AtomicUsize::new(0) }; MAXCPU];

/// 仕事がなくて眠っている CPU (ビット)
static IDLE: AtomicUsize = AtomicUsize::new(0);

/// この CPU は眠る / 起きた (スケジューラが、大きなロックを持ったまま呼ぶ)
pub fn set_idle(on: bool) {
    let bit = 1 << id();
    if on {
        IDLE.fetch_or(bit, Ordering::AcqRel);
    } else {
        IDLE.fetch_and(!bit, Ordering::AcqRel);
    }
}

/// 眠っている CPU を起こす (動けるプロセスができた)。sev ではなく割り込みで:
/// Mac の Hypervisor.framework (HVF) では wfe が眠らず sev も届かないので、ひまな CPU は wfi で眠っている
/// 眠っている CPU を 1 つ起こす。起こしたものがあれば true
pub fn wake_idle() -> bool {
    wake_idle_n(1)
}

/// 眠っている CPU を n まで起こす (動けるようになったものの数だけ)。起こしたものがあれば true。
/// みな起こすと、起きた CPU がいっせいに大きなロックを取りあい、1 つずつ表を見て「仕事がない」と眠りなおす
/// あいだ、起こしたものが待たされる (QEMU の TCG で、起こしてから走るまでが 2.4 ms だった)
pub fn wake_idle_n(n: usize) -> bool {
    let idle = IDLE.load(Ordering::Acquire) & !(1 << id());
    let mut sent = 0;
    for cpu in 0..online() {
        if sent >= n {
            break;
        }
        if idle & (1 << cpu) != 0 {
            crate::irq::send_ipi(TARGET[cpu].load(Ordering::Relaxed));
            sent += 1;
        }
    }
    sent > 0
}

/// ほかの CPU に、EL0 から戻ってくるように知らせる (殺された、シグナルが来た)
pub fn kick(cpu: usize) {
    if cpu != id() && cpu < online() {
        crate::irq::send_ipi(TARGET[cpu].load(Ordering::Relaxed));
    }
}

/// いまの CPU の番号 (0 から)
pub fn id() -> usize {
    let v: usize;
    unsafe { core::arch::asm!("mrs {}, tpidr_el1", out(reg) v) };
    v
}

pub fn online() -> usize {
    ONLINE.load(Ordering::Acquire)
}

// ---- 大きなロック ----

/// 持っている CPU の番号 + 1 (0 なら空き)
static BKL: AtomicUsize = AtomicUsize::new(0);
/// チケット: 次に配る番号と、いま持てる番号。来た順に持つ (早い者勝ちだと、放したばかりの CPU が
/// すぐまた取り、ほかの CPU (起こされた音のスレッドなど) がいつまでも取れないことがある)
static NEXT: AtomicUsize = AtomicUsize::new(0);
static SERVING: AtomicUsize = AtomicUsize::new(0);

/// 取る。取った時刻 (ns) を返す (trap.rs が、持っていた時間を数えるのに時計を読みなおさないため)。測らない回は 0。
/// 時計を読むのはくじにあたったときだけ (SAMPLE。待ったものも)
pub fn lock() -> u64 {
    let me = id() + 1;
    debug_assert!(BKL.load(Ordering::Relaxed) != me, "BKL: cpu{} locks twice", me - 1);
    let c = &STATS.cpu[me - 1];
    let timed = draw(c);
    let ticket = NEXT.fetch_add(1, Ordering::Relaxed);
    let t0 = if SERVING.load(Ordering::Acquire) == ticket {
        None
    } else {
        let t0 = if timed { crate::timer::uptime_ns() } else { 0 };
        // 順番が来るまで wfe で休む (放す側が sev で起こす)。回りつづけると、QEMU (TCG) では待っている CPU が
        // 本当の CPU を取りあって、ロックを持っている CPU まで遅くなる
        while SERVING.load(Ordering::Acquire) != ticket {
            unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
        }
        Some(t0)
    };
    BKL.store(me, Ordering::Relaxed);
    add(&c.locks, 1);
    // くじにあたったものだけ、待った時間と持っていた時間を測って SAMPLE 倍。待ったものをいつも測ると、TCG では
    // 取ったあとに時計を読むのが重く (いくつもの CPU が同時に読むと 1 回 1 µs ほど: timer の粗い時計)、ロックを持つ
    // 時間が測るために伸びていた
    let (now, w) = match t0 {
        _ if !timed => (0, 0),
        Some(t0) => {
            let now = crate::timer::uptime_ns();
            add(&c.wait, (now - t0) * SAMPLE);
            (now, SAMPLE)
        }
        None => (crate::timer::uptime_ns(), SAMPLE),
    };
    c.since.store(now, Ordering::Relaxed);
    c.weight.store(w, Ordering::Relaxed);
    now
}

/// 空いていれば取る (待たない)
fn try_lock() -> bool {
    let me = id() + 1;
    let s = SERVING.load(Ordering::Acquire);
    if NEXT.compare_exchange(s, s + 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
        return false;
    }
    BKL.store(me, Ordering::Relaxed);
    let c = &STATS.cpu[me - 1];
    add(&c.locks, 1);
    let w = if draw(c) { SAMPLE } else { 0 };
    c.since.store(if w != 0 { crate::timer::uptime_ns() } else { 0 }, Ordering::Relaxed);
    c.weight.store(w, Ordering::Relaxed);
    true
}

/// now (いまの時刻) までを持っていた時間に足して放す
fn release(now: u64) {
    let c = &STATS.cpu[id()];
    let w = c.weight.load(Ordering::Relaxed);
    if w != 0 {
        add(&c.hold, now.saturating_sub(c.since.load(Ordering::Relaxed)) * w);
    }
    BKL.store(0, Ordering::Relaxed);
    // SERVING を書くのは持っているものだけなので、読んで書くだけでいい
    SERVING.store(SERVING.load(Ordering::Relaxed) + 1, Ordering::Release);
    // wfe で待っている CPU を起こす
    unsafe { core::arch::asm!("dsb ish", "sev", options(nostack)) };
}

/// 放す。そのまえとあとに、大きなロックなしの道が頼んでいった「起こす」を片づける (defer_wake)。
/// あとにも見るのは、放すまぎわに頼まれたもの (頼んだほうは取れなかったので、こちらに任せている) のため
pub fn unlock() {
    unlock_at(timed_now());
}

/// いまの時刻。この CPU のいまの hold も、いまの例外の区間 (SEG) も測っていなければ、時計を読まずに 0
pub fn timed_now() -> u64 {
    let i = id();
    if STATS.cpu[i].weight.load(Ordering::Relaxed) != 0 || SEG[i].2.load(Ordering::Relaxed) != 0 {
        crate::timer::uptime_ns()
    } else {
        0
    }
}

/// unlock と同じ。now は読んだばかりの時刻 (trap.rs: 時計をもう一度読まない)
pub fn unlock_at(now: u64) {
    let mut now = Some(now);
    loop {
        let woke = drain_wakes();
        let t = match now.take() {
            Some(t) if !woke => t,
            _ => timed_now(),
        };
        release(t);
        // defer_wake と組 (あちらは PENDING_N を書いてから、こちらは放してから SeqCst の fence。どちらかが相手のものを見る)
        core::sync::atomic::fence(Ordering::SeqCst);
        if PENDING_N.load(Ordering::Relaxed) == 0 || !try_lock() {
            return;
        }
    }
}

// ---- 大きなロックなしの道からの「起こす」 ----
// パイプの読み書きの速い道 (file::Pipe::fast_rw) は、眠っている人や待っている人がいても進めて、起こすのは
// ここに頼む。大きなロックが空いていればその場で取って起こし、だれかが持っていれば、その CPU が放すときに起こす

const NPENDING: usize = 64;
static PENDING: crate::spinlock::SpinLock<([usize; NPENDING], usize)> = crate::spinlock::SpinLock::new(([0; NPENDING], 0));
/// PENDING の数 (PENDING を持って書く)。放すたびに見るので、PENDING を取らずに読めるように
static PENDING_N: AtomicUsize = AtomicUsize::new(0);

/// key (パイプ) で待っているものを起こしてもらう。大きなロックを持たずに呼ぶ
pub fn defer_wake(key: usize) {
    let queued = {
        let mut p = PENDING.lock();
        let n = p.1;
        if p.0[..n].contains(&key) {
            true
        } else if n < NPENDING {
            p.0[n] = key;
            p.1 = n + 1;
            PENDING_N.store(n + 1, Ordering::Relaxed);
            true
        } else {
            false
        }
    };
    if !queued {
        // いっぱい: 待って取って、自分で
        lock();
        crate::file::wake_key(key);
        unlock();
        return;
    }
    core::sync::atomic::fence(Ordering::SeqCst);
    if try_lock() {
        unlock();
    }
}

/// 頼まれていたものを起こす (大きなロックを持って)。起こしたら true
fn drain_wakes() -> bool {
    let mut woke = false;
    // 見おとしても、放したあとに unlock_at が見なおす
    while PENDING_N.load(Ordering::Relaxed) != 0 {
        let (keys, n) = {
            let mut p = PENDING.lock();
            let n = p.1;
            p.1 = 0;
            PENDING_N.store(0, Ordering::Relaxed);
            (p.0, n)
        };
        for &k in &keys[..n] {
            crate::file::wake_key(k);
        }
        woke |= n > 0;
    }
    woke
}

// ---- 大きなロックの統計 (/proc/bkl。ロックを細かくするとき、どこから分けるかを決めるため) ----

const NSYS: usize = 512;

/// ロックを待った時間と持っていた時間は、SAMPLE 回に 1 回ほどだけ時計で測り、SAMPLE 倍して足す。QEMU の TCG では時計 (cntvct) を読むのが 150〜240 ns と、システムコールのほかのところぜんぶ (1 µs ほど)
/// にくらべて重く、出入りで 2 回読むと 4 割になっていた。Linux もプロセスの時間を出入りのたびには測らず、時計の割り込みの
/// たびに見る (tick で数える)。どれを測るかはくじ (xorshift) で選ぶ: 決まった順だと、交互に呼ぶもの (read と write など)
/// の片方だけが測られる
pub const SAMPLE: u64 = 8;

struct CpuStat {
    /// ロックを待っていた時間 (ns)
    wait: AtomicU64,
    /// ロックを持っていた時間 (ns。SAMPLE のぶん、見つもり)
    hold: AtomicU64,
    locks: AtomicU64,
    /// いま持ちはじめた時刻 (測らない回は 0)
    since: AtomicU64,
    /// いま持っている分を測っているなら、その重み (1 か SAMPLE)。0 は測っていない
    weight: AtomicU64,
    /// くじ (xorshift の種)
    dice: AtomicU64,
}

/// くじを引く: SAMPLE 回に 1 回ほどあたる (上の 3 ビットが 0)
/// この CPU のくじ (SAMPLE 回に 1 回ほど true): ほかの統計も、時計を読む回をこれで選ぶ
pub fn draw_here() -> bool {
    draw(&STATS.cpu[id()])
}

fn draw(c: &CpuStat) -> bool {
    let mut x = c.dice.load(Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    c.dice.store(x, Ordering::Relaxed);
    x >> (64 - SAMPLE.trailing_zeros()) == 0
}

struct Stats {
    cpu: [CpuStat; MAXCPU],
    /// システムコールの番号ごとの (回数, ロックを持っていた時間)。途中で眠ったものは、眠るまでと起きてからの分
    sys: [(AtomicU64, AtomicU64); NSYS],
    fault: (AtomicU64, AtomicU64),
    irq: (AtomicU64, AtomicU64),
    /// カーネルのスレッド (proc::kthread) が 1 回まわるあいだ
    kthread: (AtomicU64, AtomicU64),
    /// syscall::FD_SYS のシステムコールの、fd の開いたものの種類 (file::CLASSES) ごとの (回数, 時間)。sys には数えない
    fd: [[(AtomicU64, AtomicU64); NCLASS]; NFD],
    /// 途中で眠ったので数えなかったもの
    slept: AtomicU64,
    /// 数えはじめた時刻 (書くと 0 からやりなおす)
    start: AtomicU64,
}

const Z: AtomicU64 = AtomicU64::new(0);
/// STATS の数に足す。書くのは大きなロックを持っているものだけ (stats_reset も) なので、読んで書くだけでいい
/// (fetch_add の ldxr/stxr の輪より軽い)
fn add(a: &AtomicU64, v: u64) {
    a.store(a.load(Ordering::Relaxed) + v, Ordering::Relaxed);
}

static STATS: Stats = Stats {
    cpu: [const { CpuStat { wait: Z, hold: Z, locks: Z, since: Z, weight: Z, dice: AtomicU64::new(0x9e37_79b9_7f4a_7c15) } }; MAXCPU],
    sys: [const { (Z, Z) }; NSYS],
    fault: (Z, Z),
    irq: (Z, Z),
    kthread: (Z, Z),
    fd: [const { [const { (Z, Z) }; NCLASS] }; NFD],
    slept: Z,
    start: Z,
};

/// スケジューラでほかのプロセスへ切りかえた回数 (ロックを持ったまま眠ったかを見る)
pub static SWITCHES: AtomicU64 = AtomicU64::new(0);

/// 起こされたものにゆずらせた数 (proc.rs preempt)
pub static PREEMPTS: AtomicUsize = AtomicUsize::new(0);
/// 大きなロックなしで片づけたページフォルト (vm.rs fast_fault)
pub static FAST_FAULTS: AtomicU64 = AtomicU64::new(0);
/// ページフォールトの数 (起きてからずっと。/proc/vmstat の pgfault)
pub static FAULTS: AtomicU64 = AtomicU64::new(0);

/// 例外 1 つぶんの、ロックを持っていた時間を数える (trap.rs)。sys はシステムコールの番号
pub enum Cause {
    Sys(u64),
    /// syscall::FD_SYS の何番めか、fd の開いたものの種類 (file::CLASSES の番号)
    SysFd(usize, usize),
    Fault,
    Irq,
    /// カーネルのスレッドの 1 回 (vfs::flusher)
    Kthread,
}

const NFD: usize = crate::syscall::FD_SYS.len();
const NCLASS: usize = crate::file::CLASSES.len();

impl Cause {
    /// システムコール nr (1 つめの引数 a0)。fd を取るもの (syscall::FD_SYS) は、その fd の開いたものの種類で分ける
    /// (大きなロックを持って呼ぶ: fd の表を見る)
    pub fn sys(nr: u64, a0: u64) -> Cause {
        match crate::syscall::FD_SYS.iter().position(|&n| n == nr) {
            Some(j) => Cause::SysFd(j, crate::file::class_of(a0)),
            None => Cause::Sys(nr),
        }
    }
}

fn slot_of(idx: u64) -> Option<&'static (AtomicU64, AtomicU64)> {
    match idx as usize {
        n if n < NSYS => Some(&STATS.sys[n]),
        n if n == NSYS => Some(&STATS.fault),
        n if n == NSYS + 1 => Some(&STATS.irq),
        n if n < NSYS + 2 + NFD * NCLASS => Some(&STATS.fd[(n - NSYS - 2) / NCLASS][(n - NSYS - 2) % NCLASS]),
        n if n == NSYS + 2 + NFD * NCLASS => Some(&STATS.kthread),
        _ => None,
    }
}

fn index_of(cause: &Cause) -> u64 {
    match *cause {
        Cause::Sys(n) if (n as usize) < NSYS => n,
        Cause::Sys(_) => u64::MAX,
        Cause::SysFd(j, k) => (NSYS + 2 + j * NCLASS + k) as u64,
        Cause::Fault => NSYS as u64,
        Cause::Irq => NSYS as u64 + 1,
        Cause::Kthread => (NSYS + 2 + NFD * NCLASS) as u64,
    }
}

/// 持っていた時間の長いもの (名前, 回数, 時間) の多い順。fd のシステムコールは「read pipe」のように種類ごと
/// (途中で眠ったものは、眠るまでと起きてからの分)
fn hold_rows() -> alloc::vec::Vec<(alloc::string::String, u64, u64)> {
    use alloc::format;
    let mut rows = alloc::vec::Vec::new();
    let sys_name = |i: u64| crate::syscall::name(i).map_or(format!("sys{}", i), |x| x.to_ascii_lowercase());
    for (i, (n, t)) in STATS.sys.iter().enumerate() {
        let n = n.load(Ordering::Relaxed);
        if n > 0 {
            rows.push((sys_name(i as u64), n, t.load(Ordering::Relaxed)));
        }
    }
    for (j, by) in STATS.fd.iter().enumerate() {
        for (k, (n, t)) in by.iter().enumerate() {
            let n = n.load(Ordering::Relaxed);
            if n > 0 {
                rows.push((format!("{} {}", sys_name(crate::syscall::FD_SYS[j]), crate::file::CLASSES[k]), n, t.load(Ordering::Relaxed)));
            }
        }
    }
    for (name, slot) in [("(page fault)", &STATS.fault), ("(irq)", &STATS.irq), ("(kernel thread)", &STATS.kthread)] {
        let n = slot.0.load(Ordering::Relaxed);
        if n > 0 {
            rows.push((name.into(), n, slot.1.load(Ordering::Relaxed)));
        }
    }
    rows.sort_by_key(|r| core::cmp::Reverse(r.2));
    rows
}

/// CPU ごとの、いまロックを持って動いている区間 (始まった時刻, 原因の番号, 重み)。途中で眠るシステムコールも、
/// 眠るまでと起きてからの分を数えるため (sched が区切る)。重みはロックを取ったときのくじ (SAMPLE)、0 なら時間は測らない
static SEG: [(AtomicU64, AtomicU64, AtomicU64); MAXCPU] = [const { (AtomicU64::new(0), AtomicU64::new(u64::MAX), AtomicU64::new(0)) }; MAXCPU];

/// 例外の始まり (ロックを取ったあと。t0 は取った時刻、測らない回は 0)
pub fn seg_begin(cause: &Cause, t0: u64) {
    let i = id();
    let s = &SEG[i];
    s.0.store(t0, Ordering::Relaxed);
    s.1.store(index_of(cause), Ordering::Relaxed);
    s.2.store(STATS.cpu[i].weight.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// 眠る (ほかへ切りかえる) まえ: ここまでの分を足す。返すのは (原因の番号, 重み) (起きたら seg_resume に)
pub fn seg_pause() -> (u64, u64) {
    let s = &SEG[id()];
    let idx = s.1.swap(u64::MAX, Ordering::Relaxed);
    let w = s.2.swap(0, Ordering::Relaxed);
    if w != 0
        && let Some(slot) = slot_of(idx)
    {
        add(&slot.1, crate::timer::uptime_ns().saturating_sub(s.0.load(Ordering::Relaxed)) * w);
    }
    (idx, w)
}

/// 起きた (切りかえから戻った): 続きの区間を始める
pub fn seg_resume((idx, w): (u64, u64)) {
    let s = &SEG[id()];
    s.0.store(if w != 0 { crate::timer::uptime_ns() } else { 0 }, Ordering::Relaxed);
    s.1.store(idx, Ordering::Relaxed);
    s.2.store(w, Ordering::Relaxed);
}

/// 例外 1 つぶんを数える (trap.rs)。ns はロックを取ってからの時間、now はいまの時刻 (timed_now。測らない回は 0)。
/// 回数はいつも、時間は区間の重みで
pub fn account(cause: Cause, ns: u64, slept: bool, now: u64) {
    let s = &SEG[id()];
    let w = s.2.swap(0, Ordering::Relaxed);
    s.1.store(u64::MAX, Ordering::Relaxed);
    let Some(slot) = slot_of(index_of(&cause)) else { return };
    add(&slot.0, 1);
    if slept {
        // 眠るまでの分は seg_pause が足した。起きてからの分を
        add(&STATS.slept, 1);
        if w != 0 {
            add(&slot.1, now.saturating_sub(s.0.load(Ordering::Relaxed)) * w);
        }
        return;
    }
    add(&slot.1, ns * w);
}

/// デバイスの終わりを回って待った回数と時間 (ns): 0 はディスク (virtio-blk)、1 は画面 (virtio-gpu)。
/// 2 はスケジューラが次を選んでページ表を載せるまで (ロックを持ったまま)。3 は起こされてから走りだすまで (待ち時間)。
/// 大きなロックを持ったまま待つものがどれだけあるかを /proc/bkl で見るため
pub static DEV_WAIT: [(AtomicU64, AtomicU64); 4] = [const { (AtomicU64::new(0), AtomicU64::new(0)) }; 4];

/// DEV_WAIT に 1 回分を足す
pub fn dev_wait(i: usize, t0: u64) {
    DEV_WAIT[i].0.fetch_add(1, Ordering::Relaxed);
    DEV_WAIT[i].1.fetch_add(crate::timer::uptime_ns().saturating_sub(t0), Ordering::Relaxed);
}

/// 数えなおす (/proc/bkl に書く)
pub fn stats_reset() {
    for (n, t) in DEV_WAIT.iter().chain(crate::vm::FAULT_KIND.iter()) {
        n.store(0, Ordering::Relaxed);
        t.store(0, Ordering::Relaxed);
    }
    for c in &STATS.cpu {
        c.wait.store(0, Ordering::Relaxed);
        c.hold.store(0, Ordering::Relaxed);
        c.locks.store(0, Ordering::Relaxed);
    }
    for (n, t) in STATS.sys.iter().chain([&STATS.fault, &STATS.irq, &STATS.kthread]).chain(STATS.fd.iter().flatten()) {
        n.store(0, Ordering::Relaxed);
        t.store(0, Ordering::Relaxed);
    }
    STATS.slept.store(0, Ordering::Relaxed);
    FAST_FAULTS.store(0, Ordering::Relaxed);
    PREEMPTS.store(0, Ordering::Relaxed);
    for c in crate::file::FAST_RW.iter().chain(crate::fastdir::FAST_NS.iter()).chain(crate::extfs::FS_WAIT.iter()) {
        c.store(0, Ordering::Relaxed);
    }
    STATS.start.store(crate::timer::uptime_ns(), Ordering::Relaxed);
}

/// /proc/ai/bkl: /proc/bkl と同じものを 1 行の JSON で (AI が読む)
pub fn stats_json() -> alloc::string::String {
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    let ms = |ns: u64| ns as f64 / 1e6;
    let span = crate::timer::uptime_ns().saturating_sub(STATS.start.load(Ordering::Relaxed)).max(1);
    let pct = |ns: u64| ns as f64 * 100.0 / span as f64;
    let (mut tw, mut th) = (0, 0);
    let mut cpus = Vec::new();
    for c in STATS.cpu.iter().take(online()) {
        let (w, h) = (c.wait.load(Ordering::Relaxed), c.hold.load(Ordering::Relaxed));
        tw += w;
        th += h;
        cpus.push(format!("{{\"wait_pct\":{:.1},\"hold_pct\":{:.1},\"locks\":{}}}", pct(w), pct(h), c.locks.load(Ordering::Relaxed)));
    }
    let rows = hold_rows();
    let top: Vec<String> = rows.iter().take(25).map(|(name, n, t)| format!("{{\"name\":\"{}\",\"n\":{},\"ms\":{:.1},\"avg_us\":{:.1}}}", name, n, ms(*t), *t as f64 / 1e3 / *n as f64)).collect();
    let dev: Vec<String> = ["disk", "gpu", "sched", "wake_to_run"].iter().enumerate().map(|(i, name)| format!("\"{}\":{{\"n\":{},\"ms\":{:.1}}}", name, DEV_WAIT[i].0.load(Ordering::Relaxed), ms(DEV_WAIT[i].1.load(Ordering::Relaxed)))).collect();
    let kinds = ["", "perm", "shared_file", "kernel_page", "swap", "page_cache", "anon_new", "file_new", "cow_copy", "page_cache_miss", "around"];
    let faults: Vec<String> = crate::vm::FAULT_KIND.iter().enumerate().skip(1).map(|(i, (n, t))| format!("\"{}\":{{\"n\":{},\"ms\":{:.1}}}", kinds[i], n.load(Ordering::Relaxed), ms(t.load(Ordering::Relaxed)))).collect();
    let fr: Vec<u64> = crate::file::FAST_RW.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    format!(
        "{{\"span_s\":{:.2},\"wait_pct\":{:.1},\"hold_pct\":{:.1},\"cpus\":[{}],\"top\":[{}],\"dev\":{{{}}},\"faults\":{{{}}},\"fast_pipe\":{{\"ok\":{},\"table_busy\":{}}},\"fast_faults\":{},\"slept\":{},\"hold_sample\":{}}}\n",
        span as f64 / 1e9,
        pct(tw),
        pct(th),
        cpus.join(","),
        top.join(","),
        dev.join(","),
        faults.join(","),
        fr[0],
        fr[3],
        FAST_FAULTS.load(Ordering::Relaxed),
        STATS.slept.load(Ordering::Relaxed),
        SAMPLE
    )
}

/// /proc/bkl
pub fn stats() -> alloc::string::String {
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    let ms = |ns: u64| ns as f64 / 1e6;
    let span = crate::timer::uptime_ns().saturating_sub(STATS.start.load(Ordering::Relaxed)).max(1);
    let pct = |ns: u64| ns as f64 * 100.0 / span as f64;
    let mut s = String::new();
    s.push_str(&format!("# 大きなロック (BKL)。{:.1} 秒のあいだ (echo > /proc/bkl で数えなおす)。hold は {} 回に 1 回ほど測って {} 倍した見つもり (待ったものはいつも)
", span as f64 / 1e9, SAMPLE, SAMPLE));
    s.push_str("cpu      wait (待った)          hold (持っていた)        locks
");
    let (mut tw, mut th) = (0, 0);
    for (i, c) in STATS.cpu.iter().enumerate().take(online()) {
        let (w, h) = (c.wait.load(Ordering::Relaxed), c.hold.load(Ordering::Relaxed));
        tw += w;
        th += h;
        s.push_str(&format!("{:<4} {:>10.1} ms {:>5.1}%  {:>10.1} ms {:>5.1}%  {:>10}
", i, ms(w), pct(w), ms(h), pct(h), c.locks.load(Ordering::Relaxed)));
    }
    s.push_str(&format!("all  {:>10.1} ms {:>5.1}%  {:>10.1} ms {:>5.1}%   (1 CPU = 100%)

", ms(tw), pct(tw), ms(th), pct(th)));
    let rows = hold_rows();
    s.push_str("hold の長いもの            回数       合計       平均
");
    for (name, n, t) in rows.iter().take(25) {
        s.push_str(&format!("{:<20} {:>10} {:>9.1} ms {:>8.1} us
", name, n, ms(*t), *t as f64 / 1e3 / *n as f64));
    }
    for (i, name) in ["ディスク", "画面", "スケジューラ", "起こされてから走るまで"].iter().enumerate() {
        let (n, t) = (DEV_WAIT[i].0.load(Ordering::Relaxed), DEV_WAIT[i].1.load(Ordering::Relaxed));
        s.push_str(&format!("デバイスを待った ({}): {} 回 {:.1} ms\n", name, n, ms(t)));
    }
    s.push_str("ページフォルトの種類 (回数 / ms):");
    for (i, (n, t)) in crate::vm::FAULT_KIND.iter().enumerate().skip(1) {
        s.push_str(&format!(" {}:{}/{:.1}", i, n.load(Ordering::Relaxed), ms(t.load(Ordering::Relaxed))));
    }
    s.push('\n');
    let fr: Vec<u64> = crate::file::FAST_RW.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    s.push_str(&format!("ディスクを眠って待った読み: {} 回 (大きなロックを放して)、書かれたのでやりなおした読み: {} 回\n", crate::virtio_blk::SLEPT.load(Ordering::Relaxed), crate::file::IO_RETRIES.load(Ordering::Relaxed)));
    let (paths, missing) = crate::vfs::paths_len();
    s.push_str(&format!("パスの覚え: {} 個 (うちないもの {})、覚えから答えた {} 回\n", paths, missing, crate::vfs::PATHS_HIT.load(Ordering::Relaxed)));
    s.push_str(&format!("ロックなしの stat (statc.rs): {} 回 (ふつうの道へ {})\n", crate::statc::HITS.load(Ordering::Relaxed), crate::statc::MISSES.load(Ordering::Relaxed)));
    let uf: Vec<u64> = crate::unix::FAST.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    s.push_str(&format!("AF_UNIX のロックなしの読み書き: 読んだ {} 書いた {} (ふつうの道へ {})\n", uf[0], uf[1], uf[2]));
    s.push_str(&format!("パイプのロックなしの読み書き: {} (うち起こしを頼んだ {}、空で終わりか EAGAIN を返した {}。ふつうの道へ: 大きさ {} 表を変えている途中 {} パイプでない {} ページ {} いっぱい {} 空き {} 空 {} 写せない {})\n", fr[0], fr[6], fr[7], fr[2], fr[3], fr[4], fr[5], fr[8], fr[9], fr[10], fr[11]));
    let fo: Vec<u64> = crate::file::FAST_OPEN.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    s.push_str(&format!("ロックなしの open / close (file の棚): 棚から開いた {} ない (ENOENT) と答えた {} 棚にしまった {} (ふつうの道へ: statc にない {} 棚にない {} 開けない形 {} しまえない {})\n", fo[0], fo[1], fo[2], fo[3], fo[4], fo[5], fo[6]));
    let ns: Vec<u64> = crate::fastdir::FAST_NS.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    let fw = [crate::extfs::FS_WAIT[0].load(Ordering::Relaxed), crate::extfs::FS_WAIT[1].load(Ordering::Relaxed)];
    s.push_str(&format!("ロックなしの作る・消す (fastdir、ext4 のロックで): 作った {} 消した {} (ふつうの道へ: 形 {} 親が statc にない {} ext4 が断った {})。ext4 のロックを待った {} 回 {:.1} ms\n", ns[0], ns[1], ns[2], ns[3], ns[4], fw[0], fw[1] as f64 / 1e6));
    let wf: Vec<u64> = crate::vm::WRITE_FAST.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    s.push_str(&format!("ファイルのロックなしの pwrite (vm::write_fast): {} (ふつうの道へ: 許しがない・古い {} 大きさの外かキャッシュにない {} ユーザーのページが写っていない {} 書いたページがたまった {})\n", wf[0], wf[1], wf[2], wf[3], wf[4]));
    let fd: Vec<u64> = crate::file::FAST_READ.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    s.push_str(&format!("ファイルのロックなしの read: {} (ふつうの道へ: 大きさ {} IN_ACCESS の見張り {} fd の表を分けている {} 開いたファイルを分けている {} ext4 のファイルでない {} キャッシュにない {})\n", fd[0], fd[1], fd[2], fd[3], fd[4], fd[5], fd[6]));
    s.push_str(&format!("(途中で眠ったもの: {}。ロックなしで片づけたページフォルト: {}。起こされたものにゆずらせた数: {})
", STATS.slept.load(Ordering::Relaxed), FAST_FAULTS.load(Ordering::Relaxed), PREEMPTS.load(Ordering::Relaxed)));
    // BKL が守るもの (cell::Bkl) を、BKL を持たずに触った場所 (あってはいけない。test/vm-check.sh が見る)
    s.push_str(&crate::cell::unlocked_text());
    s
}

/// lockbench (spinlock::bench): 放して取りなおす 1 回の時間 (ns、10 万回の平均)。持って呼ぶ
pub fn bench_bkl() -> f64 {
    let n = 100_000u64;
    let cnt = || {
        let c: u64;
        unsafe { core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) c) };
        c
    };
    let t0 = cnt();
    for _ in 0..n {
        unlock();
        lock();
    }
    (cnt() - t0) as f64 * 1e9 / crate::timer::freq() as f64 / n as f64
}

/// この CPU がロックを持っているか
pub fn holding() -> bool {
    BKL.load(Ordering::Relaxed) == id() + 1
}

// ---- 2 つめからの CPU ----

core::arch::global_asm!(
    r#"
.equ S_MAIR, 0xff00
.equ S_TCR, (25 | (1 << 8) | (1 << 10) | (3 << 12) | (25 << 16) | (1 << 24) | (1 << 26) | (3 << 28) | (2 << 30) | (2 << 32))

.section .text
// 物理アドレスで、MMU なしで来る。x0 = CPU の番号
.global secondary_entry
secondary_entry:
    msr     daifset, #0xf
    mov     x21, x0
    mrs     x0, CurrentEL
    lsr     x0, x0, #2
    cmp     x0, #2
    b.ne    1f
    // EL2 なら EL1 へ (boot.rs と同じ)
    mov     x0, #(1 << 31)
    msr     hcr_el2, x0
    mov     x0, #3
    msr     cnthctl_el2, x0
    msr     cntvoff_el2, xzr
    mov     x0, #0x33ff
    msr     cptr_el2, x0
    msr     hstr_el2, xzr
    mov     x0, #0x3c5
    msr     spsr_el2, x0
    adr     x0, 1f
    msr     elr_el2, x0
    eret
1:  ldr     x0, =0x30d00800
    msr     sctlr_el1, x0
    isb
    // cpu0 が作ったページ表をそのまま使う
    adrp    x0, boot_l1_lo
    add     x0, x0, :lo12:boot_l1_lo
    msr     ttbr0_el1, x0
    adrp    x0, boot_l1_hi
    add     x0, x0, :lo12:boot_l1_hi
    msr     ttbr1_el1, x0
    ldr     x0, =S_MAIR
    msr     mair_el1, x0
    ldr     x0, =S_TCR
    msr     tcr_el1, x0
    isb
    tlbi    vmalle1
    dsb     nsh
    mrs     x0, sctlr_el1
    orr     x0, x0, #(1 << 0)
    orr     x0, x0, #(1 << 2)
    orr     x0, x0, #(1 << 12)
    // EL0 にも Linux と同じく許す: DC ZVA (14、memset)、CTR_EL0 を読む (15)、
    // キャッシュの掃除 DC CVAU / IC IVAU (26、JIT が書いた命令を流すのに使う)
    orr     x0, x0, #(1 << 14)
    orr     x0, x0, #(1 << 15)
    orr     x0, x0, #(1 << 26)
    msr     sctlr_el1, x0
    isb
    ldr     x0, =2f
    br      x0
2:  // ここから上位アドレス
    ldr     x1, =secondary_stacks
    add     x2, x21, #1
    lsl     x2, x2, #14
    add     x1, x1, x2
    mov     sp, x1
    msr     tpidr_el1, x21
    mov     x0, #(3 << 20)
    msr     cpacr_el1, x0
    isb
    mov     x0, x21
    bl      secondary_main
3:  wfe
    b       3b
"#
);

unsafe extern "C" {
    fn secondary_entry();
}

#[unsafe(no_mangle)]
extern "C" fn secondary_main(cpu: usize) -> ! {
    crate::trap::init();
    crate::irq::init_cpu();
    crate::timer::init_cpu();
    lock();
    ONLINE.fetch_add(1, Ordering::AcqRel);
    println!("smp: cpu{} online", cpu);
    crate::proc::scheduler()
}

fn psci_cpu_on(mpidr: u64, entry: u64, ctx: u64) -> i64 {
    const CPU_ON: u64 = 0xc400_0003;
    let r: i64;
    unsafe {
        match crate::dtb::psci_method() {
            Some("smc") => core::arch::asm!("smc #0", inout("x0") CPU_ON => r, in("x1") mpidr, in("x2") entry, in("x3") ctx, options(nostack)),
            _ => core::arch::asm!("hvc #0", inout("x0") CPU_ON => r, in("x1") mpidr, in("x2") entry, in("x3") ctx, options(nostack)),
        }
    }
    r
}

/// spin-table: release addr に入口の物理アドレスを書いて起こす。x0 には番号が入らないので、
/// 入口を CPU ごとの小さな踏み台にする
fn spin_table_on(release: u64, cpu: usize) {
    let tramp = TRAMPOLINES[cpu] as *const () as usize;
    let va = p2v(release as usize) as *mut u64;
    unsafe {
        core::ptr::write_volatile(va, v2p(tramp) as u64);
        // MMU を入れる前の CPU が読むので、キャッシュから RAM へ
        core::arch::asm!("dc civac, {}", "dsb sy", "sev", in(reg) va);
    }
}

// spin-table の踏み台: x0 に番号を入れて secondary_entry へ
core::arch::global_asm!(
    r#"
.section .text
.global smp_tramp0
smp_tramp0: mov x0, #0
    b secondary_entry
.global smp_tramp1
smp_tramp1: mov x0, #1
    b secondary_entry
.global smp_tramp2
smp_tramp2: mov x0, #2
    b secondary_entry
.global smp_tramp3
smp_tramp3: mov x0, #3
    b secondary_entry
.global smp_tramp4
smp_tramp4: mov x0, #4
    b secondary_entry
.global smp_tramp5
smp_tramp5: mov x0, #5
    b secondary_entry
.global smp_tramp6
smp_tramp6: mov x0, #6
    b secondary_entry
.global smp_tramp7
smp_tramp7: mov x0, #7
    b secondary_entry
"#
);

unsafe extern "C" {
    fn smp_tramp0();
    fn smp_tramp1();
    fn smp_tramp2();
    fn smp_tramp3();
    fn smp_tramp4();
    fn smp_tramp5();
    fn smp_tramp6();
    fn smp_tramp7();
}

static TRAMPOLINES: [unsafe extern "C" fn(); MAXCPU] = [smp_tramp0, smp_tramp1, smp_tramp2, smp_tramp3, smp_tramp4, smp_tramp5, smp_tramp6, smp_tramp7];

/// DTB の CPU を起こす (cpu0 から、ロックを持って)。maxcpus=N / nosmp で減らせる
pub fn start() {
    let max = if crate::dtb::arg("nosmp").is_some() || crate::dtb::bootargs().is_some_and(|a| a.split_whitespace().any(|w| w == "nosmp")) {
        1
    } else {
        crate::dtb::arg("maxcpus").and_then(|n| n.parse().ok()).unwrap_or(MAXCPU).clamp(1, MAXCPU)
    };
    let me: u64;
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) me) };
    let me = me & 0xff_00ff_ffff;
    TARGET[0].store((me & 0xff) as usize, Ordering::Relaxed);
    let mut n = 1;
    let mut started = 0;
    crate::dtb::each_cpu(|mpidr, method, release| {
        if mpidr == me || n >= max {
            return;
        }
        let cpu = n;
        n += 1;
        TARGET[cpu].store((mpidr & 0xff) as usize, Ordering::Relaxed);
        match (method, release) {
            (Some("spin-table"), Some(rel)) => {
                spin_table_on(rel, cpu);
                started += 1;
            }
            _ => {
                let entry = v2p(secondary_entry as *const () as usize) as u64;
                let r = psci_cpu_on(mpidr, entry, cpu as u64);
                if r == 0 {
                    started += 1;
                } else {
                    println!("smp: cannot start cpu {:#x} ({})", mpidr, r);
                }
            }
        }
    });
    if started == 0 {
        return;
    }
    // 起きた CPU はロックを待っているので、いったん放して待つ
    let want = 1 + started;
    unlock();
    for _ in 0..50_000_000 {
        if online() >= want {
            break;
        }
        core::hint::spin_loop();
    }
    lock();
    println!("smp: {} cpus", online());
    crate::vm::calibrate_tlbi();
}
