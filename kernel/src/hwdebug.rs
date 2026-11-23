// arm64 のデバッグの仕組み (Linux の arch/arm64/kernel/debug-monitors.c と hw_breakpoint.c の、レジスタと例外のところ):
// 1 命令ずつ (MDSCR_EL1.SS と SPSR.SS) と、スレッドごとのハードウェアのブレークポイント (DBGBVR / DBGBCR) とウォッチ
// ポイント (DBGWVR / DBGWCR)。ptrace の tracer のたのみ (どこに置くか、スロットの数え方: Linux の ptrace_hbp_*) は
// ptrace.rs、perf の PERF_TYPE_BREAKPOINT (数えるもの) は perf.rs (どちらも extra) が State を書きかえ、番地と長さの
// 検査 (hw_breakpoint_arch_parse) はここ。それを切りかえのたびに CPU に載せ (Linux の hw_breakpoint_thread_switch と
// perf の sched_in)、EL0 からの例外を SIGTRAP の si_code と si_addr にするか (ptrace のもの: do_breakpoint、
// do_watchpoint、do_el0_softstep)、数えて 1 命令越える (perf のもの: ふつうのオーバーフローの手当て。越えるあいだは
// ブレークポイントかウォッチポイントをみな止め、1 命令すすんだら戻す: try_step_suspended_breakpoints)
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

/// perf のブレークポイント 1 つ (perf.rs のイベントの id): 種類 (attr の bp_type)、CPU の値、当たった数
#[derive(Clone, Copy)]
pub struct PerfBp {
    pub id: u64,
    pub watch: bool,
    pub ty: u8,
    pub addr: u64,
    pub ctrl: Ctrl,
    pub count: u64,
}

/// スレッドのブレークポイントとウォッチポイント (Linux の thread.debug の hbp_break と hbp_watch。ptrace のものは
/// fork した子にはわたさず、exec で消える) と perf のもの、越えているところ (bps_disabled、wps_disabled、
/// suspended_step と、越えるための 1 命令ずつ)
#[derive(Clone, Default)]
pub struct State {
    pub brk: [Option<Ev>; MAX],
    pub wch: [Option<Ev>; MAX],
    pub perf: alloc::vec::Vec<PerfBp>,
    bps_off: bool,
    wps_off: bool,
    suspended: bool,
    stepping_over: bool,
}

/// CPU に置くもの: ptrace のスロットの番号か perf の番号
#[derive(Clone, Copy)]
enum Who {
    Ptrace,
    Perf(usize),
}

impl State {
    /// 有効なもの (ptrace のものから、それから perf のもの) の (番地、ctrl、種類、だれの)
    fn on(&self, watch: bool) -> alloc::vec::Vec<(u64, Ctrl, u8, Who)> {
        let mut v: alloc::vec::Vec<(u64, Ctrl, u8, Who)> =
            (if watch { &self.wch } else { &self.brk }).iter().flatten().filter(|e| e.ctrl.enabled).map(|e| (e.addr, e.ctrl, e.attr.ty, Who::Ptrace)).collect();
        v.extend(self.perf.iter().enumerate().filter(|(_, b)| b.watch == watch && b.ctrl.enabled).map(|(i, b)| (b.addr, b.ctrl, b.ty, Who::Perf(i))));
        v.truncate(slots(watch));
        v
    }

    /// 使っているスロットの数 (無効なものも: Linux の reserve_bp_slot はスレッドごとに ptrace と perf を合わせて数える)
    pub fn used(&self, watch: bool) -> usize {
        (if watch { &self.wch } else { &self.brk }).iter().flatten().count() + self.perf.iter().filter(|b| b.watch == watch).count()
    }

    /// 1 命令越えはじめる (Linux の do_breakpoint / do_watchpoint の step: もう 1 命令ずつ (ptrace) なら suspended_step)
    fn step_over(&mut self, pt_step: bool, tf: &mut TrapFrame, watch: bool) {
        if watch {
            self.wps_off = true;
            if self.bps_off {
                return;
            }
        } else {
            self.bps_off = true;
            if self.wps_off {
                return;
            }
        }
        if pt_step {
            self.suspended = true;
        } else if !self.stepping_over {
            self.stepping_over = true;
            tf.spsr |= SPSR_SS;
        }
    }
}

/// attr から CPU の値に (Linux の hw_breakpoint_arch_parse と arch_build_bp_info): 種類は実行・読み・書き・両方
/// だけ、長さは 1〜8 (実行は 4 だけ: ほかの長さは 4 に)、番地は実行 4・読み書き 8 バイトにそろえて長さ (BAS) を
/// ずらす。ずらして 8 ビットをはみ出す (見張るバイトが落ちる) か、カーネルの番地 (スレッドのものは EL0 だけ) なら EINVAL
pub fn parse(a: &Attr) -> Result<(u64, Ctrl), i64> {
    const EINVAL: i64 = 22;
    let ty = match a.ty {
        HW_X => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        _ => return Err(-EINVAL),
    };
    let mut len: u8 = match a.len {
        1..=8 => ((1u16 << a.len) - 1) as u8,
        _ => return Err(-EINVAL),
    };
    if ty == 0 {
        len = 0xf;
    }
    let bytes = len.count_ones() as u64;
    if a.addr >= TASK_SIZE && a.addr.wrapping_add(bytes).wrapping_sub(1) >= TASK_SIZE {
        return Err(-EINVAL);
    }
    let mask = if ty == 0 { 3 } else { 7 };
    let off = a.addr & mask;
    if (len as u32) << off > 0xff {
        return Err(-EINVAL);
    }
    Ok((a.addr & !mask, Ctrl { len: len << off, ty, privilege: 2, enabled: !a.disabled }))
}

