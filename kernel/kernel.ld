/*
 * カーネルは上位アドレス KBASE + 1 GiB + 0x80000 にリンクし、RAM の先頭 + 0x80000 に置かれる
 * (boot.rs)。先頭 4 KiB は Linux arm64 Image と PE/COFF (EFI) のヘッダー。
 */
KBASE = 0xffffff8000000000;
_start_pa = _start - KBASE;
_image_size = __kernel_end - _start;
/* PE/COFF (efi.rs): ヘッダーの 4 KiB の後ろがセクション 1 つ */
_pe_image_size = __kernel_end - _start;
_pe_raw_size = _edata - _start - 0x1000;
_pe_virt_size = _pe_image_size - 0x1000;
_efi_entry_rva = efi_entry - _start;
ENTRY(_start_pa)

SECTIONS
{
    . = KBASE + 0x40080000;

    .text : AT(ADDR(.text) - KBASE) {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }
    .rodata : ALIGN(4096) { *(.rodata .rodata.*) }
    .data : ALIGN(4096) {
        *(.data .data.*)
        /* ファイル (Image) の終わりを 4 KiB にそろえる (PE の SizeOfRawData) */
        . = ALIGN(4096);
        _edata = .;
    }
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
