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
// 名前は Linux の arm64 の vDSO と同じ並び (clock_getres、rt_sigreturn、gettimeofday、clock_gettime)
// int __kernel_clock_getres(clockid_t clk, struct timespec *res): いつもシステムコール (答えはカーネルのものと同じ)
.global vdso_getres
vdso_getres:
    mov     x8, #114            // clock_getres
    svc     #0
    ret
// __kernel_rt_sigreturn: シグナルのハンドラから戻るところ。SA_RESTORER を渡さないもの (aarch64 の glibc) は
// ハンドラの戻り先 (x30) がここになる (Linux の arm64 の sigtramp と同じ)
.global vdso_sigtramp
vdso_sigtramp:
    mov     x8, #139            // rt_sigreturn
    svc     #0
// int __kernel_gettimeofday(struct timeval *tv, struct timezone *tz): tz があればシステムコール
.global vdso_gettimeofday
vdso_gettimeofday:
    cbnz    x1, 8f
    cbz     x0, 7f
    mov     x16, x30
    bl      vdso_now
    mov     x30, x16
    add     x13, x13, x12
    mov     x15, #1000
    udiv    x14, x14, x15       // µs
    stp     x13, x14, [x0]
7:  mov     x0, #0
    ret
8:  mov     x8, #169            // gettimeofday
    svc     #0
    ret
// int __kernel_clock_gettime(clockid_t clk, struct timespec *ts)
.global vdso_clock_gettime
vdso_clock_gettime:
    cmp     x0, #7
    b.hi    9f
    // 0 REALTIME, 1 MONOTONIC, 4 MONOTONIC_RAW, 5 REALTIME_COARSE, 6 MONOTONIC_COARSE, 7 BOOTTIME
    mov     x2, #1
    lsl     x2, x2, x0
    mov     x3, #0xf3
    tst     x2, x3
    b.eq    9f
    mov     x16, x30
    bl      vdso_now
    mov     x30, x16
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
// 起動からの秒 (x13) と ns (x14)、起動したときの UNIX 秒 (x12)。x9 から x15 だけを使う
.global vdso_now
vdso_now:
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
    static vdso_getres: u8;
    static vdso_sigtramp: u8;
    static vdso_gettimeofday: u8;
    static vdso_clock_gettime: u8;
    static vdso_now: u8;
}

/// vDSO の中での場所 (コードの先頭から)
fn code_off(sym: *const u8) -> usize {
    sym as usize - (&raw const vdso_code_start) as usize
}

/// シグナルのハンドラの戻り先 (SA_RESTORER がないとき): vDSO の __kernel_rt_sigreturn (vDSO がなければ None)
pub fn sigtramp() -> Option<u64> {
    if PAGE.val().is_null() {
        return None;
    }
    Some((VDSO_VA + TEXT + code_off(&raw const vdso_sigtramp)) as u64)
}

/// ELF の中の場所 (Linux の vDSO と同じものを: soname、DT_HASH と DT_GNU_HASH、版の表)
const PHOFF: usize = 64;
const DYNSYM: usize = 0xc0;
const DYNSTR: usize = 0x160;
const HASH: usize = 0x1e0;
const GNU_HASH: usize = 0x200;
const VERSYM: usize = 0x230;
const VERDEF: usize = 0x240;
const DYNAMIC: usize = 0x280;
const TEXT: usize = 0x400;

/// soname と、名前の版 (Linux の arm64 の vDSO のもの。Go は __kernel_clock_gettime をこの版で探し、版の表
/// (DT_VERDEF) がないと番地 0x2 を読んで落ちる)
const SONAME: &[u8] = b"linux-vdso.so.1";
const VERSION: &[u8] = b"LINUX_2.6.39";

static PAGE: crate::cell::Early<*mut u8> = crate::cell::Early::new(core::ptr::null_mut());

