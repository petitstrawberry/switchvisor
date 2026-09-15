// Four EL1 CPUs: independent MC traps, virtualized timers/SGIs, virtual off/on,
// and invalid CPU_ON requests. SV_FAULT selects one AP for a VMM fault.
.text
.org 0x40
    mrs x9, CurrentEL
    cmp x9, #4
    b.ne failure
    ldr x19, =0xaa080000
    mov x9, #4
    str x9, [x19]
    mov x9, #SV_NONCE
    str x9, [x19, #88]
.if SV_MMU
    ldr x9, =0xaa0b0000
    ldr x10, =0x0060000000000705
    str x10, [x9]
    ldr x10, =0x0060000040000705
    str x10, [x9, #8]
    ldr x10, =0x80000701
    str x10, [x9, #16]
    ldr x10, =0xc0000701
    str x10, [x9, #24]
    dsb sy
    bl enable_s1
.endif
    .irp fid,0x84000003,0xc4000003,0x84000002,0xc4000004
    ldr x0, =0x8400000a
    ldr x1, =\fid
    smc #0
    cbnz x0, failure
    .endr
    // Invalid targets/entries must not reach real CPU_ON.
    ldr x0, =0xc4000003
    mov x1, #4
    adr x2, secondary
    smc #0
    cmn x0, #2
    b.ne failure
    .irp entry,0xfec00000,0xffbffffc,0x70019000,0xaa000041,0x1000000000
    ldr x0, =0xc4000003
    mov x1, #1
    ldr x2, =\entry
    smc #0
    cmn x0, #2
    b.ne failure
    .endr
    ldr x0, =0xc4000004
    mov x1, #0
    mov x2, #0
    smc #0
    cbnz x0, failure
    // CPU0 owns distributor setup; each AP enables its banked CPU interface.
    ldr x9, =0x08000000
    mov w10, #1
    str w10, [x9]
    .irp cpu,1,2,3
.if SV_FAULT == 0 || SV_FAULT == \cpu
    ldr x0, =0xc4000003
    mov x1, #\cpu
    adr x2, secondary
    ldr x3, =(0xcafe0000 + \cpu)
.if \cpu != 2
    movk x3, #0xba98, lsl #32
    movk x3, #0xfedc, lsl #48
.endif
.if \cpu == 2
    // SMC32 arguments are truncated even when their upper halves are dirty.
    ldr x0, =0x84000003
    movk x1, #0xffff, lsl #32
    movk x2, #0xffff, lsl #32
    movk x3, #0xffff, lsl #32
.endif
    smc #0
    cbnz x0, failure
.endif
    .endr
.if SV_FAULT != 0
    b wait_forever // the host inspects the faulting AP instead of CPU0.
.else
    bl uart_output
    // Send a physical SGI to all three APs; EL2 preserves its source in a vLR.
    ldr x9, =0x08000f00
    ldr w10, =0x000e0001
    str w10, [x9]
    dsb sy
    .irp cpu,1,2,3
    ldr x9, =(0xaa081000 + \cpu * 64 + 40)
1:  ldr x10, [x9]
    cbz x10, 1b
    .endr
    // A second SGI verifies that maintenance handling deactivated the first
    // physical SGI after the guest acknowledged its software vLR.
    ldr x9, =0x08000f00
    ldr w10, =0x00060001
    str w10, [x9]
    dsb sy
    .irp cpu,1,2
    ldr x9, =(0xaa081000 + \cpu * 64 + 32)
1:  ldr x10, [x9]
    cmp x10, #2
    b.lo 1b
    .endr
    // AP3 powers off virtually, then re-enters with a new context.
affinity_off:
    ldr x0, =0xc4000004
    mov x1, #3
    mov x2, #0
    smc #0
    cmp x0, #1
    b.ne affinity_off
    ldr x0, =0xc4000003
    mov x1, #3
    adr x2, secondary
    mov x3, #0xbeef
    smc #0
    cbnz x0, failure
    ldr x9, =(0xaa081000 + 3 * 64 + 48)
1:  ldr x10, [x9]
    cbz x10, 1b
    // Native CPU_ON was issued exactly once per AP, always to our EL2 entry.
    .irp cpu,1,2,3
    ldr x9, =(0x80030000 + \cpu * 64)
    ldr x10, [x9]
    ldr x11, =0xfec00000
    cmp x10, x11
    b.lo failure
    ldr x11, =0xffc00000
    cmp x10, x11
    b.hs failure
    ldr x10, [x9, #8]
    cmp x10, #\cpu
    b.ne failure
    ldr x10, [x9, #16]
    cmp x10, #1
    b.ne failure
    .endr
    mov x9, #15
    str x9, [x19, #8]
    mov x9, #1
    str x9, [x19, #16]
    mov x0, #0
    hvc #0x5356
.endif
failure:
    mov x0, #0xbad
    hvc #0x5356
wait_forever:
    wfe
    b wait_forever
secondary:
    mrs x9, CurrentEL
    cmp x9, #4
    b.ne failure
    mrs x20, mpidr_el1
    and x20, x20, #0xff
    ldr x21, =0xaa081000
    add x21, x21, x20, lsl #6
    ldr x10, =0xaa0a0000
    add x10, x10, x20, lsl #12
    mov sp, x10
.if SV_MMU
    bl enable_s1
.endif
    bl uart_output
    mov x11, #0xbeef
    cmp x0, x11
    b.eq restarted
    ldr x10, =0xcafe0000
    add x10, x10, x20
    cmp x20, #2
    b.eq 1f
    movk x10, #0xba98, lsl #32
    movk x10, #0xfedc, lsl #48
1:
    cmp x0, x10
    b.ne failure
    mrs x9, CurrentEL
    stp x9, x0, [x21]
    ldr x9, =0x70019000
    ldr w10, [x9, #0xd4c]
    str x10, [x21, #16]
    ldr w11, =0xfec00000
    cmp w10, w11
    b.ne failure
.if SV_FAULT != 0
    ldr x9, =0xfec00000
    ldr x10, [x9]
    b failure
.else
    ldr x0, =0xc4000003
    mov x1, x20
    adr x2, secondary
    mov x3, #0
    smc #0
    cmn x0, #4
    b.ne failure
    adr x9, guest_vectors
    msr vbar_el1, x9
    ldr x9, =0x08010000
    mov w10, #1
    str w10, [x9]
    mov w10, #255
    str w10, [x9, #4]
    ldr x9, =0x08000100
    mov w10, #(1 << 27)
    str w10, [x9]
    mov x10, #5000
    msr cntv_tval_el0, x10
    mov x10, #1
    msr cntv_ctl_el0, x10
    dsb sy
    isb
    msr daifclr, #2
1:  ldr x9, [x21, #24]
    cbz x9, 1b
    ldr x9, [x21, #32]
    cbz x9, 1b
    mov x9, #1
    str x9, [x21, #40]
    dsb sy
    cmp x20, #3
    b.ne wait_forever
    ldr x0, =0x84000002
    smc #0
    b failure
restarted:
    mov x9, #1
    str x9, [x21, #48]
    dsb sy
    ldr x0, =0x84000002
    smc #0
    b failure
.endif
uart_output:
    // CPU0 and APs contend on the real load/store-only virtual-device mutex.
    ldr x9, =0x700ff000
    mov x10, #128
    mrs x11, mpidr_el1
    and w11, w11, #3
    add w11, w11, #'0'
1:  ldrb w12, [x9, #5]
    cmp w12, #0x60
    b.ne failure
    strb w11, [x9]
    subs x10, x10, #1
    b.ne 1b
    ret
enable_s1:
    ldr x9, =0xaa0b0000
    msr ttbr0_el1, x9
    mov x10, #0x44
    msr mair_el1, x10
    ldr x10, =0x10080351c
    msr tcr_el1, x10
    dsb sy
    isb
    tlbi vmalle1
    dsb sy
    isb
    mrs x10, sctlr_el1
    orr x10, x10, #1
    msr sctlr_el1, x10
    isb
    ret
irq:
    stp x9, x10, [sp, #-32]!
    stp x11, x12, [sp, #16]
    ldr x9, =0x0801000c
    ldr w10, [x9]
    and x11, x10, #0x3ff
    cmp x11, #27
    b.ne sgi
    mov x12, #2
    msr cntv_ctl_el0, x12
    ldr x11, [x21, #24]
    add x11, x11, #1
    str x11, [x21, #24]
    b eoi
sgi:
    cmp x11, #1
    b.ne failure
    ldr x11, [x21, #32]
    add x11, x11, #1
    str x11, [x21, #32]
eoi:
    str w10, [x9, #4]
    dsb sy
    ldp x11, x12, [sp, #16]
    ldp x9, x10, [sp], #32
    eret
.balign 2048
guest_vectors:
    .rept 16
    b irq
    .balign 128
    .endr
