// CPU の PMU: ARMv8 PMUv3 (Linux の drivers/perf/arm_pmuv3.c と arm_pmu.c、arm_pmu_platform.c)。
// DTB の "arm,armv8-pmuv3" があり ID_AA64DFR0_EL1.PMUVer が PMUv3 のときだけ (ACPI の機械ではまだ使わない)。
// 名前は armv8_pmuv3、perf の種類の番号は 8 (Linux の perf_pmu_register: PERF_TYPE_MAX からの空き番号。cpu_clock と
// task_clock が 6 と 7)。PERF_TYPE_HARDWARE / HW_CACHE / RAW / 8 の config を ARMv8 のできごとの番号にし (Linux の
// __armv8_pmuv3_map_event: PMCEID にないもの・表にないものは ENOENT)、カウンタを Linux と同じ順にわりあてる (cycles は
// PMCCNTR、ほかは PMEVCNTRn、64 ビットのものは PMUv3p5 より前なら 2 つをつなぐ)。カウンタを書く・読む・止めるのはここ、
// どのスレッドのときに数えるかは perf.rs (スレッドが走りはじめたら書き、止まったら読む)。あふれは PPI の割り込み
// (PMOVSCLR を読んで消す)
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};

/// perf の種類の番号
pub const TYPE: u32 = 8;

const ENOENT: i64 = 2;
const EINVAL: i64 = 22;
const EOPNOTSUPP: i64 = 95;

/// PMCCNTR (cycles) の番号 (Linux の ARMV8_PMU_CYCLE_IDX)
pub const CYCLE_IDX: u32 = 31;
const CPU_CYCLES: u16 = 0x11;
const CHAIN: u16 = 0x1e;

static PRESENT: AtomicBool = AtomicBool::new(false);
/// PMCR_EL0.N (ふつうのカウンタの数)
static NCNT: AtomicU32 = AtomicU32::new(0);
/// ID_AA64DFR0_EL1.PMUVer
static VER: AtomicU32 = AtomicU32::new(0);
/// PMCEID0 と PMCEID1 (Linux の pmceid_bitmap: 0x00..0x3f、pmceid_ext_bitmap: 0x4000..0x403f)
static PMCEID: AtomicU64 = AtomicU64::new(0);
static PMCEID_EXT: AtomicU64 = AtomicU64::new(0);
/// 割り込みの番号 (PPI。0 はなし)
static IRQ: AtomicU32 = AtomicU32::new(0);
/// 初めの設定をすませた CPU (ビット)
static READY: AtomicU64 = AtomicU64::new(0);

macro_rules! mrs {
    ($r:literal) => {{
        let v: u64;
        unsafe { core::arch::asm!(concat!("mrs {}, ", $r), out(reg) v, options(nomem, nostack)) };
        v
    }};
}
macro_rules! msr {
    ($r:literal, $v:expr) => {{
        let v: u64 = $v;
        unsafe { core::arch::asm!(concat!("msr ", $r, ", {}"), "isb", in(reg) v, options(nostack)) };
    }};
}

/// PMCR_EL0 のビット
const PMCR_E: u64 = 1 << 0;
const PMCR_P: u64 = 1 << 1;
const PMCR_C: u64 = 1 << 2;
const PMCR_LC: u64 = 1 << 6;
const PMCR_LP: u64 = 1 << 7;
/// PMEVTYPER のビット (Linux の ARMV8_PMU_EXCLUDE_EL0 / EL1、INCLUDE_EL2)
const EXCLUDE_EL0: u64 = 1 << 30;
const EXCLUDE_EL1: u64 = 1 << 31;
const INCLUDE_EL2: u64 = 1 << 27;

