// Virtio-net MMIO, guest RAM copying and virtual SPI test. No physical USB.
// The verifier defines SV_MMU, SV_PRIORITY and SV_NONCE.
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
    ldr x20, =0x700fe000
    ldr w10, [x20]
    ldr w11, =0x74726976
    cmp w10, w11
    b.ne failure
    mov x9, #0xaa080000
    str x10, [x9, #64]
    // Queue storage and buffers are in the payload's cleared runtime tail.
    mov x21, #0xaa040000
    ldr x22, =0xaa044000
    ldr x23, =0xaa047000
    ldr x24, =0xaa048000
    adr x10, packet
    mov x11, #72
copy_packet:
    ldrb w12, [x10], #1
    strb w12, [x23], #1
    subs x11, x11, #1
    b.ne copy_packet
    ldr x23, =0xaa047000
    str x24, [x21]
    mov w10, #1526
    str w10, [x21, #8]
    mov w10, #2
    strh w10, [x21, #12]
    add x10, x21, #0x1000
    mov w11, #1
    strh w11, [x10, #2]
    str x23, [x22]
    mov w10, #72
    str w10, [x22, #8]
    add x10, x22, #0x1000
    strh w11, [x10, #2]
    dsb sy
    // Feature negotiation, v2 queue addresses and DRIVER_OK.
    mov w10, #1
    str w10, [x20, #0x70]
    mov w10, #3
    str w10, [x20, #0x70]
    mov w10, #1
    str w10, [x20, #0x24]
    str w10, [x20, #0x20]
    str wzr, [x20, #0x24]
    mov w10, #0x20
    str w10, [x20, #0x20]
    mov w10, #11
    str w10, [x20, #0x70]
    str wzr, [x20, #0x30]
    mov w10, #8
    str w10, [x20, #0x38]
    str w21, [x20, #0x80]
    add x10, x21, #0x1000
    str w10, [x20, #0x90]
    add x10, x21, #0x2000
    str w10, [x20, #0xa0]
    mov w10, #1
    str w10, [x20, #0x44]
    str w10, [x20, #0x30]
    mov w10, #8
    str w10, [x20, #0x38]
    str w22, [x20, #0x80]
    add x10, x22, #0x1000
    str w10, [x20, #0x90]
    add x10, x22, #0x2000
    str w10, [x20, #0xa0]
    mov w10, #1
    str w10, [x20, #0x44]
    mov w10, #15
    str w10, [x20, #0x70]
    str w10, [x20, #0x50]
    mov x9, #0xaa080000
    ldr w10, [x20, #0x70]
    str x10, [x9, #72]
    ldr w10, [x20, #0x60]
    cmp w10, #1
    b.ne failure
    str x10, [x9, #80]
    // Enable the separate network SPI in the virtual distributor.
    mov x9, #0x08000000
    mov w10, #SV_PRIORITY
    strb w10, [x9, #0x447]
    ldrb w11, [x9, #0x447]
    cmp w10, w11
    b.ne failure
    mov w10, #1
    str w10, [x9]
    mov w10, #0x80
    str w10, [x9, #0x108]
    mov x9, #0x08010000
    // The virtual SPI must obey guest PMR, not EL2's physical priority zero.
    mov w10, #0x40
    str w10, [x9, #4]
    mov w10, #1
    str w10, [x9]
    msr daifclr, #2
    mov x11, #4096
masked_wait:
    subs x11, x11, #1
    b.ne masked_wait
    mov x10, #0xaa080000
    ldr x12, [x10, #16]
    cbnz x12, failure
    mov w10, #255
    str w10, [x9, #4]
wait_network:
    mov x9, #0xaa080000
    ldr x10, [x9, #16]
    cbnz x10, received
    wfi
    b wait_network
received:
    msr daifset, #2
    add x10, x22, #0x2000
    ldrh w11, [x10, #2]
    str x11, [x9, #32]
    add x10, x21, #0x2000
    ldrh w11, [x10, #2]
    str x11, [x9, #40]
    ldrh w11, [x24, #10]
    str x11, [x9, #48]
    ldrb w11, [x24, #33]
    str x11, [x9, #56]
    mov x10, #1
    str x10, [x9, #8]
    mov x0, #0
    hvc #0x5356
failure:
    mov x0, #0xbad
    hvc #0x5356
    b failure
irq:
    stp x9, x10, [sp, #-32]!
    stp x11, x12, [sp, #16]
    mov x9, #0x08010000
    ldr w11, [x9, #12]
    mov x10, #0xaa080000
    str x11, [x10, #24]
    ldr w12, [x9, #20] // GICC_RPR exposes the actual injected guest priority.
    str x12, [x10, #80]
    mov x12, #1
    str x12, [x10, #16]
    ldr x10, =0x700fe000
    mov w12, #3
    str w12, [x10, #0x64]
    str w11, [x9, #16]
    dsb sy
    ldp x11, x12, [sp, #16]
    ldp x9, x10, [sp], #32
    // Leave the entry stack sample clean for the common handoff verifier.
    stp xzr, xzr, [sp, #-32]
    stp xzr, xzr, [sp, #-16]
    eret
.ltorg
.balign 2048
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
packet:
    .zero 12
    .byte 255,255,255,255,255,255, 2,0x53,0x56,0,0,2
    .byte 8,6, 0,1,8,0,6,4,0,1, 2,0x53,0x56,0,0,2
    .byte 192,168,77,3, 0,0,0,0,0,0, 192,168,77,1
    .zero 18
