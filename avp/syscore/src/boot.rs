//! Entry, second-core bring-up and exception vectors.
//!
//! The same image is entered twice:
//! - on the boot core (EBase.CPUNum != 0), called by the NOR bootloader's
//!   `bootm`: install the trampoline at 0x18800280, release the second core
//!   (0x18800220 bit 1), wait for it to report in (0x1880006b = 2) and return
//!   to the bootloader - the stock AVP's contract;
//! - on the second core (CPUNum 0), via the trampoline, straight out of reset
//!   (also when Linux restarts it with new firmware): report in, invalidate
//!   the caches (geometry from Config1), set KSEG0 cacheable, install the
//!   firmware's vectors, clear BSS and continue in Rust.

use core::arch::global_asm;

global_asm!(
    r#"
    .section .text.entry, "ax"
    .global _start
    .set noreorder
    .set noat
    .set mips32r2
_start:
    nop
    mfc0    $t0, $15, 1             # EBase
    andi    $t0, $t0, 3             # CPUNum
    beqz    $t0, second_core
    nop

    # --- Boot core, called by the bootloader's bootm --------------------
    lui     $t2, 0xb880             # system control, KSEG1
    lb      $t5, 0x6b($t2)          # 1 = set by the bootloader, 2 = up
    li      $t1, 2
    bne     $t5, $t1, 1f
    nop
    jr      $ra                     # second core already running
    nop
1:  move    $t4, $ra
    # Trampoline at 0x18800280: lui/ori t0, <_start | 0xa0000000>; jr t0; nop
    la      $t0, _start
    lui     $t1, 0xa000
    or      $t0, $t0, $t1
    srl     $t1, $t0, 16
    lui     $t3, 0x3c08
    or      $t1, $t1, $t3
    sw      $t1, 0x280($t2)
    andi    $t1, $t0, 0xffff
    lui     $t3, 0x3508
    or      $t1, $t1, $t3
    sw      $t1, 0x284($t2)
    lui     $t1, 0x0100
    ori     $t1, $t1, 8
    sw      $t1, 0x288($t2)
    sw      $zero, 0x28c($t2)
    lw      $t1, 0x220($t2)         # release the second core
    ori     $t1, $t1, 2
    sw      $t1, 0x220($t2)
    li      $t6, 2000000            # wait for it; pulse the release once more
2:  lb      $t3, 0x6b($t2)
    li      $t1, 2
    beq     $t3, $t1, 3f
    nop
    addiu   $t6, $t6, -1
    bnez    $t6, 2b
    nop
    lw      $t1, 0x220($t2)
    li      $t3, -3
    and     $t1, $t1, $t3
    sw      $t1, 0x220($t2)
    li      $t6, 200000
4:  addiu   $t6, $t6, -1
    bnez    $t6, 4b
    nop
    lw      $t1, 0x220($t2)
    ori     $t1, $t1, 2
    sw      $t1, 0x220($t2)
3:  li      $t1, 1
    bne     $t5, $t1, 5f
    nop
    move    $ra, $t4
    jr      $ra                     # back to the bootloader
    nop
5:  wait
    b       5b
    nop

    # --- Second core: entered uncached (KSEG1) straight out of reset ------
second_core:
    lui     $t2, 0xb880
    li      $t1, 2
    sb      $t1, 0x6b($t2)          # report in
    # Invalidate both L1 caches: store zero tags at every index. Geometry from
    # Config1: size = (64 << S) * (2 << L) * (A + 1), step = line size.
    mtc0    $zero, $28, 0           # ITagLo
    mtc0    $zero, $28, 2           # DTagLo
    ehb
    mfc0    $s0, $16, 1
    srl     $t3, $s0, 19            # IL
    andi    $t3, $t3, 7
    beqz    $t3, 8f
    nop
    li      $t4, 2
    sllv    $t4, $t4, $t3
    srl     $t5, $s0, 22
    andi    $t5, $t5, 7
    li      $t6, 64
    sllv    $t6, $t6, $t5
    srl     $t7, $s0, 16
    andi    $t7, $t7, 7
    addiu   $t7, $t7, 1
    multu   $t6, $t4
    mflo    $t6
    multu   $t6, $t7
    mflo    $t6
    lui     $t0, 0x8000
    addu    $t1, $t0, $t6
6:  cache   0x08, 0($t0)            # Index_Store_Tag_I
    addu    $t0, $t0, $t4
    bne     $t0, $t1, 6b
    nop
8:  srl     $t3, $s0, 10            # DL
    andi    $t3, $t3, 7
    beqz    $t3, 9f
    nop
    li      $t4, 2
    sllv    $t4, $t4, $t3
    srl     $t5, $s0, 13
    andi    $t5, $t5, 7
    li      $t6, 64
    sllv    $t6, $t6, $t5
    srl     $t7, $s0, 7
    andi    $t7, $t7, 7
    addiu   $t7, $t7, 1
    multu   $t6, $t4
    mflo    $t6
    multu   $t6, $t7
    mflo    $t6
    lui     $t0, 0x8000
    addu    $t1, $t0, $t6
7:  cache   0x09, 0($t0)            # Index_Store_Tag_D
    addu    $t0, $t0, $t4
    bne     $t0, $t1, 7b
    nop
9:  # KSEG0 = cacheable non-coherent, write-back (CCA 3).
    mfc0    $t0, $16
    li      $t1, -8
    and     $t0, $t0, $t1
    ori     $t0, $t0, 3
    mtc0    $t0, $16
    # The firmware's vectors (EBase is written with BEV set - also when re-entered from
    # the park loop), then leave the boot exception state: BEV, ERL and EXL
    # off, interrupts still disabled.
    mfc0    $t0, $12
    lui     $t1, 0x40               # BEV
    or      $t0, $t0, $t1
    mtc0    $t0, $12
    ehb
    la      $t0, vectors
    mtc0    $t0, $15, 1
    mfc0    $t0, $12
    li      $t1, ~((1 << 22) | 7)
    and     $t0, $t0, $t1
    mtc0    $t0, $12
    mtc0    $zero, $13              # Cause: IV = 0, no software interrupts
    ehb
    # Clear BSS through KSEG1 (the data cache was just invalidated).
    la      $t0, __bss_start
    la      $t1, __bss_end
    lui     $t2, 0xa000
    or      $t0, $t0, $t2
    or      $t1, $t1, $t2
10: beq     $t0, $t1, 11f
    nop
    sw      $zero, 0($t0)
    b       10b
    addiu   $t0, $t0, 4
11: la      $sp, __stack_top
    la      $t9, syscore_main       # continue cached, in KSEG0
    jr.hb   $t9
    nop

    # --- Exception vectors (EBase) ----------------------------------------
    .section .text.vectors, "ax"
    .balign 4096
vectors:
    .org vectors + 0x000            # TLB refill
    b       exc_entry
    nop
    .org vectors + 0x100            # cache error (uncached alias of 0x100)
    b       exc_entry
    nop
    .org vectors + 0x180            # general exception, incl. interrupts
    b       exc_entry
    nop
    .org vectors + 0x200            # interrupt with IV = 1 (unused)
    b       exc_entry
    nop

    # Save the caller-saved registers, call rust_exception(cause, epc),
    # restore, return. Runs on the interrupted stack with EXL set.
exc_entry:
    addiu   $sp, $sp, -96
    sw      $1, 0($sp)
    sw      $2, 4($sp)
    sw      $3, 8($sp)
    sw      $4, 12($sp)
    sw      $5, 16($sp)
    sw      $6, 20($sp)
    sw      $7, 24($sp)
    sw      $8, 28($sp)
    sw      $9, 32($sp)
    sw      $10, 36($sp)
    sw      $11, 40($sp)
    sw      $12, 44($sp)
    sw      $13, 48($sp)
    sw      $14, 52($sp)
    sw      $15, 56($sp)
    sw      $24, 60($sp)
    sw      $25, 64($sp)
    sw      $31, 68($sp)
    mfhi    $8
    sw      $8, 72($sp)
    mflo    $8
    sw      $8, 76($sp)
    mfc0    $4, $13                 # Cause
    mfc0    $5, $14                 # EPC
    mfc0    $6, $8                  # BadVAddr
    la      $25, rust_exception
    jalr    $25
    addiu   $sp, $sp, -16           # o32 argument save area (delay slot)
    addiu   $sp, $sp, 16
    lw      $8, 72($sp)
    mthi    $8
    lw      $8, 76($sp)
    mtlo    $8
    lw      $1, 0($sp)
    lw      $2, 4($sp)
    lw      $3, 8($sp)
    lw      $4, 12($sp)
    lw      $5, 16($sp)
    lw      $6, 20($sp)
    lw      $7, 24($sp)
    lw      $8, 28($sp)
    lw      $9, 32($sp)
    lw      $10, 36($sp)
    lw      $11, 40($sp)
    lw      $12, 44($sp)
    lw      $13, 48($sp)
    lw      $14, 52($sp)
    lw      $15, 56($sp)
    lw      $24, 60($sp)
    lw      $25, 64($sp)
    lw      $31, 68($sp)
    addiu   $sp, $sp, 96
    eret
    nop
    .set at
    .set reorder
"#
);
