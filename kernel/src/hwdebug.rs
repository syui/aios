// arm64 のデバッグの仕組み (Linux の arch/arm64/kernel/debug-monitors.c と hw_breakpoint.c の、レジスタと例外のところ):
// 1 命令ずつ (MDSCR_EL1.SS と SPSR.SS) と、スレッドごとのハードウェアのブレークポイント (DBGBVR / DBGBCR) とウォッチ
// ポイント (DBGWVR / DBGWCR)。ptrace の tracer のたのみ (どこに置くか、まちがいの検査、スロットの数え方: Linux の
// ptrace_hbp_* と hw_breakpoint_arch_parse) は ptrace.rs (extra) が State を書きかえる。ここはそれを切りかえのたびに
// CPU に載せ (Linux の hw_breakpoint_thread_switch と perf の sched_in)、EL0 からの例外を SIGTRAP の si_code と si_addr に
// する (do_breakpoint、do_watchpoint、do_el0_softstep)
use crate::proc::Proc;
use crate::trap::TrapFrame;

/// ptrace から見えるスロットの数 (Linux の ARM_MAX_BRP と ARM_MAX_WRP)
pub const MAX: usize = 16;
/// MDSCR_EL1 の SS (1 命令ずつ) と MDE (ブレークポイントとウォッチポイントの例外)
const MDSCR_SS: u64 = 1;
const MDSCR_MDE: u64 = 1 << 15;
/// SPSR の SS: 1 で戻ると 1 命令走ってから止まる (active-not-pending)。0 で戻ると走る前に止まる (active-pending)
pub const SPSR_SS: u64 = 1 << 21;
/// si_code (TRAP_TRACE、TRAP_HWBKPT)
const TRAP_TRACE: i32 = 2;
const TRAP_HWBKPT: i32 = 4;
/// ESR の例外の種類 (EL0 から): ブレークポイント、1 命令ずつ、ウォッチポイント
pub const EC_BREAKPT_LOW: u64 = 0x30;
pub const EC_SOFTSTP_LOW: u64 = 0x32;
pub const EC_WATCHPT_LOW: u64 = 0x34;

/// ctrl (Linux の struct arch_hw_breakpoint_ctrl): len は BAS (ずらしたあと)、ty は 0 実行 / 1 読み / 2 書き / 3 両方、
/// privilege は 1 EL1 / 2 EL0
#[derive(Clone, Copy, Default)]
pub struct Ctrl {
    pub len: u8,
    pub ty: u8,
    pub privilege: u8,
    pub enabled: bool,
}

impl Ctrl {
    /// レジスタと ptrace の形 (encode_ctrl_reg)
    pub fn encode(&self) -> u32 {
        (self.len as u32) << 5 | (self.ty as u32) << 3 | (self.privilege as u32) << 1 | self.enabled as u32
    }
}

/// perf の attr (tracer の書いたもの): bp_addr、bp_len (1〜8)、bp_type (Linux の HW_BREAKPOINT_R 1、W 2、RW 3、X 4)、disabled
#[derive(Clone, Copy)]
pub struct Attr {
    pub addr: u64,
    pub len: u8,
    pub ty: u8,
    pub disabled: bool,
}

/// ptrace のスロット 1 つ (Linux の perf_event): attr と、それから作った CPU の値 (addr は 4 / 8 バイトにそろえたもの)
#[derive(Clone, Copy)]
pub struct Ev {
    pub attr: Attr,
    pub addr: u64,
    pub ctrl: Ctrl,
}

/// スレッドのブレークポイントとウォッチポイント (Linux の thread.debug の hbp_break と hbp_watch)。fork した子には
/// わたさず、exec で消える
#[derive(Clone, Default)]
pub struct State {
    pub brk: [Option<Ev>; MAX],
    pub wch: [Option<Ev>; MAX],
}

impl State {
    fn on(&self, watch: bool) -> impl Iterator<Item = &Ev> {
        (if watch { &self.wch } else { &self.brk }).iter().flatten().filter(|e| e.ctrl.enabled)
    }
}

fn dfr0() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, id_aa64dfr0_el1", out(reg) v) };
    v
}

/// CPU のスロットの数 (Linux の get_num_brps / get_num_wrps: ID_AA64DFR0_EL1 の BRPs と WRPs に 1 を足す)
pub fn slots(watch: bool) -> usize {
    ((dfr0() >> if watch { 20 } else { 12 }) & 0xf) as usize + 1
}

/// デバッグの版 (Linux の debug_monitors_arch: ID_AA64DFR0_EL1 の DebugVer)
pub fn arch() -> u32 {
    (dfr0() & 0xf) as u32
}

/// この CPU に載っているもの (MDSCR と、使ったブレークポイントとウォッチポイントの数)
struct Loaded {
    mdscr: u64,
    brk: usize,
    wch: usize,
}
static LOADED: crate::cell::PerCpu<Loaded> = crate::cell::PerCpu::new([const { Loaded { mdscr: 0, brk: 0, wch: 0 } }; crate::smp::MAXCPU]);

