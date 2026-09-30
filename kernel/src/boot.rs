// cpu0 だけが起き、スタックを用意して bss を 0 にし、kmain へ。
// ほかの core は wfe で眠る。
core::arch::global_asm!(
    r#"
.section .text.boot
.global _start
_start:
    mrs     x0, mpidr_el1
    and     x0, x0, #0xff
    cbz     x0, 1f
0:  wfe
    b       0b

1:  ldr     x0, =__stack_top
    mov     sp, x0

    ldr     x0, =__bss_start
    ldr     x1, =__bss_end
2:  cmp     x0, x1
    b.ge    3f
    str     xzr, [x0], #8
    b       2b

3:  bl      kmain
    b       0b
"#
);
