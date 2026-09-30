// EFI スタブ: UEFI のファームウェア (や systemd-boot / GRUB) からアプリとして呼ばれたとき
//
// UEFI はこの Image を好きな場所に読み込み、恒等写像の MMU を入れたまま efi_entry を呼ぶ。
// カーネルは RAM の先頭 + 0x80000 で動く作りなので (boot.rs)、
//   1. 設定テーブルから DTB を探す
//   2. RAM の先頭 + 0x80000 を AllocatePages で押さえる
//   3. ExitBootServices
//   4. そこへ自分を写し、キャッシュを掃き出し、MMU とキャッシュを切って primary へ (x0 = DTB)
// ここはリンクした場所 (上位アドレス) ではないところで動くので、PC 相対でしか
// 読み書きしない (static を使わない、パニックしない)。
use core::ptr::read_volatile;

type Status = usize;
const EFI_SUCCESS: Status = 0;
const EFI_LOAD_ERROR: Status = (1 << 63) | 1;

// EFI_SYSTEM_TABLE / EFI_BOOT_SERVICES の中の場所
const ST_CONOUT: usize = 0x40;
const ST_BOOT_SERVICES: usize = 0x60;
const ST_NUM_TABLES: usize = 0x68;
const ST_TABLES: usize = 0x70;
const BS_ALLOCATE_PAGES: usize = 0x28;
const BS_GET_MEMORY_MAP: usize = 0x38;
const BS_ALLOCATE_POOL: usize = 0x40;
const BS_EXIT_BOOT_SERVICES: usize = 0xe8;
const ALLOCATE_ADDRESS: usize = 2;
const LOADER_DATA: usize = 2;

/// DTB の設定テーブルの GUID (b1b621d5-f19c-41a5-830b-d9152c69aae0)
const DTB_GUID: [u8; 16] = [0xd5, 0x21, 0xb6, 0xb1, 0x9c, 0xf1, 0xa5, 0x41, 0x83, 0x0b, 0xd9, 0x15, 0x2c, 0x69, 0xaa, 0xe0];

core::arch::global_asm!(
    r#"
.section .text
.global efi_entry
efi_entry:
    // x0 = ImageHandle, x1 = SystemTable
    stp     x29, x30, [sp, #-16]!
    mov     x29, sp
    adr     x2, _start
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

        // 2. RAM の先頭 (読み込まれた場所を含む 1 GiB の頭) + 0x80000
        // Image ヘッダーの image_size (bss まで)
        let size = rd64(base + 0x10);
        let dest = (base & !0x3fff_ffff) + 0x80000;
        if dest != base {
            let alloc: Fn4 = core::mem::transmute(rd64(bs + BS_ALLOCATE_PAGES));
            let mut addr = dest;
            let st2 = alloc(ALLOCATE_ADDRESS, LOADER_DATA, size.div_ceil(4096), &mut addr as *mut usize as usize);
            if st2 != EFI_SUCCESS {
                say(st, b"aios: cannot reserve RAM base + 0x80000\n");
                return EFI_LOAD_ERROR;
            }
        }

        // 3. ExitBootServices (メモリマップの key が要る。変わっていたらもう一度)
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
        // もうファームウェアは使えない
        efi_enter(dest, base, size, dtb, dtb_size)
    }
}
