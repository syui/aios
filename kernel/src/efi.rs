// EFI スタブ: UEFI のファームウェア (や systemd-boot / GRUB) からアプリとして呼ばれたとき
//
// UEFI はこの Image を好きな場所に読み込み、恒等写像の MMU を入れたまま efi_entry を呼ぶ。
// カーネルは RAM の先頭 + 0x80000 で動く作りなので (boot.rs)、
//   1. 設定テーブルから DTB を探し、LoadOptions (systemd-boot の options や UEFI シェルの引数) を
//      カーネルのコマンドラインとして覚える。EFI_RNG_PROTOCOL があれば乱数を 32 バイトもらっておく (rand のもと)
//   2. メモリマップから RAM の先頭を探し、RAM の先頭 + 0x80000 を AllocatePages で押さえる
//   3. ExitBootServices。そのときのメモリマップから、あとも使えるランタイムサービスの領域 (EFI_MEMORY_RUNTIME) を
//      覚えておく (efirt.rs が呼ぶ。kalloc はそこを使わない)
//   4. そこへ自分を写し、キャッシュを掃き出し、MMU とキャッシュを切って primary へ (x0 = DTB)
// ここはリンクした場所 (上位アドレス) ではないところで動くので、PC 相対でしか
// 読み書きしない (パニックしない。static は adrp で PC 相対に指されるので、写す前の自分に書ける)。
use core::ptr::read_volatile;

type Status = usize;
const EFI_SUCCESS: Status = 0;
const EFI_LOAD_ERROR: Status = (1 << 63) | 1;

// EFI_SYSTEM_TABLE / EFI_BOOT_SERVICES の中の場所
const ST_FW_VENDOR: usize = 0x18;
const ST_FW_REVISION: usize = 0x20;
const ST_CONOUT: usize = 0x40;
const ST_RUNTIME: usize = 0x58;
const ST_BOOT_SERVICES: usize = 0x60;
const ST_NUM_TABLES: usize = 0x68;
const ST_TABLES: usize = 0x70;
const BS_ALLOCATE_PAGES: usize = 0x28;
const BS_GET_MEMORY_MAP: usize = 0x38;
const BS_ALLOCATE_POOL: usize = 0x40;
const BS_HANDLE_PROTOCOL: usize = 0x98;
const BS_EXIT_BOOT_SERVICES: usize = 0xe8;
const BS_LOCATE_PROTOCOL: usize = 0x140;
const ALLOCATE_ADDRESS: usize = 2;
const LOADER_DATA: usize = 2;

/// DTB の設定テーブルの GUID (b1b621d5-f19c-41a5-830b-d9152c69aae0)
/// EFI_LOADED_IMAGE_PROTOCOL (5b1b31a1-9562-11d2-8e3f-00a0c969723b) と、その中の LoadOptionsSize / LoadOptions
const LOADED_IMAGE_GUID: [u8; 16] = [0xa1, 0x31, 0x1b, 0x5b, 0x62, 0x95, 0xd2, 0x11, 0x8e, 0x3f, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b];
const LI_OPTIONS_SIZE: usize = 0x30;
const LI_OPTIONS: usize = 0x38;

/// UEFI から渡されたコマンドライン (NUL まで)。bss は primary で消されるので .data に置く
#[unsafe(link_section = ".data")]
static CMDLINE: crate::cell::Early<[u8; 512]> = crate::cell::Early::new([0; 512]);

/// UEFI から起動したときのコマンドライン (DTB の bootargs より優先する)
pub fn cmdline() -> Option<&'static str> {
    let b = CMDLINE.get();
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    core::str::from_utf8(&b[..n]).ok().map(str::trim).filter(|s| !s.is_empty())
}

/// LoadOptions (UTF-16) を CMDLINE へ。表示できる ASCII だけのときだけ (ブートメニューの
/// 項目は文字列でないデータを渡してくることがある)
unsafe fn save_options(image: usize, bs: usize, st: usize) {
    unsafe {
        let handle: extern "efiapi" fn(usize, usize, usize) -> Status = core::mem::transmute(rd64(bs + BS_HANDLE_PROTOCOL));
        let mut li = 0usize;
        if handle(image, LOADED_IMAGE_GUID.as_ptr() as usize, &mut li as *mut usize as usize) != EFI_SUCCESS || li == 0 {
            return;
        }
        let size = read_volatile((li + LI_OPTIONS_SIZE) as *const u32) as usize;
        let opts = rd64(li + LI_OPTIONS);
        if opts == 0 || size < 2 {
            return;
        }
        let buf = CMDLINE.get_mut();
        let mut n = 0;
        for i in 0..size / 2 {
            let c = read_volatile((opts + i * 2) as *const u16);
            if c == 0 {
                break;
            }
            if !(0x20..0x7f).contains(&c) || n + 1 >= buf.len() {
                buf[0] = 0;
                return;
            }
            buf[n] = c as u8;
            n += 1;
        }
        buf[n] = 0;
        if n > 0 {
            say(st, b"aios: command line from LoadOptions\n");
        }
    }
}

