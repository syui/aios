// CPU のぜい弱性 (Spectre / Meltdown など) の様子: /sys/devices/system/cpu/vulnerabilities/* と、
// prctl の PR_GET/SET_SPECULATION_CTRL (投機的なストアの追いこし: Spectre v4、SSB) と /proc/PID/status の
// Speculation_Store_Bypass
//
// 「当たるか」は Linux 7.3 の arm64 と同じに決める (arch/arm64/kernel/proton-pack.c と cpufeature.c): MIDR の
// 安全なものの表、ID レジスタ (ID_AA64PFR0 の CSV2 / CSV3、ID_AA64PFR1 の SSBS、ID_AA64MMFR1 の ECBHB)、
// ファームウェアへの問いあわせ (SMCCC の ARCH_FEATURES で ARCH_WORKAROUND_1 / 2 / 3。PSCI と同じ道 (hvc / smc) で、
// SMCCC 1.1 からあるときだけ)。
// 「対策」は aios がしていることだけを書く (Linux より正直に):
//   meltdown           当たれば Vulnerable (aios は KPTI をしない)
//   spectre_v1         Mitigation: __user pointer sanitization (aios はユーザーのポインタをじかにたどらない:
//                      ユーザーのページ表を引いてから、カーネルの写しで読み書きする。vm.rs の copy_in / copy_out)
//   spectre_v2         CSV2 か安全な表のものは Not affected (BHB に当たれば CSV2)。ほかはファームウェアの手当て
//                      (WORKAROUND_1) があれば Branch predictor hardening: Linux と同じく、ユーザーのアドレス空間を
//                      載せかえるとき (vm::PageTable::activate) と、ユーザーからカーネルのアドレスで落ちたとき (命令の
//                      フォルト、PC のそろい、ブレークポイント、ステップ、カーネルのアドレスでの割り込み: trap.rs) に呼ぶ。
//                      なければ Vulnerable。
//                      Spectre-BHB (v2 が Vulnerable でないときだけ: Linux の「BHB だけ手当てしても意味がない」): ユーザー
//                      から入るときのベクタ (trap.rs) で、分岐の履歴を消す。ハードウェア (ECBHB) なら何もしない、CLRBHB
//                      の命令があればそれ、CPU ごとの回数 (Linux の spectre_bhb_loop_affected) のループ、ファームウェア
//                      (WORKAROUND_3。そのときは WORKAROUND_1 を呼ばない: ベクタのものがかわりになる) の順
//   spec_store_bypass  SSBS があれば Linux と同じ「prctl で」: カーネルは SSBS を落として動き (SCTLR_EL1.DSSBS = 0)、
//                      ユーザーはふだん SSBS を立てて (速く)、prctl で止めたスレッドだけ落とす。切りかえのたびに
//                      スレッドの印から決めなおす。SSBS がなく、ファームウェアの手当て (WORKAROUND_2) だけなら Vulnerable
//   ほか (x86 のもの)   Not affected
use alloc::string::String;
use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    Unaffected,
    Mitigated,
    Vulnerable,
}

pub struct Vuln {
    pub meltdown: State,
    pub v2: State,
    pub bhb: State,
    pub v4: State,
}

static VULN: crate::cell::Early<Vuln> = crate::cell::Early::new(Vuln { meltdown: State::Vulnerable, v2: State::Vulnerable, bhb: State::Vulnerable, v4: State::Vulnerable });

