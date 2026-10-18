// SPDX-License-Identifier: GPL-2.0
/*
 * HiChip HC16xx graphics layer (GMA) takeover for the boot framebuffer.
 *
 * The bootloader leaves the panel running with its framebuffer (640x480
 * ARGB8888 at 0x0af93000, showing the boot logo), which Linux uses through
 * simple-framebuffer. (Not 0x0af92000, as an earlier reading of the vendor
 * DT had it: pointing the layer 4 KiB early showed the logo wrapped 384
 * pixels sideways until something redrew the screen.) The
 * layer reads a descriptor from RAM (address in +0x304); the bootloader's copy
 * (0x0af92a80) lies inside the framebuffer, so it is copied as early as
 * possible to DESC_PHYS, given the driver's geometry (the bootloader's is not the
 * 640x480 stride-2560 layout simple-framebuffer uses: text came out skewed),
 * and the layer is pointed at the copy and switched on.
 *
 * DESC_PHYS is a page the DT reserves (no-map), like the boot framebuffer: no
 * kernel ever allocates it, so the descriptor survives a kexec into a new
 * kernel, which then finds the layer already pointing at it. (Memory above
 * Linux RAM is not safe: pixels appeared over a descriptor at 0x0afbe000.)
 *
 * The DRM driver (drivers/gpu/drm/hichip) takes over from simple-framebuffer
 * later; its descriptors live in the same page, from offset 0x400.
 *
 * Registers (from the vendor 4.4 hcfb driver, gma_show_onoff/gma_dmba_update):
 *   +0x300 bit 0  layer on
 *   +0x304        physical address of the layer descriptor in RAM
 *   +0x350 bit 0  update lock: set, change 0x300/0x304, clear to commit
 * Descriptor (32-bit words): 0 flags (bit0 enable, 4-7 format, byte3 0xaa),
 * 2/3 dst x/y window, 4 src size, 5 alpha + pitch, 7 framebuffer address,
 * 9 scale ratios; 0x40.. scale coefficients; 0x240.. colour enhancement.
 */
#include <linux/init.h>
#include <linux/io.h>
#include <linux/mm.h>
#include <linux/delay.h>
#include <linux/of.h>
#include <linux/of_address.h>
#include <asm/addrspace.h>

#define GMA_CTRL	0x300
#define GMA_DESC	0x304
#define GMA_LOCK	0x350
#define DESC_BYTES	0x300

/* Boot framebuffer geometry (matches the DT simple-framebuffer node). */
#define FB_PHYS		0x0af93000
#define FB_WIDTH	640
#define FB_HEIGHT	480
#define FB_PITCH	(FB_WIDTH * 4)
#define DESC_PHYS	0x0af90000	/* DT reserved-memory gma-descriptor */

static void __iomem *gma;

static void gma_commit(u32 ctrl, u32 desc_phys)
{
	writel(readl(gma + GMA_LOCK) | 1, gma + GMA_LOCK);
	writel(desc_phys, gma + GMA_DESC);
	writel(ctrl, gma + GMA_CTRL);
	writel(readl(gma + GMA_LOCK) & ~1, gma + GMA_LOCK);
}

static int __init hc16xx_gma_takeover(void)
{
	struct device_node *np;
	u32 ctrl, desc, *d;

	np = of_find_compatible_node(NULL, NULL, "hichip,hc16xx-gma");
	if (!np)
		return 0;
	gma = of_iomap(np, 0);
	of_node_put(np);
	if (!gma)
		return -ENOMEM;

	ctrl = readl(gma + GMA_CTRL);
	desc = readl(gma + GMA_DESC) & 0x1fffffff;
	pr_info("hc16xx-gma: boot state ctrl %08x desc @%08x\n", ctrl, desc);

	/*
	 * The display engine reads the descriptor from memory: only ever touch
	 * it through the uncached alias (the page is no-map, so no cached alias
	 * is in use). After a kexec it is already in place.
	 */
	d = (u32 *)CKSEG1ADDR(DESC_PHYS);
	if (desc && desc != DESC_PHYS)
		memcpy_fromio(d, (void __iomem *)CKSEG1ADDR(desc), DESC_BYTES);
	else if (!desc)
		memset_io((void __iomem *)d, 0, DESC_BYTES);
	if ((d[0] >> 24) != 0xaa)
		pr_warn("hc16xx-gma: descriptor @%08x looks corrupted (w0 %08x)\n", desc, d[0]);
	pr_info("hc16xx-gma: boot desc w0 %08x dst %08x/%08x src %08x pitch %08x fb %08x scale %08x\n",
		d[0], d[2], d[3], d[4], d[5], d[7], d[9]);

	/*
	 * The driver's geometry, as the vendor driver programs a 640x480 1:1 layer (live
	 * values from the stock system): keep format/alpha/enhancement, set the
	 * window, source size, pitch and buffer, and turn scaling off.
	 */
	/* ARGB8888, scaling and edge preservation off (a previous kernel's DRM
	 * driver may have left another format or the scaler on). */
	d[0] = (d[0] & ~0xfeU) | 0x10 | 1;
	d[2] = (FB_WIDTH - 1) << 16;			/* dst x 0..639 */
	d[3] = (FB_HEIGHT - 1) << 16;			/* dst y 0..479 */
	d[4] = FB_WIDTH | (FB_HEIGHT << 16);		/* src 640x480 */
	d[5] = (d[5] & 0xc000ffff) | (FB_PITCH << 16);	/* pitch in bytes */
	d[6] = 0;
	d[7] = FB_PHYS;
	d[8] = 0;					/* filter (not duplicate) */
	d[9] = 0x10001000;				/* 1:1 */
	pr_info("hc16xx-gma: new  desc w0 %08x dst %08x/%08x src %08x pitch %08x fb %08x scale %08x\n",
		d[0], d[2], d[3], d[4], d[5], d[7], d[9]);

	gma_commit(ctrl | 1, DESC_PHYS);
	pr_info("hc16xx-gma: layer on, descriptor at %08x\n", DESC_PHYS);
	return 0;
}
/* As early as possible: before the page allocator hands out the memory the
 * bootloader's descriptor sits in. */
core_initcall(hc16xx_gma_takeover);
