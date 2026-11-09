// ARM generic timer (EL1 仮想タイマ, PPI 11 = INTID 27)。Mac の Hypervisor (HVF) では
// 物理タイマは使えないので、仮想タイマとそのカウンタ (CNTVCT) を使う (EL2 から来たときは CNTVOFF = 0)
use core::sync::atomic::{AtomicU64, Ordering};

pub const IRQ: u32 = crate::irq::TIMER;
pub const HZ: u64 = 100;

/// 前の割り込みのときの tick (cpu0 だけが使う)
static TICKS: AtomicU64 = AtomicU64::new(0);

// [lockless]
pub fn freq() -> u64 {
    let f: u64;
    unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) f) };
    // ファームウェアが設定していなければ、ラズパイの水晶 (19.2 MHz)
    if f == 0 { 19_200_000 } else { f }
}

fn rearm() {
    unsafe {
        core::arch::asm!("msr cntv_tval_el0, {}", in(reg) freq() / HZ);
        core::arch::asm!("msr cntv_ctl_el0, {}", in(reg) 1u64);
    }
}

/// 起動したときの時刻 (UNIX 秒)。PL031 RTC があればそこから、なければ (ラズパイには
/// RTC がない) カーネルを作った時刻から始める。
/// UEFI から起動すると DTB の PL031 は disabled (ファームウェアの時計) だが、読むだけなのでそれも使う
static BOOT_EPOCH: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    let rtc = if crate::dtb::present() { crate::dtb::reg_of_any("arm,pl031", 0).map(|(a, _)| a as usize) } else { Some(0x0901_0000) };
    let epoch = match rtc {
        Some(pa) => crate::mmio::r32(crate::memlayout::p2v(pa)) as u64,
        None => env!("AIOS_BUILD_EPOCH").parse().unwrap_or(0),
    };
    BOOT_EPOCH.store(epoch.saturating_sub(uptime_ns() / 1_000_000_000), Ordering::Relaxed);
    COARSE.fetch_max(uptime_ns(), Ordering::Relaxed);
    crate::irq::enable(IRQ);
    crate::vdso::allow_counter();
    rearm();
}

/// カウンタの 1 刻みの ns (周波数が 1 秒を割りきるとき: QEMU の virt は 62.5 MHz で 16)。0 はまだ、MAX は割りきれない
static NS_PER_TICK: AtomicU64 = AtomicU64::new(0);

/// 起動してからの時間 (ns)。割りきれる周波数なら掛け算 1 つ (答えは割り算と同じ): 大きなロックの数えで、システムコールの
/// たびに 2 度読む (TCG では cntfrq の読みと 3 つの割り算が重い)
// [lockless]
pub fn uptime_ns() -> u64 {
    let cnt: u64;
    unsafe { core::arch::asm!("mrs {}, cntvct_el0", out(reg) cnt) };
    let k = NS_PER_TICK.load(Ordering::Relaxed);
    if k != 0 && k != u64::MAX {
        return cnt * k;
    }
    let f = freq();
    if k == 0 {
        NS_PER_TICK.store(if 1_000_000_000 % f == 0 { 1_000_000_000 / f } else { u64::MAX }, Ordering::Relaxed);
    }
    (cnt / f) * 1_000_000_000 + (cnt % f) * 1_000_000_000 / f
}

// ---- 粗い時計 (Linux の ktime_get_coarse と current_time) ----
// ファイルの時刻 (ext4 の秒、パイプ) と、ext4 の「そろそろ書き出すか」は、時計 (cntvct) を読まずに、tick (割り込み) の
// ときに覚えた時刻を使う。QEMU の TCG では cntvct を読むたびに QEMU の大きなロック (BQL) を取るので、いくつもの CPU が
// 同時に読むと 1 回が 110 ns から 1.1 µs (4 つ) になり、読むところで 1 つずつになる (ext4 の作る・消すは 1 回に 7 度
// 読んでいた)。本当の機械でも、読むたびに isb と 20〜40 サイクル。Linux もファイルの時刻は tick の時刻 (current_time)。
// 細かい時計でつけた時刻 (utimensat の NOW、tmpfs) は床 (FLOOR) に覚え、粗い時刻はそれより前にならない (Linux の
// multigrain の floor と同じ: ちがうファイル、ちがう道でつけた時刻が逆になってはいけない)

/// いちばん新しい tick の時刻 (起動からの ns。どの CPU の tick でも、大きいほうに)
static COARSE: AtomicU64 = AtomicU64::new(0);
/// 細かい時計で inode につけた、いちばん新しい時刻 (UNIX ns)
static FLOOR: AtomicU64 = AtomicU64::new(0);

/// tick の時刻 (起動からの ns)。時計を読まない。tick 1 つ (10 ms) ほど遅れる
// [lockless]
pub fn coarse_uptime_ns() -> u64 {
    COARSE.load(Ordering::Relaxed)
}