/// 起動のとき (DTB を読んだあと): PMU があれば覚えて、この CPU を設定する
pub fn init() {
    if !crate::dtb::present() || crate::dtb::scan(|n| n.is_compatible("arm,armv8-pmuv3").then_some(())).is_none() {
        return;
    }
    let ver = ((mrs!("id_aa64dfr0_el1") >> 8) & 0xf) as u32;
    if ver == 0 || ver == 0xf {
        return;
    }
    VER.store(ver, Relaxed);
    NCNT.store(((mrs!("pmcr_el0") >> 11) & 0x1f) as u32, Relaxed);
    let (e0, e1) = (mrs!("pmceid0_el0"), mrs!("pmceid1_el0"));
    PMCEID.store((e0 & 0xffff_ffff) | (e1 << 32), Relaxed);
    PMCEID_EXT.store((e0 >> 32) | (e1 & 0xffff_ffff_0000_0000), Relaxed);
    // interrupts = <GIC_PPI n flags> (PPI は 16 から)
    let irq = match (crate::dtb::irq_cell("arm,armv8-pmuv3", 0), crate::dtb::irq_cell("arm,armv8-pmuv3", 1)) {
        (Some(1), Some(n)) => n + 16,
        (Some(0), Some(n)) => n + 32,
        _ => 0,
    };
    IRQ.store(irq, Relaxed);
    crate::proc::PMU_IRQ.store(irq, Relaxed);
    PRESENT.store(true, Relaxed);
    cpu_ready();
    let n = NCNT.load(Relaxed);
    crate::println!("hw perfevents: enabled with armv8_pmuv3 PMU driver, {} ({:x}) counters available", n + 1, counters_mask());
}

pub fn present() -> bool {
    PRESENT.load(Relaxed)
}

pub fn name() -> &'static str {
    "armv8_pmuv3"
}

/// 使えるカウンタ (ビット: 0..N とサイクルの 31)
pub fn counters_mask() -> u64 {
    ((1u64 << NCNT.load(Relaxed)) - 1) | 1 << CYCLE_IDX
}

/// PMUv3p5 から (64 ビットのカウンタ: PMCR.LP。つながない)
fn has_long_event() -> bool {
    VER.load(Relaxed) >= 6
}

/// この CPU で初めてなら、レジスタをきれいにして割り込みを有効に (Linux の armv8pmu_reset と cpuhp の starting)
pub fn cpu_ready() {
    if !present() {
        return;
    }
    let bit = 1u64 << crate::smp::id();
    if READY.load(Relaxed) & bit != 0 {
        return;
    }
    READY.fetch_or(bit, Relaxed);
    let mask = counters_mask();
    msr!("pmcntenclr_el0", mask);
    msr!("pmintenclr_el1", mask);
    msr!("pmovsclr_el0", mask);
    // ユーザーから読ませない (Linux の perf_user_access 0)
    msr!("pmuserenr_el0", 0);
    msr!("pmcr_el0", PMCR_P | PMCR_C | PMCR_LC | if has_long_event() { PMCR_LP } else { 0 });
    let irq = IRQ.load(Relaxed);
    if irq != 0 {
        crate::irq::enable(irq);
    }
}

/// できごとが PMCEID にあるか (Linux の pmceid_bitmap と pmceid_ext_bitmap)
pub fn supported(id: u16) -> bool {
    match id {
        0..0x40 => PMCEID.load(Relaxed) >> id & 1 != 0,
        0x4000..0x4040 => PMCEID_EXT.load(Relaxed) >> (id - 0x4000) & 1 != 0,
        _ => false,
    }
}

/// 数えるもの (PMEVTYPER の値と、64 ビットか、rdpmc)
#[derive(Clone, Copy)]
pub struct Hw {
    pub evtype: u64,
    pub long: bool,
    pub rdpmc: bool,
}

impl Hw {
    pub fn event(&self) -> u16 {
        (self.evtype & 0xffff) as u16
    }

    /// カウンタのいちばん大きな値 (Linux の max_period)
    pub fn max_period(&self) -> u64 {
        if self.long { u64::MAX } else { 0xffff_ffff }
    }

    /// 2 つをつなぐか (Linux の armv8pmu_event_is_chained)
    fn chained(&self, idx: u32) -> bool {
        !self.rdpmc && self.long && !has_long_event() && idx < CYCLE_IDX
    }

    /// 64 ビットのカウンタで 32 ビットであふれさせる (Linux の armv8pmu_event_needs_bias)
    fn needs_bias(&self, idx: u32) -> bool {
        !self.long && (has_long_event() || idx >= CYCLE_IDX)
    }
}

/// PERF_TYPE_HARDWARE の番号 → ARMv8 のできごと (Linux の armv8_pmuv3_perf_map。0xffff はない)
const HW_MAP: [u16; 10] = [0x11, 0x08, 0x04, 0x03, 0xffff, 0x10, 0x1d, 0x23, 0x24, 0xffff];
/// PERF_TYPE_HW_CACHE (読むものだけ: [キャッシュ][access, miss]。Linux の armv8_pmuv3_perf_cache_map)
const CACHE_MAP: [[u16; 2]; 7] = [[0x04, 0x03], [0x14, 0x01], [0x36, 0x37], [0x25, 0x05], [0x26, 0x02], [0x12, 0x10], [0xffff, 0xffff]];

