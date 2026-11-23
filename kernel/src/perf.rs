// perf_event_open (241): Linux の kernel/events/core.c の、ソフトウェアの数を数えるところ (task-clock、cpu-clock、
// page-faults と min / maj、context-switches、cpu-migrations、alignment-faults、emulation-faults、dummy、bpf-output)。
// attr のたしかめ (perf_copy_attr) と perf_event_open の順、paranoid (perf_allow_kernel / perf_allow_cpu)、ほかの
// スレッドは ptrace で読めるときだけ、グループ (group_fd、PERF_FORMAT_GROUP)、read の形 (TOTAL_TIME_ENABLED /
// RUNNING、ID、LOST)、ioctl (ENABLE、DISABLE、RESET、ID、PERIOD ほか)、inherit (fork した子も)、enable_on_exec /
// remove_on_exec、prctl の PR_TASK_PERF_EVENTS_DISABLE / ENABLE、/proc/sys/kernel/perf_* と /sys/bus/event_source。
// 数はイベントごとに数えず、スレッドの数 (proc.rs の cpu_ns、pgfault、min_flt、maj_flt、nvcsw + nivcsw、
// nr_migrations) を読んだときの差で出す: 動いている (on で、グループならリーダーも on) あいだの差を足し、変わるたびに
// 写しなおす (sync)。時間 (time_enabled、time_running) は Linux のタスクの文脈と同じく、そのスレッドが走った時間。
// PERF_TYPE_BREAKPOINT (ブレークポイントとウォッチポイントで数える) は、数えるスレッドの hwdebug.rs の State に置き、
// 当たった数をそこから読む (当たったら hwdebug.rs が数えて 1 命令越える)。
// サンプリング (sample_period / freq): 輪 (mmap: 頭のページと 2^n のページ、data_head と data_tail、LOST、
// wakeup_events と watermark、poll) に PERF_RECORD_SAMPLE を書く (sample_type の順: Linux の perf_output_sample)。
// きっかけはページフォルト (trap.rs)、EL0 で鳴ったタイマ (task-clock と cpu-clock: Linux の hrtimer のかわりに、間が
// tick より短ければ timer::hr_arm で CPU のタイマをそのときにも鳴らす)、perf のブレークポイント (hwdebug.rs)。
// 間は Linux と同じく hw.sample_period・last_period・period_left (perf_swevent_set_period): PERIOD を読む決まった間の
// ものは間で割らずに 1 回ごとに 1 つ (perf_swevent_event)。freq のものは、はじめ間 1 から、あふれるたびと tick ごとに
// 数と時間を見て間をなおす (perf_adjust_period: 差の 1/8 ずつ)。時計のものは freq を 1e9 / freq ns の決まった間にして
// しまい (perf_swevent_init_hrtimer)、PERIOD ははじめの間のまま。IOC_PERIOD は freq か間を変える。
// 1 tick に perf_event_max_sample_rate / HZ (切り上げ) まであふれた時計のものは、次の tick まで止める (Linux の
// __perf_event_account_interrupt と perf_event_throttle_group: PERF_RECORD_THROTTLE / UNTHROTTLE。数は進む)。
// ソフトウェアのできごとの 1 つめのあふれは止めない (perf_swevent_overflow の throttle 0)。
// sigtrap は SIGTRAP (TRAP_PERF)。attr.task と comm なら PERF_RECORD_FORK、EXIT、COMM も。
// attr.mmap (と mmap2、mmap_data) なら PERF_RECORD_MMAP / MMAP2 (mmap、実行できるようにした mprotect、exec の
// プログラムの領域: Linux の perf_event_mmap)。
// CPU ごとのもの (pid -1、cpu >= 0: perf stat -a、perf top。paranoid が 0 より上なら CAP_PERFMON): その CPU のみなの
// フォルト、切りかえ、移り (proc::CPU_SW) と、時計は有効だった時間。cpu-clock と task-clock とフォルトのサンプリングは、
// その CPU で EL0 で鳴った tick とフォルトで。pid と cpu の両方なら、そのスレッドがいまその CPU にいるときの差だけ。
// まだないもの (todo.json の syscall-gaps): PMU (ハードウェアの数: ENOENT)
use crate::cell::Bkl;
use crate::file::{self, Kind};
use crate::hwdebug;
use crate::proc::{self, Proc};
use alloc::rc::{Rc, Weak};
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::sync::atomic::{AtomicI32, AtomicU64, Ordering::Relaxed};

type R = Result<i64, i64>;
const ENOENT: i64 = 2;
const ESRCH: i64 = 3;
const E2BIG: i64 = 7;
const EBADF: i64 = 9;
const EACCES: i64 = 13;
const EFAULT: i64 = 14;
const ENODEV: i64 = 19;
const EINVAL: i64 = 22;
const ENOTTY: i64 = 25;
const ENOSPC: i64 = 28;
const EMFILE: i64 = 24;
const EOPNOTSUPP: i64 = 95;

/// struct perf_event_attr の大きさ (PERF_ATTR_SIZE_VER10) と、いちばん小さいもの (VER0)
const ATTR_SIZE: usize = 176;
const ATTR_VER0: usize = 64;
/// type
const TYPE_SOFTWARE: u32 = 1;
const TYPE_BREAKPOINT: u32 = 5;
/// PERF_TYPE_SOFTWARE の config
const SW_CPU_CLOCK: u64 = 0;
const SW_TASK_CLOCK: u64 = 1;
const SW_PAGE_FAULTS: u64 = 2;
const SW_CONTEXT_SWITCHES: u64 = 3;
const SW_CPU_MIGRATIONS: u64 = 4;
const SW_PAGE_FAULTS_MIN: u64 = 5;
const SW_PAGE_FAULTS_MAJ: u64 = 6;
const SW_MAX: u64 = 12;
/// read_format
const FMT_TOTAL_TIME_ENABLED: u64 = 1;
const FMT_TOTAL_TIME_RUNNING: u64 = 2;
const FMT_ID: u64 = 4;
const FMT_GROUP: u64 = 8;
const FMT_LOST: u64 = 16;
const FMT_MAX: u64 = 32;
/// sample_type
const SAMPLE_IP: u64 = 1;
const SAMPLE_TID: u64 = 1 << 1;
const SAMPLE_TIME: u64 = 1 << 2;
const SAMPLE_ADDR: u64 = 1 << 3;
const SAMPLE_READ: u64 = 1 << 4;
const SAMPLE_CALLCHAIN: u64 = 1 << 5;
const SAMPLE_ID: u64 = 1 << 6;
const SAMPLE_CPU: u64 = 1 << 7;
const SAMPLE_PERIOD: u64 = 1 << 8;
const SAMPLE_STREAM_ID: u64 = 1 << 9;
const SAMPLE_RAW: u64 = 1 << 10;
const SAMPLE_BRANCH_STACK: u64 = 1 << 11;
const SAMPLE_REGS_USER: u64 = 1 << 12;
const SAMPLE_STACK_USER: u64 = 1 << 13;
const SAMPLE_WEIGHT: u64 = 1 << 14;
const SAMPLE_DATA_SRC: u64 = 1 << 15;
const SAMPLE_IDENTIFIER: u64 = 1 << 16;
const SAMPLE_TRANSACTION: u64 = 1 << 17;
const SAMPLE_PHYS_ADDR: u64 = 1 << 19;
const SAMPLE_REGS_INTR: u64 = 1 << 18;
const SAMPLE_AUX: u64 = 1 << 20;
const SAMPLE_DATA_PAGE_SIZE: u64 = 1 << 22;
const SAMPLE_CODE_PAGE_SIZE: u64 = 1 << 23;
const SAMPLE_WEIGHT_STRUCT: u64 = 1 << 24;
const SAMPLE_MAX: u64 = 1 << 25;
/// 輪の記録の種類 (PERF_RECORD_*) と misc
const REC_MMAP: u32 = 1;
const REC_MMAP2: u32 = 10;
const MISC_MMAP_DATA: u16 = 1 << 13;
const REC_LOST: u32 = 2;
const REC_COMM: u32 = 3;
const REC_EXIT: u32 = 4;
const REC_FORK: u32 = 7;
const REC_SAMPLE: u32 = 9;
const REC_THROTTLE: u32 = 5;
const REC_UNTHROTTLE: u32 = 6;

/// 止めた印 (Linux の MAX_INTERRUPTS)
const MAX_INTERRUPTS: u64 = u64::MAX;
/// CPU ごとの tick の番号 (Linux の perf_throttled_seq: あふれた数を tick ごとに数えなおす)
static THROTTLE_SEQ: [core::sync::atomic::AtomicU64; crate::smp::MAXCPU] = [const { core::sync::atomic::AtomicU64::new(0) }; crate::smp::MAXCPU];

/// 1 tick にあふれてよい数 (Linux の max_samples_per_tick: perf_event_max_sample_rate / HZ を切り上げ)
fn max_samples_per_tick() -> u64 {
    (MAX_SAMPLE_RATE.load(Relaxed).max(1) as u64).div_ceil(crate::timer::HZ)
}
const MISC_USER: u16 = 2;
const MISC_COMM_EXEC: u16 = 1 << 13;
/// コールチェーンの「ここからユーザー」 (PERF_CONTEXT_USER)
const CONTEXT_USER: u64 = (-512i64) as u64;
const PG: usize = 4096;
/// perf_event_open の flags
const FLAG_FD_NO_GROUP: u64 = 1;
const FLAG_PID_CGROUP: u64 = 4;
const FLAG_FD_CLOEXEC: u64 = 8;
const FLAG_ALL: u64 = 0xf;
/// attr のビット (disabled から)
const B_DISABLED: u32 = 0;
const B_INHERIT: u32 = 1;
const B_PINNED: u32 = 2;
const B_EXCLUSIVE: u32 = 3;
const B_EXCLUDE_USER: u32 = 4;
const B_EXCLUDE_KERNEL: u32 = 5;
const B_MMAP: u32 = 8;
const B_COMM: u32 = 9;
const B_MMAP_DATA: u32 = 17;
const B_MMAP2: u32 = 23;
const B_FREQ: u32 = 10;
const B_ENABLE_ON_EXEC: u32 = 12;
const B_TASK: u32 = 13;
const B_WATERMARK: u32 = 14;
const B_SAMPLE_ID_ALL: u32 = 18;
const B_EXCLUDE_CALLCHAIN_KERNEL: u32 = 21;
const B_USE_CLOCKID: u32 = 25;
const B_NAMESPACES: u32 = 28;
const B_TEXT_POKE: u32 = 33;
const B_BUILD_ID: u32 = 34;
const B_INHERIT_THREAD: u32 = 35;
const B_REMOVE_ON_EXEC: u32 = 36;
const B_SIGTRAP: u32 = 37;
/// ioctl (_IO / _IOR / _IOW の '$')
const IOC_ENABLE: u64 = 0x2400;
const IOC_DISABLE: u64 = 0x2401;
const IOC_REFRESH: u64 = 0x2402;
const IOC_RESET: u64 = 0x2403;
const IOC_PERIOD: u64 = 0x4008_2404;
const IOC_SET_OUTPUT: u64 = 0x2405;
const IOC_SET_FILTER: u64 = 0x4008_2406;
const IOC_ID: u64 = 0x8008_2407;
const IOC_SET_BPF: u64 = 0x4004_2408;
const IOC_PAUSE_OUTPUT: u64 = 0x4004_2409;
const IOC_QUERY_BPF: u64 = 0xc008_240a;
const IOC_MODIFY_ATTRIBUTES: u64 = 0x4008_240b;
const IOC_FLAG_GROUP: u64 = 1;