// MIDR (実装者, 部品の番号) の表 (Linux の MIDR_ALL_VERSIONS)
const A35: (u32, u32) = (0x41, 0xd04);
const A53: (u32, u32) = (0x41, 0xd03);
const A55: (u32, u32) = (0x41, 0xd05);
const A57: (u32, u32) = (0x41, 0xd07);
const A72: (u32, u32) = (0x41, 0xd08);
const A73: (u32, u32) = (0x41, 0xd09);
const A510: (u32, u32) = (0x41, 0xd46);
const A520: (u32, u32) = (0x41, 0xd80);
const BRAHMA_B53: (u32, u32) = (0x42, 0x100);
const BRCM_VULCAN: (u32, u32) = (0x42, 0x516);
const THUNDERX2: (u32, u32) = (0x43, 0x0af);
const TSV110: (u32, u32) = (0x48, 0xd01);
const CARMEL: (u32, u32) = (0x4e, 0x004);
const KRYO_2XX_GOLD: (u32, u32) = (0x51, 0x800);
const KRYO_2XX_SILVER: (u32, u32) = (0x51, 0x801);
const KRYO_3XX_SILVER: (u32, u32) = (0x51, 0x803);
const KRYO_4XX_SILVER: (u32, u32) = (0x51, 0x805);
const A76: (u32, u32) = (0x41, 0xd0b);
const N1: (u32, u32) = (0x41, 0xd0c);
const A77: (u32, u32) = (0x41, 0xd0d);
const A76AE: (u32, u32) = (0x41, 0xd0e);
const V1: (u32, u32) = (0x41, 0xd40);
const A78: (u32, u32) = (0x41, 0xd41);
const A78AE: (u32, u32) = (0x41, 0xd42);
const X1: (u32, u32) = (0x41, 0xd44);
const A710: (u32, u32) = (0x41, 0xd47);
const X2: (u32, u32) = (0x41, 0xd48);
const N2: (u32, u32) = (0x41, 0xd49);
const A78C: (u32, u32) = (0x41, 0xd4b);
const X1C: (u32, u32) = (0x41, 0xd4c);
const A715: (u32, u32) = (0x41, 0xd4d);
const X3: (u32, u32) = (0x41, 0xd4e);
const V2: (u32, u32) = (0x41, 0xd4f);
const A720: (u32, u32) = (0x41, 0xd81);
const A720AE: (u32, u32) = (0x41, 0xd89);
const KRYO_4XX_GOLD: (u32, u32) = (0x51, 0x804);
const HIP09: (u32, u32) = (0x48, 0xd02);
const AMPERE1: (u32, u32) = (0xc0, 0xac3);

/// Meltdown に当たらないもの (kpti_safe_list)
const KPTI_SAFE: [(u32, u32); 15] = [THUNDERX2, BRCM_VULCAN, BRAHMA_B53, A35, A53, A55, A57, A72, A73, TSV110, CARMEL, KRYO_2XX_GOLD, KRYO_2XX_SILVER, KRYO_3XX_SILVER, KRYO_4XX_SILVER];
/// Spectre v2 に当たらないもの (spectre_v2_safe_list)
const V2_SAFE: [(u32, u32); 8] = [A35, A53, A55, BRAHMA_B53, TSV110, KRYO_2XX_SILVER, KRYO_3XX_SILVER, KRYO_4XX_SILVER];
/// Spectre v4 に当たらないもの (spectre_v4_safe_list)
const V4_SAFE: [(u32, u32); 6] = [A35, A53, A55, BRAHMA_B53, KRYO_3XX_SILVER, KRYO_4XX_SILVER];
/// Spectre-BHB に当たらないもの (spectre_bhb_safe_list)
const BHB_SAFE: [(u32, u32); 9] = [A35, A53, A55, A510, A520, BRAHMA_B53, KRYO_2XX_SILVER, KRYO_3XX_SILVER, KRYO_4XX_SILVER];

/// Spectre-BHB の、分岐の履歴を消すループの回数 (Linux 7.4 の spectre_bhb_loop_affected の表)
const BHB_K132: [(u32, u32); 2] = [X3, V2];
const BHB_K38: [(u32, u32); 3] = [A715, A720, A720AE];
const BHB_K32: [(u32, u32); 10] = [A78, A78AE, A78C, X1, X1C, A710, X2, N2, V1, TSV110];
const BHB_K24: [(u32, u32); 6] = [A76, A76AE, A77, N1, KRYO_4XX_GOLD, HIP09];
const BHB_K11: [(u32, u32); 1] = [AMPERE1];
const BHB_K8: [(u32, u32); 2] = [A72, A57];

fn bhb_loop_k(me: (u32, u32)) -> u32 {
    let is = |list: &[(u32, u32)]| list.contains(&me);
    if is(&BHB_K132) {
        132
    } else if is(&BHB_K38) {
        38
    } else if is(&BHB_K32) {
        32
    } else if is(&BHB_K24) {
        24
    } else if is(&BHB_K11) {
        11
    } else if is(&BHB_K8) {
        8
    } else {
        0
    }
}

/// ユーザーから入るときの Spectre-BHB の手当て (trap.rs がベクタを選ぶ)
pub const BHB_NONE: u8 = 0;
pub const BHB_LOOP: u8 = 1;
pub const BHB_INSN: u8 = 2;
pub const BHB_FW_HVC: u8 = 3;
pub const BHB_FW_SMC: u8 = 4;
static BHB_HOW: AtomicU8 = AtomicU8::new(BHB_NONE);
/// ループの回数 (trap.rs のベクタが読む)
#[unsafe(no_mangle)]
pub static BHB_K: AtomicU32 = AtomicU32::new(0);
/// Spectre v2 のファームウェアの手当て (ARCH_WORKAROUND_1) を呼ぶ道: 0 呼ばない、1 hvc、2 smc
static BP_FW: AtomicU8 = AtomicU8::new(0);