/// 粗い時計の UNIX ns: ファイルの時刻に (細かい時計でつけたものより前にはならない)
// [lockless]
pub fn coarse_epoch_ns() -> u64 {
    (BOOT_EPOCH.load(Ordering::Relaxed) * 1_000_000_000 + coarse_uptime_ns()).max(FLOOR.load(Ordering::Relaxed))
}

/// 粗い時計の tick (ticks() と同じ数え方。時計を読まない)
// [lockless]
pub fn coarse_ticks() -> u64 {
    coarse_uptime_ns() / (1_000_000_000 / HZ)
}

/// 細かい時計で inode につける時刻 (UNIX ns): 床に覚えて、あとの粗い時刻がこれより前にならないように
pub fn stamp_ns() -> u64 {
    let t = epoch_ns();
    FLOOR.fetch_max(t, Ordering::Relaxed);
    t
}

/// 起動したときの UNIX 秒 (vDSO の vvar に入れる)
pub fn boot_epoch() -> u64 {
    BOOT_EPOCH.load(Ordering::Relaxed)
}

// [lockless]
pub fn epoch_ns() -> u64 {
    BOOT_EPOCH.load(Ordering::Relaxed) * 1_000_000_000 + uptime_ns()
}

// ---- 時計 (clock_gettime / clock_getres / timer_create の clockid: Linux の posix_clocks と posix-cpu-timers) ----

/// 1 tick の ns (Linux の TICK_NSEC)。aios のタイマは tick の境めで鳴るので、壁と起動からの時計の細かさはこれ
pub const TICK_NS: u64 = 1_000_000_000 / HZ;

/// clockid の中身
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Clock {
    /// 壁の時計 (REALTIME 0、REALTIME_COARSE 5、REALTIME_ALARM 8、TAI 11: TAI と UTC の差は 0)
    Real,
    /// 起動からの時計 (MONOTONIC 1、MONOTONIC_RAW 4、MONOTONIC_COARSE 6、BOOTTIME 7、BOOTTIME_ALARM 9: 眠らないので同じ)
    Mono,
    /// CPU の時計: スレッドのものか (でなければプロセス)、本当の番号 (tid か tgid)、PROF (0) / VIRT (1) / SCHED (2)
    Cpu { thread: bool, pid: u32, which: u8 },
}

/// clockid をほどく (Linux の clockid_to_kclock と pid_for_clock)。PROCESS_CPUTIME_ID (2) と THREAD_CPUTIME_ID (3) は
/// 自分、負のものは CPU の時計 (~pid << 3 | スレッド 4 | which) で、pid 0 は自分。スレッドの時計は同じスレッド
/// グループのものだけ、プロセスの時計はスレッドグループの代表 (gettime なら自分のスレッドの番号も)。知らない時計、
/// いないもの、which が 3 (と fd の時計) は EINVAL
pub fn clock_of(clk: i32, gettime: bool) -> Result<Clock, i64> {
    const EINVAL: i64 = 22;
    let me = crate::proc::current();
    match clk {
        0 | 5 | 8 | 11 => Ok(Clock::Real),
        1 | 4 | 6 | 7 | 9 => Ok(Clock::Mono),
        2 => Ok(Clock::Cpu { thread: false, pid: me.tgid, which: 2 }),
        3 => Ok(Clock::Cpu { thread: true, pid: me.pid, which: 2 }),
        c if c < 0 => {
            let which = (c & 3) as u8;
            let thread = c & 4 != 0;
            if which == 3 {
                return Err(-EINVAL);
            }
            let upid = !(c >> 3) as u32;
            if upid == 0 {
                return Ok(Clock::Cpu { thread, pid: if thread { me.pid } else { me.tgid }, which });
            }
            let g = crate::ns::to_global(upid).ok_or(-EINVAL)?;
            if thread {
                let t = crate::proc::find_thread(g).ok_or(-EINVAL)?;
                if t.tgid != me.tgid {
                    return Err(-EINVAL);
                }
                return Ok(Clock::Cpu { thread, pid: g, which });
            }
            if gettime && g == me.pid {
                return Ok(Clock::Cpu { thread, pid: me.tgid, which });
            }
            crate::proc::find_leader_any(g).ok_or(-EINVAL)?;
            Ok(Clock::Cpu { thread, pid: g, which })
        }
        _ => Err(-EINVAL),
    }
}

/// 時計のいまの値 (ns)。CPU の時計: SCHED と PROF は走った時間、VIRT はユーザーで走った時間。もういなければ EINVAL
pub fn clock_ns(c: Clock) -> Result<u64, i64> {
    const EINVAL: i64 = 22;
    Ok(match c {
        Clock::Real => epoch_ns(),
        Clock::Mono => uptime_ns(),
        Clock::Cpu { thread: true, pid, which } => {
            let t = crate::proc::find_thread(pid).ok_or(-EINVAL)?;
            if which == 1 { crate::proc::user_sys_ns(t, true).0 } else { crate::proc::cpu_ns(t) }
        }
        Clock::Cpu { thread: false, pid, which } => {
            let l = crate::proc::find_leader_any(pid).ok_or(-EINVAL)?;
            if which == 1 { crate::proc::user_sys_ns(l, false).0 } else { crate::proc::group_cpu_ns(pid) }
        }
    })
}

