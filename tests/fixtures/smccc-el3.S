// Minimal EL3 firmware for testing native SMC forwarding; not a Switch BL31.
.text
    msr spsel, #1
    mov x9, #0x80020000
    mov sp, x9
    adr x9, vectors
    msr vbar_el3, x9
    mov x9, #0x501 // NS, RW, HCE; physical interrupts remain non-secure.
    msr scr_el3, x9
    msr cptr_el3, xzr
    mov x9, #0x80000000
    msr hcr_el2, x9
    mrs x9, sctlr_el2
    bic x9, x9, #1
    bic x9, x9, #4
    msr sctlr_el2, x9
    mov x9, #0x3c9
    msr spsr_el3, x9
    adr x9, el2
    msr elr_el3, x9
    isb
    eret
el2:
    msr spsel, #0
    .irp reg,0,1,2,3,4,5,6,7
    mov x\reg, #(0x100 + \reg)
    .endr
    mov x16, #0xaa000000
    br x16
smc:
    stp x9, x10, [sp, #-16]!
    mrs x9, esr_el3
    lsr x9, x9, #26
    cmp x9, #0x17
    b.ne failure
    mov x9, #0x84000000
    cmp x0, x9
    b.ne failure
    .irp reg,1,2,3,4,5,6,7
    cmp x\reg, #(0x200 + \reg)
    b.ne failure
    .endr
    mov x0, #1
    movk x0, #1, lsl #16 // PSCI_VERSION 1.1.
    mov x1, #0x1234
    mov x2, #0x5678
    mov x3, #0x9abc
    ldp x9, x10, [sp], #16
    eret
failure:
    wfe
    b failure
.org 0x800
vectors:
    .rept 16
    b smc
    .balign 128
    .endr