// SMCCC
const SMCCC_VERSION: u32 = 0x8000_0000;
const SMCCC_ARCH_FEATURES: u32 = 0x8000_0001;
const WORKAROUND_1: u32 = 0x8000_8000;
const WORKAROUND_2: u32 = 0x8000_7fff;
const WORKAROUND_3: u32 = 0x8000_3fff;
const PSCI_VERSION: u32 = 0x8400_0000;
const PSCI_FEATURES: u32 = 0x8400_000a;
const RET_SUCCESS: i64 = 0;
const RET_NOT_REQUIRED: i64 = -2;
/// SMCCC_ARCH_WORKAROUND_RET_UNAFFECTED
const RET_UNAFFECTED: i64 = 1;

/// PSCI と同じ道 (DTB の psci の method) で呼ぶ
fn call(fid: u32, arg: u32) -> i64 {
    let mut r: u64 = fid as u64;
    unsafe {
        match crate::dtb::psci_method() {
            Some("smc") => core::arch::asm!("smc #0", inout("x0") r, in("x1") arg as u64, out("x2") _, out("x3") _, options(nostack)),
            Some("hvc") => core::arch::asm!("hvc #0", inout("x0") r, in("x1") arg as u64, out("x2") _, out("x3") _, options(nostack)),
            _ => return -1,
        }
    }
    r as i32 as i64
}

/// SMCCC 1.1 から (Linux の psci_init_smccc: PSCI 1.0 からで、PSCI_FEATURES が SMCCC_VERSION を知っていて、1.1 以上)
fn smccc_1_1() -> bool {
    if crate::dtb::psci_method().is_none() || (call(PSCI_VERSION, 0) as u32) >> 16 < 1 {
        return false;
    }
    call(PSCI_FEATURES, SMCCC_VERSION) >= 0 && call(SMCCC_VERSION, 0) >= 0x10001
}

/// ファームウェアの手当て (ARCH_WORKAROUND_N) があるか: Mitigated (ある) / Unaffected (要らない) / Vulnerable (ない)
fn firmware(wa: u32, smccc: bool) -> State {
    if !smccc {
        return State::Vulnerable;
    }
    match call(SMCCC_ARCH_FEATURES, wa) {
        RET_SUCCESS => State::Mitigated,
        RET_UNAFFECTED => State::Unaffected,
        RET_NOT_REQUIRED if wa == WORKAROUND_2 => State::Unaffected,
        _ => State::Vulnerable,
    }
}

fn field(v: u64, shift: u32) -> u64 {
    (v >> shift) & 0xf
}