const DTB_GUID: [u8; 16] = [0xd5, 0x21, 0xb6, 0xb1, 0x9c, 0xf1, 0xa5, 0x41, 0x83, 0x0b, 0xd9, 0x15, 0x2c, 0x69, 0xaa, 0xe0];

/// EFI_RNG_PROTOCOL (3152bca5-eade-433d-862e-c01cdc291f44)。GetRNG は 2 つめ (+8)
const RNG_GUID: [u8; 16] = [0xa5, 0xbc, 0x52, 0x31, 0xde, 0xea, 0x3d, 0x43, 0x86, 0x2e, 0xc0, 0x1c, 0xdc, 0x29, 0x1f, 0x44];

/// UEFI からもらった乱数: 頭の 1 バイトが長さ。bss は primary で消されるので .data に置く
#[unsafe(link_section = ".data")]
static SEED: crate::cell::Early<[u8; 33]> = crate::cell::Early::new([0; 33]);

/// EFI_RNG_PROTOCOL があれば 32 バイトを SEED へ (ExitBootServices のあとは使えないので、いま)
unsafe fn save_seed(bs: usize, st: usize) {
    unsafe {
        let locate: extern "efiapi" fn(usize, usize, usize) -> Status = core::mem::transmute(rd64(bs + BS_LOCATE_PROTOCOL));
        let mut rng = 0usize;
        if locate(RNG_GUID.as_ptr() as usize, 0, &mut rng as *mut usize as usize) != EFI_SUCCESS || rng == 0 {
            return;
        }
        // GetRNG(This, RNGAlgorithm (NULL: ファームウェアのふつうのもの), RNGValueLength, RNGValue)
        let get: Fn4 = core::mem::transmute(rd64(rng + 8));
        let buf = SEED.get_mut().as_mut_ptr();
        if get(rng, 0, 32, buf.add(1) as usize) == EFI_SUCCESS {
            core::ptr::write_volatile(buf, 32);
            say(st, b"aios: 32 random bytes from EFI_RNG_PROTOCOL\n");
        }
    }
}

/// ランタイムの領域の数の上限 (QEMU の AAVMF で 20 ほど)
pub const RT_MAX: usize = 64;
/// メモリマップの属性: キャッシュできる (WB)、書けない (RO)、実行しない (XP)、ランタイム
pub const MEM_WB: u64 = 1 << 3;
pub const MEM_XP: u64 = 1 << 14;
pub const MEM_RO: u64 = 1 << 17;
const MEM_RUNTIME: u64 = 1 << 63;

/// UEFI のランタイムサービス (ExitBootServices のあとも呼べるもの。efirt.rs) を呼ぶのに要るもの
#[derive(Clone, Copy)]
pub struct Runtime {
    /// EFI_SYSTEM_TABLE と EFI_RUNTIME_SERVICES の物理アドレス (UEFI から起動していなければ 0)
    pub systab: u64,
    pub runtime: u64,
    /// UEFI の版 (表の頭の Revision: 0x0002_0046 なら 2.70) とファームウェアの版
    pub uefi_revision: u32,
    pub fw_revision: u32,
    /// ファームウェアの名前 (FirmwareVendor。ASCII の範囲だけ。NUL まで)
    pub vendor: [u8; 32],
    /// 設定テーブル: ACPI 2.0、ACPI、SMBIOS 3、SMBIOS (/sys/firmware/efi/systab。なければ 0)
    pub acpi20: u64,
    pub acpi: u64,
    pub smbios3: u64,
    pub smbios: u64,
    /// メモリマップのうち EFI_MEMORY_RUNTIME のもの: (種類, 物理アドレス, ページ数, 属性)
    pub n: usize,
    pub map: [[u64; 4]; RT_MAX],
}

/// bss は primary で消されるので .data に置く
#[unsafe(link_section = ".data")]
static RT: crate::cell::Early<Runtime> = crate::cell::Early::new(Runtime {
    systab: 0,
    runtime: 0,
    uefi_revision: 0,
    fw_revision: 0,
    vendor: [0; 32],
    acpi20: 0,
    acpi: 0,
    smbios3: 0,
    smbios: 0,
    n: 0,
    map: [[0; 4]; RT_MAX],
});

