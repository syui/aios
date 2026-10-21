// カーネルの memcpy / memmove / memset (mem.rs が global_asm で入れる。test/mem.c が外の qemu-aarch64 でためす)。
// Linux の arch/arm64/lib (Arm の optimized-routines) と同じやりかた: ldp / stp で 16 バイトずつ、長いものは 64 バイトずつ
// まわし、はしは前と後ろから重ねて写す (場所のずれた (8 でそろっていない) ものもそのまま: SCTLR_EL1.A は 0)。0 で埋める
// 長いものは DC ZVA (64 バイトずつ。Linux の memset と clear_page)。ふつうのメモリ (Normal) だけに使うこと (デバイスの
// メモリにはずれたアクセスも DC ZVA もできない。MMIO は read_volatile / write_volatile で)。x18 は使わない

    .text

// memcpy(x0 dst, x1 src, x2 n) -> x0。重なっていないこと
    .global memcpy
    .type   memcpy, %function
    .p2align 6
memcpy:
    add     x4, x1, x2              // src の終わり
    add     x5, x0, x2              // dst の終わり
    cmp     x2, #128
    b.hi    .Lcpy_long
    cmp     x2, #32
    b.hi    .Lcpy_33_128
    cmp     x2, #16
    b.lo    .Lcpy_0_15
    // 16..32: 前の 16 と後ろの 16
    ldp     x6, x7, [x1]
    ldp     x8, x9, [x4, #-16]
    stp     x6, x7, [x0]
    stp     x8, x9, [x5, #-16]
    ret
.Lcpy_0_15:
    tbz     x2, #3, .Lcpy_0_7
    ldr     x6, [x1]
    ldr     x7, [x4, #-8]
    str     x6, [x0]
    str     x7, [x5, #-8]
    ret
.Lcpy_0_7:
    tbz     x2, #2, .Lcpy_0_3
    ldr     w6, [x1]
    ldr     w7, [x4, #-4]
    str     w6, [x0]
    str     w7, [x5, #-4]
    ret
.Lcpy_0_3:
    // 1..3: はじめ、まん中 (n / 2)、終わりの 1 バイトずつ
    cbz     x2, .Lcpy_ret
    lsr     x3, x2, #1
    ldrb    w6, [x1]
    ldrb    w7, [x1, x3]
    ldrb    w8, [x4, #-1]
    strb    w6, [x0]
    strb    w7, [x0, x3]
    strb    w8, [x5, #-1]
.Lcpy_ret:
    ret
.Lcpy_33_128:
    // 33..64: 前の 32 と後ろの 32。65..128: 前の 64 と後ろの 64
    ldp     x6, x7, [x1]
    ldp     x8, x9, [x1, #16]
    ldp     x10, x11, [x4, #-32]
    ldp     x12, x13, [x4, #-16]
    cmp     x2, #64
    b.hi    .Lcpy_65_128
    stp     x6, x7, [x0]
    stp     x8, x9, [x0, #16]
    stp     x10, x11, [x5, #-32]
    stp     x12, x13, [x5, #-16]
    ret
.Lcpy_65_128:
    ldp     x14, x15, [x1, #32]
    ldp     x16, x17, [x1, #48]
    stp     x6, x7, [x0]
    stp     x8, x9, [x0, #16]
    ldp     x6, x7, [x4, #-64]
    ldp     x8, x9, [x4, #-48]
    stp     x14, x15, [x0, #32]
    stp     x16, x17, [x0, #48]
    stp     x6, x7, [x5, #-64]
    stp     x8, x9, [x5, #-48]
    stp     x10, x11, [x5, #-32]
    stp     x12, x13, [x5, #-16]
    ret
.Lcpy_long:
    // 129 から: はじめの 16 を写してから、dst を 16 にそろえて 64 バイトずつ。終わりの 64 は後ろから
    ldp     x6, x7, [x1]
    stp     x6, x7, [x0]
    and     x3, x0, #15
    sub     x3, x3, #16             // -(16 - ずれ)
    sub     x1, x1, x3
    sub     x14, x0, x3             // 16 にそろった dst (はじめの 16 の中)
    add     x2, x2, x3              // x14 から終わりまで
    subs    x2, x2, #64
    b.ls    .Lcpy_last64
.Lcpy_loop64:
    ldp     x6, x7, [x1]
    ldp     x8, x9, [x1, #16]
    ldp     x10, x11, [x1, #32]
    ldp     x12, x13, [x1, #48]
    add     x1, x1, #64
    stp     x6, x7, [x14]
    stp     x8, x9, [x14, #16]
    stp     x10, x11, [x14, #32]
    stp     x12, x13, [x14, #48]
    add     x14, x14, #64
    subs    x2, x2, #64
    b.hi    .Lcpy_loop64
.Lcpy_last64:
    ldp     x6, x7, [x4, #-64]
    ldp     x8, x9, [x4, #-48]
    ldp     x10, x11, [x4, #-32]
    ldp     x12, x13, [x4, #-16]
    stp     x6, x7, [x5, #-64]
    stp     x8, x9, [x5, #-48]
    stp     x10, x11, [x5, #-32]
    stp     x12, x13, [x5, #-16]
    ret
    .size   memcpy, . - memcpy

// memmove(x0 dst, x1 src, x2 n) -> x0。重なっていなければ memcpy、重なっていれば、まだ読んでいないところを
// 書かない向きに 16 バイトずつ (重なるのはまれ: Vec の insert や copy_within)
    .global memmove
    .type   memmove, %function
    .p2align 4
memmove:
    sub     x3, x0, x1
    cmp     x3, x2
    b.lo    .Lmove_back             // src <= dst < src + n: 後ろから
    sub     x3, x1, x0
    cmp     x3, x2
    b.hs    memcpy                  // 重なっていない
    // dst < src < dst + n: 前から
    mov     x4, x0
    subs    x2, x2, #16
    b.lo    2f
1:  ldp     x6, x7, [x1], #16
    subs    x2, x2, #16
    stp     x6, x7, [x4], #16
    b.hs    1b
2:  adds    x2, x2, #16
    b.eq    4f
3:  ldrb    w6, [x1], #1
    subs    x2, x2, #1
    strb    w6, [x4], #1
    b.ne    3b
4:  ret
.Lmove_back:
    add     x1, x1, x2
    add     x4, x0, x2
    subs    x2, x2, #16
    b.lo    2f
1:  ldp     x6, x7, [x1, #-16]!
    subs    x2, x2, #16
    stp     x6, x7, [x4, #-16]!
    b.hs    1b
2:  adds    x2, x2, #16
    b.eq    4f
3:  ldrb    w6, [x1, #-1]!
    subs    x2, x2, #1
    strb    w6, [x4, #-1]!
    b.ne    3b
4:  ret
    .size   memmove, . - memmove

// memset(x0 dst, w1 c, x2 n) -> x0
    .global memset
    .type   memset, %function
    .p2align 6
memset:
    and     x1, x1, #0xff
    mov     x3, #0x0101010101010101
    mul     x1, x1, x3              // c を 8 バイトに
    add     x5, x0, x2
    cmp     x2, #64
    b.hi    .Lset_long
    cmp     x2, #16
    b.lo    .Lset_0_15
    stp     x1, x1, [x0]
    stp     x1, x1, [x5, #-16]
    cmp     x2, #32
    b.ls    .Lset_ret
    // 33..64
    stp     x1, x1, [x0, #16]
    stp     x1, x1, [x5, #-32]
.Lset_ret:
    ret
.Lset_0_15:
    tbz     x2, #3, .Lset_0_7
    str     x1, [x0]
    str     x1, [x5, #-8]
    ret
.Lset_0_7:
    tbz     x2, #2, .Lset_0_3
    str     w1, [x0]
    str     w1, [x5, #-4]
    ret
.Lset_0_3:
    cbz     x2, .Lset_ret
    lsr     x3, x2, #1
    strb    w1, [x0]
    strb    w1, [x0, x3]
    strb    w1, [x5, #-1]
    ret
.Lset_long:
    // 65 から: はじめの 16 を書いて、16 にそろえて続ける。終わりの 64 は後ろから
    stp     x1, x1, [x0]
    bic     x14, x0, #15
    // 0 で 256 バイトから、DC ZVA が使えて 64 バイトずつなら (DCZID_EL0: DZP 0、BS 4)
    cbnz    x1, .Lset_stp
    cmp     x2, #256
    b.lo    .Lset_stp
    mrs     x3, dczid_el0
    and     w3, w3, #31
    cmp     w3, #4
    b.ne    .Lset_stp
    // 64 のさかいめまで stp で、そこから DC ZVA、終わりの 64 は stp で
    stp     x1, x1, [x14, #16]
    stp     x1, x1, [x14, #32]
    stp     x1, x1, [x14, #48]
    add     x3, x14, #64
    bic     x3, x3, #63
    sub     x4, x5, #64
.Lset_zva:
    dc      zva, x3
    add     x3, x3, #64
    cmp     x3, x4
    b.ls    .Lset_zva
    b       .Lset_last64
.Lset_stp:
    add     x14, x14, #16
    sub     x2, x5, x14
    subs    x2, x2, #64
    b.ls    .Lset_last64
.Lset_loop64:
    stp     x1, x1, [x14]
    stp     x1, x1, [x14, #16]
    stp     x1, x1, [x14, #32]
    stp     x1, x1, [x14, #48]
    add     x14, x14, #64
    subs    x2, x2, #64
    b.hi    .Lset_loop64
.Lset_last64:
    stp     x1, x1, [x5, #-64]
    stp     x1, x1, [x5, #-48]
    stp     x1, x1, [x5, #-32]
    stp     x1, x1, [x5, #-16]
    ret
    .size   memset, . - memset