/// perf の attr をできごとと PMEVTYPER の値に (Linux の armpmu_event_init、__hw_perf_event_init、
/// __armv8_pmuv3_map_event、armv8pmu_set_event_filter の順)。task は特定のスレッドのもの
#[allow(clippy::too_many_arguments)]
pub fn map_event(ty: u32, config: u64, config1: u64, excl_user: bool, excl_kernel: bool, excl_hv: bool, excl_idle: bool, task: bool) -> Result<Hw, i64> {
    const HARDWARE: u32 = 0;
    const HW_CACHE: u32 = 3;
    const RAW: u32 = 4;
    if !present() {
        return Err(-ENOENT);
    }
    let pmceid = |id: u16| (id > 0 && id < 0x40 && supported(id)).then_some(id);
    let id: u16 = match ty {
        TYPE | RAW => (config & 0xffff) as u16,
        HARDWARE => {
            // 上の 32 ビットは PMU の番号 (Linux の PERF_PMU_CAP_EXTENDED_HW_TYPE)
            let ext = config >> 32;
            if ext != 0 && ext != TYPE as u64 {
                return Err(-ENOENT);
            }
            let c = config & 0xffff_ffff;
            if c >= HW_MAP.len() as u64 {
                return Err(-EINVAL);
            }
            // 分岐の数は BR_RETIRED、なければ PC_WRITE_RETIRED
            let mapped = if c == 4 { [0x21, 0x0c].into_iter().find(|&e| supported(e)).unwrap_or(0xffff) } else { HW_MAP[c as usize] };
            pmceid(mapped).ok_or(-ENOENT)?
        }
        HW_CACHE => {
            let ext = config >> 32;
            if ext != 0 && ext != TYPE as u64 {
                return Err(-ENOENT);
            }
            let (cache, op, result) = (config & 0xff, (config >> 8) & 0xff, (config >> 16) & 0xff);
            if cache >= 7 || op >= 3 || result >= 2 {
                return Err(-EINVAL);
            }
            let mapped = if op == 0 { CACHE_MAP[cache as usize][result as usize] } else { 0xffff };
            pmceid(mapped).ok_or(-ENOENT)?
        }
        _ => return Err(-ENOENT),
    };
    if id == CHAIN {
        return Err(-EINVAL);
    }
    let long = config1 & 1 != 0;
    let rdpmc = config1 & 2 != 0;
    if rdpmc {
        if !task {
            return Err(-EINVAL);
        }
        if long && id != CPU_CYCLES && !has_long_event() {
            return Err(-EOPNOTSUPP);
        }
    }
    // armv8pmu_set_event_filter: カーネルは EL1 で動く (VHE でない)
    if excl_idle {
        return Err(-EOPNOTSUPP);
    }
    let mut evtype = id as u64;
    if !excl_hv {
        evtype |= INCLUDE_EL2;
    }
    if excl_kernel {
        evtype |= EXCLUDE_EL1;
    }
    if excl_user {
        evtype |= EXCLUDE_EL0;
    }
    // threshold (config1 の 5..16) は PMMIR.THWIDTH まで: PMUv3p4 より前は 0
    if (config1 >> 5) & 0xfff != 0 {
        return Err(-EINVAL);
    }
    Ok(Hw { evtype, long, rdpmc })
}

/// カウンタをわりあてる (Linux の armv8pmu_get_event_idx): cycles は PMCCNTR、64 ビットでつなぐものは奇数とその前、
/// ほかは空いているいちばん小さいもの。used に印をつける
pub fn get_idx(used: &mut u64, hw: &Hw) -> Option<u32> {
    let n = NCNT.load(Relaxed);
    if hw.event() == CPU_CYCLES && *used >> CYCLE_IDX & 1 == 0 {
        *used |= 1 << CYCLE_IDX;
        return Some(CYCLE_IDX);
    }
    if hw.chained(0) {
        let idx = (1..n).step_by(2).find(|&i| *used >> i & 1 == 0 && *used >> (i - 1) & 1 == 0)?;
        *used |= 3 << (idx - 1);
        return Some(idx);
    }
    let idx = (0..n).find(|&i| *used >> i & 1 == 0)?;
    *used |= 1 << idx;
    Some(idx)
}

