ENTRY(_start)
SECTIONS {
  . = 0x200000;
  .text.start : { *(.text.start) }
  .text : { *(.text*) }
  .rodata : { *(.rodata*) }
  .data : { *(.data*) }
  .bss : { *(.bss*) *(COMMON) }
}