// ---- /proc/sys/kernel/perf_* (Linux の kernel/events/core.c の sysctl) ----

/// perf_event_paranoid (2: ふつうの人はカーネルの数も CPU ごとのものもだめ)
static PARANOID: AtomicI32 = AtomicI32::new(2);
static MAX_SAMPLE_RATE: AtomicI32 = AtomicI32::new(100_000);
/// 512 KiB とページ 1 つ
static MLOCK_KB: AtomicI32 = AtomicI32::new(516);
static MAX_STACK: AtomicI32 = AtomicI32::new(127);
static CPU_TIME_MAX_PERCENT: AtomicI32 = AtomicI32::new(25);
static MAX_CONTEXTS_PER_STACK: AtomicI32 = AtomicI32::new(8);

/// sysctl.rs の表から: (名前、値)
pub static KNOBS: [(&str, &AtomicI32, i32, i32); 6] = [
    ("perf_event_paranoid", &PARANOID, i32::MIN, i32::MAX),
    ("perf_event_max_sample_rate", &MAX_SAMPLE_RATE, 1, i32::MAX),
    ("perf_event_mlock_kb", &MLOCK_KB, i32::MIN, i32::MAX),
    ("perf_event_max_stack", &MAX_STACK, 0, 640 * 1024),
    ("perf_cpu_time_max_percent", &CPU_TIME_MAX_PERCENT, 0, 100),
    ("perf_event_max_contexts_per_stack", &MAX_CONTEXTS_PER_STACK, 0, 1000),
];

pub fn knob_get(i: usize) -> alloc::string::String {
    alloc::format!("{}", KNOBS[i].1.load(Relaxed))
}

pub fn knob_set(i: usize, s: &str) -> Result<(), i64> {
    // 止めないとき (perf_cpu_time_max_percent が 0 か 100) は max_sample_rate を書けない
    // (Linux の perf_event_max_sample_rate_handler)
    if i == 1 && matches!(CPU_TIME_MAX_PERCENT.load(Relaxed), 0 | 100) {
        return Err(-EINVAL);
    }
    let v = s.trim().parse::<i32>().map_err(|_| -EINVAL)?;
    if v < KNOBS[i].2 || v > KNOBS[i].3 {
        return Err(-EINVAL);
    }
    KNOBS[i].1.store(v, Relaxed);
    Ok(())
}

/// /sys/bus/event_source/devices の種類 (名前、PERF_TYPE_*)。並べた順
pub const SOURCES: [(&str, u32); 2] = [("breakpoint", TYPE_BREAKPOINT), ("software", TYPE_SOFTWARE)];

/// CAP_PERFMON か CAP_SYS_ADMIN (Linux の perfmon_capable)
fn perfmon_capable() -> bool {
    let c = crate::cred::current();
    c.capable(crate::cred::CAP_PERFMON) || c.capable(crate::cred::CAP_SYS_ADMIN)
}

/// カーネルの数 (perf_allow_kernel: paranoid が 1 より上なら CAP_PERFMON がいる)
fn allow_kernel() -> Result<(), i64> {
    if PARANOID.load(Relaxed) > 1 && !perfmon_capable() { Err(-EACCES) } else { Ok(()) }
}

/// CPU ごとのもの (perf_allow_cpu: paranoid が 0 より上なら CAP_PERFMON がいる)
fn allow_cpu() -> Result<(), i64> {
    if PARANOID.load(Relaxed) > 0 && !perfmon_capable() { Err(-EACCES) } else { Ok(()) }
}

// ---- イベント ----

/// 数えているスレッド (はじめのものと、inherit で fork した子) と、前に写したときのその数と走った時間
struct Task {
    tid: u32,
    c: u64,
    t: u64,
}

struct St {
    /// 有効 (Linux の state >= INACTIVE)
    on: bool,
    /// CPU ごとのもの: 前に写したときの CPU の数と時刻
    cpu_c: u64,
    cpu_t: u64,
    /// 写したところまでの数、有効だった時間、動いた時間
    cnt: u64,
    en: u64,
    run: u64,
    tasks: Vec<Task>,
}

pub struct Event {
    id: u64,
    ty: u32,
    config: u64,
    /// ブレークポイントなら (ウォッチポイントか、bp_type、CPU の番地、ctrl)
    bp: Option<(bool, u8, u64, hwdebug::Ctrl)>,
    read_format: u64,
    bits: u64,
    /// サンプリング: sample_type、間 (下)、時計のものの前のサンプルの時刻、wakeup_events、sigtrap の sig_data、
    /// レジスタとスタック、コールチェーンの深さ、ブレークポイントの番地
    sample_type: u64,
    /// サンプリングのものか (attr の sample_period / sample_freq が 0 でない: Linux の is_sampling_event)
    sampling: bool,
    /// 間 (Linux の hw.sample_period: freq ならなおしていく)、前の間 (last_period: サンプルの PERIOD)、間ののこり
    /// (period_left: 負のあいだは待つ)、freq (sample_freq。0 は決まった間)、freq をなおした時刻と数 (freq_time_stamp、
    /// freq_count_stamp)、サンプリングで見た数
    period: Cell<u64>,
    last_period: Cell<u64>,
    left: Cell<i64>,
    freq: Cell<u64>,
    freq_time: Cell<u64>,
    freq_stamp: Cell<u64>,
    hits: Cell<u64>,
    last_clock: Cell<u64>,
    /// この tick のあふれの数と、その tick の番号 (Linux の hw.interrupts と interrupts_seq。MAX_INTERRUPTS は止めた)
    interrupts: Cell<u64>,
    int_seq: Cell<u64>,
    wakeup_events: u32,
    watermark: u32,
    sig_data: u64,
    regs_user: u64,
    stack_user: u32,
    regs_intr: u64,
    max_stack: u16,
    bp_addr: u64,
    rb: RefCell<Option<Rc<Rb>>>,
    /// 数える相手 (はじめのスレッドの本当の番号) と、開いたスレッド (prctl の PR_TASK_PERF_EVENTS_*)
    target: u32,
    /// CPU (-1 はどこでも)。target が 0 なら CPU ごとのもの
    cpu: i32,
    owner: u32,
    enable_on_exec: Cell<bool>,
    /// PERF_EVENT_IOC_REFRESH の残り (0 はかぎりなし。0 になったら止まる: Linux の event_limit)
    limit: Cell<i64>,
    st: RefCell<St>,
    /// グループ: メンバーならリーダー、リーダーならメンバーたち
    leader: RefCell<Option<Weak<Event>>>,
    siblings: RefCell<Vec<Weak<Event>>>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// 開いているイベント (fork と exit と exec で見る)
static EVENTS: Bkl<Vec<Weak<Event>>> = Bkl::new(Vec::new());

impl Event {
    fn bit(&self, b: u32) -> bool {
        self.bits >> b & 1 != 0
    }

    fn leader(&self) -> Option<Rc<Event>> {
        self.leader.borrow().as_ref().and_then(|w| w.upgrade())
    }

    fn members(&self) -> Vec<Rc<Event>> {
        self.siblings.borrow().iter().filter_map(|w| w.upgrade()).collect()
    }

    /// 数えている (有効で、グループのリーダーも有効)
    fn active(&self) -> bool {
        self.st.borrow().on && self.leader().map_or(true, |l| l.st.borrow().on)
    }

    /// スレッドの今の数 (exclude_user と exclude_kernel: フォルトはユーザーで、切りかえと移りはカーネルで起きる)。
    /// 時計のものは、いっしょに読んだ走った時間 now (time_enabled と同じ時刻にするため)
    fn source(&self, t: &Proc, now: u64) -> u64 {
        let user = !self.bit(B_EXCLUDE_USER);
        let kernel = !self.bit(B_EXCLUDE_KERNEL);
        if self.ty == TYPE_BREAKPOINT {
            return t.hwdbg.as_ref().and_then(|s| s.perf.iter().find(|b| b.id == self.id)).map_or(0, |b| b.count);
        }
        match self.config {
            SW_CPU_CLOCK | SW_TASK_CLOCK => now,
            SW_PAGE_FAULTS if user => t.pgfault,
            SW_PAGE_FAULTS_MIN if user => t.min_flt,
            SW_PAGE_FAULTS_MAJ if user => t.maj_flt,
            SW_CONTEXT_SWITCHES if kernel => t.nvcsw + t.nivcsw,
            SW_CPU_MIGRATIONS if kernel => t.nr_migrations,
            _ => 0,
        }
    }

    /// CPU ごとのものか (数える相手のスレッドがない)
    fn per_cpu(&self) -> bool {
        self.target == 0
    }

    /// CPU ごとのものの、その CPU の今の数 (時計のものは now: 経った時間)
    fn source_cpu(&self, now: u64) -> u64 {
        let user = !self.bit(B_EXCLUDE_USER);
        let kernel = !self.bit(B_EXCLUDE_KERNEL);
        let n = |k: usize| proc::CPU_SW[self.cpu as usize][k].load(Relaxed);
        match self.config {
            SW_CPU_CLOCK | SW_TASK_CLOCK if self.ty == TYPE_SOFTWARE => now,
            _ if self.ty != TYPE_SOFTWARE => 0,
            SW_PAGE_FAULTS if user => n(0),
            SW_PAGE_FAULTS_MIN if user => n(1),
            SW_PAGE_FAULTS_MAJ if user => n(2),
            SW_CONTEXT_SWITCHES if kernel => n(3),
            SW_CPU_MIGRATIONS if kernel => n(4),
            _ => 0,
        }
    }

