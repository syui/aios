// UEFI のランタイムサービス: ExitBootServices のあとも、ファームウェアの変数 (GetVariable、SetVariable、
// GetNextVariableName、QueryVariableInfo) を呼ぶ (efivarfs.rs が使う。Linux の drivers/firmware/efi/runtime-wrappers.c)
//
// EFI スタブ (efi.rs) が覚えたランタイムの領域 (メモリマップの EFI_MEMORY_RUNTIME) を、物理アドレスと同じ仮想アドレス
// (1:1) に写した表を作っておき、呼ぶあいだだけ TTBR0 をその表にする (SetVirtualAddressMap は呼ばない: UEFI では、
// 呼ばなければ 1:1 のまま呼べる)。カーネルの半分 (TTBR1) はそのままなので、引数のバッファとスタックはカーネルの
// もののまま渡せる。ランタイムの RAM は kalloc が使わない (kalloc::init)。
// ファームウェアは同時に呼べないので 1 つのロックで (割り込みも止める)。FP/SIMD のレジスタ (ユーザーのもの) と
// x18 (プラットフォームのレジスタ) は呼ぶ前と同じに戻す
use crate::efi;
use crate::memlayout::{p2v, v2p};
use crate::spinlock::SpinLock;
use alloc::vec;
use alloc::vec::Vec;

pub type Status = usize;
pub const SUCCESS: Status = 0;
const ERR: usize = 1 << 63;
pub const UNSUPPORTED: Status = ERR | 3;
pub const BUFFER_TOO_SMALL: Status = ERR | 5;
pub const DEVICE_ERROR: Status = ERR | 7;
pub const WRITE_PROTECTED: Status = ERR | 8;
pub const OUT_OF_RESOURCES: Status = ERR | 9;
pub const NOT_FOUND: Status = ERR | 14;
pub const ABORTED: Status = ERR | 21;
pub const SECURITY_VIOLATION: Status = ERR | 26;

// EFI_RUNTIME_SERVICES の中の場所
const RT_GET_VARIABLE: usize = 0x48;
const RT_GET_NEXT_VARIABLE_NAME: usize = 0x50;
const RT_SET_VARIABLE: usize = 0x58;
const RT_QUERY_VARIABLE_INFO: usize = 0x80;
/// 表の頭の HeaderSize (QueryVariableInfo は UEFI 2.0 から: 表がそこまであるか)
const RT_HDR_SIZE: usize = 12;

// 1:1 の表の項目 (4 KiB のページ、39 ビット。MAIR の 0 は Device-nGnRnE、1 は通常のメモリ: boot.rs)
const PTE_PAGE: u64 = 0b11;
const PTE_NORMAL: u64 = 1 << 2;
const PTE_SH_INNER: u64 = 3 << 8;
const PTE_RDONLY: u64 = 1 << 7;
const PTE_AF: u64 = 1 << 10;
const PTE_NG: u64 = 1 << 11;
const PTE_PXN: u64 = 1 << 53;
const PTE_UXN: u64 = 1 << 54;
const VA_BITS: u32 = 39;

/// 1:1 の表 (L1) の物理アドレス。0 ならランタイムサービスはない
static ROOT: crate::cell::Early<usize> = crate::cell::Early::new(0);
static LOCK: SpinLock<()> = SpinLock::new(());

