/* Loaded by the NOR bootloader as cubegm/avp.uImage at 0x8bda4000, or by the
 * Linux gb350-syscore driver into the same place; the second core enters
 * through the KSEG1 alias 0xabda4000. RAM up to __stack_top belongs to this firmware. */
ENTRY(_start)
SECTIONS
{
    . = 0x8bda4000;
    .text : {
        KEEP(*(.text.entry))
        . = ALIGN(4096);
        KEEP(*(.text.vectors))
        *(.text .text.*)
    }
    .rodata : { *(.rodata .rodata.*) }
    .data : { *(.data .data.*) *(.sdata .sdata.*) }
    .got : { *(.got) }
    .bss (NOLOAD) : {
        . = ALIGN(16);
        __bss_start = .;
        *(.bss .bss.* .sbss .sbss.* COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }
    /DISCARD/ : { *(.MIPS.abiflags) *(.reginfo) *(.comment) *(.pdr) *(.eh_frame*) }
    __stack_top = 0x8bf00000;
    ASSERT(__bss_end <= 0x8bee0000, "image too large for the stack area")
}