/// 起動のとき (DTB のあと、ほかの CPU を起こす前): CPU 0 を見て決める (aios はみな同じ CPU とみなす)
pub fn init() {
    let (midr, pfr0, pfr1, mmfr1, isar2): (u64, u64, u64, u64, u64);
    unsafe {
        core::arch::asm!("mrs {}, midr_el1", out(reg) midr);
        core::arch::asm!("mrs {}, id_aa64pfr0_el1", out(reg) pfr0);
        core::arch::asm!("mrs {}, id_aa64pfr1_el1", out(reg) pfr1);
        core::arch::asm!("mrs {}, id_aa64mmfr1_el1", out(reg) mmfr1);
        // ID_AA64ISAR2_EL1 (S3_0_C0_C6_2。古いアセンブラは名前を知らない)
        core::arch::asm!("mrs {}, s3_0_c0_c6_2", out(reg) isar2);
    }
    let me = (((midr >> 24) & 0xff) as u32, ((midr >> 4) & 0xfff) as u32);
    let is = |list: &[(u32, u32)]| list.contains(&me);
    let smccc = smccc_1_1();
    let csv2 = field(pfr0, 56);
    let csv3 = field(pfr0, 60);
    let ssbs = field(pfr1, 4) >= 1;
    let ecbhb = field(mmfr1, 60) >= 1;
    let clrbhb = field(isar2, 28) >= 1;
    let conduit = match crate::dtb::psci_method() {
        Some("hvc") => 1,
        Some("smc") => 2,
        _ => 0,
    };
    let meltdown = if is(&KPTI_SAFE) || csv3 >= 1 { State::Unaffected } else { State::Vulnerable };
    let v2 = if csv2 >= 1 || is(&V2_SAFE) { State::Unaffected } else { firmware(WORKAROUND_1, smccc) };
    // Spectre-BHB: 当たるもの (CSV2_3 でも安全な表でもない) は、v2 が Vulnerable なら手当てしない (Linux と同じ)。
    // ハードウェア (ECBHB)、CLRBHB、ループ、ファームウェア (WORKAROUND_3) の順
    let k = bhb_loop_k(me);
    let (bhb, how) = if csv2 == 3 || is(&BHB_SAFE) {
        (State::Unaffected, BHB_NONE)
    } else if v2 == State::Vulnerable {
        (State::Vulnerable, BHB_NONE)
    } else if ecbhb {
        (State::Mitigated, BHB_NONE)
    } else if clrbhb {
        (State::Mitigated, BHB_INSN)
    } else if k > 0 {
        (State::Mitigated, BHB_LOOP)
    } else if conduit != 0 && firmware(WORKAROUND_3, smccc) == State::Mitigated {
        (State::Mitigated, if conduit == 1 { BHB_FW_HVC } else { BHB_FW_SMC })
    } else {
        (State::Vulnerable, BHB_NONE)
    };
    BHB_K.store(k, Ordering::Relaxed);
    BHB_HOW.store(how, Ordering::Relaxed);
    // v2 をファームウェアで手当てする (ベクタで WORKAROUND_3 を呼ぶなら、そちらがかわり)
    if v2 == State::Mitigated && how != BHB_FW_HVC && how != BHB_FW_SMC {
        BP_FW.store(conduit, Ordering::Relaxed);
    }
    // この CPU のベクタを選びなおす (ほかの CPU は起きるときに trap::init で)
    crate::trap::init();
    let v4 = if is(&V4_SAFE) {
        State::Unaffected
    } else if ssbs {
        State::Mitigated
    } else {
        match firmware(WORKAROUND_2, smccc) {
            State::Unaffected => State::Unaffected,
            _ => State::Vulnerable,
        }
    };
    if v4 == State::Mitigated {
        // いまのカーネルも SSBS を落として (例外で入るときは SCTLR_EL1.DSSBS = 0 で落ちる: boot.rs)。MSR SSBS, #0
        unsafe { core::arch::asm!(".inst 0xd503403f") };
    }
    VULN.set(Vuln { meltdown, v2, bhb, v4 });
}

pub fn get() -> &'static Vuln {
    VULN.get()
}

/// ユーザーから入るときの Spectre-BHB の手当て (BHB_*)
pub fn bhb_how() -> u8 {
    BHB_HOW.load(Ordering::Relaxed)
}

/// Spectre v2 のファームウェアの手当てを呼ぶか (trap.rs が、ユーザーからカーネルのアドレスで落ちたときに見る)
pub fn bp_fw_on() -> bool {
    BP_FW.load(Ordering::Relaxed) != 0
}

/// Spectre v2 のファームウェアの手当て (SMCCC_ARCH_WORKAROUND_1: 分岐予測をきれいにする)。手当てがなければ何もしない
/// (Linux の arm64_apply_bp_hardening)。SMCCC 1.1 は x4 からあとを変えない
pub fn bp_harden() {
    match BP_FW.load(Ordering::Relaxed) {
        1 => unsafe { core::arch::asm!("hvc #0", inout("x0") WORKAROUND_1 as u64 => _, out("x1") _, out("x2") _, out("x3") _, options(nostack)) },
        2 => unsafe { core::arch::asm!("smc #0", inout("x0") WORKAROUND_1 as u64 => _, out("x1") _, out("x2") _, out("x3") _, options(nostack)) },
        _ => {}
    }
}

/// /sys/devices/system/cpu/vulnerabilities/NAME の中身 (Linux の cpu_show_* と同じ書き方。ファイルの一覧は sysfs.rs)
pub fn show(name: &str) -> String {
    let v = get();
    let s = match name {
        "meltdown" => match v.meltdown {
            State::Unaffected => "Not affected",
            State::Mitigated => "Mitigation: PTI",
            State::Vulnerable => "Vulnerable",
        },
        "spectre_v1" => "Mitigation: __user pointer sanitization",
        "spectre_v2" => {
            let bhb = match v.bhb {
                State::Unaffected => "",
                State::Mitigated => ", BHB",
                State::Vulnerable => ", but not BHB",
            };
            return match v.v2 {
                State::Unaffected if v.bhb == State::Unaffected => "Not affected\n".into(),
                // BHB に当たるものは Not affected と言えない
                State::Unaffected => alloc::format!("Mitigation: CSV2{}\n", bhb),
                State::Mitigated => alloc::format!("Mitigation: Branch predictor hardening{}\n", bhb),
                State::Vulnerable => "Vulnerable\n".into(),
            };
        }
        "spec_store_bypass" => match v.v4 {
            State::Unaffected => "Not affected",
            State::Mitigated => "Mitigation: Speculative Store Bypass disabled via prctl",
            State::Vulnerable => "Vulnerable",
        },
        _ => "Not affected",
    };
    alloc::format!("{}\n", s)
}

