.section .head, "ax"
.global _start
_start:
    b .Lentry
    nop
    .quad _start
    .quad __image_end - _start
    .quad __bss_start - _start
    .quad __bss_end - _start
    .ascii "SVBOOT03"
.Lentry:
    msr daifset, #15
    mrs x9, CurrentEL
    cmp x9, #8
    b.ne .Lpark
    mrs x9, mpidr_el1
    tst x9, #0xff
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
    // Validate physical RAM before the first write to resident EL2 memory.
    // The Rust helper is position independent and uses no resident/global data.
    msr spsel, #1
    adr x9, .Lentry_stack_top
    mov sp, x9
    bl mc_placement_valid
    cbz x0, .Lpark
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
    .balign 16
    .space 2048
.Lentry_stack_top:

.section .text.vectors, "ax"
.balign 2048
.global _el2_vectors
_el2_vectors:
    // Lower AArch64 IRQ has a short integer-only path. All other exceptions
    // retain the diagnostic frame and common Rust dispatcher.
    .rept 9
    b .Lexception
    .balign 128
    .endr
    b .Lirq
    .balign 128
    .rept 6
    b .Lexception
    .balign 128
    .endr
.Lirq:
    msr daifset, #15
    // AAPCS64 preserves x19-x29. Save the caller-clobbered guest registers,
    // x19 used as our frame pointer, and LR before calling Rust.
    sub sp, sp, #176
    stp x0, x1, [sp, #0]
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    str x30, [sp, #160]
    mov x19, sp
    mrs x11, mpidr_el1
    and x11, x11, #0xff
    msr spsel, #1
    adrp x9, __exception_stack_top
    add x9, x9, :lo12:__exception_stack_top
    add x9, x9, x11, lsl #14
    mov sp, x9
    bl rust_irq
    mov sp, x19
    ldp x0, x1, [sp, #0]
    ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldp x18, x19, [sp, #144]
    ldr x30, [sp, #160]
    add sp, sp, #176
    eret
.Lexception:
    msr daifset, #15
    sub sp, sp, #256
    stp x0, x1, [sp, #0]
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    stp x20, x21, [sp, #160]
    stp x22, x23, [sp, #176]
    stp x24, x25, [sp, #192]
    stp x26, x27, [sp, #208]
    stp x28, x29, [sp, #224]
    str x30, [sp, #240]
    mrs x11, mpidr_el1
    and x11, x11, #0xff
    adrp x9, _exception_active
    add x9, x9, :lo12:_exception_active
    add x9, x9, x11, lsl #3
    ldr x10, [x9]
    cbnz x10, .Lexception_park
    mov x10, #1
    str x10, [x9]
    mov x19, sp
    mov x0, sp
    msr spsel, #1
    adrp x9, __exception_stack_top
    add x9, x9, :lo12:__exception_stack_top
    add x9, x9, x11, lsl #14
    mov sp, x9
    bl rust_exception
    mov sp, x19
    mrs x11, mpidr_el1
    and x11, x11, #0xff
    adrp x9, _exception_active
    add x9, x9, :lo12:_exception_active
    add x9, x9, x11, lsl #3
    str xzr, [x9]
    ldp x0, x1, [sp, #0]
    ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldp x18, x19, [sp, #144]
    ldp x20, x21, [sp, #160]
    ldp x22, x23, [sp, #176]
    ldp x24, x25, [sp, #192]
    ldp x26, x27, [sp, #208]
    ldp x28, x29, [sp, #224]
    ldr x30, [sp, #240]
    add sp, sp, #256
    eret
.Lexception_park:
    wfe
    b .Lexception_park

// Raw BL33 entry. The existing bootloader establishes its own runtime after this handoff.
// Stage-2 identity map with the virtual GIC CPU interface at the native GPA.
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
    msr mdcr_el2, xzr
    mrs x10, midr_el1
    msr vpidr_el2, x10
    mrs x10, mpidr_el1
    msr vmpidr_el2, x10
    // RW | VM | TSC | IMO. XUDC events wake EL2 through its owned physical IRQ.
    // Physical IRQs enter the short EL2 forwarding path; FIQ/SError remain EL1.
    adrp x10, _guest_hcr
    add x10, x10, :lo12:_guest_hcr
    ldr x10, [x10]
    msr hcr_el2, x10
    isb
    tlbi vmalle1
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

// BL31 CPU_ON entry. x0 is our physical CPU ID, never the guest context.
.global secondary_el2_entry
.type secondary_el2_entry, %function
secondary_el2_entry:
    msr daifset, #15
    mrs x9, CurrentEL
    cmp x9, #8
    b.ne .Lpark
    mrs x9, mpidr_el1
    ldr x10, =0xff00ffffff
    and x9, x9, x10
    cmp x9, #4
    b.hs .Lpark
    cmp x0, x9
    b.ne .Lpark
    mrs x10, sctlr_el2
    tbz x10, #0, .Lsecondary_mmu_off
    b .Lpark
.Lsecondary_mmu_off:
    tbz x10, #2, .Lsecondary_caches_off
    b .Lpark
.Lsecondary_caches_off:
    msr spsel, #1
    adrp x10, __boot_stack_top
    add x10, x10, :lo12:__boot_stack_top
    add x10, x10, x9, lsl #16
    mov sp, x10
    adrp x10, _el2_vectors
    add x10, x10, :lo12:_el2_vectors
    msr vbar_el2, x10
    isb
    b rust_secondary
.size secondary_el2_entry, . - secondary_el2_entry

// Virtual CPU_OFF stays in EL2 and discards the old exception frame.
.global park_vcpu
.type park_vcpu, %function
park_vcpu:
    msr daifset, #15
    mov x10, #2
    msr cntv_ctl_el0, x10
    msr cntp_ctl_el0, x10
    mrs x9, mpidr_el1
    and x9, x9, #0xff
    adrp x10, _exception_active
    add x10, x10, :lo12:_exception_active
    str xzr, [x10, x9, lsl #3]
    msr spsel, #1
    adrp x10, __boot_stack_top
    add x10, x10, :lo12:__boot_stack_top
    add x10, x10, x9, lsl #16
    mov sp, x10
    b rust_cpu_dispatch
.size park_vcpu, . - park_vcpu

// Native SMCCC forwarding. Rust/EL2 helpers use no FP/SIMD, so guest Q registers
// are untouched. The SMCCC firmware must preserve x18-x30 and FP/SIMD state.
.global forward_smc
.type forward_smc, %function
forward_smc:
    stp x19, x30, [sp, #-16]!
    mov x19, x0
    ldp x0, x1, [x19, #0]
    ldp x2, x3, [x19, #16]
    ldp x4, x5, [x19, #32]
    ldp x6, x7, [x19, #48]
    ldp x8, x9, [x19, #64]
    ldp x10, x11, [x19, #80]
    ldp x12, x13, [x19, #96]
    ldp x14, x15, [x19, #112]
    ldp x16, x17, [x19, #128]
    smc #0
    stp x0, x1, [x19, #0]
    stp x2, x3, [x19, #16]
    stp x4, x5, [x19, #32]
    stp x6, x7, [x19, #48]
    stp x8, x9, [x19, #64]
    stp x10, x11, [x19, #80]
    stp x12, x13, [x19, #96]
    stp x14, x15, [x19, #112]
    stp x16, x17, [x19, #128]
    ldp x19, x30, [sp], #16
    ret
.size forward_smc, . - forward_smc

.section .data.handoff, "aw"
.balign 8
.global _boot_handoff
_boot_handoff:
    .zero 64
_exception_active:
    .zero 32
