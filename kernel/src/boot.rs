// cpu0 だけが起き、MMU を有効にして上位アドレスへ跳び、kmain へ。
// ほかの core は wfe で眠る。
//
// boot_l1 は TTBR0 (恒等写像) と TTBR1 (KBASE + PA) で共用する 1GiB ブロック表:
//   [0] PA 0x0000_0000.. デバイス (UART, GIC)
//   [1] PA 0x4000_0000.. RAM
core::arch::global_asm!(
    r#"
.equ MAIR_VALUE, 0xff00
.equ TCR_VALUE, (25 | (1 << 8) | (1 << 10) | (3 << 12) | (25 << 16) | (1 << 24) | (1 << 26) | (3 << 28) | (2 << 30) | (2 << 32))
.equ PTE_DEVICE, (0x00000000 | (1 << 54) | (1 << 53) | (1 << 10) | (0 << 2) | 1)
.equ PTE_NORMAL, (0x40000000 | (1 << 54) | (1 << 10) | (3 << 8) | (1 << 2) | 1)

.section .text.boot
.global _start
_start:
    mrs     x0, mpidr_el1
    and     x0, x0, #0xff
    cbz     x0, 1f
0:  wfe
    b       0b

1:  adrp    x0, boot_l1
    msr     ttbr0_el1, x0
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
    msr     sctlr_el1, x0
    isb

    ldr     x0, =2f
    br      x0

2:  ldr     x0, =__stack_top
    mov     sp, x0

    ldr     x0, =__bss_start
    ldr     x1, =__bss_end
3:  cmp     x0, x1
    b.ge    4f
    str     xzr, [x0], #8
    b       3b

4:  bl      kmain
    b       0b

.section .data
.balign 4096
.global boot_l1
boot_l1:
    .quad   PTE_DEVICE
    .quad   PTE_NORMAL
    .fill   510, 8, 0
"#
);
