.section .head, "ax"
.global _start
_start:
    b .Lentry
    nop
    .quad _start
    .quad __image_end - _start
    .quad __bss_start - _start
    .quad __bss_end - _start
    .ascii "SVBOOT01"
.Lentry:
    msr daifset, #15
    mrs x9, CurrentEL
    cmp x9, #8
    b.ne .Lpark
    mrs x10, sctlr_el2
    tbnz x10, #0, .Lpark
    tbnz x10, #2, .Lpark
    adrp x16, _boot_handoff
    add x16, x16, :lo12:_boot_handoff
    stp x0, x1, [x16, #0]
    stp x2, x3, [x16, #16]
    stp x4, x5, [x16, #32]
    stp x6, x7, [x16, #48]
    // The payload build links EL2 at a fixed resident base. Hekate still loads at AA000000.
    adr x20, _start
    ldr x21, [x20, #8]
    cmp x20, x21
    b.eq .Lrelocated
    mov x9, x20
    mov x10, x21
    ldr x11, [x20, #16]
.Lcopy_bootstrap:
    ldr x12, [x9], #8
    str x12, [x10], #8
    subs x11, x11, #8
    b.ne .Lcopy_bootstrap
    dsb sy
    ic iallu
    dsb sy
    isb
    adr x9, .Lrelocated
    sub x9, x9, x20
    add x9, x9, x21
    br x9
.Lrelocated:
    msr spsel, #1
    adrp x9, __boot_stack_top
    add x9, x9, :lo12:__boot_stack_top
    mov sp, x9
    adrp x9, _el2_vectors
    add x9, x9, :lo12:_el2_vectors
    msr vbar_el2, x9
    isb
    adrp x9, __bss_start
    add x9, x9, :lo12:__bss_start
    adrp x10, __bss_end
    add x10, x10, :lo12:__bss_end
.Lclear_bss:
    cmp x9, x10
    b.hs .Lbss_ready
    str xzr, [x9], #8
    b .Lclear_bss
.Lbss_ready:
    b rust_boot
.Lpark:
    wfe
    b .Lpark

.section .text.vectors, "ax"
.balign 2048
.global _el2_vectors
_el2_vectors:
    .rept 16
    b .Lexception
    .balign 128
    .endr
.Lexception:
    msr daifset, #15
    adrp x9, _exception_active
    add x9, x9, :lo12:_exception_active
    ldr x10, [x9]
    cbnz x10, .Lexception_park
    mov x10, #1
    str x10, [x9]
    msr spsel, #1
    adrp x9, __exception_stack_top
    add x9, x9, :lo12:__exception_stack_top
    mov sp, x9
    b rust_exception
.Lexception_park:
    wfe
    b .Lexception_park

// Raw BL33 entry. The existing bootloader establishes its own runtime after this handoff.
// CPU0 scaffold only; Stage-2, virtual PSCI/vGIC and device ownership are subsequent phases.
.section .text.handoff, "ax"
.global enter_payload
.type enter_payload, %function
enter_payload:
    msr daifset, #15
    mov x9, x0
    msr elr_el2, x1
    msr sp_el1, x2
    msr sp_el0, xzr
    mov x10, #0x3c5
    msr spsr_el2, x10
    // ARMv8.0 SCTLR_EL1 RES1, with MMU and both caches disabled.
    movz x10, #0x0800
    movk x10, #0x30d0, lsl #16
    msr sctlr_el1, x10
    msr vbar_el1, xzr
    msr ttbr0_el1, xzr
    msr ttbr1_el1, xzr
    msr tcr_el1, xzr
    msr mair_el1, xzr
    msr contextidr_el1, xzr
    msr tpidr_el1, xzr
    msr tpidr_el0, xzr
    msr tpidrro_el0, xzr
    msr cntvoff_el2, xzr
    msr cntp_ctl_el0, xzr
    msr cntv_ctl_el0, xzr
    mov x10, #3
    msr cnthctl_el2, x10
    // A57 CPTR_EL2 RES1; permit guest FP/SIMD without using it in the EL2 helpers.
    mov x10, #0x33ff
    msr cptr_el2, x10
    mov x10, #0x300000
    msr cpacr_el1, x10
    msr hstr_el2, xzr
    mrs x10, midr_el1
    msr vpidr_el2, x10
    mrs x10, mpidr_el1
    msr vmpidr_el2, x10
    // AArch64 EL1; physical exceptions and guest SMC enter EL2. Stage-2 remains off here.
    movz x10, #0x0038
    movk x10, #0x8008, lsl #16
    msr hcr_el2, x10
    isb
    tlbi vmalle1is
    dsb sy
    isb
    ldp x0, x1, [x9, #0]
    ldp x2, x3, [x9, #16]
    ldp x4, x5, [x9, #32]
    ldp x6, x7, [x9, #48]
    mov x8, xzr
    mov x9, xzr
    mov x10, xzr
    mov x11, xzr
    mov x12, xzr
    mov x13, xzr
    mov x14, xzr
    mov x15, xzr
    mov x16, xzr
    mov x17, xzr
    mov x18, xzr
    mov x19, xzr
    mov x20, xzr
    mov x21, xzr
    mov x22, xzr
    mov x23, xzr
    mov x24, xzr
    mov x25, xzr
    mov x26, xzr
    mov x27, xzr
    mov x28, xzr
    mov x29, xzr
    mov x30, xzr
    eret
.size enter_payload, . - enter_payload

.section .data.handoff, "aw"
.balign 8
.global _boot_handoff
_boot_handoff:
    .zero 64
_exception_active:
    .quad 0
