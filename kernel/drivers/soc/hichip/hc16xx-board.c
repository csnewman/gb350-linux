// SPDX-License-Identifier: GPL-2.0
/*
 * HiChip HC16xx / GB350 restart and power-off.
 *
 * Watchdog at 0x18818500: +0 counter (counts up, fires on overflow), +4
 * control: bit 2 run, bits 1:0 divider of a 750 kHz tick (/1 /2 /4; measured),
 * bit 5 presumably reset-on-overflow (set by the vendor, not tested alone).
 *
 * Restart: arm the watchdog to fire after RESTART_DELAY_MS; the board comes
 * back through the bootloader (warm-boot flag 0x18818a70 = 0x5a). The agent's
 * fast restart is kexec, which arms the watchdog as a 30 s safety net first;
 * that is stopped here, early in boot.
 *
 * Both handlers must not use SYS_OFF_PRIO_PLATFORM: the kernel keeps a single
 * static handler for that priority, so registering the second one silently
 * replaced the first's callback - restart then ran the power-off handler and
 * dropped the power latch (the long-standing "reboot powers off" bug).
 *
 * Power-off (stock cubevol mp5_enter_standby): drive GPIO L1 low.
 *   bank L: 0x18800044, +0x10 output, +0x14 direction (1 = output)
 */
#include <linux/bits.h>
#include <linux/delay.h>
#include <linux/err.h>
#include <linux/init.h>
#include <linux/io.h>
#include <linux/printk.h>
#include <linux/reboot.h>

#define HC16XX_WDT		0x18818500
#define WDT_COUNT		0x0
#define WDT_CTRL		0x4
#define WDT_RESET_RUN_DIV4	0x26	/* reset on overflow, 750 kHz / 4 */
#define WDT_TICKS_PER_MS	188	/* 187.5 kHz */
#define RESTART_DELAY_MS	100
#define HC16XX_GPIO_L		0x18800044
#define GPIO_OUT		0x10
#define GPIO_DIR		0x14
#define GB350_POWER_LATCH	BIT(1)

static void __iomem *wdt;
static void __iomem *gpio_l;

static int hc16xx_restart(struct sys_off_data *data)
{
	writel(0, wdt + WDT_CTRL);
	writel(0U - RESTART_DELAY_MS * WDT_TICKS_PER_MS, wdt + WDT_COUNT);
	writel(WDT_RESET_RUN_DIV4, wdt + WDT_CTRL);
	mdelay(RESTART_DELAY_MS + 1000);
	return NOTIFY_DONE;
}

static int gb350_power_off(struct sys_off_data *data)
{
	writel(readl(gpio_l + GPIO_OUT) & ~GB350_POWER_LATCH, gpio_l + GPIO_OUT);
	writel(readl(gpio_l + GPIO_DIR) | GB350_POWER_LATCH, gpio_l + GPIO_DIR);
	mdelay(1000);
	return NOTIFY_DONE;
}

static int __init hc16xx_board_init(void)
{
	u32 ctrl;
	int ret;

	wdt = ioremap(HC16XX_WDT, 8);
	gpio_l = ioremap(HC16XX_GPIO_L, 0x1c);
	if (!wdt || !gpio_l)
		return -ENOMEM;
	/* Disarm a kexec safety net (see above). */
	ctrl = readl(wdt + WDT_CTRL);
	if (ctrl) {
		writel(0, wdt + WDT_CTRL);
		pr_info("hc16xx-board: watchdog was running (ctrl %#x, count %#x), stopped\n",
			ctrl, readl(wdt + WDT_COUNT));
	}
	ret = PTR_ERR_OR_ZERO(register_sys_off_handler(SYS_OFF_MODE_RESTART, SYS_OFF_PRIO_DEFAULT,
						       hc16xx_restart, NULL));
	if (!ret)
		ret = PTR_ERR_OR_ZERO(register_sys_off_handler(SYS_OFF_MODE_POWER_OFF,
							       SYS_OFF_PRIO_DEFAULT,
							       gb350_power_off, NULL));
	if (ret) {
		pr_err("hc16xx-board: registering restart/power-off: %d\n", ret);
		return ret;
	}
	pr_info("hc16xx-board: restart via watchdog (%u ms), power-off via GPIO L1\n", RESTART_DELAY_MS);
	return 0;
}
arch_initcall(hc16xx_board_init);
