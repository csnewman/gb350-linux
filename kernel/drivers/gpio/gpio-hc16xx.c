// SPDX-License-Identifier: GPL-2.0
/*
 * HiChip HC16xx GPIO banks.
 *
 * Four banks of 32 (L @0x18800044, B @0x188000c4, R @0x188000e4,
 * T @0x18800344), one device per bank. Registers (vendor 4.4 gpio-hc driver
 * hc_gpio_*):
 *   +0x00 IRQ enable     +0x04 rising-edge enable  +0x08 falling-edge enable
 *   +0x0c input          +0x10 output              +0x14 direction (1 = out)
 *   +0x18 IRQ status (write 1 to clear)
 * Edge interrupts only (rising, falling or both). All banks share one
 * interrupt controller input (hwirq 0), so each bank requests it shared
 * rather than chaining it. The rising-edge register resets to all ones;
 * it is only meaningful together with +0x00.
 *
 * Pin muxing (function 0 = GPIO) is done by pinctrl-single through the
 * gpio-ranges in the DT.
 *
 * GB350: the second core (the gb350-syscore firmware) drives bank L pins too
 * (button scan, power latch), so output/direction changes are real
 * read-modify-writes - no cached shadow copies - under the cross-core
 * hwspinlock named by the bank's "hwlocks" property. The interrupt
 * registers are Linux's alone.
 */
#include <linux/bitops.h>
#include <linux/gpio/driver.h>
#include <linux/hwspinlock.h>
#include <linux/interrupt.h>
#include <linux/irq.h>
#include <linux/module.h>
#include <linux/platform_device.h>

#define HC_GPIO_IRQ_EN		0x00
#define HC_GPIO_IRQ_RISE	0x04
#define HC_GPIO_IRQ_FALL	0x08
#define HC_GPIO_IN		0x0c
#define HC_GPIO_OUT		0x10
#define HC_GPIO_DIR		0x14
#define HC_GPIO_IRQ_STATUS	0x18

struct hc16xx_gpio {
	struct gpio_chip gc;
	void __iomem *base;
	raw_spinlock_t lock;
	struct hwspinlock *hwlock;	/* output/direction, shared with syscore */
};

static struct hc16xx_gpio *irqd_to_hc16xx_gpio(struct irq_data *d)
{
	return gpiochip_get_data(irq_data_get_irq_chip_data(d));
}

static void hc16xx_gpio_update(struct hc16xx_gpio *g, unsigned int reg, u32 mask, u32 bits)
{
	writel((readl(g->base + reg) & ~mask) | (bits & mask), g->base + reg);
}

/* Output and direction registers: locked against the second core. */
static void hc16xx_gpio_update_shared(struct hc16xx_gpio *g, unsigned int reg, u32 mask, u32 bits)
{
	unsigned long flags;

	if (!g->hwlock) {
		guard(raw_spinlock_irqsave)(&g->lock);
		hc16xx_gpio_update(g, reg, mask, bits);
		return;
	}
	if (hwspin_lock_timeout_irqsave(g->hwlock, 10, &flags)) {
		/* The other side never holds it for long: write regardless. */
		dev_err_ratelimited(g->gc.parent, "cross-core GPIO lock timed out\n");
		guard(raw_spinlock_irqsave)(&g->lock);
		hc16xx_gpio_update(g, reg, mask, bits);
		return;
	}
	hc16xx_gpio_update(g, reg, mask, bits);
	hwspin_unlock_irqrestore(g->hwlock, &flags);
}

static int hc16xx_gpio_get_direction(struct gpio_chip *gc, unsigned int offset)
{
	struct hc16xx_gpio *g = gpiochip_get_data(gc);

	return readl(g->base + HC_GPIO_DIR) & BIT(offset) ? GPIO_LINE_DIRECTION_OUT :
							     GPIO_LINE_DIRECTION_IN;
}

