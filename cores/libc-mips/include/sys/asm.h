/* Minimal LEAF/END and pointer-size macros for MIPS o32 assembly. */
#define LEAF(name, ...) \
	.globl name; .align 2; .type name, @function; .ent name, 0; name: .frame sp, 0, ra
#define END(name) .end name; .size name, .-name
#define PTR_ADDU	addu
#define PTR_ADDIU	addiu
#define PTR_SUBU	subu
#define PTR_SUBIU	addiu
#define PTR_SRL		srl
#define PTR_SRA		sra
#define PTR_SLL		sll
#define PTR_LA		la
#define PTR_L		lw
#define PTR_S		sw
#define LONG_L		lw
#define LONG_S		sw
#define REG_L		lw
#define REG_S		sw
#define SZREG		4
#define NARGSAVE	4
