// CPU0 Stage-2 tests. The verifier defines SV_MMU, SV_ACTION, SV_ADDRESS, SV_NONCE.
.text
    b failure
.org 64
entry:
    mov x9, #0xaa080000
    mrs x10, CurrentEL
    str x10, [x9]
    mov x10, #SV_NONCE
    str x10, [x9, #88]
    adr x10, vectors
    msr vbar_el1, x10
.if SV_MMU
    // Stage-1 deliberately maps VMM RAM: Stage-2 must still prevent access.
    mov x9, #0xaa090000
    mov x10, #0x705
    movk x10, #0x60, lsl #48 // PXN/UXN device mapping for physical GIC/MMIO.
    str x10, [x9]
    mov x10, #0x40000000
    orr x10, x10, #0x700
    orr x10, x10, #1
    orr x10, x10, #4
    movk x10, #0x60, lsl #48
    str x10, [x9, #8]
    mov x10, #0x80000000
    orr x10, x10, #0x700
    orr x10, x10, #1
    str x10, [x9, #16]
    mov x10, #0xc0000000
    orr x10, x10, #0x700
    orr x10, x10, #1
    str x10, [x9, #24]
    mov x10, #0xff
    msr mair_el1, x10
    msr ttbr0_el1, x9
    ldr x10, =0x10080351c
    msr tcr_el1, x10
    dsb sy
    isb
    tlbi vmalle1
    dsb sy
    isb
    mrs x10, sctlr_el1
    orr x10, x10, #1
    orr x10, x10, #4
    orr x10, x10, #0x1000
    msr sctlr_el1, x10
    isb
.endif
.if SV_ACTION == 0 // Read/write permitted RAM on either side of the resident hole.
    ldr x10, =SV_ADDRESS
    ldr x11, =0x1122334455667788
    str x11, [x10]
    ldr x11, [x10]
    mov x9, #0xaa080000
    str x11, [x9, #16]
.elseif SV_ACTION == 1 // Forbidden read.
    ldr x10, =SV_ADDRESS
    ldr x11, [x10]
    b failure
.elseif SV_ACTION == 2 // Forbidden write.
    ldr x10, =SV_ADDRESS
    ldr x11, =0x1122334455667788
    str x11, [x10]
    b failure
.elseif SV_ACTION == 3 // Forbidden instruction fetch.
    ldr x10, =SV_ADDRESS
    br x10
    b failure
.elseif SV_ACTION == 4 // Physical CNTP interrupt goes straight to the EL1 GIC/vector.
    mov x9, #0x08000000
    mov w10, #1
    str w10, [x9]
    mov w10, #0x40000000
    str w10, [x9, #0x100]
    mov x9, #0x08010000
    mov w10, #0xff
    str w10, [x9, #4]
    mov w10, #1
    str w10, [x9]
    mrs x10, cntfrq_el0
    mov x11, #100
    udiv x10, x10, x11
    msr cntp_tval_el0, x10
    mov x10, #1
    msr cntp_ctl_el0, x10
    dsb sy
    isb
    msr daifclr, #2
wait_irq:
    wfi
    mov x9, #0xaa080000
    ldr x10, [x9, #16]
    cbz x10, wait_irq
    cmp x10, #4
    b.ne failure
.elseif SV_ACTION == 5 // Unimplemented suspend cannot bypass the protected EL2 entry.
    .irp reg,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30
    mov x\reg, #(0x200 + \reg)
    .endr
    mov x0, #0xc4000000
    add x0, x0, #1
    smc #0
    cmn x0, #1
    b.ne failure
    .irp reg,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30
    cmp x\reg, #(0x200 + \reg)
    b.ne failure
    .endr
.elseif SV_ACTION == 6 // Read the memory policy from the DTB supplied in x0.
    ldr w9, [x0]
    mov w10, #0x0dd0
    movk w10, #0xedfe, lsl #16
    cmp w9, w10
    b.ne failure
    add x1, x0, x1 // memory/reg offset supplied by the verifier.
    add x2, x0, x2 // resident-region offset supplied by the verifier.
    mov x9, #0xaa080000
    .irp offset,0,8,16,24
    ldr x10, [x1, #\offset]
    rev x10, x10
    str x10, [x9, #(16 + \offset)]
    .endr
    ldp x10, x11, [x2]
    rev x10, x10
    rev x11, x11
    stp x10, x11, [x9, #48]
.elseif SV_ACTION == 7 // Native SMCCC service in the verifier's minimal EL3 firmware.
    .irp reg,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30
    mov x\reg, #(0x200 + \reg)
    .endr
    mov x0, #0x84000000
    smc #0
    cmp x9, #0x209
    b.ne failure
    mov x9, #0xaa080000
    stp x0, x1, [x9, #16]
    stp x2, x3, [x9, #32]
    // x9 was used to store results; the remaining non-result registers survive.
    .irp reg,4,5,6,7,8,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30
    cmp x\reg, #(0x200 + \reg)
    b.ne failure
    .endr
.elseif SV_ACTION == 8 // Memory discovery and an immutable virtual GSC5 descriptor.
    mov x9, #0xaa080000
    ldr x10, =0x70019000
    ldr w11, [x10, #0x50]
    str x11, [x9, #16]
    ldr w11, [x10, #0xd4c]
    str x11, [x9, #24]
    ldr w11, [x10, #0xd50]
    str x11, [x9, #32]
    ldr w11, [x10, #0xd54]
    str x11, [x9, #40]
    str wzr, [x10, #0xd4c]
    str wzr, [x10, #0xd50]
    str wzr, [x10, #0xd54]
    ldr w11, [x10, #0xd4c]
    mov x12, #0xfec00000
    cmp x11, x12
    b.ne failure
    ldr w11, [x10, #0xd54]
    cmp w11, #128
    b.ne failure
    // An unrelated firmware carveout retains its real physical value.
    ldr w11, [x10, #0xc5c]
    mov w12, #0xffd00000
    cmp w11, w12
    b.ne failure
.elseif SV_ACTION == 9 // Pass through ordinary MC access, signed loads, acquire/release and XZR.
    mov x9, #0xaa080000
    ldr x10, =0x70019100
    ldr w11, =0xfeedface
    str w11, [x10]
    ldrsw x12, [x10]
    str x12, [x9, #16]
    ldr w12, [x10]
    str x12, [x9, #24]
    ldar w12, [x10]
    str x12, [x9, #32]
    add x13, x10, #8
    stlr w11, [x13]
    str wzr, [x10, #4]
    ldr w12, [x10, #4]
    str x12, [x9, #40]
    ldr wzr, [x10]
    ldr x13, =0x70019100
    cmp x10, x13
    b.ne failure
.elseif SV_ACTION == 10 // VA != IPA: HPFAR, rather than FAR's upper bits, selects the MC page.
    ldr x10, =0xaa092000
    ldr x11, =0xaa093003
    str x11, [x10, #2048]
    ldr x10, =0xaa093000
    ldr x11, =0x60000070019707
    str x11, [x10, #128]
    mov x10, #0xaa090000
    str xzr, [x10, #8]
    dsb ish
    tlbi vmalle1
    dsb ish
    ldr x11, =0xaa092003
    str x11, [x10, #8]
    dsb ish
    isb
    ldr x10, =0x60010000
    ldr w11, [x10, #0xd4c]
    mov x9, #0xaa080000
    str x11, [x9, #16]
.elseif SV_ACTION == 11 // Unsupported 64-bit MMIO access must fault without a physical access.
    ldr x10, =SV_ADDRESS
    ldr x11, [x10]
    b failure
.elseif SV_ACTION == 12 || SV_ACTION == 13 // Byte-spaced NS16550A, including a Stage-1 alias.
.if SV_ACTION == 13
    ldr x10, =0xaa092000
    ldr x11, =0xaa093003
    str x11, [x10, #2048]
    ldr x10, =0xaa093000
    ldr x11, =0x600000700ff707
    str x11, [x10, #128]
    mov x10, #0xaa090000
    str xzr, [x10, #8]
    dsb ish
    tlbi vmalle1
    dsb ish
    ldr x11, =0xaa092003
    str x11, [x10, #8]
    dsb ish
    isb
    ldr x10, =0x60010000
.else
    ldr x10, =0x700ff000
.endif
    mov x9, #0xaa080000
    // Scarlet's initialization writes DLAB/divisors before enabling 8N1/FIFO.
    mov w11, #0x80
    strb w11, [x10, #3]
    mov w11, #1
    strb w11, [x10]
    strb wzr, [x10, #1]
    ldrb w12, [x10]
    str x12, [x9, #16]
    mov w11, #3
    strb w11, [x10, #3]
    mov w11, #7
    strb w11, [x10, #2]
    strb w11, [x10, #1] // Interrupts remain disabled.
    ldrb w12, [x10, #5]
    str x12, [x9, #24]
    ldrb w12, [x10, #2]
    str x12, [x9, #32]
    ldrb w12, [x10, #1]
    str x12, [x9, #40]
    mov w11, #0xa5
    strb w11, [x10, #7]
    ldrsb w12, [x10, #7]
    str x12, [x9, #48]
    ldrsb x12, [x10, #7]
    str x12, [x9, #56]
    add x13, x10, #7
    ldarb w12, [x13]
    str x12, [x9, #64]
    ldrb wzr, [x13]
    mov w11, #'A'
    strb w11, [x10]
    stlrb wzr, [x13]
    ldrb w12, [x13]
    str x12, [x9, #72]
    ldrb w12, [x10] // No RX loopback or host input.
    str x12, [x9, #80]
.elseif SV_ACTION == 14 // USB is absent to EL1; shared resources retain EL2-owned bits.
    .irp address,0x70090000,0x7009f000,0x700d0000,0x700d9180,0x7d000000,0x7d004000,0x7d008000,0x7d1ffff8
    ldr x10, =\address
    ldr x11, =0x1122334455667788
    str x11, [x10]
    ldr x12, [x10]
    cbnz x12, failure
    strb wzr, [x10]
    ldrb w12, [x10]
    cbnz w12, failure
    strh wzr, [x10]
    ldrh w12, [x10]
    cbnz w12, failure
    str wzr, [x10]
    ldr w12, [x10]
    cbnz w12, failure
    .endr
    mov x9, #0xaa080000
    ldr x10, =0x60006000
    mov w11, #-1
    str w11, [x10, #0x10]
    ldr w12, [x10, #0x10]
    str x12, [x9, #16]
    str wzr, [x10, #0x610] // XUSB clock source is read-only to the guest.
    ldr w12, [x10, #0x610]
    str x12, [x9, #24]
    str w11, [x10, #0x330] // SET/CLR preserve write-one semantics.
    ldr w12, [x10, #0x330]
    str x12, [x9, #32]
    ldr x10, =0x7000e400
    str wzr, [x10, #0xf0]
    ldr w12, [x10, #0xf0]
    str x12, [x9, #40]
    mov w11, #0x114 // Guest cannot toggle XUSBA power.
    str w11, [x10, #0x30]
    ldr w12, [x10, #0x30]
    str x12, [x9, #48]
    mov w11, #0x107 // Other power domains remain accessible.
    str w11, [x10, #0x30]
    ldr w12, [x10, #0x30]
    str x12, [x9, #56]
    ldr x10, =0x7001928c
    str wzr, [x10]
    ldr w12, [x10]
    str x12, [x9, #64]
.elseif SV_ACTION == 15 // Unsupported UART access width must not touch the physical page.
    ldr x10, =SV_ADDRESS
    ldr w11, [x10]
    b failure
.endif
    mov x9, #0xaa080000
    mov x10, #1
    str x10, [x9, #8]
    mov x0, #0
    hvc #0x5356
failure:
    mov x0, #0xbad
    hvc #0x5356
    b failure
irq:
    mov x9, #0xaa080000
    mrs x10, CurrentEL
    str x10, [x9, #16]
    mov x9, #0x08010000
    ldr w11, [x9, #12]
    mov x9, #0xaa080000
    str x11, [x9, #24]
    mov x9, #0x08010000
    str w11, [x9, #16]
    mov x10, #2
    msr cntp_ctl_el0, x10
    dsb sy
    eret
.ltorg
.org 0x800
vectors:
    .rept 5
    b failure
    .balign 128
    .endr
    b irq
    .balign 128
    .rept 10
    b failure
    .balign 128
    .endr