fn select(idx: u32) {
    msr!("pmselr_el0", idx as u64);
}

fn write_type(idx: u32, v: u64) {
    if idx == CYCLE_IDX {
        msr!("pmccfiltr_el0", v & (EXCLUDE_EL0 | EXCLUDE_EL1 | INCLUDE_EL2));
    } else {
        select(idx);
        msr!("pmxevtyper_el0", v & (0xffff | EXCLUDE_EL0 | EXCLUDE_EL1 | INCLUDE_EL2));
    }
}

fn read_raw(idx: u32) -> u64 {
    if idx == CYCLE_IDX {
        mrs!("pmccntr_el0")
    } else {
        select(idx);
        mrs!("pmxevcntr_el0")
    }
}

fn write_raw(idx: u32, v: u64) {
    if idx == CYCLE_IDX {
        msr!("pmccntr_el0", v);
    } else {
        select(idx);
        msr!("pmxevcntr_el0", v);
    }
}

/// カウンタの値 (Linux の armv8pmu_read_counter: つないだものは上と下、bias は下の 32 ビット)
pub fn read(idx: u32, hw: &Hw) -> u64 {
    if hw.chained(idx) {
        return (read_raw(idx) & 0xffff_ffff) << 32 | (read_raw(idx - 1) & 0xffff_ffff);
    }
    let v = read_raw(idx);
    if hw.needs_bias(idx) { v & 0xffff_ffff } else { v }
}

/// カウンタに書く (Linux の armv8pmu_write_counter)
pub fn write(idx: u32, hw: &Hw, v: u64) {
    if hw.chained(idx) {
        write_raw(idx, v >> 32);
        write_raw(idx - 1, v & 0xffff_ffff);
        return;
    }
    write_raw(idx, if hw.needs_bias(idx) { v | 0xffff_ffff_0000_0000 } else { v });
}

/// 動かす (Linux の armv8pmu_enable_event: 種類、割り込み、カウンタ)。つなぐものは前のものも
pub fn enable(idx: u32, hw: &Hw) {
    let mut bits = 1u64 << idx;
    if hw.chained(idx) {
        write_type(idx - 1, hw.evtype);
        write_type(idx, CHAIN as u64 | INCLUDE_EL2);
        bits |= 1 << (idx - 1);
    } else {
        write_type(idx, hw.evtype);
    }
    msr!("pmintenset_el1", 1u64 << idx);
    msr!("pmcntenset_el0", bits);
}

/// みな止めて、割り込みとあふれも消す
pub fn disable_all() {
    let mask = counters_mask();
    msr!("pmcntenclr_el0", mask);
    msr!("pmintenclr_el1", mask);
    msr!("pmovsclr_el0", mask);
}

/// PMCR.E (Linux の armv8pmu_start / stop)
pub fn start() {
    msr!("pmcr_el0", (mrs!("pmcr_el0") & 0xff) | PMCR_E);
}

pub fn stop() {
    msr!("pmcr_el0", mrs!("pmcr_el0") & 0xff & !PMCR_E);
}

/// あふれたカウンタ (読んで消す: Linux の armv8pmu_getreset_flags)
pub fn take_overflow() -> u64 {
    if !present() {
        return 0;
    }
    let v = mrs!("pmovsclr_el0") & counters_mask();
    if v != 0 {
        msr!("pmovsclr_el0", v);
    }
    v
}

// ---- /sys/bus/event_source/devices/armv8_pmuv3 (Linux の armv8_pmuv3_format_attrs、events_attrs、caps_attrs) ----

/// format/ (並べた順)
pub const FORMAT: [(&str, &str); 6] =
    [("event", "config:0-15"), ("long", "config1:0"), ("rdpmc", "config1:1"), ("threshold", "config1:5-16"), ("threshold_compare", "config1:3-4"), ("threshold_count", "config1:2")];

/// caps/ (PMUv3p4 より前は PMMIR がないので 0)
pub const CAPS: [&str; 4] = ["bus_slots", "bus_width", "slots", "threshold_max"];

pub fn cap(_i: usize) -> String {
    format!("0x{:08x}", 0)
}

