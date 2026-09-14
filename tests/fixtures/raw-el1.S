// A headerless raw BL33 used only by the QEMU handoff verification.
// The verifier supplies SV_NONCE and enters at offset 64, rather than offset 0.
.text
.global _start
_start:
    b failure
.org 64
entry:
    mov x9, #0xaa080000
    mov x10, #12
    mov x12, x9
check_zero_tail:
    ldr x11, [x12], #8
    cbnz x11, failure
    subs x10, x10, #1
    b.ne check_zero_tail
    mrs x10, CurrentEL
    str x10, [x9]
    cmp x10, #4
    b.ne failure
    mov x10, sp
    str x10, [x9, #8]
    mov x11, #0x8a800000
    cmp x10, x11
    b.ne failure
    stp x0, x1, [x9, #16]
    stp x2, x3, [x9, #32]
    stp x4, x5, [x9, #48]
    stp x6, x7, [x9, #64]
    mrs x10, sctlr_el1
    str x10, [x9, #80]
    tbnz x10, #0, failure
    tbnz x10, #2, failure
    tbnz x10, #12, failure
    mov x10, #SV_NONCE
    str x10, [x9, #88]
    mov x0, #0
    hvc #0x5356
failure:
    mov x0, #0xbad
    hvc #0x5356
    b failure
