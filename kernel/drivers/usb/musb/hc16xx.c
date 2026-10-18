// SPDX-License-Identifier: GPL-2.0
/*
 * HiChip HC16xx MUSB glue.
 *
 * Reverse-engineered from the vendor 4.4 kernel (drivers/hcdrivers/musb/hcusb.c:
 * hcmusb_reset, hcmusb_set_phy_power, __hcmusb_16xx_set_mode, hc16xx_ops):
 *
 *   clock gate   sysctrl 0x64 bit 24 (USB0) / 25 (USB1), clear = running
 *   reset        sysctrl 0x80 bit 28 (USB0) / 0x84 bit 29 (USB1), pulse high
 *   misc enable  sysctrl 0x7c bit 12 (USB0) / 13 (USB1)
 *   PHY tune     byte at controller base + 0x1004 (USB0, 0x1104 for USB1):
 *                (old & 0x0f) | 0xd0
 *   PHY/role     byte at controller base + 0x380: host 0x80 then |0x8c,
 *                peripheral 0x90 then |0x9c; bit 0 = PHY suspend
 *
 * The vendor driver uses MUSB_INDEXED_EP plus its own DMA engine; this glue
 * runs the controller in PIO mode. VBUS needs no software control: the PHY
 * role bits switch it.
 */
#include <linux/delay.h>
#include <linux/dma-mapping.h>
#include <linux/io.h>
#include <linux/module.h>
#include <linux/of.h>
#include <linux/platform_device.h>
#include <linux/usb/of.h>
#include "musb_core.h"

#define SYS_GATE	0x64
#define SYS_MISC	0x7c
#define SYS_RESET0	0x80
#define SYS_RESET1	0x84

#define HC_PHY_ROLE	0x380

struct hc16xx_glue {
	struct platform_device *musb;
	void __iomem *sys;
	void __iomem *phy_tune;
	unsigned int index;
	enum usb_dr_mode dr_mode;
};

static const struct musb_fifo_cfg hc16xx_fifo_cfg[] = {
	{ .hw_ep_num = 1, .style = FIFO_TX, .maxpacket = 512, },
	{ .hw_ep_num = 1, .style = FIFO_RX, .maxpacket = 512, },
	{ .hw_ep_num = 2, .style = FIFO_TX, .maxpacket = 512, },
	{ .hw_ep_num = 2, .style = FIFO_RX, .maxpacket = 512, },
	{ .hw_ep_num = 3, .style = FIFO_TX, .maxpacket = 512, },
	{ .hw_ep_num = 3, .style = FIFO_RX, .maxpacket = 512, },
	{ .hw_ep_num = 4, .style = FIFO_TX, .maxpacket = 512, },
	{ .hw_ep_num = 4, .style = FIFO_RX, .maxpacket = 512, },
	{ .hw_ep_num = 5, .style = FIFO_TX, .maxpacket = 512, },
	{ .hw_ep_num = 5, .style = FIFO_RX, .maxpacket = 512, },
};

static const struct musb_hdrc_config hc16xx_musb_config = {
	.multipoint	= 1,
	.num_eps	= 7,
	.ram_bits	= 12,
	.fifo_cfg	= hc16xx_fifo_cfg,
	.fifo_cfg_size	= ARRAY_SIZE(hc16xx_fifo_cfg),
};

static void hc16xx_set_bits(void __iomem *reg, u32 set, u32 clear)
{
	writel((readl(reg) & ~clear) | set, reg);
}

/* Clock on, reset pulse, PHY tuning (vendor hcmusb_reset). */
static void hc16xx_hw_reset(struct hc16xx_glue *glue)
{
	u32 gate = glue->index ? BIT(25) : BIT(24);
	u32 reset_reg = glue->index ? SYS_RESET1 : SYS_RESET0;
	u32 reset_bit = glue->index ? BIT(29) : BIT(28);
	u32 misc = glue->index ? BIT(13) : BIT(12);

	hc16xx_set_bits(glue->sys + SYS_GATE, 0, gate);
	udelay(900);
	hc16xx_set_bits(glue->sys + reset_reg, reset_bit, 0);
	udelay(900);
	hc16xx_set_bits(glue->sys + reset_reg, 0, reset_bit);
	hc16xx_set_bits(glue->sys + SYS_MISC, misc, 0);
	writeb((readb(glue->phy_tune) & 0x0f) | 0xd0, glue->phy_tune);
}

/* PHY role byte (vendor __hcmusb_16xx_set_mode). */
static void hc16xx_phy_role(struct musb *musb, bool host)
{
	void __iomem *role = musb->mregs + HC_PHY_ROLE;

	if (host) {
		writeb(0x80, role);
		udelay(1);
		writeb((readb(role) & ~0x10) | 0x8c, role);
	} else {
		writeb(0x90, role);
		mdelay(1);
		writeb(readb(role) | 0x9c, role);
	}
}