/// 時計の細かさ (ns): CPU の SCHED は 1、ほかは tick (Linux も高精度タイマがなければ tick)
pub fn clock_res(c: Clock) -> u64 {
    match c {
        Clock::Cpu { which: 2, .. } => 1,
        _ => TICK_NS,
    }
}

/// 起動してからの tick (HZ 分の 1 秒)。割り込みの回数ではなく、カウンタ (CNTVCT) から計算する。
/// 割り込みは遅れたりまとまったりする (Mac の HVF では特に) ので、数えると時計が遅れる
pub fn ticks() -> u64 {
    uptime_ns() / (1_000_000_000 / HZ)
}

/// いまから ns たったときの期限 (tick)。その tick になれば、かならず ns たっている: 早く起きない (Linux の hrtimer と
/// 同じきまり。aios は tick の境め (10 ms ごと) で起きるので、そこまで遅れる)。前の ticks() + 切り上げた tick の数は、
/// tick の途中から待つと 1 tick 近く早く起きた。0 はいま (待たない)
pub fn ticks_after(ns: u64) -> u64 {
    if ns == 0 {
        return ticks();
    }
    uptime_ns().saturating_add(ns).div_ceil(1_000_000_000 / HZ)
}

/// 仕事がなくて眠る CPU (cpu0 以外) はタイマを止める (起こすのは IPI。時刻や期限は cpu0 が見る)。
/// 眠っている CPU が 1 秒に 100 回起きると、QEMU (TCG / HVF) の中で取りあいになって遅くなる
pub fn idle(on: bool) {
    if crate::smp::id() == 0 {
        return;
    }
    if on {
        unsafe { core::arch::asm!("msr cntv_ctl_el0, {}", in(reg) 0u64) };
    } else {
        rearm();
    }
}

/// 2 つめからの CPU: 自分のタイマを動かす
pub fn init_cpu() {
    crate::irq::enable(IRQ);
    crate::vdso::allow_counter();
    rearm();
}

/// タイマの割り込み (CPU ごと)。時刻を進めるなどは cpu0 だけ
pub fn tick() {
    crate::proc::account_tick();
    // 粗い時計 (どの CPU でも: cpu0 が長く割り込みを止めていても進む)
    COARSE.fetch_max(uptime_ns(), Ordering::Relaxed);
    if crate::smp::id() != 0 {
        rearm();
        return;
    }
    // 前の割り込みから何 tick も進んでいることがある (期限は <= で比べるので、飛んでもよい)
    let now = ticks();
    let prev = TICKS.swap(now, Ordering::Relaxed);
    crate::proc::wake_expired(now);
    if now / HZ != prev / HZ {
        crate::proc::decay_recent();
    }
    // 置いたもの (timerfd の期限、割り込みの来なかったディスクの要求、5 tick ごとの TCP の再送など)
    for (p, f) in &EVERY {
        let f = f.load(Ordering::Acquire);
        if f == 0 {
            break;
        }
        // 1 は割り込みのたびに (tick が進んでいなくても)
        let p = p.load(Ordering::Relaxed);
        if p <= 1 || now / p != prev / p {
            // SAFETY: every が fn() だけを置く
            let f: fn() = unsafe { core::mem::transmute::<usize, fn()>(f) };
            f();
        }
    }
    rearm();
}

/// tick ごと (cpu0) に呼ぶもの (Linux では装置やプロトコルがそれぞれ回す timer_list: itimer と POSIX タイマー、timerfd の期限、割り込みの来なかった
/// ディスクの要求、TCP の再送など)。(何 tick ごとか, 呼ぶもの)。置いた順に呼ぶ。起動のとき (や modprobe のとき) に every で
const NR_EVERY: usize = 8;
static EVERY: [(AtomicU64, core::sync::atomic::AtomicUsize); NR_EVERY] = [const { (AtomicU64::new(0), core::sync::atomic::AtomicUsize::new(0)) }; NR_EVERY];
static EVERY_LOCK: crate::spinlock::SpinLock<()> = crate::spinlock::SpinLock::new(());

/// period tick ごとに f を呼ぶ (1 なら tick ごと)
pub fn every(period: u64, f: fn()) {
    let g = EVERY_LOCK.lock();
    let Some((p, h)) = EVERY.iter().find(|e| e.1.load(Ordering::Relaxed) == 0) else { panic!("timer: more than {} tick hooks", NR_EVERY) };
    p.store(period, Ordering::Relaxed);
    h.store(f as usize, Ordering::Release);
    drop(g);
}