// ---- スレッドの SSB の印 (prctl) ----

/// 止めた (PR_SPEC_DISABLE。Linux の TIF_SSBD と PFA_SPEC_SSB_DISABLE)
pub const SSB_DISABLE: u8 = 1;
/// 止めて、戻せない (PR_SPEC_FORCE_DISABLE)
pub const SSB_FORCE: u8 = 2;
/// exec まで止める (PR_SPEC_DISABLE_NOEXEC)
pub const SSB_NOEXEC: u8 = 4;

pub const PR_SPEC_STORE_BYPASS: u64 = 0;
pub const PR_SPEC_NOT_AFFECTED: i64 = 0;
pub const PR_SPEC_PRCTL: i64 = 1;
pub const PR_SPEC_ENABLE: u64 = 2;
pub const PR_SPEC_DISABLE: u64 = 4;
pub const PR_SPEC_FORCE_DISABLE: u64 = 8;
pub const PR_SPEC_DISABLE_NOEXEC: u64 = 16;
/// SPSR (PSTATE) の SSBS
const PSR_SSBS: u64 = 1 << 12;

/// ユーザーに戻るときの PSTATE.SSBS をスレッドの印から (Linux の spectre_v4_enable_task_mitigation。いつも
/// 「prctl で」の決まりなので、CPU に SSBS がなくても印のとおりにビットを置く: Linux と同じ)
pub fn task_spsr(spsr: &mut u64, ssb: u8) {
    if ssb & SSB_DISABLE != 0 {
        *spsr &= !PSR_SSBS;
    } else {
        *spsr |= PSR_SSBS;
    }
}

/// exec のとき: PR_SPEC_DISABLE_NOEXEC なら止めるのをやめる (arch_setup_new_exec)
pub fn exec(ssb: &mut u8) {
    if *ssb & SSB_NOEXEC != 0 {
        *ssb &= !(SSB_NOEXEC | SSB_DISABLE);
    }
}

/// PR_GET_SPECULATION_CTRL (ssbd_prctl_get)
pub fn prctl_get(which: u64, ssb: u8) -> Result<i64, i64> {
    if which != PR_SPEC_STORE_BYPASS {
        return Err(-19);
    }
    Ok(match get().v4 {
        State::Unaffected => PR_SPEC_NOT_AFFECTED,
        State::Vulnerable => PR_SPEC_ENABLE as i64,
        State::Mitigated => {
            PR_SPEC_PRCTL
                | if ssb & SSB_FORCE != 0 {
                    PR_SPEC_FORCE_DISABLE
                } else if ssb & SSB_NOEXEC != 0 {
                    PR_SPEC_DISABLE_NOEXEC
                } else if ssb & SSB_DISABLE != 0 {
                    PR_SPEC_DISABLE
                } else {
                    PR_SPEC_ENABLE
                } as i64
        }
    })
}

/// PR_SET_SPECULATION_CTRL (ssbd_prctl_set)。変えた印を返す (呼んだ人が spsr に置く)
pub fn prctl_set(which: u64, ctrl: u64, ssb: u8) -> Result<u8, i64> {
    if which != PR_SPEC_STORE_BYPASS {
        return Err(-19);
    }
    const EPERM: i64 = 1;
    const ERANGE: i64 = 34;
    let mut s = ssb;
    match ctrl {
        PR_SPEC_ENABLE => {
            if s & SSB_FORCE != 0 {
                return Err(-EPERM);
            }
            s &= !(SSB_NOEXEC | SSB_DISABLE);
        }
        PR_SPEC_FORCE_DISABLE | PR_SPEC_DISABLE => {
            if ctrl == PR_SPEC_FORCE_DISABLE {
                s |= SSB_FORCE;
            }
            s = (s & !SSB_NOEXEC) | SSB_DISABLE;
        }
        PR_SPEC_DISABLE_NOEXEC => {
            if s & SSB_FORCE != 0 {
                return Err(-EPERM);
            }
            s |= SSB_DISABLE | SSB_NOEXEC;
        }
        _ => return Err(-ERANGE),
    }
    Ok(s)
}