/* Outputs read back the driven level from the output register. */
static int hc16xx_gpio_get(struct gpio_chip *gc, unsigned int offset)
{
	struct hc16xx_gpio *g = gpiochip_get_data(gc);
	unsigned int reg = readl(g->base + HC_GPIO_DIR) & BIT(offset) ? HC_GPIO_OUT : HC_GPIO_IN;

	return !!(readl(g->base + reg) & BIT(offset));
}

static int hc16xx_gpio_set(struct gpio_chip *gc, unsigned int offset, int value)
{
	struct hc16xx_gpio *g = gpiochip_get_data(gc);

	hc16xx_gpio_update_shared(g, HC_GPIO_OUT, BIT(offset), value ? BIT(offset) : 0);
	return 0;
}

static int hc16xx_gpio_direction_input(struct gpio_chip *gc, unsigned int offset)
{
	struct hc16xx_gpio *g = gpiochip_get_data(gc);

	hc16xx_gpio_update_shared(g, HC_GPIO_DIR, BIT(offset), 0);
	return 0;
}

static int hc16xx_gpio_direction_output(struct gpio_chip *gc, unsigned int offset, int value)
{
	struct hc16xx_gpio *g = gpiochip_get_data(gc);

	hc16xx_gpio_update_shared(g, HC_GPIO_OUT, BIT(offset), value ? BIT(offset) : 0);
	hc16xx_gpio_update_shared(g, HC_GPIO_DIR, BIT(offset), BIT(offset));
	return 0;
}

static void hc16xx_gpio_irq_ack(struct irq_data *d)
{
	struct hc16xx_gpio *g = irqd_to_hc16xx_gpio(d);

	writel(BIT(irqd_to_hwirq(d)), g->base + HC_GPIO_IRQ_STATUS);
}

static void hc16xx_gpio_irq_mask(struct irq_data *d)
{
	struct hc16xx_gpio *g = irqd_to_hc16xx_gpio(d);
	u32 mask = BIT(irqd_to_hwirq(d));

	scoped_guard(raw_spinlock_irqsave, &g->lock)
		hc16xx_gpio_update(g, HC_GPIO_IRQ_EN, mask, 0);
	gpiochip_disable_irq(&g->gc, irqd_to_hwirq(d));
}

static void hc16xx_gpio_irq_unmask(struct irq_data *d)
{
	struct hc16xx_gpio *g = irqd_to_hc16xx_gpio(d);
	u32 mask = BIT(irqd_to_hwirq(d));

	gpiochip_enable_irq(&g->gc, irqd_to_hwirq(d));
	guard(raw_spinlock_irqsave)(&g->lock);
	hc16xx_gpio_update(g, HC_GPIO_IRQ_EN, mask, mask);
}

static int hc16xx_gpio_irq_set_type(struct irq_data *d, unsigned int type)
{
	struct hc16xx_gpio *g = irqd_to_hc16xx_gpio(d);
	u32 mask = BIT(irqd_to_hwirq(d));

	if (type & ~IRQ_TYPE_EDGE_BOTH || !(type & IRQ_TYPE_EDGE_BOTH))
		return -EINVAL;

	scoped_guard(raw_spinlock_irqsave, &g->lock) {
		hc16xx_gpio_update(g, HC_GPIO_IRQ_RISE, mask,
				   type & IRQ_TYPE_EDGE_RISING ? mask : 0);
		hc16xx_gpio_update(g, HC_GPIO_IRQ_FALL, mask,
				   type & IRQ_TYPE_EDGE_FALLING ? mask : 0);
		/* Drop an edge latched under the old configuration. */
		writel(mask, g->base + HC_GPIO_IRQ_STATUS);
	}
	irq_set_handler_locked(d, handle_edge_irq);
	return 0;
}

static const struct irq_chip hc16xx_gpio_irqchip = {
	.name		= "hc16xx-gpio",
	.irq_ack	= hc16xx_gpio_irq_ack,
	.irq_mask	= hc16xx_gpio_irq_mask,
	.irq_unmask	= hc16xx_gpio_irq_unmask,
	.irq_set_type	= hc16xx_gpio_irq_set_type,
	.flags		= IRQCHIP_IMMUTABLE | IRQCHIP_SKIP_SET_WAKE,
	GPIOCHIP_IRQ_RESOURCE_HELPERS,
};

