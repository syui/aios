/* qemu virt: RAM は 0x4000_0000 から。先頭は DTB 用にあけておく */
ENTRY(_start)

SECTIONS
{
    . = 0x40080000;

    .text : {
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

    /DISCARD/ : { *(.eh_frame*) }
}