static irqreturn_t hc16xx_musb_interrupt(int irq, void *__hci)
{
	struct musb *musb = __hci;
	irqreturn_t ret = IRQ_NONE;
	unsigned long flags;

	spin_lock_irqsave(&musb->lock, flags);
	musb->int_usb = musb_readb(musb->mregs, MUSB_INTRUSB);
	musb->int_tx = musb_readw(musb->mregs, MUSB_INTRTX);
	musb->int_rx = musb_readw(musb->mregs, MUSB_INTRRX);
	if (musb->int_usb || musb->int_tx || musb->int_rx)
		ret = musb_interrupt(musb);
	spin_unlock_irqrestore(&musb->lock, flags);
	return ret;
}

static int hc16xx_musb_init(struct musb *musb)
{
	struct device *dev = musb->controller->parent;
	struct hc16xx_glue *glue = dev_get_drvdata(dev);

	musb->xceiv = devm_usb_get_phy_by_phandle(dev, "phys", 0);
	if (IS_ERR(musb->xceiv))
		return dev_err_probe(dev, PTR_ERR(musb->xceiv), "no transceiver (phys)\n");

	hc16xx_phy_role(musb, glue->dr_mode == USB_DR_MODE_HOST);
	/* Restart the session (vendor toggles DEVCTL.SESSION after the role). */
	musb_writeb(musb->mregs, MUSB_DEVCTL, musb_readb(musb->mregs, MUSB_DEVCTL) & ~MUSB_DEVCTL_SESSION);
	mdelay(1);
	musb_writeb(musb->mregs, MUSB_DEVCTL, musb_readb(musb->mregs, MUSB_DEVCTL) | MUSB_DEVCTL_SESSION);

	/* The vendor configuration declares a dynamic FIFO (no ConfigData). */
	musb->dyn_fifo = true;
	musb->isr = hc16xx_musb_interrupt;
	return 0;
}

static int hc16xx_musb_exit(struct musb *musb)
{
	return 0;
}

static const struct musb_platform_ops hc16xx_musb_ops = {
	.quirks		= MUSB_INDEXED_EP,
	.init		= hc16xx_musb_init,
	.exit		= hc16xx_musb_exit,
};

static int hc16xx_probe(struct platform_device *pdev)
{
	struct device *dev = &pdev->dev;
	struct musb_hdrc_platform_data pdata = {
		.config = &hc16xx_musb_config,
		.platform_ops = &hc16xx_musb_ops,
	};
	struct hc16xx_glue *glue;
	struct platform_device *musb;
	struct resource *res;
	u32 sys_base = 0x18800000;
	int ret;

	glue = devm_kzalloc(dev, sizeof(*glue), GFP_KERNEL);
	if (!glue)
		return -ENOMEM;

	of_property_read_u32(dev->of_node, "index_num", &glue->index);
	of_property_read_u32(dev->of_node, "ctrl_base", &sys_base);
	glue->dr_mode = usb_get_dr_mode(dev);
	if (glue->dr_mode == USB_DR_MODE_UNKNOWN)
		glue->dr_mode = USB_DR_MODE_PERIPHERAL;
	pdata.mode = glue->dr_mode == USB_DR_MODE_HOST ? MUSB_HOST : MUSB_PERIPHERAL;

	res = platform_get_resource(pdev, IORESOURCE_MEM, 0);
	if (!res)
		return -EINVAL;
	/* System control is shared with many blocks: map, don't claim. */
	glue->sys = devm_ioremap(dev, sys_base, 0x100);
	glue->phy_tune = devm_ioremap(dev, res->start + 0x1004 + glue->index * 0x100, 4);
	if (!glue->sys || !glue->phy_tune)
		return -ENOMEM;
	platform_set_drvdata(pdev, glue);
	hc16xx_hw_reset(glue);

	musb = platform_device_alloc("musb-hdrc", PLATFORM_DEVID_AUTO);
	if (!musb)
		return -ENOMEM;
	musb->dev.parent = dev;
	musb->dev.dma_mask = &musb->dev.coherent_dma_mask;
	musb->dev.coherent_dma_mask = DMA_BIT_MASK(32);
	device_set_of_node_from_dev(&musb->dev, dev);
	glue->musb = musb;

	ret = platform_device_add_resources(musb, pdev->resource, pdev->num_resources);
	if (!ret)
		ret = platform_device_add_data(musb, &pdata, sizeof(pdata));
	if (!ret)
		ret = platform_device_add(musb);
	if (ret) {
		platform_device_put(musb);
		return ret;
	}
	dev_info(dev, "usb%u as %s\n", glue->index, glue->dr_mode == USB_DR_MODE_HOST ? "host" : "peripheral");
	return 0;
}

static void hc16xx_remove(struct platform_device *pdev)
{
	struct hc16xx_glue *glue = platform_get_drvdata(pdev);

	platform_device_unregister(glue->musb);
}

static const struct of_device_id hc16xx_musb_of_match[] = {
	{ .compatible = "hichip,hc16xx-musb" },
	{ }
};
MODULE_DEVICE_TABLE(of, hc16xx_musb_of_match);

static struct platform_driver hc16xx_musb_driver = {
	.probe = hc16xx_probe,
	.remove = hc16xx_remove,
	.driver = {
		.name = "musb-hc16xx",
		.of_match_table = hc16xx_musb_of_match,
	},
};
module_platform_driver(hc16xx_musb_driver);

MODULE_DESCRIPTION("HiChip HC16xx MUSB glue");
MODULE_LICENSE("GPL");