/// CPU ごとに起動で: OS lock を外し (Linux の clear_os_lock。外さないとデバッグの例外が来ない)、MDSCR を 0 に
pub fn init_cpu() {
    unsafe { core::arch::asm!("msr oslar_el1, xzr", "msr mdscr_el1, xzr", "isb") };
    let l = LOADED.get();
    (l.mdscr, l.brk, l.wch) = (0, 0, 0);
}

/// n 番の DBGBVR (0)、DBGBCR (1)、DBGWVR (2)、DBGWCR (3) に書く (write_wb_reg)
fn write_reg(kind: u8, n: usize, v: u64) {
    macro_rules! arms {
        ($($i:literal),*) => {
            match (kind, n) {
                $(
                    (0, $i) => unsafe { core::arch::asm!(concat!("msr dbgbvr", stringify!($i), "_el1, {}"), in(reg) v) },
                    (1, $i) => unsafe { core::arch::asm!(concat!("msr dbgbcr", stringify!($i), "_el1, {}"), in(reg) v) },
                    (2, $i) => unsafe { core::arch::asm!(concat!("msr dbgwvr", stringify!($i), "_el1, {}"), in(reg) v) },
                    (3, $i) => unsafe { core::arch::asm!(concat!("msr dbgwcr", stringify!($i), "_el1, {}"), in(reg) v) },
                )*
                _ => {}
            }
        };
    }
    arms!(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
}

/// 切りかえで走らせる前に (スケジューラ): このスレッドの 1 命令ずつと、有効なブレークポイントとウォッチポイントを
/// CPU に。有効なものは前から詰めて置き (Linux の hw_breakpoint_slot_setup)、前のスレッドの残りは消す。なにもない
/// スレッドが続くあいだは MDSCR を見るだけ
pub fn switch_in(p: &Proc) {
    let l = LOADED.get();
    let st = p.hwdbg.as_deref();
    let (mut nb, mut nw) = (0, 0);
    if let Some(st) = st {
        for e in st.on(false).take(slots(false)) {
            write_reg(0, nb, e.addr);
            write_reg(1, nb, e.ctrl.encode() as u64);
            nb += 1;
        }
        for e in st.on(true).take(slots(true)) {
            write_reg(2, nw, e.addr);
            write_reg(3, nw, e.ctrl.encode() as u64);
            nw += 1;
        }
    }
    for i in nb..l.brk {
        write_reg(1, i, 0);
    }
    for i in nw..l.wch {
        write_reg(3, i, 0);
    }
    (l.brk, l.wch) = (nb, nw);
    let want = if p.pt_step { MDSCR_SS } else { 0 } | if nb + nw > 0 { MDSCR_MDE } else { 0 };
    if want != l.mdscr || nb + nw > 0 {
        unsafe { core::arch::asm!("msr mdscr_el1, {}", "isb", in(reg) want) };
        l.mdscr = want;
    }
}

/// 1 命令ずつ進めているなら、次に戻るときも 1 命令走ってから止まるように (Linux の user_rewind_single_step)
pub fn rewind(p: &Proc, tf: &mut TrapFrame) {
    if p.pt_step {
        tf.spsr |= SPSR_SS;
    }
}

/// EL0 からのデバッグの例外を SIGTRAP の (si_code、si_addr) に。合うものがなければ None (なにもせずに戻る)
pub fn exception(tf: &mut TrapFrame, ec: u64, esr: u64, far: u64) -> Option<(i32, u64)> {
    let p = crate::proc::current();
    match ec {
        // 1 命令すすんだ (do_el0_softstep): si_addr は次の pc
        EC_SOFTSTP_LOW => {
            rewind(p, tf);
            Some((TRAP_TRACE, tf.elr))
        }
        // ブレークポイント (do_breakpoint): 番地と BAS が pc に合うもの。ptrace のものは自分で越えない (tracer が消して進める)
        EC_BREAKPT_LOW => {
            let pc = tf.elr;
            let st = p.hwdbg.as_deref()?;
            st.on(false).take(slots(false)).any(|e| e.addr == pc & !3 && (1u32 << (pc & 3)) & e.ctrl.len as u32 != 0).then_some((TRAP_HWBKPT, pc))
        }
        // ウォッチポイント (do_watchpoint): 読み書きの種類が合うもののうち、far を見張るもの。なければいちばん近いもの
        // (1 つの命令が見張るところと見張らないところにまたがると、far が外を指すことがある)。si_addr は far
        EC_WATCHPT_LOW => {
            let st = p.hwdbg.as_deref()?;
            let access = if esr & (1 << 6) != 0 { 2 } else { 1 };
            let mut min = u64::MAX;
            for e in st.on(true).take(slots(true)) {
                if access & e.attr.ty == 0 {
                    continue;
                }
                let lo = e.addr + e.ctrl.len.trailing_zeros() as u64;
                let hi = e.addr + 7 - e.ctrl.len.leading_zeros() as u64;
                let d = if far < lo { lo - far } else { far.saturating_sub(hi) };
                min = min.min(d);
            }
            (min != u64::MAX).then_some((TRAP_HWBKPT, far))
        }
        _ => None,
    }
}