unsafe extern "C" {
    fn fp_save(st: *mut crate::proc::FpState);
    fn fp_load(st: *const crate::proc::FpState);
    fn efi_rt_call(f: usize, a0: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> usize;
}

core::arch::global_asm!(
    r#"
.section .text
// efi_rt_call(f, a0, a1, a2, a3, a4): f(a0..a4) を呼ぶ。x18 はファームウェアが変えても戻す
.global efi_rt_call
efi_rt_call:
    stp     x29, x30, [sp, #-32]!
    mov     x29, sp
    str     x18, [sp, #16]
    mov     x9, x0
    mov     x0, x1
    mov     x1, x2
    mov     x2, x3
    mov     x3, x4
    mov     x4, x5
    blr     x9
    ldr     x18, [sp, #16]
    ldp     x29, x30, [sp], #32
    ret
"#
);

/// ランタイムサービスが使えるか
pub fn available() -> bool {
    *ROOT.get() != 0
}

/// 起動のとき (kalloc のあと): 1:1 の表を作る。Linux と同じく「efi: EFI v2.7 by EDK II」と出す
pub fn init() {
    let rt = efi::runtime();
    if rt.runtime == 0 {
        return;
    }
    let vendor = core::str::from_utf8(&rt.vendor[..rt.vendor.iter().position(|&c| c == 0).unwrap_or(rt.vendor.len())]).unwrap_or("?");
    let (major, minor) = (rt.uefi_revision >> 16, rt.uefi_revision & 0xffff);
    if minor % 10 == 0 {
        crate::println!("efi: EFI v{}.{} by {}", major, minor / 10, vendor);
    } else {
        crate::println!("efi: EFI v{}.{}.{} by {}", major, minor / 10, minor % 10, vendor);
    }
    let mut tables = alloc::string::String::new();
    for (name, a) in [("ACPI 2.0", rt.acpi20), ("ACPI", rt.acpi), ("SMBIOS 3.0", rt.smbios3), ("SMBIOS", rt.smbios)] {
        if a != 0 {
            tables.push_str(&alloc::format!(" {}={:#x}", name, a));
        }
    }
    if !tables.is_empty() {
        crate::println!("efi:{}", tables);
    }
    let Some(root) = crate::kalloc::alloc() else { return };
    let root = root as *mut u64;
    for d in &rt.map[..rt.n] {
        let (pa, pages, attr) = (d[1] as usize, d[2] as usize, d[3]);
        if pa + pages * 4096 > 1 << VA_BITS {
            crate::println!("efi: runtime region {:#x} is above {} bits, runtime services disabled", pa, VA_BITS);
            return;
        }
        // キャッシュできるもの (RAM) は通常のメモリで実行してよい (XP なら実行しない、RO なら書けない)。
        // ほか (フラッシュ、RTC などの MMIO) はデバイスで実行しない
        let bits = if attr & efi::MEM_WB != 0 {
            PTE_NORMAL | PTE_SH_INNER | if attr & efi::MEM_XP != 0 { PTE_PXN } else { 0 } | if attr & efi::MEM_RO != 0 { PTE_RDONLY } else { 0 }
        } else {
            PTE_PXN
        };
        for i in 0..pages {
            let a = pa + i * 4096;
            if map(root, a, a as u64 | PTE_PAGE | PTE_AF | PTE_NG | PTE_UXN | bits).is_none() {
                crate::println!("efi: no memory for the runtime map");
                return;
            }
        }
    }
    ROOT.set(v2p(root as usize));
}

/// 1:1 の表に 1 ページ (L1 → L2 → L3)
fn map(root: *mut u64, va: usize, pte: u64) -> Option<()> {
    let mut t = root;
    for shift in [30, 21] {
        let e = unsafe { t.add((va >> shift) & 511) };
        if unsafe { *e } & 1 == 0 {
            let p = crate::kalloc::alloc()?;
            unsafe { *e = v2p(p as usize) as u64 | 0b11 };
        }
        t = p2v((unsafe { *e } & 0x0000_ffff_ffff_f000) as usize) as *mut u64;
    }
    unsafe { *t.add((va >> 12) & 511) = pte };
    Some(())
}

/// EFI_RUNTIME_SERVICES の off の関数を、1:1 の表に切りかえて呼ぶ
fn call(off: usize, a: [usize; 5]) -> Status {
    let root = *ROOT.get();
    if root == 0 {
        return UNSUPPORTED;
    }
    let rt = efi::runtime().runtime as usize;
    // 表がそこまであるか (頭の HeaderSize)
    let hdr = unsafe { core::ptr::read_volatile(p2v(rt + RT_HDR_SIZE) as *const u32) } as usize;
    if off + 8 > hdr {
        return UNSUPPORTED;
    }
    let f = unsafe { core::ptr::read_volatile(p2v(rt + off) as *const usize) };
    if f == 0 {
        return UNSUPPORTED;
    }
    let _g = LOCK.lock();
    let mut fp = crate::proc::FpState { q: [0; 32], fpcr: 0, fpsr: 0 };
    let old: usize;
    let r;
    unsafe {
        fp_save(&mut fp);
        core::arch::asm!("mrs {}, ttbr0_el1", out(reg) old);
        core::arch::asm!("msr ttbr0_el1, {}", "isb", "tlbi vmalle1", "dsb ish", "isb", in(reg) root);
        r = efi_rt_call(f, a[0], a[1], a[2], a[3], a[4]);
        core::arch::asm!("msr ttbr0_el1, {}", "isb", "tlbi vmalle1", "dsb ish", "isb", in(reg) old);
        fp_load(&fp);
    }
    r
}

/// UEFI の変数の名前 (UTF-16、NUL で終わる) と GUID
#[repr(C, align(8))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Guid(pub [u8; 16]);

/// GetVariable: 中身を buf へ。(状態, 属性, 大きさ)。buf が小さければ BUFFER_TOO_SMALL と要る大きさ
pub fn get_variable(name: &[u16], guid: &Guid, buf: &mut [u8]) -> (Status, u32, usize) {
    let mut attr = 0u32;
    let mut size = buf.len();
    let data = if buf.is_empty() { 0 } else { buf.as_mut_ptr() as usize };
    let r = call(RT_GET_VARIABLE, [name.as_ptr() as usize, guid as *const Guid as usize, &mut attr as *mut u32 as usize, &mut size as *mut usize as usize, data]);
    (r, attr, size)
}

/// GetVariable の中身をみな: (属性, 中身)
pub fn read_variable(name: &[u16], guid: &Guid) -> Result<(u32, Vec<u8>), Status> {
    let mut buf = Vec::new();
    loop {
        let (r, attr, size) = get_variable(name, guid, &mut buf);
        match r {
            SUCCESS => {
                buf.truncate(size);
                return Ok((attr, buf));
            }
            BUFFER_TOO_SMALL if size > buf.len() => buf = vec![0; size],
            e => return Err(e),
        }
    }
}

/// 変数の中身の大きさ (GetVariable を 0 バイトで: Linux の efivar_entry_size)
pub fn variable_size(name: &[u16], guid: &Guid) -> Result<usize, Status> {
    match get_variable(name, guid, &mut []) {
        (BUFFER_TOO_SMALL, _, size) => Ok(size),
        // 0 バイトの変数はない (SetVariable の 0 バイトは消すこと) が、あれば 0
        (SUCCESS, _, size) => Ok(size),
        (e, _, _) => Err(e),
    }
}

/// SetVariable (data が空なら消す。APPEND_WRITE なら足す)
pub fn set_variable(name: &[u16], guid: &Guid, attr: u32, data: &[u8]) -> Status {
    let p = if data.is_empty() { 0 } else { data.as_ptr() as usize };
    call(RT_SET_VARIABLE, [name.as_ptr() as usize, guid as *const Guid as usize, attr as usize, data.len(), p])
}

/// 変数の一覧 (GetNextVariableName をくりかえす): (名前 (NUL なし), GUID)
pub fn variables() -> Result<Vec<(Vec<u16>, Guid)>, Status> {
    let mut out = Vec::new();
    let mut name: Vec<u16> = vec![0; 512];
    let mut guid = Guid([0; 16]);
    loop {
        let mut size = name.len() * 2;
        let r = call(RT_GET_NEXT_VARIABLE_NAME, [&mut size as *mut usize as usize, name.as_mut_ptr() as usize, &mut guid as *mut Guid as usize, 0, 0]);
        match r {
            SUCCESS => {
                let n = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                out.push((name[..n].to_vec(), guid));
            }
            // 名前の入れものが小さい: 大きくして同じところから (いまの名前は変わっていない)
            BUFFER_TOO_SMALL if size / 2 > name.len() => name.resize(size / 2, 0),
            NOT_FOUND => return Ok(out),
            e => return Err(e),
        }
        if out.len() > 4096 {
            // ファームウェアがまちがって同じものを返しつづけるとき
            return Ok(out);
        }
    }
}

/// QueryVariableInfo: (全部の大きさ, 残り, 1 つの変数の上限)
pub fn query_variable_info(attr: u32) -> Result<(u64, u64, u64), Status> {
    let (mut total, mut left, mut max) = (0u64, 0u64, 0u64);
    match call(RT_QUERY_VARIABLE_INFO, [attr as usize, &mut total as *mut u64 as usize, &mut left as *mut u64 as usize, &mut max as *mut u64 as usize, 0]) {
        SUCCESS => Ok((total, left, max)),
        e => Err(e),
    }
}

/// UEFI の状態を Linux の errno に (efi_status_to_err)
pub fn errno(s: Status) -> i64 {
    match s {
        SUCCESS => 0,
        OUT_OF_RESOURCES => -28,
        DEVICE_ERROR => -5,
        WRITE_PROTECTED => -30,
        SECURITY_VIOLATION => -13,
        NOT_FOUND => -2,
        ABORTED => -4,
        _ => -22,
    }
}