/// events/ の名前と番号 (Linux の armv8_pmuv3_event_attrs)
const EVENTS: &[(&str, u16)] = &[
    ("l1i_cache_refill", 0x01),
    ("l1i_tlb_refill", 0x02),
    ("l1d_cache_refill", 0x03),
    ("l1d_cache", 0x04),
    ("l1d_tlb_refill", 0x05),
    ("ld_retired", 0x06),
    ("st_retired", 0x07),
    ("inst_retired", 0x08),
    ("exc_taken", 0x09),
    ("exc_return", 0x0a),
    ("cid_write_retired", 0x0b),
    ("pc_write_retired", 0x0c),
    ("br_immed_retired", 0x0d),
    ("br_return_retired", 0x0e),
    ("unaligned_ldst_retired", 0x0f),
    ("br_mis_pred", 0x10),
    ("cpu_cycles", 0x11),
    ("br_pred", 0x12),
    ("mem_access", 0x13),
    ("l1i_cache", 0x14),
    ("l1d_cache_wb", 0x15),
    ("l2d_cache", 0x16),
    ("l2d_cache_refill", 0x17),
    ("l2d_cache_wb", 0x18),
    ("bus_access", 0x19),
    ("memory_error", 0x1a),
    ("inst_spec", 0x1b),
    ("ttbr_write_retired", 0x1c),
    ("bus_cycles", 0x1d),
    ("l1d_cache_allocate", 0x1f),
    ("l2d_cache_allocate", 0x20),
    ("br_retired", 0x21),
    ("br_mis_pred_retired", 0x22),
    ("stall_frontend", 0x23),
    ("stall_backend", 0x24),
    ("l1d_tlb", 0x25),
    ("l1i_tlb", 0x26),
    ("l2i_cache", 0x27),
    ("l2i_cache_refill", 0x28),
    ("l3d_cache_allocate", 0x29),
    ("l3d_cache_refill", 0x2a),
    ("l3d_cache", 0x2b),
    ("l3d_cache_wb", 0x2c),
    ("l2d_tlb_refill", 0x2d),
    ("l2i_tlb_refill", 0x2e),
    ("l2d_tlb", 0x2f),
    ("l2i_tlb", 0x30),
    ("remote_access", 0x31),
    ("ll_cache", 0x32),
    ("ll_cache_miss", 0x33),
    ("dtlb_walk", 0x34),
    ("itlb_walk", 0x35),
    ("ll_cache_rd", 0x36),
    ("ll_cache_miss_rd", 0x37),
    ("remote_access_rd", 0x38),
    ("l1d_cache_lmiss_rd", 0x39),
    ("op_retired", 0x3a),
    ("op_spec", 0x3b),
    ("stall", 0x3c),
    ("stall_slot_backend", 0x3d),
    ("stall_slot_frontend", 0x3e),
    ("stall_slot", 0x3f),
    ("sample_pop", 0x4000),
    ("sample_feed", 0x4001),
    ("sample_filtrate", 0x4002),
    ("sample_collision", 0x4003),
    ("cnt_cycles", 0x4004),
    ("stall_backend_mem", 0x4005),
    ("l1i_cache_lmiss", 0x4006),
    ("l2d_cache_lmiss_rd", 0x4009),
    ("l2i_cache_lmiss", 0x400a),
    ("l3d_cache_lmiss_rd", 0x400b),
    ("trb_wrap", 0x400c),
    ("trb_trig", 0x400e),
    ("trcextout0", 0x4010),
    ("trcextout1", 0x4011),
    ("trcextout2", 0x4012),
    ("trcextout3", 0x4013),
    ("cti_trigout4", 0x4018),
    ("cti_trigout5", 0x4019),
    ("cti_trigout6", 0x401a),
    ("cti_trigout7", 0x401b),
    ("ldst_align_lat", 0x4020),
    ("ld_align_lat", 0x4021),
    ("st_align_lat", 0x4022),
    ("mem_access_checked", 0x4024),
    ("mem_access_checked_rd", 0x4025),
    ("mem_access_checked_wr", 0x4026),
];

/// events/ に見せるもの (PMCEID にあるもの: Linux の armv8pmu_event_attr_is_visible)
pub fn events() -> Vec<(&'static str, u16)> {
    EVENTS.iter().filter(|e| supported(e.1)).copied().collect()
}

/// cpus (Linux の supported_cpus: 動いている CPU みな)
pub fn cpus() -> String {
    let n = crate::smp::online().max(1);
    if n == 1 { "0".into() } else { format!("0-{}", n - 1) }
}
