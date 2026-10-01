// vDSO: clock_gettime をカーネルに入らずに済ませる
//
// 1 ページの小さな ELF 共有ライブラリを起動のときに組み立て、すべてのプロセスの VDSO_VA に
// 読み取り専用で写す (場所は auxv の AT_SYSINFO_EHDR で渡す)。musl は静的リンクでも
// これを見つけて __kernel_clock_gettime (LINUX_2.6.39) を呼ぶ (Rust の Instant::now も)。
// 中身はタイマのカウンタ (CNTVCT_EL0、CNTKCTL_EL1 で EL0 から読めるようにする) と、
// 同じページの終わりにある vvar (周波数、起動したときの UNIX 秒) から時刻を計算する。
// 知らない時計は svc でふつうのシステムコールにする。計算は timer.rs と同じ。
use crate::kalloc;
use crate::memlayout::PGSIZE;

/// ユーザー空間での場所 (スタックの領域の下)
pub const VDSO_VA: usize = 0x3f_0000_0000;

core::arch::global_asm!(
    r#"
.section .rodata
.balign 16
.global vdso_code_start
vdso_code_start:
// int __kernel_clock_gettime(clockid_t clk, struct timespec *ts)
    cmp     x0, #7
    b.hi    9f
    // 0 REALTIME, 1 MONOTONIC, 4 MONOTONIC_RAW, 5 REALTIME_COARSE, 6 MONOTONIC_COARSE, 7 BOOTTIME
    mov     x2, #1
    lsl     x2, x2, x0
    mov     x3, #0xf3
    tst     x2, x3
    b.eq    9f
    adr     x9, vdso_vvar
    isb
    mrs     x10, cntvct_el0
    ldr     x11, [x9]           // 周波数
    ldr     x12, [x9, #8]       // 起動したときの UNIX 秒
    udiv    x13, x10, x11       // 秒 = cnt / f
    msub    x14, x13, x11, x10  // 余り
    movz    x15, #0xca00
    movk    x15, #0x3b9a, lsl #16   // 1e9
    mul     x14, x14, x15
    udiv    x14, x14, x11       // ns = 余り * 1e9 / f
    cmp     x0, #0
    b.eq    1f
    cmp     x0, #5
    b.ne    2f
1:  add     x13, x13, x12
2:  stp     x13, x14, [x1]
    mov     x0, #0
    ret
9:  mov     x8, #113            // clock_gettime
    svc     #0
    ret
.balign 8
vdso_vvar:
    .quad   0, 0
.global vdso_code_end
vdso_code_end:
"#
);

unsafe extern "C" {
    static vdso_code_start: u8;
    static vdso_code_end: u8;
    static vdso_vvar: u8;
}

/// ELF の中の場所
const PHOFF: usize = 64;
const DYNSYM: usize = 0x100;
const DYNSTR: usize = 0x140;
const HASH: usize = 0x180;
const DYNAMIC: usize = 0x1c0;
const TEXT: usize = 0x400;

static mut PAGE: *mut u8 = core::ptr::null_mut();

fn put16(p: &mut [u8], o: usize, v: u16) {
    p[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(p: &mut [u8], o: usize, v: u32) {
    p[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(p: &mut [u8], o: usize, v: u64) {
    p[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// 起動のときに 1 回 (timer::init のあと)
pub fn init() {
    let page = kalloc::alloc().expect("vdso: no page");
    let p = unsafe { core::slice::from_raw_parts_mut(page, PGSIZE) };
    let (start, end, vvar) = (&raw const vdso_code_start as usize, &raw const vdso_code_end as usize, &raw const vdso_vvar as usize);
    let code = unsafe { core::slice::from_raw_parts(start as *const u8, end - start) };
    p[TEXT..TEXT + code.len()].copy_from_slice(code);
    let vvar_off = TEXT + (vvar - start);

    // ELF ヘッダ
    p[0..4].copy_from_slice(b"\x7fELF");
    p[4] = 2; // 64 bit
    p[5] = 1; // little endian
    p[6] = 1; // version
    put16(p, 16, 3); // ET_DYN
    put16(p, 18, 183); // aarch64
    put32(p, 20, 1);
    put64(p, 32, PHOFF as u64);
    put16(p, 52, 64); // ehsize
    put16(p, 54, 56); // phentsize
    put16(p, 56, 2); // phnum
    put16(p, 58, 64); // shentsize (セクションは無し)
    // PT_LOAD (ページまるごと、R+X) と PT_DYNAMIC
    let ph = |p: &mut [u8], i: usize, ty: u32, flags: u32, off: usize, size: usize| {
        let o = PHOFF + i * 56;
        put32(p, o, ty);
        put32(p, o + 4, flags);
        put64(p, o + 8, off as u64);
        put64(p, o + 16, off as u64);
        put64(p, o + 24, off as u64);
        put64(p, o + 32, size as u64);
        put64(p, o + 40, size as u64);
        put64(p, o + 48, if ty == 1 { PGSIZE as u64 } else { 8 });
    };
    ph(p, 0, 1, 5, 0, PGSIZE);
    ph(p, 1, 2, 4, DYNAMIC, 6 * 16);
    // 文字列: "\0__kernel_clock_gettime\0"
    let name = b"__kernel_clock_gettime\0";
    p[DYNSTR + 1..DYNSTR + 1 + name.len()].copy_from_slice(name);
    // シンボル: 0 番は空、1 番が __kernel_clock_gettime (GLOBAL FUNC)
    let s = DYNSYM + 24;
    put32(p, s, 1);
    p[s + 4] = 0x12;
    put16(p, s + 6, 1); // st_shndx (0 でなければよい)
    put64(p, s + 8, TEXT as u64);
    put64(p, s + 16, (vvar_off - TEXT) as u64);
    // DT_HASH: nbucket 1, nchain 2, bucket[0] = 1, chain = [0, 0]
    put32(p, HASH, 1);
    put32(p, HASH + 4, 2);
    put32(p, HASH + 8, 1);
    // dynamic: HASH, STRTAB, SYMTAB, STRSZ, SYMENT, NULL
    let dynv: [(u64, u64); 6] = [(4, HASH as u64), (5, DYNSTR as u64), (6, DYNSYM as u64), (10, (1 + name.len()) as u64), (11, 24), (0, 0)];
    for (i, (k, v)) in dynv.iter().enumerate() {
        put64(p, DYNAMIC + i * 16, *k);
        put64(p, DYNAMIC + i * 16 + 8, *v);
    }
    // vvar
    put64(p, vvar_off, crate::timer::freq());
    put64(p, vvar_off + 8, crate::timer::boot_epoch());
    // 命令として読めるように、キャッシュを掃き出す
    let mut a = page as usize;
    while a < page as usize + PGSIZE {
        unsafe { core::arch::asm!("dc cvau, {0}", "ic ivau, {0}", in(reg) a) };
        a += 64;
    }
    unsafe { core::arch::asm!("dsb ish", "isb") };
    unsafe { PAGE = page };
}

/// プロセスのアドレス空間に写す
pub fn map(pt: &mut crate::vm::PageTable) -> Option<()> {
    let page = unsafe { PAGE };
    if page.is_null() {
        return Some(());
    }
    pt.map(VDSO_VA, VDSO_VA + PGSIZE, crate::vm::PROT_READ | crate::vm::PROT_EXEC, false, crate::vm::Backing::Anon)?;
    pt.install(VDSO_VA, page, crate::vm::PROT_READ | crate::vm::PROT_EXEC)
}

/// EL0 からタイマのカウンタを読めるようにする (CPU ごと)
pub fn allow_counter() {
    // EL0PCTEN と EL0VCTEN (物理と仮想のカウンタ)
    unsafe { core::arch::asm!("msr cntkctl_el1, {}", "isb", in(reg) 3u64) };
}