fn put16(p: &mut [u8], o: usize, v: u16) {
    p[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(p: &mut [u8], o: usize, v: u32) {
    p[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(p: &mut [u8], o: usize, v: u64) {
    p[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// ELF の hash (DT_HASH と版の表の vd_hash)
fn elf_hash(s: &[u8]) -> u32 {
    let mut h: u32 = 0;
    for &c in s {
        h = (h << 4).wrapping_add(c as u32);
        let g = h & 0xf000_0000;
        if g != 0 {
            h ^= g >> 24;
        }
        h &= !g;
    }
    h
}

/// GNU の hash (DT_GNU_HASH)
fn gnu_hash(s: &[u8]) -> u32 {
    s.iter().fold(5381u32, |h, &c| h.wrapping_mul(33).wrapping_add(c as u32))
}

/// 文字列を足して、その場所 (DYNSTR から)
fn add_str(p: &mut [u8], at: &mut usize, s: &[u8]) -> u32 {
    let o = *at;
    p[DYNSTR + o..DYNSTR + o + s.len()].copy_from_slice(s);
    *at += s.len() + 1;
    o as u32
}

/// 起動のときに 1 回 (timer::init のあと)
pub fn init() {
    let page = kalloc::alloc().expect("vdso: no page");
    let p = unsafe { core::slice::from_raw_parts_mut(page, PGSIZE) };
    p.fill(0);
    let (start, end) = (&raw const vdso_code_start as usize, &raw const vdso_code_end as usize);
    let code = unsafe { core::slice::from_raw_parts(start as *const u8, end - start) };
    p[TEXT..TEXT + code.len()].copy_from_slice(code);
    let vvar_off = TEXT + code_off(&raw const vdso_vvar);

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

    // 文字列 (0 は空)
    let mut strsz = 1;
    let soname = add_str(p, &mut strsz, SONAME);
    let version = add_str(p, &mut strsz, VERSION);
    // シンボル: 0 番は空、1 番から (名前、場所、大きさ、種類: 2 FUNC、0 NOTYPE)
    let (getres, sigtramp, gtod, cgt, now) = (
        code_off(&raw const vdso_getres),
        code_off(&raw const vdso_sigtramp),
        code_off(&raw const vdso_gettimeofday),
        code_off(&raw const vdso_clock_gettime),
        code_off(&raw const vdso_now),
    );
    let syms: [(&[u8], usize, usize, u8); 4] = [
        (b"__kernel_clock_getres", getres, sigtramp - getres, 2),
        (b"__kernel_rt_sigreturn", sigtramp, gtod - sigtramp, 0),
        (b"__kernel_gettimeofday", gtod, cgt - gtod, 2),
        (b"__kernel_clock_gettime", cgt, now - cgt, 2),
    ];
    let n = syms.len() + 1;
    let mut bloom = 0u64;
    for (i, (name, off, size, ty)) in syms.iter().enumerate() {
        let s = DYNSYM + (i + 1) * 24;
        let name_at = add_str(p, &mut strsz, name);
        put32(p, s, name_at);
        p[s + 4] = 0x10 | ty; // GLOBAL
        put16(p, s + 6, 1); // st_shndx (0 でなければよい)
        put64(p, s + 8, (TEXT + off) as u64);
        put64(p, s + 16, *size as u64);
        // DT_HASH: バケツ 1 つ、鎖は 1 → 2 → ... → 0
        put32(p, HASH + 12 + (i + 1) * 4, if i + 2 < n { (i + 2) as u32 } else { 0 });
        // DT_GNU_HASH: 鎖は hash の下の 1 ビットを終わりの印に
        let h = gnu_hash(name);
        bloom |= 1 << (h % 64) | 1 << ((h >> 6) % 64);
        put32(p, GNU_HASH + 28 + i * 4, (h & !1) | (i + 2 == n) as u32);
        // 版: みな LINUX_2.6.39 (2 番)。0 番は LOCAL (0)
        put16(p, VERSYM + (i + 1) * 2, 2);
    }
    // DT_HASH: nbucket 1, nchain n, bucket[0] = 1
    put32(p, HASH, 1);
    put32(p, HASH + 4, n as u32);
    put32(p, HASH + 8, 1);
    // DT_GNU_HASH: nbuckets 1, symoffset 1, bloom 1 語 (shift 6)、bucket[0] = 1
    put32(p, GNU_HASH, 1);
    put32(p, GNU_HASH + 4, 1);
    put32(p, GNU_HASH + 8, 1);
    put32(p, GNU_HASH + 12, 6);
    put64(p, GNU_HASH + 16, bloom);
    put32(p, GNU_HASH + 24, 1);
    // 版の表: 1 番は soname (BASE)、2 番は LINUX_2.6.39。Elf64_Verdef (20 バイト) のすぐあとに Elf64_Verdaux
    let verdef = |p: &mut [u8], o: usize, flags: u16, ndx: u16, name: &[u8], name_at: u32, next: u32| {
        put16(p, o, 1); // vd_version
        put16(p, o + 2, flags);
        put16(p, o + 4, ndx);
        put16(p, o + 6, 1); // vd_cnt
        put32(p, o + 8, elf_hash(name));
        put32(p, o + 12, 20); // vd_aux
        put32(p, o + 16, next);
        put32(p, o + 20, name_at); // vda_name
        put32(p, o + 24, 0); // vda_next
    };
    verdef(p, VERDEF, 1, 1, SONAME, soname, 28);
    verdef(p, VERDEF + 28, 0, 2, VERSION, version, 0);
    // dynamic (Linux と同じ並び)
    let dynv: [(u64, u64); 11] = [
        (14, soname as u64),             // DT_SONAME
        (4, HASH as u64),                // DT_HASH
        (0x6fff_fef5, GNU_HASH as u64),  // DT_GNU_HASH
        (5, DYNSTR as u64),              // DT_STRTAB
        (6, DYNSYM as u64),              // DT_SYMTAB
        (10, strsz as u64),              // DT_STRSZ
        (11, 24),                        // DT_SYMENT
        (0x6fff_fffc, VERDEF as u64),    // DT_VERDEF
        (0x6fff_fffd, 2),                // DT_VERDEFNUM
        (0x6fff_fff0, VERSYM as u64),    // DT_VERSYM
        (0, 0),                          // DT_NULL
    ];
    for (i, (k, v)) in dynv.iter().enumerate() {
        put64(p, DYNAMIC + i * 16, *k);
        put64(p, DYNAMIC + i * 16 + 8, *v);
    }
    ph(p, 1, 2, 4, DYNAMIC, dynv.len() * 16);
    // 場所が重なっていないか (足したときに)
    debug_assert!(PHOFF + 2 * 56 <= DYNSYM && DYNSYM + n * 24 <= DYNSTR && DYNSTR + strsz <= HASH);
    debug_assert!(HASH + (3 + n) * 4 <= GNU_HASH && GNU_HASH + 28 + (n - 1) * 4 <= VERSYM && VERSYM + n * 2 <= VERDEF);
    debug_assert!(VERDEF + 56 <= DYNAMIC && DYNAMIC + dynv.len() * 16 <= TEXT && vvar_off + 16 <= PGSIZE);
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
    PAGE.set(page);
}

/// プロセスのアドレス空間に写す
pub fn map(pt: &mut crate::vm::PageTable) -> Option<()> {
    let page = PAGE.val();
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