pub fn runtime() -> &'static Runtime {
    RT.get()
}

/// kalloc::init の前に: ランタイムサービスの RAM (キャッシュできるもの。ファームウェアがあとも使う) を kalloc によけてもらう
/// (Linux の efi_init の reserve_regions が memblock_reserve を呼ぶのと同じ向き: kalloc は UEFI を知らない)
pub fn reserve_runtime() {
    let rt = RT.get();
    for d in &rt.map[..rt.n] {
        if d[3] & MEM_WB != 0 {
            crate::kalloc::reserve(d[1] as usize, d[2] as usize * crate::memlayout::PGSIZE);
        }
    }
}

/// 設定テーブルの GUID: ACPI 2.0 (8868e871-e4f1-11d3-bc22-0080c73c8881)、ACPI (eb9d2d30-2d88-11d3-9a16-0090273fc14d)、
/// SMBIOS 3 (f2fd1544-9794-4a2c-992e-e5bbcf20e394)、SMBIOS (eb9d2d31-2d88-11d3-9a16-0090273fc14d)
const ACPI20_GUID: [u8; 16] = [0x71, 0xe8, 0x68, 0x88, 0xf1, 0xe4, 0xd3, 0x11, 0xbc, 0x22, 0x00, 0x80, 0xc7, 0x3c, 0x88, 0x81];
const ACPI_GUID: [u8; 16] = [0x30, 0x2d, 0x9d, 0xeb, 0x88, 0x2d, 0xd3, 0x11, 0x9a, 0x16, 0x00, 0x90, 0x27, 0x3f, 0xc1, 0x4d];
const SMBIOS3_GUID: [u8; 16] = [0x44, 0x15, 0xfd, 0xf2, 0x94, 0x97, 0x2c, 0x4a, 0x99, 0x2e, 0xe5, 0xbb, 0xcf, 0x20, 0xe3, 0x94];
const SMBIOS_GUID: [u8; 16] = [0x31, 0x2d, 0x9d, 0xeb, 0x88, 0x2d, 0xd3, 0x11, 0x9a, 0x16, 0x00, 0x90, 0x27, 0x3f, 0xc1, 0x4d];
/// EFI_RUNTIME_SERVICES の頭の印 ("RUNTSERV")
const RT_SIGNATURE: u64 = 0x5652_4553_544e_5552;

/// ExitBootServices の前に: 表のアドレス、版、名前、設定テーブル
unsafe fn save_runtime(st: usize) {
    unsafe {
        let rt = RT.get_mut();
        let runtime = rd64(st + ST_RUNTIME);
        if runtime == 0 || rd64(runtime) as u64 != RT_SIGNATURE {
            return;
        }
        rt.systab = st as u64;
        rt.runtime = runtime as u64;
        rt.uefi_revision = read_volatile((st + 8) as *const u32);
        rt.fw_revision = read_volatile((st + ST_FW_REVISION) as *const u32);
        let vendor = rd64(st + ST_FW_VENDOR);
        if vendor != 0 {
            let mut i = 0;
            while i + 1 < rt.vendor.len() {
                let c = read_volatile((vendor + i * 2) as *const u16);
                if c == 0 {
                    break;
                }
                rt.vendor[i] = if (0x20..0x7f).contains(&c) { c as u8 } else { b'?' };
                i += 1;
            }
        }
        let n = rd64(st + ST_NUM_TABLES);
        let tables = rd64(st + ST_TABLES);
        for i in 0..n {
            let e = tables + i * 24;
            let a = rd64(e + 16) as u64;
            if guid_eq(e, &ACPI20_GUID) {
                rt.acpi20 = a;
            } else if guid_eq(e, &ACPI_GUID) {
                rt.acpi = a;
            } else if guid_eq(e, &SMBIOS3_GUID) {
                rt.smbios3 = a;
            } else if guid_eq(e, &SMBIOS_GUID) {
                rt.smbios = a;
            }
        }
    }
}

