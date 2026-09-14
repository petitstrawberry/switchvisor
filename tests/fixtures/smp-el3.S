// QEMU-only EL3 fixture. All physical CPUs boot this flash image. CPU_ON
// dispatches the recorded EL2 entry through a mailbox; this is not Switch BL31.
.text
    msr daifset, #15
    msr spsel, #1
    mrs x19, mpidr_el1
    and x19, x19, #0xff
    ldr x9, =0x80020000
    add x9, x9, x19, lsl #12
    mov sp, x9
    adr x9, vectors
    msr vbar_el3, x9
    msr cptr_el3, xzr
    // Banked SGI/PPI must belong to the non-secure guest.
    ldr x9, =0x08000080
    mov w10, #-1
    str w10, [x9]
    // A secure PMR below 0x80 prevents non-secure software from changing it.
    ldr x9, =0x08010004
    mov w10, #255
    str w10, [x9]
    ldr x9, =0x08010000
    mov w10, #3
    str w10, [x9]
    dsb sy
    mov x9, #0x501
    msr scr_el3, x9
    cbnz x19, physical_wait
    ldr x9, =0x08000000
    mov w10, #3
    str w10, [x9]
    ldr x9, =0xaa000000
    mov x0, #0x100
    b enter_el2
physical_wait:
    ldr x20, =0x80030000
    add x20, x20, x19, lsl #6
1:  ldr x9, [x20, #24]
    cbnz x9, physical_start
    wfe
    b 1b
physical_start:
    dsb sy
    ldr x9, [x20]
    ldr x0, [x20, #8]
enter_el2:
    mov x10, #0x80000000
    msr hcr_el2, x10
    mrs x10, sctlr_el2
    bic x10, x10, #1
    bic x10, x10, #4
    msr sctlr_el2, x10
    mov x10, #0x3c9
    msr spsr_el3, x10
    msr elr_el3, x9
    isb
    eret
smc:
    stp x9, x10, [sp, #-16]!
    ldr x9, =0xc4000003
    cmp x0, x9
    b.eq cpu_on
    ldr x9, =0x84000000
    cmp x0, x9
    b.ne failure
    mov x0, #1
    movk x0, #1, lsl #16
    b returned
cpu_on:
    cbz x1, failure
    cmp x1, #4
    b.hs failure
    cmp x3, x1
    b.ne failure
    ldr x9, =0xfec00000
    cmp x2, x9
    b.lo failure
    ldr x9, =0xffc00000
    cmp x2, x9
    b.hs failure
    ldr x9, =0x80030000
    add x9, x9, x1, lsl #6
    ldr x10, [x9, #16]
    cbnz x10, failure // virtual off/on must not boot this hardware twice.
    stp x2, x3, [x9]
    mov x10, #1
    str x10, [x9, #16]
    dsb sy
    str x10, [x9, #24]
    dsb sy
    sev
    mov x0, #0
returned:
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