static irqreturn_t hc16xx_gpio_irq(int irq, void *data)
{
	struct hc16xx_gpio *g = data;
	unsigned long pending;
	unsigned int bit;

	pending = readl(g->base + HC_GPIO_IRQ_STATUS) & readl(g->base + HC_GPIO_IRQ_EN);
	for_each_set_bit(bit, &pending, 32)
		generic_handle_domain_irq(g->gc.irq.domain, bit);
	return pending ? IRQ_HANDLED : IRQ_NONE;
}

static int hc16xx_gpio_probe(struct platform_device *pdev)
{
	struct device *dev = &pdev->dev;
	struct gpio_irq_chip *girq;
	struct hc16xx_gpio *g;
	int irq, ret;

	g = devm_kzalloc(dev, sizeof(*g), GFP_KERNEL);
	if (!g)
		return -ENOMEM;
	g->base = devm_platform_ioremap_resource(pdev, 0);
	if (IS_ERR(g->base))
		return PTR_ERR(g->base);

	raw_spin_lock_init(&g->lock);

	ret = of_hwspin_lock_get_id(dev->of_node, 0);
	if (ret >= 0) {
		g->hwlock = devm_hwspin_lock_request_specific(dev, ret);
		if (!g->hwlock)
			return dev_err_probe(dev, -EBUSY, "hwlock %d\n", ret);
	} else if (ret != -ENOENT) {
		return dev_err_probe(dev, ret, "hwlocks\n");
	}

	/*
	 * Registers are only ever read-modify-written, so outputs set up by
	 * the bootloader (LCD reset, ...) keep their level.
	 */
	g->gc = (struct gpio_chip) {
		.label			= dev_name(dev),
		.parent			= dev,
		.owner			= THIS_MODULE,
		.base			= -1,
		.ngpio			= 32,
		/* Requesting a line selects its GPIO function (pinctrl gpio-ranges). */
		.request		= gpiochip_generic_request,
		.free			= gpiochip_generic_free,
		.get_direction		= hc16xx_gpio_get_direction,
		.direction_input	= hc16xx_gpio_direction_input,
		.direction_output	= hc16xx_gpio_direction_output,
		.get			= hc16xx_gpio_get,
		.set			= hc16xx_gpio_set,
	};

	irq = platform_get_irq_optional(pdev, 0);
	if (irq > 0) {
		/* Nothing enabled until a consumer asks for it. */
		writel(0, g->base + HC_GPIO_IRQ_EN);
		writel(~0U, g->base + HC_GPIO_IRQ_STATUS);

		girq = &g->gc.irq;
		gpio_irq_chip_set_chip(girq, &hc16xx_gpio_irqchip);
		girq->default_type = IRQ_TYPE_NONE;
		girq->handler = handle_bad_irq;
	}

	ret = devm_gpiochip_add_data(dev, &g->gc, g);
	if (ret)
		return dev_err_probe(dev, ret, "add gpiochip\n");

	if (irq > 0) {
		ret = devm_request_irq(dev, irq, hc16xx_gpio_irq, IRQF_SHARED,
				       dev_name(dev), g);
		if (ret)
			return dev_err_probe(dev, ret, "request IRQ %d\n", irq);
	}
	return 0;
}

static const struct of_device_id hc16xx_gpio_of_match[] = {
	{ .compatible = "hichip,hc16xx-gpio" },
	{ }
};
MODULE_DEVICE_TABLE(of, hc16xx_gpio_of_match);

static struct platform_driver hc16xx_gpio_driver = {
	.probe	= hc16xx_gpio_probe,
	.driver	= {
		.name		= "hc16xx-gpio",
		.of_match_table	= hc16xx_gpio_of_match,
	},
};
module_platform_driver(hc16xx_gpio_driver);

MODULE_DESCRIPTION("HiChip HC16xx GPIO");
MODULE_LICENSE("GPL");
