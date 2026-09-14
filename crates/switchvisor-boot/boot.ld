OUTPUT_ARCH(aarch64)
ENTRY(_start)

SECTIONS {
    . = DEFINED(SWITCHVISOR_LINK_BASE) ? SWITCHVISOR_LINK_BASE : 0xAA000000;
    .head : { KEEP(*(.head)) }
    ASSERT(SIZEOF(.head) <= 0x1000, "entry overlaps payload descriptor")
    . = _start + 0x1000;
    .launch_config : { KEEP(*(.launch_config)) }
    ASSERT(SIZEOF(.launch_config) == 256, "payload descriptor size mismatch")
    .text : { *(.text .text.*) }
    .rodata : { *(.rodata .rodata.*) }
    .data : {
        *(.data .data.*) *(.got .got.*)
        . = ALIGN(16);
        __image_end = .;
    }
    .bss (NOLOAD) : {
        __bss_start = .;
        *(.bss .bss.* COMMON)
        . = ALIGN(16);
        __boot_stack_bottom = .;
        . += 64K;
        __boot_stack_top = .;
        __exception_stack_bottom = .;
        . += 16K;
        __exception_stack_top = .;
        __bss_end = .;
    }
    ASSERT(_start == 0xAA000000 || _start == 0xFEC00000, "unsupported bootstrap placement")
    ASSERT(__bss_end - _start <= 1M, "diagnostic exceeds transient BL33 budget")
    ASSERT(_start != 0xFEC00000 || __bss_end <= 0xFFC00000, "VMM exceeds protected region")
    /DISCARD/ : { *(.eh_frame*) *(.comment*) *(.note*) }
}
