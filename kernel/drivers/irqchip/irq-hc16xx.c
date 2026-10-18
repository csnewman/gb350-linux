// SPDX-License-Identifier: GPL-2.0
/*
 * HiChip HC16xx system interrupt controller.
 *
 * Reverse-engineered from the vendor 4.4 kernel (intc_of_init,
 * hc16xx_intc_irq_handler, sys_irq_enable/disable). 96 inputs, chained to two
 * CPU interrupt lines. Level triggered, no acknowledge register:
 *
 *   status   words for hwirq 0-31 and 32-63 (read-only pending bits)
 *   enable   words for hwirq 0-31, 32-63, 64-95 (1 = enabled)
 *   summary  hwirq 64-95 have no status word; only hwirq 66 (AVP mailbox) is
 *            used, pending when (summary & 0xff) != 0
 *
 * DT: reg = <status>, <enable>, <summary>.
 */
#include <linux/bitops.h>
#include <linux/io.h>
#include <linux/irq.h>
#include <linux/irqchip.h>
#include <linux/irqchip/chained_irq.h>
#include <linux/irqdomain.h>
#include <linux/of_address.h>
#include <linux/of_irq.h>
#include <linux/spinlock.h>

#define HC16XX_NR_IRQS	96
#define HC16XX_AVP_IRQ	66

struct hc16xx_intc {
	void __iomem *status;
	void __iomem *enable;
	void __iomem *summary;
	struct irq_domain *domain;
	raw_spinlock_t lock;
};

static void hc16xx_irq_set(struct irq_data *d, bool on)
{
	struct hc16xx_intc *intc = irq_data_get_irq_chip_data(d);
	void __iomem *reg = intc->enable + 4 * (d->hwirq / 32);
	unsigned long flags;
	u32 val;

	raw_spin_lock_irqsave(&intc->lock, flags);
	val = readl(reg);
	if (on)
		val |= BIT(d->hwirq % 32);
	else
		val &= ~BIT(d->hwirq % 32);
	writel(val, reg);
	raw_spin_unlock_irqrestore(&intc->lock, flags);
}

static void hc16xx_irq_mask(struct irq_data *d)
{
	hc16xx_irq_set(d, false);
}

static void hc16xx_irq_unmask(struct irq_data *d)
{
	hc16xx_irq_set(d, true);
}

static struct irq_chip hc16xx_irq_chip = {
	.name		= "hc16xx-intc",
	.irq_mask	= hc16xx_irq_mask,
	.irq_unmask	= hc16xx_irq_unmask,
};

static void hc16xx_intc_handler(struct irq_desc *desc)
{
	struct hc16xx_intc *intc = irq_desc_get_handler_data(desc);
	struct irq_chip *chip = irq_desc_get_chip(desc);
	unsigned long pending;
	unsigned int word, bit;
	bool handled = false;

	chained_irq_enter(chip, desc);
	for (word = 0; word < HC16XX_NR_IRQS / 32; word++) {
		if (word < 2)
			pending = readl(intc->status + 4 * word);
		else
			pending = (readl(intc->summary) & 0xff) ?
				  BIT(HC16XX_AVP_IRQ % 32) : 0;
		pending &= readl(intc->enable + 4 * word);
		for_each_set_bit(bit, &pending, 32) {
			generic_handle_domain_irq(intc->domain, word * 32 + bit);
			handled = true;
		}
	}
	if (!handled)
		spurious_interrupt();
	chained_irq_exit(chip, desc);
}

static int hc16xx_intc_map(struct irq_domain *d, unsigned int virq, irq_hw_number_t hw)
{
	irq_set_chip_data(virq, d->host_data);
	irq_set_chip_and_handler(virq, &hc16xx_irq_chip, handle_level_irq);
	irq_set_probe(virq);
	return 0;
}

static const struct irq_domain_ops hc16xx_intc_domain_ops = {
	.map	= hc16xx_intc_map,
	.xlate	= irq_domain_xlate_onecell,
};

static int __init hc16xx_intc_of_init(struct device_node *node, struct device_node *parent)
{
	struct hc16xx_intc *intc;
	unsigned int i, nparents;

	intc = kzalloc(sizeof(*intc), GFP_KERNEL);
	if (!intc)
		return -ENOMEM;
	raw_spin_lock_init(&intc->lock);

	/* The register windows overlap, so map without claiming regions. */
	intc->status = of_iomap(node, 0);
	intc->enable = of_iomap(node, 1);
	intc->summary = of_iomap(node, 2);
	if (!intc->status || !intc->enable || !intc->summary) {
		pr_err("hc16xx-intc: cannot map registers\n");
		return -ENOMEM;
	}

	/* Start with everything masked; drivers unmask what they request. */
	for (i = 0; i < HC16XX_NR_IRQS / 32; i++)
		writel(0, intc->enable + 4 * i);

	intc->domain = irq_domain_create_linear(of_fwnode_handle(node), HC16XX_NR_IRQS,
						&hc16xx_intc_domain_ops, intc);
	if (!intc->domain)
		return -ENOMEM;

	nparents = of_irq_count(node);
	for (i = 0; i < nparents; i++) {
		unsigned int irq = irq_of_parse_and_map(node, i);

		if (irq)
			irq_set_chained_handler_and_data(irq, hc16xx_intc_handler, intc);
	}
	pr_info("hc16xx-intc: %d inputs on %u parent interrupts\n", HC16XX_NR_IRQS, nparents);
	return 0;
}
IRQCHIP_DECLARE(hc16xx_intc, "hichip,hc16xx-intc", hc16xx_intc_of_init);