/// perf の bp_type の実行 (HW_BREAKPOINT_X)
pub const HW_X: u8 = 4;
/// ユーザーの番地の上 (Linux の TASK_SIZE: VA_BITS 48)
const TASK_SIZE: u64 = 1 << 48;

/// exec: ptrace のものは消える (Linux の flush_ptrace_hw_breakpoint)。perf のものは残る
pub fn flush_ptrace(p: &mut Proc) {
    if let Some(st) = p.hwdbg.as_mut() {
        st.brk = Default::default();
        st.wch = Default::default();
        if st.perf.is_empty() {
            p.hwdbg = None;
        }
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
    let mut stepping_over = false;
    if let Some(st) = st {
        // 越えているあいだは E を落とす (Linux の toggle_bp_registers)
        for (addr, ctrl, _, _) in st.on(false) {
            write_reg(0, nb, addr);
            write_reg(1, nb, (ctrl.encode() & if st.bps_off { !1 } else { !0 }) as u64);
            nb += 1;
        }
        for (addr, ctrl, _, _) in st.on(true) {
            write_reg(2, nw, addr);
            write_reg(3, nw, (ctrl.encode() & if st.wps_off { !1 } else { !0 }) as u64);
            nw += 1;
        }
        stepping_over = st.stepping_over;
    }
    for i in nb..l.brk {
        write_reg(1, i, 0);
    }
    for i in nw..l.wch {
        write_reg(3, i, 0);
    }
    (l.brk, l.wch) = (nb, nw);
    let want = if p.pt_step || stepping_over { MDSCR_SS } else { 0 } | if nb + nw > 0 { MDSCR_MDE } else { 0 };
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

/// EL0 からのデバッグの例外: ptrace のものは SIGTRAP の (si_code、si_addr) に、perf のものは数えて 1 命令越える。
/// SIGTRAP がいらなければ None (なにもせずに戻る)
pub fn exception(tf: &mut TrapFrame, ec: u64, esr: u64, far: u64) -> Option<(i32, u64)> {
    let p = crate::proc::current();
    let pt_step = p.pt_step;
    let r = match ec {
        // 1 命令すすんだ: 越えていたのなら戻して、ptrace の 1 命令ずつでもなければおしまい (try_step_suspended_breakpoints)。
        // でなければ SIGTRAP、si_addr は次の pc (do_el0_softstep)
        EC_SOFTSTP_LOW => {
            let mut done = false;
            if let Some(st) = p.hwdbg.as_deref_mut()
                && (st.bps_off || st.wps_off)
            {
                (st.bps_off, st.wps_off) = (false, false);
                if core::mem::take(&mut st.suspended) {
                    // ptrace も 1 命令ずつ: 知らせる
                } else {
                    st.stepping_over = false;
                    done = true;
                }
            }
            if done {
                None
            } else {
                rewind(p, tf);
                Some((TRAP_TRACE, tf.elr))
            }
        }
        // ブレークポイント (do_breakpoint): 番地と BAS が pc に合うもの。ptrace のものは SIGTRAP (自分では越えない:
        // tracer が消して進める)、perf のものは数えて越える
        EC_BREAKPT_LOW => {
            let pc = tf.elr;
            let st = p.hwdbg.as_deref_mut()?;
            let mut sig = None;
            let mut step = false;
            for (addr, ctrl, _, who) in st.on(false) {
                if addr != pc & !3 || (1u32 << (pc & 3)) & ctrl.len as u32 == 0 {
                    continue;
                }
                match who {
                    Who::Ptrace => sig = Some((TRAP_HWBKPT, pc)),
                    Who::Perf(i) => {
                        st.perf[i].count += 1;
                        step = true;
                    }
                }
            }
            if step {
                st.step_over(pt_step, tf, false);
            }
            sig
        }
        // ウォッチポイント (do_watchpoint): 読み書きの種類が合うもののうち、far を見張るもの。なければいちばん近いもの
        // (1 つの命令が見張るところと見張らないところにまたがると、far が外を指すことがある)。si_addr は far
        EC_WATCHPT_LOW => {
            let st = p.hwdbg.as_deref_mut()?;
            let access = if esr & (1 << 6) != 0 { 2 } else { 1 };
            let mut best: Option<(u64, Who)> = None;
            let mut hits = alloc::vec::Vec::new();
            for (addr, ctrl, ty, who) in st.on(true) {
                if access & ty == 0 {
                    continue;
                }
                let lo = addr + ctrl.len.trailing_zeros() as u64;
                let hi = addr + 7 - ctrl.len.leading_zeros() as u64;
                let d = if far < lo { lo - far } else { far.saturating_sub(hi) };
                if best.map_or(true, |b| d < b.0) {
                    best = Some((d, who));
                }
                if d == 0 {
                    hits.push(who);
                }
            }
            if hits.is_empty() {
                hits.extend(best.map(|b| b.1));
            }
            let (mut sig, mut step) = (None, false);
            for who in hits {
                match who {
                    Who::Ptrace => sig = Some((TRAP_HWBKPT, far)),
                    Who::Perf(i) => {
                        st.perf[i].count += 1;
                        step = true;
                    }
                }
            }
            if step {
                st.step_over(pt_step, tf, true);
            }
            sig
        }
        _ => None,
    };
    // 止めたか戻したブレークポイントと 1 命令ずつを、戻る前に CPU に
    switch_in(p);
    r
}
