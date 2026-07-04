 MEMORY {
     RAM : ORIGIN = 0x0D000000, LENGTH = 1536K
 }

 REGION_ALIAS("FLASH", RAM);

 EXTERN(__stack);
 _stack_start = ORIGIN(RAM) + LENGTH(RAM);
 PROVIDE(__stack = _stack_start);
