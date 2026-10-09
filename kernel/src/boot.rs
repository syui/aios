// cpu0 だけが起き、MMU を有効にして上位アドレスへ跳び、kmain へ。
// ほかの core は wfe で眠る。
//
// 先頭は Linux の arm64 Image ヘッダー (Documentation/arch/arm64/booting.rst)。
// これがあると QEMU やラズパイのファームウェアが Linux と同じように、
// RAM の先頭 + text_offset に置き、x0 に DTB の物理アドレスを入れて跳んでくる。
// Linux の EFI スタブと同じく PE/COFF のヘッダーも持つので、UEFI からも起動できる (efi.rs)。
// EL2 で来たら (ラズパイ、QEMU の raspi3b) EL1 に下りる。
//
// RAM の先頭 (= 置かれた場所 - 0x80000) は機械ごとに違う (qemu virt 0x4000_0000、ラズパイ 0)
// ので、ページ表はここで作る (memlayout.rs):
//   TTBR0 (MMU を入れる間だけ): 置かれた場所を含む 1 GiB を恒等写像
//   TTBR1: KBASE + i GiB (1 GiB ブロック) = RAM の先頭 + (i - 1) GiB
//     [0] 1 つ前の 1 GiB   デバイス (qemu virt: UART, GIC, virtio)
//     [1] RAM              通常 (ラズパイは 2 MiB ずつの表にして 0x3f00_0000 から上をデバイス)
//     [2] .. [16]          RAM が 0 からでなければ (qemu virt) 通常 = RAM の 1 GiB から先 (MAX_RAM まで。
//                          UEFI は DTB を RAM の上のほうに置く)。ラズパイは [2], [3] がデバイス
//                          (0x4000_0000 の local intc)
//     DTB で RAM の大きさがわかったら、memlayout::set_ram が RAM に合わせてなおす
core::arch::global_asm!(
    r#"
.equ MAIR_VALUE, 0xff00
.equ TCR_VALUE, (25 | (1 << 8) | (1 << 10) | (3 << 12) | (25 << 16) | (1 << 24) | (1 << 26) | (3 << 28) | (2 << 30) | (2 << 32))
// ブロック記述子の属性 (アドレスは別に足す)。L1 (1 GiB) と L2 (2 MiB) で同じ形
.equ ATTR_DEVICE, ((1 << 54) | (1 << 53) | (1 << 10) | (0 << 2) | 1)
.equ ATTR_NORMAL, ((1 << 54) | (1 << 10) | (3 << 8) | (1 << 2) | 1)
.equ GIB, 0x40000000
// ラズパイ (RAM が 0 から) の周辺機器の始まり
.equ RPI_PERIPH, 0x3f000000

.section .text.boot
.global _start
_start:
    // code0: "MZ" (PE/COFF の印) になる、何もしない命令 (ccmp x18, #0, #0xd, pl)
    .long   0xfa405a4d
    b       primary             // code1
    .quad   0x80000             // text_offset: RAM の先頭 + 0x80000 に置く
    .quad   _image_size         // image_size (bss まで)
    .quad   0x2                 // flags: little endian, 4KiB ページ
    .quad   0, 0, 0             // res2 - res4
    .ascii  "ARM\x64"           // magic
    .long   pe_header - _start  // res5: PE ヘッダーの場所

    // PE/COFF: UEFI のアプリとしても起動できるように (入口は efi.rs の efi_entry)
pe_header:
    .ascii  "PE\0\0"
    .short  0xaa64              // Machine: arm64
    .short  1                   // NumberOfSections
    .long   0, 0, 0             // TimeDateStamp, PointerToSymbolTable, NumberOfSymbols
    .short  opt_end - opt_header // SizeOfOptionalHeader
    .short  0x206               // EXECUTABLE_IMAGE | LINE_NUMS_STRIPPED | DEBUG_STRIPPED
opt_header:
    .short  0x20b               // PE32+
    .byte   0x02, 0x14          // リンカの版
    .long   _pe_raw_size        // SizeOfCode
    .long   0                   // SizeOfInitializedData
    .long   0                   // SizeOfUninitializedData
    .long   _efi_entry_rva      // AddressOfEntryPoint
    .long   0x1000              // BaseOfCode
    .quad   0                   // ImageBase
    .long   0x1000              // SectionAlignment
    .long   0x1000              // FileAlignment
    .short  0, 0, 0, 0, 0, 0    // OS / Image / Subsystem の版
    .long   0                   // Win32VersionValue
    .long   _pe_image_size      // SizeOfImage
    .long   0x1000              // SizeOfHeaders
    .long   0                   // CheckSum
    .short  10                  // Subsystem: EFI アプリ
    .short  0                   // DllCharacteristics
    .quad   0, 0, 0, 0          // Stack / Heap の Reserve, Commit
    .long   0                   // LoaderFlags
    .long   6                   // NumberOfRvaAndSizes
    .quad   0, 0, 0, 0, 0, 0    // データディレクトリ (再配置はなく、どこに置かれてもよい)
opt_end:
    .ascii  ".text\0\0\0"
    .long   _pe_virt_size       // VirtualSize (bss まで)
    .long   0x1000              // VirtualAddress
    .long   _pe_raw_size        // SizeOfRawData
    .long   0x1000              // PointerToRawData
    .long   0, 0                // PointerToRelocations, PointerToLinenumbers
    .short  0, 0
    .long   0xe0000020          // code | execute | read | write

    .balign 4096
primary:
    mov     x21, x0             // DTB の物理アドレス (ELF で起動したときは 0 など)
    mrs     x0, mpidr_el1
    and     x0, x0, #0xff
    cbz     x0, 1f
0:  wfe
    b       0b

    // EL2 なら EL1 へ (EL1 は AArch64、タイマを使える、割り込みは止めたまま)
1:  mrs     x0, CurrentEL
    lsr     x0, x0, #2
    cmp     x0, #2
    b.ne    2f
    mov     x0, #(1 << 31)
    msr     hcr_el2, x0
    mov     x0, #3
    msr     cnthctl_el2, x0
    msr     cntvoff_el2, xzr
    mov     x0, #0x33ff
    msr     cptr_el2, x0
    msr     hstr_el2, xzr
    ldr     x0, =0x30d00800
    msr     sctlr_el1, x0
    mov     x0, #0x3c5
    msr     spsr_el2, x0
    adr     x0, 2f
    msr     elr_el2, x0
    eret

    // x19 = RAM の先頭 (物理)。memlayout が使う
2:  adrp    x19, _start
    add     x19, x19, :lo12:_start
    sub     x19, x19, #0x80000
    adrp    x0, boot_ram_base
    add     x0, x0, :lo12:boot_ram_base
    str     x19, [x0]
    ldr     x5, =ATTR_NORMAL
    ldr     x6, =ATTR_DEVICE
    mov     x7, #GIB

    // TTBR0: 置かれた場所を含む 1 GiB を恒等写像
    adrp    x1, boot_l1_lo
    add     x1, x1, :lo12:boot_l1_lo
    adrp    x2, _start
    add     x2, x2, :lo12:_start
    lsr     x2, x2, #30
    lsl     x3, x2, #30
    orr     x3, x3, x5
    str     x3, [x1, x2, lsl #3]

    // TTBR1
    adrp    x1, boot_l1_hi
    add     x1, x1, :lo12:boot_l1_hi
    // [0]: 1 つ前の 1 GiB (RAM が 1 GiB より上から始まるときだけ)
    cmp     x19, x7
    b.lo    3f
    sub     x3, x19, x7
    orr     x3, x3, x6
    str     x3, [x1, #0]
3:  // [1]: RAM
    cbnz    x19, 5f
    // RAM が 0 から (ラズパイ): 2 MiB ずつ、RPI_PERIPH から上はデバイス
    adrp    x8, boot_l2_hi
    add     x8, x8, :lo12:boot_l2_hi
    mov     x9, #0              // 物理アドレス
    mov     x10, #0             // 添字
    ldr     x11, =RPI_PERIPH
4:  cmp     x9, x11
    csel    x12, x5, x6, lo
    orr     x12, x12, x9
    str     x12, [x8, x10, lsl #3]
    add     x9, x9, #0x200000
    add     x10, x10, #1
    cmp     x10, #512
    b.lo    4b
    orr     x3, x8, #3          // 表の記述子
    str     x3, [x1, #8]
    b       6f
5:  orr     x3, x19, x5
    str     x3, [x1, #8]
6:  // [2] から: RAM が 0 からなら (ラズパイ) [2], [3] をデバイス
    cbnz    x19, 7f
    add     x3, x19, x7
    orr     x3, x3, x6
    str     x3, [x1, #16]
    add     x3, x19, x7, lsl #1
    orr     x3, x3, x6
    str     x3, [x1, #24]
    b       9f
    // ほかは [2] .. [MAX_RAM_GIB] を RAM の続き (通常)。RAM の大きさがわかったら set_ram がなおす
7:  mov     x10, #2
    add     x9, x19, x7
8:  orr     x3, x9, x5
    str     x3, [x1, x10, lsl #3]
    add     x9, x9, x7
    add     x10, x10, #1
    cmp     x10, #({max_ram_gib} + 1)
    b.lo    8b
9:

    adrp    x0, boot_l1_lo
    add     x0, x0, :lo12:boot_l1_lo
    msr     ttbr0_el1, x0
    adrp    x0, boot_l1_hi
    add     x0, x0, :lo12:boot_l1_hi
    msr     ttbr1_el1, x0
    ldr     x0, =MAIR_VALUE
    msr     mair_el1, x0
    ldr     x0, =TCR_VALUE
    msr     tcr_el1, x0
    isb
    tlbi    vmalle1
    dsb     nsh
    mrs     x0, sctlr_el1
    orr     x0, x0, #(1 << 0)
    orr     x0, x0, #(1 << 2)
    orr     x0, x0, #(1 << 12)
    // EL0 にも Linux と同じく許す: DC ZVA (14、memset)、CTR_EL0 を読む (15)、
    // キャッシュの掃除 DC CVAU / IC IVAU (26、JIT が書いた命令を流すのに使う)
    orr     x0, x0, #(1 << 14)
    orr     x0, x0, #(1 << 15)
    orr     x0, x0, #(1 << 26)
    msr     sctlr_el1, x0
    isb

    ldr     x0, =7f
    br      x0

7:  ldr     x0, =__stack_top
    mov     sp, x0

    ldr     x0, =__bss_start
    ldr     x1, =__bss_end
8:  cmp     x0, x1
    b.ge    9f
    str     xzr, [x0], #8
    b       8b

    // DTB の場所を覚える (bss は消したあとなので .data に置く)
9:  ldr     x0, =boot_dtb
    str     x21, [x0]
    // EL0/EL1 の FP/SIMD を使えるようにする (カーネルは softfloat で触らない)
    mov     x0, #(3 << 20)
    msr     cpacr_el1, x0
    isb
    bl      kmain
    b       0b

.section .data
.balign 8
.global boot_dtb
boot_dtb:
    .quad   0
.global boot_ram_base
boot_ram_base:
    .quad   0
.balign 4096
.global boot_l1_lo
boot_l1_lo:
    .fill   512, 8, 0
.global boot_l1_hi
boot_l1_hi:
    .fill   512, 8, 0
boot_l2_hi:
    .fill   512, 8, 0
"#,
    max_ram_gib = const crate::memlayout::MAX_RAM >> 30
);