    /// (数、有効だった時間、動いた時間): 写したところまでと、そこからの差
    fn values(&self) -> (u64, u64, u64) {
        let act = self.active();
        let st = self.st.borrow();
        let (mut c, mut e, mut r) = (st.cnt, st.en, st.run);
        if self.per_cpu() {
            let now = crate::timer::uptime_ns();
            let (dc, dt) = (self.source_cpu(now).wrapping_sub(st.cpu_c), now.wrapping_sub(st.cpu_t));
            if act {
                c = c.wrapping_add(dc);
                r += dt;
            }
            if st.on {
                e += dt;
            }
            return (c, e, r);
        }
        for k in &st.tasks {
            let Some(t) = proc::find_thread(k.tid) else { continue };
            let now = proc::cpu_ns(t);
            let (dc, dt) = (self.source(t, now).wrapping_sub(k.c), now.wrapping_sub(k.t));
            // CPU を決めたもの: スレッドがいまその CPU にいるときの差だけ
            let here = self.cpu < 0 || t.cpu == self.cpu as usize;
            if act && here {
                c = c.wrapping_add(dc);
                r += dt;
            }
            if st.on {
                e += dt;
            }
        }
        (c, e, r)
    }

    /// 差を足して写しなおす (on や、リーダーの on が変わる前に)
    fn sync(&self) {
        let (c, e, r) = self.values();
        let mut st = self.st.borrow_mut();
        (st.cnt, st.en, st.run) = (c, e, r);
        if self.per_cpu() {
            let now = crate::timer::uptime_ns();
            (st.cpu_c, st.cpu_t) = (self.source_cpu(now), now);
            return;
        }
        for k in st.tasks.iter_mut() {
            if let Some(t) = proc::find_thread(k.tid) {
                let now = proc::cpu_ns(t);
                (k.c, k.t) = (self.source(t, now), now);
            }
        }
    }

    /// 有効にする / 止める (リーダーならメンバーも写してから)
    fn set_on(&self, on: bool) {
        if self.st.borrow().on == on {
            return;
        }
        let members = self.members();
        for m in &members {
            m.sync();
        }
        self.sync();
        self.st.borrow_mut().on = on;
        self.apply_hw();
        for m in &members {
            m.apply_hw();
        }
        if on {
            if self.active() {
                self.start();
            }
            for m in &members {
                if m.active() {
                    m.start();
                }
            }
        }
    }

    /// task-clock か cpu-clock か (Linux では hrtimer で鳴らすもの)
    fn is_clock(&self) -> bool {
        self.ty == TYPE_SOFTWARE && matches!(self.config, SW_CPU_CLOCK | SW_TASK_CLOCK)
    }

    /// 動きはじめた (Linux の perf_swevent_add の set_period と、時計の hrtimer を始めるところ)
    fn start(&self) {
        if !self.sampling {
            return;
        }
        if self.is_clock() {
            self.restart_clock();
            return;
        }
        self.last_period.set(self.period.get());
        self.set_period();
    }

    /// 時計のものの間をいまから数えなおす (この CPU で数えているものなら、間ののちにタイマを鳴らす)
    fn restart_clock(&self) {
        let p = proc::current();
        let cpu = crate::smp::id() as i32;
        let now = if self.per_cpu() {
            (self.cpu == cpu).then(crate::timer::uptime_ns)
        } else {
            (self.tracks(p.pid) && (self.cpu < 0 || self.cpu == cpu)).then(|| proc::cpu_ns(p))
        };
        match now {
            Some(t) => {
                self.last_clock.set(t);
                // Linux の hrtimer も 10 us より短くしない
                crate::timer::hr_arm(self.period.get().max(10_000));
            }
            None => self.last_clock.set(0),
        }
    }

    /// 間ののこりを次の間に (Linux の perf_swevent_set_period): のこりが負でなければ、いくつあふれたか。次の間は
    /// 前の間 (last_period) で数え、last_period を今の間にする
    fn set_period(&self) -> u64 {
        let period = self.last_period.get().max(1);
        self.last_period.set(self.period.get());
        let val = self.left.get();
        if val < 0 {
            return 0;
        }
        let nr = (period + val as u64) / period;
        self.left.set(val - (nr * period) as i64);
        nr
    }

    /// freq のものの間をなおす (Linux の perf_adjust_period と perf_calculate_period: nsec に count 回なら、1 秒に freq 回に
    /// するには count × 1e9 / (nsec × freq) 回ごと。いきなり変えずに差の 1/8 ずつ)
    fn adjust_period(&self, nsec: u64, count: u64) {
        let div = nsec as u128 * self.freq.get() as u128;
        let want = count as u128 * 1_000_000_000 / div.max(1);
        let want = want.min(i64::MAX as u128 / 2) as i64;
        let cur = self.period.get().min(i64::MAX as u64 / 2) as i64;
        let d = want - cur;
        let d = if d >= 0 { d + 7 } else { d - 7 } / 8;
        let n = match cur + d {
            0 => 1,
            n => n,
        };
        self.period.set(n as u64);
        if self.left.get() > n.saturating_mul(8) {
            self.left.set(0);
        }
    }

    /// PERF_EVENT_IOC_PERIOD (Linux の __perf_event_period): freq のものは freq を、ほかは間を変え、のこりを 0 に
    /// (時計のものは、いまから新しい間を数えなおす)
    fn change_period(&self, v: u64) {
        if self.freq.get() != 0 {
            self.freq.set(v);
        } else {
            self.period.set(v);
        }
        self.left.set(0);
        if self.is_clock() && self.active() {
            self.restart_clock();
        }
    }

    /// ブレークポイントなら、数えているスレッドの CPU の値を動いているかに合わせる (自分なら、戻る前に CPU にも)
    fn apply_hw(&self) {
        if self.bp.is_none() {
            return;
        }
        let on = self.active() && !self.bit(B_EXCLUDE_USER);
        let tids: Vec<u32> = self.st.borrow().tasks.iter().map(|k| k.tid).collect();
        for tid in tids {
            let Some(t) = proc::find_thread(tid) else { continue };
            if let Some(b) = t.hwdbg.as_mut().and_then(|s| s.perf.iter_mut().find(|b| b.id == self.id)) {
                b.ctrl.enabled = on;
            }
            reload(t);
        }
    }

    /// ブレークポイントを、スレッド t の State に (はじめは止めて。スロットがなければ ENOSPC)
    fn install(&self, t: &mut Proc) -> Result<(), i64> {
        let Some((watch, ty, addr, ctrl)) = self.bp else { return Ok(()) };
        let st = t.hwdbg.get_or_insert_with(Default::default);
        if st.used(watch) >= hwdebug::slots(watch) {
            return Err(-ENOSPC);
        }
        st.perf.push(hwdebug::PerfBp { id: self.id, watch, ty, addr, ctrl: hwdebug::Ctrl { enabled: false, ..ctrl }, count: 0 });
        Ok(())
    }

    /// スレッド t の State から外す
    fn uninstall(&self, t: &mut Proc) {
        if self.bp.is_some() {
            if let Some(st) = t.hwdbg.as_mut() {
                st.perf.retain(|b| b.id != self.id);
            }
            reload(t);
        }
    }

    fn reset(&self) {
        self.sync();
        self.st.borrow_mut().cnt = 0;
    }

    /// 自分とグループのみな (PERF_IOC_FLAG_GROUP: Linux の perf_event_for_each)
    fn group(self: &Rc<Self>) -> Vec<Rc<Event>> {
        let leader = self.leader().unwrap_or_else(|| self.clone());
        let mut v = alloc::vec![leader.clone()];
        v.extend(leader.members());
        v
    }

    fn wants_mmap(&self) -> bool {
        self.bit(B_MMAP) || self.bit(B_MMAP2) || self.bit(B_MMAP_DATA)
    }

    fn tracks(&self, tid: u32) -> bool {
        self.st.borrow().tasks.iter().any(|k| k.tid == tid)
    }

