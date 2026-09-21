OUTPUT_ARCH(aarch64)
ENTRY(_start)

SECTIONS {
    . = 0xFEC00000;
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
        . = ALIGN(64);
        __boot_stack_bottom = .;
        __boot_stack_top = . + 64K;
        . += 256K;
        __exception_stack_bottom = .;
        __exception_stack_top = . + 16K;
        . += 64K;
        __bss_end = .;
    }
    ASSERT(__bss_end - _start <= 1M, "bootstrap exceeds relocation budget")
    ASSERT(__bss_end <= 0xFFC00000, "VMM exceeds protected region")
    /* A separate NC block: never share a cache line with monitor state. The
       driver initializes this storage before giving any descriptor to XUDC. */
    .dma 0xFEE00000 (NOLOAD) : { KEEP(*(.dma)) }
    ASSERT(SIZEOF(.dma) <= 2M, "DMA exceeds its non-cacheable block")
    /DISCARD/ : { *(.eh_frame*) *(.comment*) *(.note*) }
}
