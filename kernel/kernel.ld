/*
 * カーネルは上位アドレス (KBASE + PA) にリンクし、物理 0x4008_0000 にロードする。
 * qemu virt: RAM は 0x4000_0000 から。先頭は DTB 用にあけておく。
 */
KBASE = 0xffffff8000000000;
_start_pa = _start - KBASE;
_image_size = __kernel_end - _start;
ENTRY(_start_pa)

SECTIONS
{
    . = KBASE + 0x40080000;

    .text : AT(ADDR(.text) - KBASE) {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }
    .rodata : ALIGN(4096) { *(.rodata .rodata.*) }
    .data : ALIGN(4096) { *(.data .data.*) }
    .bss : ALIGN(4096) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    }

    . = ALIGN(16);
    . += 0x10000;
    __stack_top = .;

    . = ALIGN(4096);
    __kernel_end = .;

    /DISCARD/ : { *(.eh_frame*) }
}