    fn untrack(&self, tid: u32) {
        self.sync();
        self.st.borrow_mut().tasks.retain(|k| k.tid != tid);
        if let Some(t) = proc::find_thread(tid) {
            self.uninstall(t);
        }
    }
}

/// いまのスレッドのものを変えたら、戻る前に CPU に (ほかのスレッドは、次に走るときに)
fn reload(t: &Proc) {
    if t.pid == proc::current().pid {
        hwdebug::switch_in(t);
    }
}

/// read で返す大きさ (Linux の __perf_event_read_size)
fn read_size(fmt: u64, nr_siblings: usize) -> usize {
    let mut entry = 8;
    let mut size = 0;
    let mut nr = 1;
    if fmt & FMT_TOTAL_TIME_ENABLED != 0 {
        size += 8;
    }
    if fmt & FMT_TOTAL_TIME_RUNNING != 0 {
        size += 8;
    }
    if fmt & FMT_ID != 0 {
        entry += 8;
    }
    if fmt & FMT_LOST != 0 {
        entry += 8;
    }
    if fmt & FMT_GROUP != 0 {
        nr += nr_siblings;
        size += 8;
    }
    size + nr * entry
}

/// perf の fd の中身 (Linux の perf_fops)
pub struct PerfFile(Rc<Event>);

impl PerfFile {
    fn read_into(&self, dst: &mut [u8]) -> Result<usize, i64> {
        let ev = &self.0;
        let fmt = ev.read_format;
        let leader = ev.leader().unwrap_or_else(|| ev.clone());
        let size = read_size(fmt, leader.members().len());
        if dst.len() < size {
            return Err(-ENOSPC);
        }
        let mut out: Vec<u64> = Vec::new();
        if fmt & FMT_GROUP != 0 {
            let all = ev.group();
            let (_, e, r) = leader.values();
            out.push(all.len() as u64);
            if fmt & FMT_TOTAL_TIME_ENABLED != 0 {
                out.push(e);
            }
            if fmt & FMT_TOTAL_TIME_RUNNING != 0 {
                out.push(r);
            }
            for m in &all {
                out.push(m.values().0);
                if fmt & FMT_ID != 0 {
                    out.push(m.id);
                }
                if fmt & FMT_LOST != 0 {
                    out.push(0);
                }
            }
        } else {
            let (c, e, r) = ev.values();
            out.push(c);
            if fmt & FMT_TOTAL_TIME_ENABLED != 0 {
                out.push(e);
            }
            if fmt & FMT_TOTAL_TIME_RUNNING != 0 {
                out.push(r);
            }
            if fmt & FMT_ID != 0 {
                out.push(ev.id);
            }
            if fmt & FMT_LOST != 0 {
                out.push(0);
            }
        }
        for (i, v) in out.iter().enumerate() {
            dst[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        Ok(out.len() * 8)
    }

    fn ioctl_inner(&self, req: u64, arg: usize) -> R {
        let ev = &self.0;
        let me = proc::current();
        let each = |f: &dyn Fn(&Event)| {
            if arg as u64 & IOC_FLAG_GROUP != 0 {
                for e in ev.group() {
                    f(&e);
                }
            } else {
                f(ev);
            }
        };
        match req {
            IOC_ENABLE => each(&|e| e.set_on(true)),
            IOC_DISABLE => each(&|e| e.set_on(false)),
            IOC_RESET => each(&|e| e.reset()),
            // サンプリングのもの (止まるまでの数を足す) だけ
            IOC_REFRESH => {
                if ev.bit(B_INHERIT) || !ev.sampling {
                    return Err(-EINVAL);
                }
                ev.limit.set(ev.limit.get() + arg as i32 as i64);
                ev.set_on(true);
            }
            IOC_PERIOD => {
                let mut b = [0u8; 8];
                me.pt().copy_in(&mut b, arg).ok_or(-EFAULT)?;
                let v = u64::from_le_bytes(b);
                // freq のもの (時計のものは freq をやめている) は freq の上限まで、ほかは間 (上のビットはだめ)
                let freq = ev.freq.get() != 0;
                if !ev.sampling || v == 0 || (freq && v > MAX_SAMPLE_RATE.load(Relaxed) as u64) || (!freq && v >> 63 != 0) {
                    return Err(-EINVAL);
                }
                ev.change_period(v);
            }
            // 輪 (mmap) はまだないので、出し先を決めるだけ (-1 か perf の fd)
            IOC_SET_OUTPUT => {
                if arg as i32 != -1 {
                    let f = me.files().get(arg as u32 as u64).cloned().ok_or(-EBADF)?;
                    f.borrow().kind.ops::<PerfFile>().ok_or(-EBADF)?;
                }
            }
            // フィルタはトレースポイントと番地のフィルタのあるものだけ (ここにはない)
            IOC_SET_FILTER => {
                let mut b = [0u8; 1];
                me.pt().copy_in(&mut b, arg).ok_or(-EFAULT)?;
                return Err(-EINVAL);
            }
            IOC_ID => {
                me.pt().copy_out(arg, &ev.id.to_le_bytes()).ok_or(-EFAULT)?;
            }
            // BPF はない: fd がなければ EBADF、BPF のものでなければ EINVAL
            IOC_SET_BPF => {
                me.files().get(arg as u32 as u64).ok_or(-EBADF)?;
                return Err(-EINVAL);
            }
            IOC_PAUSE_OUTPUT | IOC_QUERY_BPF => return Err(-EINVAL),
            // 変えられるのはブレークポイントのものだけ
            IOC_MODIFY_ATTRIBUTES => {
                let (a, _) = copy_attr(arg)?;
                if u32::from_le_bytes(a[0..4].try_into().unwrap()) != TYPE_SOFTWARE {
                    return Err(-EINVAL);
                }
                return Err(-EOPNOTSUPP);
            }
            _ => return Err(-ENOTTY),
        }
        Ok(0)
    }
}

impl PerfFile {
    /// 輪を写す (Linux の perf_mmap): MAP_SHARED、位置 0、1 + 2^n ページ。inherit のスレッドのものはだめ。2 度目は
    /// 同じ大きさのときだけ (同じ輪)
    fn mmap_inner(&self, fr: &file::FileRef, off: usize, len: usize, prot: u64) -> Result<crate::vm::Backing, i64> {
        reap();
        let ev = &self.0;
        if off != 0 || ev.bit(B_INHERIT) {
            return Err(-EINVAL);
        }
        let n = len.div_ceil(PG);
        if n == 0 || (n - 1 != 0 && !(n - 1).is_power_of_two()) {
            return Err(-EINVAL);
        }
        let nr = n - 1;
        if let Some(rb) = ev.rb.borrow().as_ref() {
            if rb.nr != nr {
                return Err(-EINVAL);
            }
            return Ok(crate::vm::Backing::Pages { pages: rb.pages.clone(), off: 0, hold: Some(fr.clone()) });
        }
        let mut pages = Vec::with_capacity(n);
        for _ in 0..n {
            match crate::kalloc::alloc() {
                Some(p) => pages.push(p),
                None => {
                    for p in pages {
                        crate::kalloc::free(p);
                    }
                    return Err(-12);
                }
            }
        }
        let size = (nr * PG) as u64;
        let wm = if ev.bit(B_WATERMARK) && ev.watermark != 0 { (ev.watermark as u64).min(size) } else { size / 2 };
        let rb = Rc::new(Rb {
            pages: Rc::new(pages),
            nr,
            overwrite: prot & 2 == 0,
            head: Cell::new(0),
            lost: Cell::new(0),
            wakeup: Cell::new(0),
            events: Cell::new(0),
            watermark: wm,
            poll: Cell::new(0),
            generation: Cell::new(0),
        });
        // 頭のページ (perf_event_init_userpage): cap_bit0_is_deprecated、size (__reserved の位置)、data_offset と data_size
        rb.put_hdr(40, 2);
        rb.put_hdr32(72, 96);
        rb.put_hdr(1040, PG as u64);
        rb.put_hdr(1048, size);
        let pages = rb.pages.clone();
        *ev.rb.borrow_mut() = Some(rb);
        Ok(crate::vm::Backing::Pages { pages, off: 0, hold: Some(fr.clone()) })
    }

    fn chan(&self) -> usize {
        Rc::as_ptr(&self.0) as usize + 1
    }
}

impl file::FileOps for PerfFile {
    fn describe(&self) -> alloc::string::String {
        "anon_inode:[perf_event]".into()
    }
    fn read(&self, dst: &mut [u8], _nonblock: bool) -> Result<usize, i64> {
        self.read_into(dst)
    }
    fn write(&self, _src: &[u8], _nonblock: bool) -> Result<usize, i64> {
        Err(-EINVAL)
    }
    fn ioctl(&self, req: u64, arg: usize, _nonblock: bool) -> Option<Result<i64, i64>> {
        Some(self.ioctl_inner(req, arg))
    }
    /// Linux の perf_poll: 数える相手がもういなければ POLLHUP。輪があれば、起こしの印 (POLLIN) をとって返す。なければ POLLHUP
    fn poll_mask(&self) -> Option<u32> {
        let ev = &self.0;
        if !ev.per_cpu() && ev.st.borrow().tasks.is_empty() {
            return Some(file::POLLHUP);
        }
        Some(ev.rb.borrow().as_ref().map_or(file::POLLHUP, |rb| rb.poll.replace(0)))
    }
    fn poll_keys(&self) -> Option<Vec<usize>> {
        Some(alloc::vec![self.chan()])
    }
    fn event_gen(&self) -> u64 {
        self.0.rb.borrow().as_ref().map_or(0, |rb| rb.generation.get())
    }
    fn mmap(&self, fr: &file::FileRef, off: usize, len: usize, prot: u64) -> Option<Result<crate::vm::Backing, i64>> {
        Some(self.mmap_inner(fr, off, len, prot))
    }
    fn mmap_shared_only(&self) -> bool {
        true
    }
    /// O_ASYNC (Linux の perf_fasync): あふれるたびに持ち主に知らせる
    fn fasync_key(&self) -> Option<usize> {
        Some(fasync_key(&self.0))
    }
    fn release(&self) {
        let ev = &self.0;
        // リーダーなら、メンバーはひとりだちする (Linux の perf_group_detach)
        for m in ev.members() {
            m.sync();
            *m.leader.borrow_mut() = None;
        }
        ev.siblings.borrow_mut().clear();
        if let Some(l) = ev.leader() {
            l.siblings.borrow_mut().retain(|w| w.upgrade().is_some_and(|m| !Rc::ptr_eq(&m, ev)));
        }
        let tids: Vec<u32> = ev.st.borrow().tasks.iter().map(|k| k.tid).collect();
        for tid in tids {
            if let Some(t) = proc::find_thread(tid) {
                ev.uninstall(t);
            }
        }
        let p = Rc::as_ptr(ev);
        EVENTS.get().retain(|w| w.strong_count() > 0 && w.as_ptr() != p);
        if ev.sampling {
            proc::PERF_SAMPLING.fetch_sub(1, Relaxed);
        }
        if ev.wants_mmap() {
            proc::PERF_MMAP.fetch_sub(1, Relaxed);
        }
        *ev.rb.borrow_mut() = None;
    }
}

// ---- perf_event_open ----

fn le64(a: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(a[o..o + 8].try_into().unwrap())
}

/// attr を読む (Linux の perf_copy_attr): size が 0 なら VER0、VER0 より小さいかページより大きいと、size に知っている
/// 大きさを書いて E2BIG。知っているより大きければ、その先は 0 でなければならない (E2BIG)。予約のビットは EINVAL。
/// (attr、size)
fn copy_attr(uattr: usize) -> Result<([u8; ATTR_SIZE], usize), i64> {
    let me = proc::current();
    let mut sz = [0u8; 4];
    me.pt().copy_in(&mut sz, uattr + 4).ok_or(-EFAULT)?;
    let mut size = u32::from_le_bytes(sz) as usize;
    if size == 0 {
        size = ATTR_VER0;
    }
    let too_big = |me: &mut Proc| {
        let _ = me.pt().copy_out(uattr + 4, &(ATTR_SIZE as u32).to_le_bytes());
        Err(-E2BIG)
    };
    if !(ATTR_VER0..=4096).contains(&size) {
        return too_big(me);
    }
    let mut a = [0u8; ATTR_SIZE];
    me.pt().copy_in(&mut a[..size.min(ATTR_SIZE)], uattr).ok_or(-EFAULT)?;
    if size > ATTR_SIZE {
        let mut rest = alloc::vec![0u8; size - ATTR_SIZE];
        me.pt().copy_in(&mut rest, uattr + ATTR_SIZE).ok_or(-EFAULT)?;
        if rest.iter().any(|&b| b != 0) {
            return too_big(me);
        }
    }
    let bits = le64(&a, 40);
    let sample_type = le64(&a, 24);
    let reserved2 = u16::from_le_bytes([a[110], a[111]]);
    let reserved3 = u32::from_le_bytes(a[116..120].try_into().unwrap()) >> 3;
    let reserved4 = u16::from_le_bytes([a[174], a[175]]);
    if bits >> 40 != 0 || reserved2 != 0 || reserved3 != 0 || reserved4 != 0 {
        return Err(-EINVAL);
    }
    if sample_type & !(SAMPLE_MAX - 1) != 0 || le64(&a, 32) & !(FMT_MAX - 1) != 0 {
        return Err(-EINVAL);
    }
    // 枝の記録 (ソフトウェアのものにはないので、あとで EOPNOTSUPP)、レジスタ、スタック
    if sample_type & SAMPLE_BRANCH_STACK != 0 && le64(&a, 72) == 0 {
        return Err(-EINVAL);
    }
    if sample_type & SAMPLE_REGS_USER != 0 && (le64(&a, 80) == 0 || le64(&a, 80) >> 33 != 0) {
        return Err(-EINVAL);
    }
    if sample_type & SAMPLE_STACK_USER != 0 {
        let s = u32::from_le_bytes(a[88..92].try_into().unwrap());
        if s >= 0xffff || s % 8 != 0 {
            return Err(-EINVAL);
        }
    }
    if sample_type & SAMPLE_REGS_INTR != 0 && (le64(&a, 96) == 0 || le64(&a, 96) >> 33 != 0) {
        return Err(-EINVAL);
    }
    if sample_type & SAMPLE_WEIGHT != 0 && sample_type & SAMPLE_WEIGHT_STRUCT != 0 {
        return Err(-EINVAL);
    }
    let b = |n: u32| bits >> n & 1 != 0;
    if (!b(B_INHERIT) && b(B_INHERIT_THREAD)) || (b(B_REMOVE_ON_EXEC) && b(B_ENABLE_ON_EXEC)) || (b(B_SIGTRAP) && !b(B_REMOVE_ON_EXEC)) {
        return Err(-EINVAL);
    }
    Ok((a, size))
}

/// ほかのスレッドを数えてよいか (Linux の perf_check_permission: CAP_PERFMON か、ptrace の PTRACE_MODE_READ_REALCREDS)
fn may_observe(t: &Proc) -> bool {
    let me = proc::current();
    if perfmon_capable() || t.tgid == me.tgid {
        return true;
    }
    let (c, tc) = (&me.cred, &t.cred);
    let ids = [tc.uid, tc.euid, tc.suid].iter().all(|&u| u == c.uid) && [tc.gid, tc.egid, tc.sgid].iter().all(|&g| g == c.gid);
    (ids && crate::pidfd::dumpable(t) == 1 || c.capable(crate::cred::CAP_SYS_PTRACE)) && crate::security::ptrace_access_check(tc, crate::security::PTRACE_MODE_READ).is_ok()
}

fn open(uattr: usize, pid: i32, cpu: i32, group_fd: i32, flags: u64) -> R {
    if flags & !FLAG_ALL != 0 {
        return Err(-EINVAL);
    }
    let (a, _) = copy_attr(uattr)?;
    let ty = u32::from_le_bytes(a[0..4].try_into().unwrap());
    let config = le64(&a, 8);
    let period = le64(&a, 16);
    let sample_type = le64(&a, 24);
    let read_format = le64(&a, 32);
    let bits = le64(&a, 40);
    let b = |n: u32| bits >> n & 1 != 0;
    if !b(B_EXCLUDE_KERNEL) || b(B_TEXT_POKE) || (sample_type & SAMPLE_CALLCHAIN != 0 && !b(B_EXCLUDE_CALLCHAIN_KERNEL)) {
        allow_kernel()?;
    }
    if b(B_NAMESPACES) && !perfmon_capable() {
        return Err(-EACCES);
    }
    if b(B_FREQ) {
        if period > MAX_SAMPLE_RATE.load(Relaxed) as u64 {
            return Err(-EINVAL);
        }
    } else if period >> 63 != 0 {
        return Err(-EINVAL);
    }
    if sample_type & SAMPLE_PHYS_ADDR != 0 {
        allow_kernel()?;
    }
    if flags & FLAG_PID_CGROUP != 0 && (pid == -1 || cpu == -1) {
        return Err(-EINVAL);
    }
    let me = proc::current();
    // グループのリーダー (perf の fd でなければ EBADF)
    let mut leader = None;
    if group_fd != -1 {
        let f = me.files().get(group_fd as u32 as u64).cloned().ok_or(-EBADF)?;
        let l = f.borrow().kind.ops::<PerfFile>().ok_or(-EBADF)?;
        // 数える相手が終わったもの (Linux の PERF_EVENT_STATE_EXIT。CPU ごとのものは終わらない)
        if !l.0.per_cpu() && l.0.st.borrow().tasks.is_empty() {
            return Err(-ENODEV);
        }
        if flags & FLAG_FD_NO_GROUP == 0 {
            leader = Some(l.0.clone());
        }
    }
    // 相手のスレッド (0 は自分。終わったものは ESRCH)
    let task = if pid != -1 && flags & FLAG_PID_CGROUP == 0 {
        let t = if pid == 0 {
            Some(proc::current())
        } else {
            crate::ns::to_global(pid as u32).and_then(proc::find_thread)
        };
        Some(t.filter(|t| t.state != proc::State::Zombie).ok_or(-ESRCH)?)
    } else {
        None
    };
    if let (Some(_), Some(l)) = (&task, &leader) {
        if l.bit(B_INHERIT) != b(B_INHERIT) {
            return Err(-EINVAL);
        }
    }
    // perf_event_alloc: CPU の番号、シグナルは相手のスレッドに
    let ncpu = crate::smp::online() as i32;
    if (cpu < 0 || cpu >= ncpu) && (task.is_none() || cpu != -1) {
        return Err(-EINVAL);
    }
    if b(B_SIGTRAP) && task.is_none() {
        return Err(-EINVAL);
    }
    if b(B_INHERIT) && sample_type & SAMPLE_READ != 0 && sample_type & SAMPLE_TID == 0 {
        return Err(-EINVAL);
    }
    // 種類 (perf_init_event): ソフトウェアとブレークポイント。PMU (ハードウェア、キャッシュ、RAW) もトレースポイントもない
    let mut bp = None;
    match ty {
        TYPE_SOFTWARE if config >= SW_MAX => return Err(-ENOENT),
        TYPE_SOFTWARE | TYPE_BREAKPOINT if sample_type & SAMPLE_BRANCH_STACK != 0 => return Err(-EOPNOTSUPP),
        TYPE_SOFTWARE => {}
        // Linux の hw_breakpoint_event_init: スロットをとって (EMPTY と INVALID は EINVAL、スレッドの ptrace と perf の
        // ものを合わせてスロットの数をこえると ENOSPC)、番地と長さを hwdebug::parse で
        TYPE_BREAKPOINT => {
            let bty = u32::from_le_bytes(a[52..56].try_into().unwrap());
            let (baddr, blen) = (le64(&a, 56), le64(&a, 64));
            if bty == 0 || bty >= 7 {
                return Err(-EINVAL);
            }
            let watch = bty & 3 != 0;
            if let Some(t) = &task
                && t.hwdbg.as_ref().map_or(0, |s| s.used(watch)) >= hwdebug::slots(watch)
            {
                return Err(-ENOSPC);
            }
            let len = if blen > 8 { 0 } else { blen as u8 };
            let (addr, ctrl) = hwdebug::parse(&hwdebug::Attr { addr: baddr, len, ty: bty as u8, disabled: false })?;
            bp = Some((watch, bty as u8, addr, ctrl));
        }
        _ => return Err(-ENOENT),
    }
    // cgroup の perf (CGROUP_PERF) はない: Linux の perf_cgroup_connect のないものと同じ
    if flags & FLAG_PID_CGROUP != 0 {
        return Err(-EINVAL);
    }
    if b(B_USE_CLOCKID) {
        let clk = i32::from_le_bytes(a[92..96].try_into().unwrap());
        if ![0, 1, 4, 7, 11].contains(&clk) {
            return Err(-EINVAL);
        }
    }
    if let Some(t) = &task {
        if !may_observe(t) {
            return Err(-EACCES);
        }
    } else {
        allow_cpu()?;
    }
    // グループのリーダーは、同じ相手 (スレッドか CPU) を数えるもの
    let target = task.as_ref().map_or(0, |t| t.pid);
    if let Some(l) = &leader {
        if l.leader().is_some() || l.target != target || (target == 0 && l.cpu != cpu) || b(B_EXCLUSIVE) || b(B_PINNED) {
            return Err(-EINVAL);
        }
        if read_size(read_format, l.members().len() + 1) > 16 * 1024 {
            return Err(-E2BIG);
        }
    }
    // サンプルの間: 時計のものは freq を 1e9 / freq ns の決まった間に (Linux の perf_swevent_init_hrtimer: freq をやめる)。
    // ほかの freq のものは 1 から、数と時間を見てなおしていく (perf_event_alloc と perf_adjust_period)
    let clock = ty == TYPE_SOFTWARE && matches!(config, SW_CPU_CLOCK | SW_TASK_CLOCK);
    let (speriod, sfreq) = match (b(B_FREQ), period) {
        (_, 0) => (0, 0),
        (true, f) if clock => ((1_000_000_000 / f).max(1), 0),
        (true, f) => (1, f),
        (false, p) => (p, 0),
    };
    let bp_addr = le64(&a, 56);
    let ev = Rc::new(Event {
        id: NEXT_ID.fetch_add(1, Relaxed),
        ty,
        config,
        bp,
        read_format,
        bits,
        sample_type,
        sampling: period != 0,
        period: Cell::new(speriod),
        last_period: Cell::new(speriod),
        left: Cell::new(speriod as i64),
        freq: Cell::new(sfreq),
        freq_time: Cell::new(0),
        freq_stamp: Cell::new(0),
        hits: Cell::new(0),
        last_clock: Cell::new(0),
        interrupts: Cell::new(0),
        int_seq: Cell::new(0),
        wakeup_events: u32::from_le_bytes(a[48..52].try_into().unwrap()),
        watermark: u32::from_le_bytes(a[48..52].try_into().unwrap()),
        sig_data: le64(&a, 120),
        regs_user: le64(&a, 80),
        stack_user: u32::from_le_bytes(a[88..92].try_into().unwrap()),
        regs_intr: le64(&a, 96),
        max_stack: match u16::from_le_bytes([a[108], a[109]]) {
            0 => MAX_STACK.load(Relaxed) as u16,
            n => n,
        },
        bp_addr,
        rb: RefCell::new(None),
        target,
        cpu,
        owner: me.pid,
        enable_on_exec: Cell::new(b(B_ENABLE_ON_EXEC)),
        limit: Cell::new(0),
        st: RefCell::new(St { on: false, cpu_c: 0, cpu_t: 0, cnt: 0, en: 0, run: 0, tasks: Vec::new() }),
        leader: RefCell::new(leader.as_ref().map(Rc::downgrade)),
        siblings: RefCell::new(Vec::new()),
    });
    if let Some(task) = task {
        ev.install(task)?;
        let t = proc::cpu_ns(task);
        let c = ev.source(task, t);
        ev.st.borrow_mut().tasks.push(Task { tid: task.pid, c, t });
    } else {
        ev.sync();
    }
    if !b(B_DISABLED) {
        ev.st.borrow_mut().on = true;
        if ev.active() {
            ev.start();
        }
    }
    ev.apply_hw();
    let fl = file::O_RDWR;
    let fd = proc::current().files().add(file::new(Kind::Ops(Rc::new(PerfFile(ev.clone()))), fl), flags & FLAG_FD_CLOEXEC != 0, 0).ok_or(-EMFILE)?;
    if let Some(l) = &leader {
        l.siblings.borrow_mut().push(Rc::downgrade(&ev));
    }
    EVENTS.get().push(Rc::downgrade(&ev));
    if ev.sampling {
        proc::PERF_SAMPLING.fetch_add(1, Relaxed);
    }
    if ev.wants_mmap() {
        proc::PERF_MMAP.fetch_add(1, Relaxed);
    }
    Ok(fd as i64)
}

/// PERF_RECORD_MMAP / MMAP2 (Linux の perf_event_mmap_output): 実行できるものは attr.mmap か mmap2、ほかは mmap_data
/// (MMAP_DATA の印)。pgoff はバイト、名前は 8 バイトにそろえて 0 で終わる
fn mmap_record(ev: &Event, r: &crate::vm::MapRec, name: &str) {
    let exec = r.prot & 4 != 0;
    if !(if exec { ev.bit(B_MMAP) || ev.bit(B_MMAP2) } else { ev.bit(B_MMAP_DATA) }) {
        return;
    }
    let rb = ev.rb.borrow().clone();
    let Some(rb) = rb else { return };
    let me = proc::current();
    let l = crate::ns::local_or_0;
    // build id (attr.build_id と mmap2: Linux の build_id_parse。あれば maj / min / ino のかわりに)
    let bid = if ev.bit(B_MMAP2) && ev.bit(B_BUILD_ID) { r.file.as_ref().and_then(build_id) } else { None };
    let misc = MISC_USER | if exec { 0 } else { MISC_MMAP_DATA } | if bid.is_some() { MISC_MMAP_BUILD_ID } else { 0 };
    let mut v = header(if ev.bit(B_MMAP2) { REC_MMAP2 } else { REC_MMAP }, misc);
    put32(&mut v, l(me.tgid));
    put32(&mut v, l(me.pid));
    put64(&mut v, r.start as u64);
    put64(&mut v, (r.end - r.start) as u64);
    put64(&mut v, r.pgoff);
    if ev.bit(B_MMAP2) {
        if let Some((n, id)) = bid {
            v.extend_from_slice(&[n, 0, 0, 0]);
            v.extend_from_slice(&id);
        } else {
            put32(&mut v, r.dev.0);
            put32(&mut v, r.dev.1);
            put64(&mut v, r.ino);
            put64(&mut v, 0);
        }
        put32(&mut v, r.prot as u32);
        put32(&mut v, if r.shared { 1 } else { 2 });
    }
    v.extend_from_slice(name.as_bytes());
    v.push(0);
    while v.len() % 8 != 0 {
        v.push(0);
    }
    if ev.bit(B_SAMPLE_ID_ALL) {
        sample_id(ev, &mut v);
    }
    fix_size(&mut v);
    rb.output(ev, &v, false, Rc::as_ptr(&read_event(ev)) as usize + 1);
}

/// MMAP2 の印: maj / min / ino のかわりに build id (Linux の PERF_RECORD_MISC_MMAP_BUILD_ID)
const MISC_MMAP_BUILD_ID: u16 = 0x4000;

/// ELF の NT_GNU_BUILD_ID (Linux の lib/buildid.c: ファイルのはじめの ELF ヘッダから、プログラムヘッダの PT_NOTE を見る。
/// 実行ファイルか共有ライブラリだけ)。(大きさ, 20 バイト (のこりは 0))
fn build_id(ino: &crate::vfs::InodeRef) -> Option<(u8, [u8; 20])> {
    let rd = |off: usize, n: usize| -> Option<Vec<u8>> {
        let mut b = alloc::vec![0u8; n];
        (ino.read_at(off, &mut b).ok()? == n).then_some(b)
    };
    let eh = rd(0, 64)?;
    // \x7fELF、64 ビット、ET_EXEC か ET_DYN
    if eh[..4] != *b"\x7fELF" || eh[4] != 2 || !matches!(u16::from_le_bytes([eh[16], eh[17]]), 2 | 3) {
        return None;
    }
    let phoff = le64(&eh, 32) as usize;
    let phentsize = u16::from_le_bytes([eh[54], eh[55]]) as usize;
    let phnum = u16::from_le_bytes([eh[56], eh[57]]) as usize;
    for i in 0..phnum.min(64) {
        let ph = rd(phoff + i * phentsize, 56)?;
        // PT_NOTE
        if u32::from_le_bytes(ph[..4].try_into().ok()?) != 4 {
            continue;
        }
        let (off, size) = (le64(&ph, 8) as usize, (le64(&ph, 32) as usize).min(4096));
        let notes = rd(off, size)?;
        let mut o = 0;
        while o + 12 <= notes.len() {
            let w = |k: usize| u32::from_le_bytes(notes[o + k..o + k + 4].try_into().unwrap()) as usize;
            let (namesz, descsz, typ) = (w(0), w(4), w(8));
            let desc = o + 12 + namesz.next_multiple_of(4);
            // NT_GNU_BUILD_ID (3)、名前は "GNU"
            if typ == 3 && namesz == 4 && notes.get(o + 12..o + 16) == Some(b"GNU\0") && descsz > 0 && descsz <= 20 && desc + descsz <= notes.len() {
                let mut id = [0u8; 20];
                id[..descsz].copy_from_slice(&notes[desc..desc + descsz]);
                return Some((descsz as u8, id));
            }
            o = desc + descsz.next_multiple_of(4);
        }
    }
    None
}

// ---- 輪とサンプル (Linux の kernel/events/ring_buffer.c と perf_output_sample) ----

/// 写した輪: 頭のページと nr (0 か 2^n) のページ。head は書いたところ、lost はなくしたサンプルの数、wakeup と events
/// は起こすための数 (watermark のバイトごと、wakeup_events のサンプルごと)、poll は poll の印 (POLLIN)
pub struct Rb {
    pages: Rc<Vec<*mut u8>>,
    nr: usize,
    overwrite: bool,
    head: Cell<u64>,
    lost: Cell<u64>,
    wakeup: Cell<u64>,
    events: Cell<u32>,
    watermark: u64,
    poll: Cell<u32>,
    generation: Cell<u64>,
}

/// 写しが持っているかもしれないページ: だれも持たなくなってから返す (drm.rs の Bo と同じ)
static DEAD: Bkl<Vec<Rc<Vec<*mut u8>>>> = Bkl::new(Vec::new());

impl Drop for Rb {
    fn drop(&mut self) {
        DEAD.get().push(self.pages.clone());
    }
}

fn reap() {
    DEAD.get().retain(|p| {
        if Rc::strong_count(p) > 1 {
            return true;
        }
        for &pg in p.iter() {
            crate::kalloc::free(pg);
        }
        false
    });
}

impl Rb {
    fn put_hdr(&self, off: usize, v: u64) {
        unsafe { core::ptr::write_volatile(self.pages[0].add(off) as *mut u64, v) };
    }
    fn put_hdr32(&self, off: usize, v: u32) {
        unsafe { core::ptr::write_volatile(self.pages[0].add(off) as *mut u32, v) };
    }
    fn hdr(&self, off: usize) -> u64 {
        unsafe { core::ptr::read_volatile(self.pages[0].add(off) as *const u64) }
    }

    /// データの pos (ずっとふえる位置) から b を書く (終わりから頭へまわる)
    fn write_at(&self, mut pos: u64, b: &[u8]) {
        let size = (self.nr * PG) as u64;
        let mut i = 0;
        while i < b.len() {
            let o = (pos % size) as usize;
            let (pg, po) = (o / PG, o % PG);
            let n = (PG - po).min(b.len() - i);
            unsafe { core::ptr::copy_nonoverlapping(b.as_ptr().add(i), self.pages[1 + pg].add(po), n) };
            i += n;
            pos += n as u64;
        }
    }

    /// 記録を 1 つ (Linux の __perf_output_begin と perf_output_end): 前になくしたものがあれば先に PERF_RECORD_LOST。
    /// data_tail まで空きがなければなくす (上書きの写しでなければ)。watermark をこえたか、サンプルが wakeup_events に
    /// なったら起こす (poll の POLLIN)
    fn output(&self, ev: &Event, rec: &[u8], sample: bool, chan: usize) {
        if self.nr == 0 {
            self.lost.set(self.lost.get() + 1);
            return;
        }
        let mut lost_rec = Vec::new();
        if self.lost.get() != 0 {
            lost_rec = header(REC_LOST, 0);
            put64(&mut lost_rec, ev.id);
            put64(&mut lost_rec, self.lost.get());
            if ev.bit(B_SAMPLE_ID_ALL) {
                sample_id(ev, &mut lost_rec);
            }
            fix_size(&mut lost_rec);
        }
        let total = (lost_rec.len() + rec.len()) as u64;
        let size = (self.nr * PG) as u64;
        let head = self.head.get();
        let tail = self.hdr(1032);
        if !self.overwrite && head.wrapping_sub(tail) + total > size {
            self.lost.set(self.lost.get() + 1);
            return;
        }
        if !lost_rec.is_empty() {
            self.lost.set(0);
        }
        let w0 = self.wakeup.get();
        let nh = head + total;
        if nh - self.wakeup.get() > self.watermark {
            self.wakeup.set(self.wakeup.get() + self.watermark);
        }
        self.write_at(head, &lost_rec);
        self.write_at(head + lost_rec.len() as u64, rec);
        if sample && !ev.bit(B_WATERMARK) && ev.wakeup_events != 0 {
            self.events.set(self.events.get() + 1);
            if self.events.get() >= ev.wakeup_events {
                self.events.set(self.events.get() - ev.wakeup_events);
                self.wakeup.set(self.wakeup.get() + 1);
            }
        }
        self.head.set(nh);
        unsafe { core::arch::asm!("dmb ish") };
        self.put_hdr(1024, nh);
        if self.wakeup.get() != w0 {
            self.poll.set(file::POLLIN | file::POLLRDNORM);
            self.generation.set(self.generation.get() + 1);
            proc::wakeup(chan);
        }
    }
}

fn put64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
/// 記録の頭 (perf_event_header: type、misc、size はあとで fix_size)
fn header(ty: u32, misc: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(64);
    put32(&mut v, ty);
    v.extend_from_slice(&misc.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v
}
fn fix_size(v: &mut Vec<u8>) {
    let n = v.len() as u16;
    v[6..8].copy_from_slice(&n.to_le_bytes());
}

/// サンプルでない記録の終わりにつける sample_id (sample_id_all: TID、TIME、ID、STREAM_ID、CPU、IDENTIFIER)
fn sample_id(ev: &Event, v: &mut Vec<u8>) {
    let me = proc::current();
    let st = ev.sample_type;
    if st & SAMPLE_TID != 0 {
        put32(v, crate::ns::local_or_0(me.tgid));
        put32(v, crate::ns::local_or_0(me.pid));
    }
    if st & SAMPLE_TIME != 0 {
        put64(v, crate::timer::uptime_ns());
    }
    if st & SAMPLE_ID != 0 {
        put64(v, ev.id);
    }
    if st & SAMPLE_STREAM_ID != 0 {
        put64(v, ev.id);
    }
    if st & SAMPLE_CPU != 0 {
        put32(v, crate::smp::id() as u32);
        put32(v, 0);
    }
    if st & SAMPLE_IDENTIFIER != 0 {
        put64(v, ev.id);
    }
}

/// レジスタ (arm64 の perf_regs: x0〜x30、sp、pc) を abi (64 ビット: 2) とマスクの順に
fn regs(v: &mut Vec<u8>, tf: &crate::trap::TrapFrame, mask: u64) {
    put64(v, 2);
    for i in 0..33 {
        if mask >> i & 1 != 0 {
            put64(v, match i {
                0..=30 => tf.x[i],
                31 => tf.sp_el0,
                _ => tf.elr,
            });
        }
    }
}

/// PERF_RECORD_SAMPLE (Linux の perf_output_sample の順)。ip と addr はユーザーのもの
fn sample_record(ev: &Event, tf: &crate::trap::TrapFrame, addr: u64, period: u64) -> Vec<u8> {
    let me = proc::current();
    let st = ev.sample_type;
    let mut v = header(REC_SAMPLE, MISC_USER);
    if st & SAMPLE_IDENTIFIER != 0 {
        put64(&mut v, ev.id);
    }
    if st & SAMPLE_IP != 0 {
        put64(&mut v, tf.elr);
    }
    if st & SAMPLE_TID != 0 {
        put32(&mut v, crate::ns::local_or_0(me.tgid));
        put32(&mut v, crate::ns::local_or_0(me.pid));
    }
    if st & SAMPLE_TIME != 0 {
        put64(&mut v, crate::timer::uptime_ns());
    }
    if st & SAMPLE_ADDR != 0 {
        put64(&mut v, addr);
    }
    if st & SAMPLE_ID != 0 {
        put64(&mut v, ev.id);
    }
    if st & SAMPLE_STREAM_ID != 0 {
        put64(&mut v, ev.id);
    }
    if st & SAMPLE_CPU != 0 {
        put32(&mut v, crate::smp::id() as u32);
        put32(&mut v, 0);
    }
    if st & SAMPLE_PERIOD != 0 {
        put64(&mut v, period);
    }
    if st & SAMPLE_READ != 0 {
        let mut b = alloc::vec![0u8; 8 * (5 + 3 * 64)];
        let n = PerfFile(read_event(ev)).read_into(&mut b).unwrap_or(0);
        v.extend_from_slice(&b[..n]);
    }
    // コールチェーン (Linux の perf_callchain_user: pc から、x29 の frame record をたどる。上へ進まなければやめる)
    if st & SAMPLE_CALLCHAIN != 0 {
        let mut ips = alloc::vec![CONTEXT_USER, tf.elr];
        let mut fp = tf.x[29];
        while ips.len() <= ev.max_stack as usize && fp != 0 && fp & 7 == 0 {
            let mut b = [0u8; 16];
            if me.pt().copy_in(&mut b, fp as usize).is_none() {
                break;
            }
            let (next, lr) = (u64::from_le_bytes(b[..8].try_into().unwrap()), u64::from_le_bytes(b[8..].try_into().unwrap()));
            ips.push(lr);
            if next <= fp {
                break;
            }
            fp = next;
        }
        put64(&mut v, ips.len() as u64);
        for ip in ips {
            put64(&mut v, ip);
        }
    }
    // RAW (ソフトウェアのものにはない: 大きさ 4 と 0)
    if st & SAMPLE_RAW != 0 {
        put32(&mut v, 4);
        put32(&mut v, 0);
    }
    if st & SAMPLE_REGS_USER != 0 {
        regs(&mut v, tf, ev.regs_user);
    }
    // スタック (sp から stack_user バイト。読めないところは 0、読めた大きさを dyn_size に)
    if st & SAMPLE_STACK_USER != 0 {
        let size = ev.stack_user as usize;
        put64(&mut v, size as u64);
        if size != 0 {
            let mut b = alloc::vec![0u8; size];
            let mut got = 0;
            while got < size {
                let a = tf.sp_el0 as usize + got;
                let n = (PG - a % PG).min(size - got);
                if me.pt().copy_in(&mut b[got..got + n], a).is_none() {
                    break;
                }
                got += n;
            }
            v.extend_from_slice(&b);
            put64(&mut v, got as u64);
        }
    }
    if st & (SAMPLE_WEIGHT | SAMPLE_WEIGHT_STRUCT) != 0 {
        put64(&mut v, 0);
    }
    if st & SAMPLE_DATA_SRC != 0 {
        put64(&mut v, 0);
    }
    if st & SAMPLE_TRANSACTION != 0 {
        put64(&mut v, 0);
    }
    if st & SAMPLE_REGS_INTR != 0 {
        regs(&mut v, tf, ev.regs_intr);
    }
    if st & SAMPLE_PHYS_ADDR != 0 {
        put64(&mut v, 0);
    }
    if st & SAMPLE_DATA_PAGE_SIZE != 0 {
        put64(&mut v, if addr != 0 { PG as u64 } else { 0 });
    }
    if st & SAMPLE_CODE_PAGE_SIZE != 0 {
        put64(&mut v, PG as u64);
    }
    if st & SAMPLE_AUX != 0 {
        put64(&mut v, 0);
    }
    fix_size(&mut v);
    v
}

/// ev の Rc (サンプルの READ のため)
fn read_event(ev: &Event) -> Rc<Event> {
    live().into_iter().find(|e| e.id == ev.id).expect("perf: live event")
}

/// THROTTLE / UNTHROTTLE の記録 (Linux の perf_log_throttle: time、id、stream_id)
fn throttle_record(ev: &Rc<Event>, unthrottle: bool) {
    let rb = ev.rb.borrow().clone();
    let Some(rb) = rb else { return };
    let mut v = header(if unthrottle { REC_UNTHROTTLE } else { REC_THROTTLE }, 0);
    put64(&mut v, crate::timer::uptime_ns());
    put64(&mut v, ev.id);
    put64(&mut v, ev.id);
    if ev.bit(B_SAMPLE_ID_ALL) {
        sample_id(ev, &mut v);
    }
    fix_size(&mut v);
    rb.output(ev, &v, false, Rc::as_ptr(ev) as usize + 1);
}

/// グループのリーダーとメンバー
fn group_of(ev: &Rc<Event>) -> (Rc<Event>, Vec<Rc<Event>>) {
    let leader = ev.leader().unwrap_or_else(|| ev.clone());
    let members = leader.members();
    (leader, members)
}

/// グループをまとめて止める (Linux の perf_event_throttle_group: 記録はリーダーだけ)
fn throttle_group(ev: &Rc<Event>) {
    let (leader, members) = group_of(ev);
    for e in core::iter::once(&leader).chain(members.iter()) {
        if e.active() {
            e.interrupts.set(MAX_INTERRUPTS);
            if Rc::ptr_eq(e, &leader) {
                throttle_record(e, false);
            }
        }
    }
}

/// 止めたグループをまた動かす (Linux の perf_event_unthrottle_group)
fn unthrottle_group(ev: &Rc<Event>) {
    let (leader, members) = group_of(ev);
    for e in core::iter::once(&leader).chain(members.iter()) {
        if e.active() {
            e.interrupts.set(0);
            if e.is_clock() {
                e.restart_clock();
            }
            if Rc::ptr_eq(e, &leader) {
                throttle_record(e, true);
            }
        }
    }
}

/// あふれた (Linux の __perf_event_overflow): この tick のあふれを数え、throttle で上限に届いたら止める。freq なら間を
/// なおし、sigtrap なら SIGTRAP、輪があればサンプル (PERIOD は period) を書く。止めたか REFRESH の残りで止まったら true
fn overflow(ev: &Rc<Event>, tf: &crate::trap::TrapFrame, addr: u64, period: u64, throttle: bool) -> bool {
    let mut ret = false;
    // この tick のあふれの数 (Linux の __perf_event_account_interrupt)
    let seq = THROTTLE_SEQ[crate::smp::id()].load(Relaxed);
    if ev.int_seq.get() != seq {
        ev.int_seq.set(seq);
        ev.interrupts.set(1);
    } else {
        ev.interrupts.set(ev.interrupts.get().saturating_add(1));
    }
    if throttle && ev.interrupts.get() >= max_samples_per_tick() {
        throttle_group(ev);
        ret = true;
    }
    // 前のあふれから 2 tick より短ければ、その時間に last_period 回だったとして間をなおす
    if ev.freq.get() != 0 {
        let now = crate::timer::uptime_ns();
        let d = now.wrapping_sub(ev.freq_time.get()) as i64;
        ev.freq_time.set(now);
        if d > 0 && (d as u64) < 2 * crate::timer::TICK_NS {
            ev.adjust_period(d as u64, ev.last_period.get());
        }
    }
    // REFRESH の残りが 0 になったら止まって POLL_HUP (Linux の __perf_event_overflow の event_limit)
    let mut band = crate::fasync::POLL_IN;
    let left = ev.limit.get();
    if left != 0 {
        ev.limit.set(left - 1);
        if left == 1 {
            band = crate::fasync::POLL_HUP;
            ev.set_on(false);
            ret = true;
        }
    }
    if ev.bit(B_SIGTRAP) {
        let info = crate::signal::SigInfo {
            code: crate::signal::TRAP_PERF,
            addr: if ev.ty == TYPE_BREAKPOINT { ev.bp_addr } else { addr },
            value: ev.sig_data,
            status: ev.ty as i32,
            ..crate::signal::SigInfo::ZERO
        };
        crate::signal::send_thread(proc::current(), crate::signal::SIGTRAP, info);
    }
    let rb = ev.rb.borrow().clone();
    if let Some(rb) = rb {
        let rec = sample_record(ev, tf, addr, period);
        rb.output(ev, &rec, true, Rc::as_ptr(ev) as usize + 1);
    }
    // O_ASYNC の持ち主に (Linux の perf_event_wakeup の kill_fasync)
    crate::fasync::kill(fasync_key(ev), crate::signal::SIGIO, band);
    ret
}

/// O_ASYNC の表の鍵 (fasync.rs): イベントの場所 (inherit で数える子も同じイベント)
fn fasync_key(ev: &Event) -> usize {
    ev as *const Event as usize
}

/// ev のできごとが nr 回 (Linux の perf_swevent_event): PERIOD を読む決まった間のものは、間で割らずに 1 つのサンプル
/// (PERIOD は nr: 2011 年の「don't divide events if it has field period」)。間が 1 なら毎回。ほかは間ののこりが 0 に
/// 届いたら、届いた数だけ (PERIOD は前の間)
fn sw_hit(ev: &Rc<Event>, nr: u64, tf: &crate::trap::TrapFrame, addr: u64) {
    ev.hits.set(ev.hits.get().wrapping_add(nr));
    let freq = ev.freq.get() != 0;
    // 止めているあいだはあふれない (Linux の perf_swevent_overflow の MAX_INTERRUPTS)
    let throttled = ev.interrupts.get() == MAX_INTERRUPTS;
    if ev.sample_type & SAMPLE_PERIOD != 0 && !freq {
        if !throttled {
            overflow(ev, tf, addr, nr, false);
        }
        return;
    }
    let data = ev.last_period.get();
    if nr == 1 && ev.period.get() == 1 && !freq {
        if !throttled {
            overflow(ev, tf, addr, data, false);
        }
        return;
    }
    let left = ev.left.get() + nr as i64;
    ev.left.set(left);
    if left < 0 {
        return;
    }
    let n = ev.set_period();
    if throttled {
        return;
    }
    // 1 つめは止めず、続けてあふれたものは止める (Linux の perf_swevent_overflow)。止めたか REFRESH の残りで止まったら
    // そこまで
    for i in 0..n {
        if overflow(ev, tf, addr, data, i > 0) {
            break;
        }
    }
}

/// FORK / EXIT の記録 (attr.task: pid、ppid、tid、ptid、time)
fn task_record(ev: &Event, ty: u32, t: &Proc, parent_tgid: u32, parent_tid: u32) {
    let rb = ev.rb.borrow().clone();
    let Some(rb) = rb else { return };
    let l = crate::ns::local_or_0;
    let mut v = header(ty, 0);
    put32(&mut v, l(t.tgid));
    put32(&mut v, l(parent_tgid));
    put32(&mut v, l(t.pid));
    put32(&mut v, l(parent_tid));
    put64(&mut v, crate::timer::uptime_ns());
    if ev.bit(B_SAMPLE_ID_ALL) {
        sample_id(ev, &mut v);
    }
    fix_size(&mut v);
    rb.output(ev, &v, false, Rc::as_ptr(&read_event(ev)) as usize + 1);
}

fn live() -> Vec<Rc<Event>> {
    let v = EVENTS.get();
    if v.is_empty() {
        return Vec::new();
    }
    v.retain(|w| w.strong_count() > 0);
    v.iter().filter_map(|w| w.upgrade()).collect()
}

// ---- proc.rs から (fork、exit、exec、prctl) ----

pub struct Hooks;
pub static HOOKS: Hooks = Hooks;

impl proc::PerfHooks for Hooks {
    /// inherit: 親を数えているものは、子も (inherit_thread ならスレッドだけ。Linux の inherit_task_group)。attr.task で
    /// 有効なものには PERF_RECORD_FORK (親の文脈のもの: Linux の perf_event_fork)
    fn fork(&self, parent: &Proc, child: &mut Proc, thread: bool) {
        for ev in live() {
            if ev.bit(B_TASK) && ev.st.borrow().on && ev.tracks(parent.pid) {
                task_record(&ev, REC_FORK, child, parent.tgid, parent.pid);
            }
            if ev.bit(B_INHERIT) && (thread || !ev.bit(B_INHERIT_THREAD)) && ev.tracks(parent.pid) {
                // ブレークポイントは子にも置く (スロットがなければ数えない)
                if ev.install(child).is_err() {
                    continue;
                }
                // 子はまだ find_thread で見えないので、ここで有効に
                let on = ev.active() && !ev.bit(B_EXCLUDE_USER);
                if let Some(b) = child.hwdbg.as_mut().and_then(|s| s.perf.iter_mut().find(|b| b.id == ev.id)) {
                    b.ctrl.enabled = on;
                }
                let t = proc::cpu_ns(child);
                let c = ev.source(child, t);
                ev.st.borrow_mut().tasks.push(Task { tid: child.pid, c, t });
            }
        }
    }

    /// スレッドが終わる: そこまでの数を足して、もう数えない (Linux の perf_event_exit_task と sync_child_event)
    fn exit(&self, p: &Proc) {
        for ev in live() {
            if ev.tracks(p.pid) {
                if ev.bit(B_TASK) && ev.st.borrow().on {
                    task_record(&ev, REC_EXIT, p, p.ppid, p.ppid);
                }
                ev.untrack(p.pid);
            }
        }
    }

    /// exec: enable_on_exec は有効に (1 度だけ)、remove_on_exec はもう数えない (Linux の perf_event_exec)
    fn exec(&self, p: &Proc) {
        for ev in live() {
            if !ev.tracks(p.pid) {
                continue;
            }
            // PERF_RECORD_COMM (新しい名前、exec の印)
            if ev.bit(B_COMM) && ev.st.borrow().on && let Some(rb) = ev.rb.borrow().clone() {
                let l = crate::ns::local_or_0;
                let mut v = header(REC_COMM, MISC_COMM_EXEC);
                put32(&mut v, l(p.tgid));
                put32(&mut v, l(p.pid));
                let n = p.comm.iter().position(|&c| c == 0).unwrap_or(p.comm.len());
                v.extend_from_slice(&p.comm[..n]);
                v.push(0);
                while v.len() % 8 != 0 {
                    v.push(0);
                }
                if ev.bit(B_SAMPLE_ID_ALL) {
                    sample_id(&ev, &mut v);
                }
                fix_size(&mut v);
                rb.output(&ev, &v, false, Rc::as_ptr(&ev) as usize + 1);
            }
            // 新しいプログラムの領域の PERF_RECORD_MMAP (Linux は exec で写すたびに: ここではまとめて番地の順に)
            if ev.wants_mmap() && ev.st.borrow().on {
                let names = proc::map_names(proc::current());
                for r in proc::current().pt().map_recs(names, None) {
                    mmap_record(&ev, &r, &r.name);
                }
            }
            if ev.bit(B_REMOVE_ON_EXEC) {
                ev.untrack(p.pid);
            } else if ev.enable_on_exec.replace(false) {
                ev.set_on(true);
            }
        }
    }

    /// 自分が開いたものをみな止める / 動かす (Linux の perf_event_task_disable / enable)
    fn task_enable(&self, on: bool) -> i64 {
        let me = proc::current().pid;
        for ev in live() {
            if ev.owner == me {
                ev.set_on(on);
            }
        }
        0
    }

    /// ページフォルト (EL0 から): そのフォルトを数えるサンプリングのもの (ユーザーを数えるもの)
    fn sw_event(&self, config: u64, tf: &crate::trap::TrapFrame, addr: u64) {
        let me = proc::current().pid;
        let cpu = crate::smp::id() as i32;
        for ev in live() {
            let mine = if ev.per_cpu() { ev.cpu == cpu } else { ev.tracks(me) && (ev.cpu < 0 || ev.cpu == cpu) };
            if ev.sampling && ev.ty == TYPE_SOFTWARE && ev.config == config && !ev.bit(B_EXCLUDE_USER) && ev.active() && mine {
                sw_hit(&ev, 1, tf, addr);
            }
        }
    }

    /// EL0 で鳴ったタイマ (tick か、perf が置いた期限): task-clock と cpu-clock のサンプリングのものは、前のサンプルから
    /// 間だけ走ったらあふれ、次の期限が tick より前ならタイマをそのときに (Linux の perf_swevent_hrtimer)。tick なら freq の
    /// ものの間を、この tick に見た数でなおす (Linux の perf_adjust_freq_unthr_events)
    fn tick(&self, tf: &crate::trap::TrapFrame, real: bool) {
        let p = proc::current();
        let cpu = crate::smp::id() as i32;
        let mut next = u64::MAX;
        // tick ごとにあふれの数を数えなおし、止めたものをまた動かす (Linux の perf_event_task_tick)
        if real {
            THROTTLE_SEQ[cpu as usize].fetch_add(1, Relaxed);
        }
        for ev in live() {
            let mine = if ev.per_cpu() { ev.cpu == cpu } else { ev.tracks(p.pid) && (ev.cpu < 0 || ev.cpu == cpu) };
            if !ev.sampling || !ev.active() || !mine {
                continue;
            }
            if ev.interrupts.get() == MAX_INTERRUPTS {
                if !real {
                    continue;
                }
                unthrottle_group(&ev);
            }
            if ev.ty != TYPE_SOFTWARE {
                continue;
            }
            if ev.is_clock() {
                if ev.bit(B_EXCLUDE_USER) {
                    continue;
                }
                // CPU ごとのものは、その CPU で経った時間 (Linux の cpu-clock の hrtimer)
                let now = if ev.per_cpu() { crate::timer::uptime_ns() } else { proc::cpu_ns(p) };
                let (period, last) = (ev.period.get().max(1), ev.last_clock.get());
                if last == 0 || now < last {
                    ev.last_clock.set(now);
                    next = next.min(period);
                } else if now - last >= period {
                    // 間の倍だけ進めて 1 つ (Linux の hrtimer_forward_now)。PERIOD ははじめの間のまま
                    let k = (now - last) / period;
                    ev.last_clock.set(last + k * period);
                    if overflow(&ev, tf, 0, ev.last_period.get(), true) {
                        // 止めた: 次の tick まで鳴らさない
                        continue;
                    }
                    next = next.min(period - (now - last - k * period));
                } else {
                    next = next.min(period - (now - last));
                }
            } else if real && ev.freq.get() != 0 {
                let h = ev.hits.get();
                let d = h.wrapping_sub(ev.freq_stamp.get());
                ev.freq_stamp.set(h);
                if d > 0 {
                    ev.adjust_period(crate::timer::TICK_NS, d);
                }
            }
        }
        if next < crate::timer::TICK_NS {
            // Linux の hrtimer も 10 us より短くしない
            crate::timer::hr_arm(next.max(10_000));
        }
    }

    /// 写した: いまのスレッドを数えている有効なものに PERF_RECORD_MMAP / MMAP2
    fn mmap(&self, rec: &crate::vm::MapRec, name: Option<&str>) {
        let me = proc::current().pid;
        for ev in live() {
            if ev.wants_mmap() && ev.st.borrow().on && ev.tracks(me) {
                mmap_record(&ev, rec, name.unwrap_or(&rec.name));
            }
        }
    }

    /// perf のブレークポイントに当たった (hwdebug.rs が数えたあと): サンプリングのものは間ごとにあふれる
    fn bp_hit(&self, id: u64, tf: &crate::trap::TrapFrame, _addr: u64) {
        if let Some(ev) = live().into_iter().find(|e| e.id == id && e.sampling) {
            sw_hit(&ev, 1, tf, ev.bp_addr);
        }
    }
}

pub const SYSCALLS: &[u64] = {
    use crate::syscall::nr::*;
    &[PERF_EVENT_OPEN]
};

pub fn sys(n: u64, a: &[u64; 6]) -> R {
    match n {
        crate::syscall::nr::PERF_EVENT_OPEN => open(a[0] as usize, a[1] as i32, a[2] as i32, a[3] as i32, a[4]),
        _ => Err(-38),
    }
}

/// /sys の perf_event_mux_interval_ms (Linux の PERF_CPU_HRTIMER: 1000 / HZ)
pub fn mux_interval_ms() -> u64 {
    4
}