/// ExitBootServices のあとに: そのときのメモリマップのランタイムの領域
unsafe fn save_runtime_map(buf: usize, map_size: usize, desc_size: usize) {
    unsafe {
        let rt = RT.get_mut();
        if rt.runtime == 0 {
            return;
        }
        for i in 0..map_size / desc_size {
            let d = buf + i * desc_size;
            let attr = rd64(d + 32) as u64;
            if attr & MEM_RUNTIME == 0 || rt.n >= RT_MAX {
                continue;
            }
            let e = rt.map.as_mut_ptr().add(rt.n) as *mut u64;
            core::ptr::write_volatile(e, read_volatile(d as *const u32) as u64);
            core::ptr::write_volatile(e.add(1), rd64(d + 8) as u64);
            core::ptr::write_volatile(e.add(2), rd64(d + 24) as u64);
            core::ptr::write_volatile(e.add(3), attr);
            rt.n += 1;
        }
    }
}

/// UEFI からもらった乱数を f に渡して消す (rand の池へ。なければ呼ばない)
pub fn take_seed(f: impl FnOnce(&[u8])) {
    let s = SEED.get_mut();
    let n = (s[0] as usize).min(32);
    if n > 0 {
        f(&s[1..1 + n]);
    }
    crate::crypto::wipe(s);
}

core::arch::global_asm!(
    r#"
.section .text
.global efi_entry
efi_entry:
    // x0 = ImageHandle, x1 = SystemTable
    stp     x29, x30, [sp, #-16]!
    mov     x29, sp
    // adr は ±1 MiB までなので、カーネルが大きくなっても届く adrp + add で
    adrp    x2, _start
    add     x2, x2, :lo12:_start
    bl      efi_main
    ldp     x29, x30, [sp], #16
    ret

// efi_enter(dest, src, size, dtb, dtb_size): 写して、掃き出して、MMU を切って跳ぶ
.global efi_enter
efi_enter:
    msr     daifset, #0xf
    mov     x19, x0
    mov     x20, x3
    mov     x21, x4
    // 写す (重ならないことは AllocatePages で確かめてある)
    mov     x5, x0
    mov     x6, x1
    mov     x7, x2
1:  cbz     x7, 2f
    ldr     x8, [x6], #8
    str     x8, [x5], #8
    sub     x7, x7, #8
    b       1b
    // 写したところと DTB を PoC まで掃き出す (MMU を切った後もそのまま読めるように)
2:  mov     x5, x19
    add     x6, x19, x2
3:  dc      cvac, x5
    add     x5, x5, #64
    cmp     x5, x6
    b.lo    3b
    mov     x5, x20
    add     x6, x20, x21
4:  cmp     x5, x6
    b.hs    5f
    dc      cvac, x5
    add     x5, x5, #64
    b       4b
5:  dsb     sy
    ic      iallu
    dsb     sy
    isb
    // いまの EL の MMU と キャッシュを切る
    mrs     x5, CurrentEL
    lsr     x5, x5, #2
    cmp     x5, #2
    b.ne    6f
    mrs     x5, sctlr_el2
    bic     x5, x5, #1
    bic     x5, x5, #(1 << 2)
    bic     x5, x5, #(1 << 12)
    msr     sctlr_el2, x5
    b       7f
6:  mrs     x5, sctlr_el1
    bic     x5, x5, #1
    bic     x5, x5, #(1 << 2)
    bic     x5, x5, #(1 << 12)
    msr     sctlr_el1, x5
7:  isb
    ic      iallu
    dsb     sy
    isb
    mov     x0, x20
    // primary は先頭から 0x1000 (ヘッダーの後ろ)
    add     x5, x19, #0x1000
    br      x5
"#
);

unsafe extern "C" {
    fn efi_enter(dest: usize, src: usize, size: usize, dtb: usize, dtb_size: usize) -> !;
}

unsafe fn rd64(a: usize) -> usize {
    unsafe { read_volatile(a as *const u64) as usize }
}

type Fn2 = extern "efiapi" fn(usize, usize) -> Status;
type Fn4 = extern "efiapi" fn(usize, usize, usize, usize) -> Status;
type Fn5 = extern "efiapi" fn(usize, usize, usize, usize, usize) -> Status;

/// ConOut に ASCII を出す (UTF-16 に直して)
unsafe fn say(st: usize, msg: &[u8]) {
    unsafe {
        let conout = rd64(st + ST_CONOUT);
        if conout == 0 {
            return;
        }
        let mut buf = [0u16; 96];
        let mut n = 0;
        for &c in msg {
            if n + 2 >= buf.len() {
                break;
            }
            if c == b'\n' {
                buf[n] = b'\r' as u16;
                n += 1;
            }
            buf[n] = c as u16;
            n += 1;
        }
        buf[n] = 0;
        let output: Fn2 = core::mem::transmute(rd64(conout + 8));
        output(conout, buf.as_ptr() as usize);
    }
}

fn guid_eq(a: usize, b: &[u8; 16]) -> bool {
    (0..16).all(|i| unsafe { read_volatile((a + i) as *const u8) } == b[i])
}

#[unsafe(no_mangle)]
unsafe extern "C" fn efi_main(image: usize, st: usize, base: usize) -> Status {
    unsafe {
        say(st, b"aios: EFI stub\n");

        let bs = rd64(st + ST_BOOT_SERVICES);

        // 1. DTB
        let mut dtb = 0;
        let n = rd64(st + ST_NUM_TABLES);
        let tables = rd64(st + ST_TABLES);
        for i in 0..n {
            let e = tables + i * 24;
            if guid_eq(e, &DTB_GUID) {
                dtb = rd64(e + 16);
            }
        }
        if dtb == 0 {
            say(st, b"aios: no device tree from the firmware\n");
            return EFI_LOAD_ERROR;
        }
        let dtb_size = u32::from_be(read_volatile((dtb + 4) as *const u32)) as usize;
        save_options(image, bs, st);
        save_seed(bs, st);
        save_runtime(st);

        // 2. メモリマップ (RAM の先頭を探すのと、ExitBootServices の key に使う)
        let get_map: Fn5 = core::mem::transmute(rd64(bs + BS_GET_MEMORY_MAP));
        let pool: extern "efiapi" fn(usize, usize, usize) -> Status = core::mem::transmute(rd64(bs + BS_ALLOCATE_POOL));
        let exit: Fn2 = core::mem::transmute(rd64(bs + BS_EXIT_BOOT_SERVICES));
        let (mut map_size, mut key, mut desc_size, mut ver) = (0usize, 0usize, 0usize, 0u32);
        get_map(&mut map_size as *mut usize as usize, 0, &mut key as *mut usize as usize, &mut desc_size as *mut usize as usize, &mut ver as *mut u32 as usize);
        let cap = map_size + 8 * desc_size.max(48);
        let mut buf = 0usize;
        if pool(LOADER_DATA, cap, &mut buf as *mut usize as usize) != EFI_SUCCESS {
            return EFI_LOAD_ERROR;
        }
        map_size = cap;
        if get_map(&mut map_size as *mut usize as usize, buf, &mut key as *mut usize as usize, &mut desc_size as *mut usize as usize, &mut ver as *mut u32 as usize) != EFI_SUCCESS
            || desc_size == 0
        {
            say(st, b"aios: cannot get the memory map\n");
            return EFI_LOAD_ERROR;
        }
        // RAM の先頭: RAM の種類 (LoaderCode .. ACPI NVS と Persistent) でいちばん低いところの 1 GiB の頭。
        // 読み込まれた場所 (base) は RAM の上のほうのこともある (RAM が 1 GiB より大きいとき)
        let mut ram = base;
        for i in 0..map_size / desc_size {
            let d = buf + i * desc_size;
            let ty = read_volatile(d as *const u32);
            if (1..=10).contains(&ty) || ty == 14 {
                ram = ram.min(rd64(d + 8));
            }
        }

        // 3. RAM の先頭 + 0x80000 へ写す。Image ヘッダーの image_size (bss まで)
        let size = rd64(base + 0x10);
        let dest = (ram & !0x3fff_ffff) + 0x80000;
        if dest != base {
            let alloc: Fn4 = core::mem::transmute(rd64(bs + BS_ALLOCATE_PAGES));
            let mut addr = dest;
            let st2 = alloc(ALLOCATE_ADDRESS, LOADER_DATA, size.div_ceil(4096), &mut addr as *mut usize as usize);
            if st2 != EFI_SUCCESS {
                say(st, b"aios: cannot reserve RAM base + 0x80000\n");
                return EFI_LOAD_ERROR;
            }
        }

        // 4. ExitBootServices (メモリマップの key が要る。変わっていたらもう一度)
        say(st, b"aios: exiting boot services\n");
        let mut ok = false;
        for _ in 0..4 {
            map_size = cap;
            if get_map(&mut map_size as *mut usize as usize, buf, &mut key as *mut usize as usize, &mut desc_size as *mut usize as usize, &mut ver as *mut u32 as usize) != EFI_SUCCESS {
                continue;
            }
            if exit(image, key) == EFI_SUCCESS {
                ok = true;
                break;
            }
        }
        if !ok {
            return EFI_LOAD_ERROR;
        }
        save_runtime_map(buf, map_size, desc_size);
        // もうファームウェアのブートサービスは使えない (ランタイムサービスは efirt.rs が呼ぶ)
        efi_enter(dest, base, size, dtb, dtb_size)
    }
}
