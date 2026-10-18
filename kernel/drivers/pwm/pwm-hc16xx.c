// SPDX-License-Identifier: GPL-2.0
/*
 * HiChip HC16xx PWM: six channels sharing one clock divider (HCRTOS SDK
 * hc-pwm.c, pwm_reg_struct.h). GB350: channel 0 drives the backlight
 * (GPIO L0, pin function 4), left at 100 kHz / 60 % by the bootloader.
 *
 *   +0x00        bit 31 enable, bit 30 clock enable, [29:16] divider
 *   +0x04 + ch   (byte) bit 7 channel enable, bit 4 polarity (inverted)
 *   +0x0c + 4 ch [15:0] low time, [31:16] high time, in divided ticks
 * The input clock is 27 MHz (a 37 ns tick).
 */
#include <linux/io.h>
#include <linux/math64.h>
#include <linux/module.h>
#include <linux/platform_device.h>
#include <linux/pwm.h>

#define HC_PWM_CLK		0x00
#define HC_PWM_CLK_ENABLE	(BIT(31) | BIT(30))
#define HC_PWM_DIV_SHIFT	16
#define HC_PWM_DIV_MASK		GENMASK(29, 16)
#define HC_PWM_CH_CTRL(ch)	(0x04 + (ch))
#define HC_PWM_CH_EN		BIT(7)
#define HC_PWM_CH_INVERT	BIT(4)
#define HC_PWM_CH_COUNT(ch)	(0x0c + 4 * (ch))
#define HC_PWM_CHANNELS		6
#define HC_PWM_RATE		27000000ULL
#define HC_PWM_MAX_COUNT	0xffff

struct hc16xx_pwm {
	void __iomem *base;
	spinlock_t lock;
};

static struct hc16xx_pwm *to_hc(struct pwm_chip *chip)
{
	return pwmchip_get_drvdata(chip);
}

static unsigned int hc16xx_pwm_div(struct hc16xx_pwm *hc)
{
	return max((readl(hc->base + HC_PWM_CLK) & HC_PWM_DIV_MASK) >> HC_PWM_DIV_SHIFT, 1U);
}

static bool hc16xx_pwm_others_on(struct hc16xx_pwm *hc, unsigned int ch)
{
	int i;

	for (i = 0; i < HC_PWM_CHANNELS; i++)
		if (i != ch && readb(hc->base + HC_PWM_CH_CTRL(i)) & HC_PWM_CH_EN)
			return true;
	return false;
}

static int hc16xx_pwm_apply(struct pwm_chip *chip, struct pwm_device *pwm,
			    const struct pwm_state *state)
{
	struct hc16xx_pwm *hc = to_hc(chip);
	unsigned int ch = pwm->hwpwm, div;
	u64 period, high;
	u8 ctrl;

	guard(spinlock_irqsave)(&hc->lock);
	ctrl = readb(hc->base + HC_PWM_CH_CTRL(ch));
	if (!state->enabled) {
		writeb(ctrl & ~HC_PWM_CH_EN, hc->base + HC_PWM_CH_CTRL(ch));
		return 0;
	}

	/* Ticks at 27 MHz; high and low each fit 16 bits after division. */
	period = mul_u64_u64_div_u64(state->period, HC_PWM_RATE, NSEC_PER_SEC);
	high = mul_u64_u64_div_u64(state->duty_cycle, HC_PWM_RATE, NSEC_PER_SEC);
	if (period < 2)
		return -EINVAL;
	div = hc16xx_pwm_div(hc);
	if (!hc16xx_pwm_others_on(hc, ch)) {
		/* The divider is the driver's choice: the finest one that fits. */
		div = DIV_ROUND_UP_ULL(period, 2 * HC_PWM_MAX_COUNT);
		if (div > HC_PWM_DIV_MASK >> HC_PWM_DIV_SHIFT)
			return -ERANGE;
		writel((readl(hc->base + HC_PWM_CLK) & ~HC_PWM_DIV_MASK) | HC_PWM_CLK_ENABLE |
		       div << HC_PWM_DIV_SHIFT, hc->base + HC_PWM_CLK);
	}
	period = div_u64(period, div);
	high = min(div_u64(high, div), period);
	if (high > HC_PWM_MAX_COUNT || period - high > HC_PWM_MAX_COUNT)
		return -ERANGE;	/* shared divider set by another channel */

	writel((u32)high << 16 | (u32)(period - high), hc->base + HC_PWM_CH_COUNT(ch));
	ctrl &= ~HC_PWM_CH_INVERT;
	if (state->polarity == PWM_POLARITY_INVERSED)
		ctrl |= HC_PWM_CH_INVERT;
	writeb(ctrl | HC_PWM_CH_EN, hc->base + HC_PWM_CH_CTRL(ch));
	return 0;
}

static int hc16xx_pwm_get_state(struct pwm_chip *chip, struct pwm_device *pwm,
				struct pwm_state *state)
{
	struct hc16xx_pwm *hc = to_hc(chip);
	unsigned int ch = pwm->hwpwm;
	u32 count = readl(hc->base + HC_PWM_CH_COUNT(ch));
	u64 tick_div = hc16xx_pwm_div(hc);
	u8 ctrl = readb(hc->base + HC_PWM_CH_CTRL(ch));
	u64 high = count >> 16, low = count & 0xffff;

	state->enabled = ctrl & HC_PWM_CH_EN && readl(hc->base + HC_PWM_CLK) & BIT(31);
	state->polarity = ctrl & HC_PWM_CH_INVERT ? PWM_POLARITY_INVERSED : PWM_POLARITY_NORMAL;
	state->period = DIV_ROUND_UP_ULL((high + low) * tick_div * NSEC_PER_SEC, HC_PWM_RATE);
	state->duty_cycle = DIV_ROUND_UP_ULL(high * tick_div * NSEC_PER_SEC, HC_PWM_RATE);
	return 0;
}

static const struct pwm_ops hc16xx_pwm_ops = {
	.apply		= hc16xx_pwm_apply,
	.get_state	= hc16xx_pwm_get_state,
};

static int hc16xx_pwm_probe(struct platform_device *pdev)
{
	struct pwm_chip *chip;
	struct hc16xx_pwm *hc;
	struct resource *res;

	chip = devm_pwmchip_alloc(&pdev->dev, HC_PWM_CHANNELS, sizeof(*hc));
	if (IS_ERR(chip))
		return PTR_ERR(chip);
	hc = to_hc(chip);
	/* Inside the SAR ADC's register window (hc16xx-adc claims it). */
	res = platform_get_resource(pdev, IORESOURCE_MEM, 0);
	if (!res)
		return -EINVAL;
	hc->base = devm_ioremap(&pdev->dev, res->start, resource_size(res));
	if (!hc->base)
		return -ENOMEM;
	spin_lock_init(&hc->lock);
	chip->ops = &hc16xx_pwm_ops;
	return devm_pwmchip_add(&pdev->dev, chip);
}

static const struct of_device_id hc16xx_pwm_of_match[] = {
	{ .compatible = "hichip,hc16xx-pwm" },
	{ }
};
MODULE_DEVICE_TABLE(of, hc16xx_pwm_of_match);

static struct platform_driver hc16xx_pwm_driver = {
	.probe	= hc16xx_pwm_probe,
	.driver	= {
		.name		= "hc16xx-pwm",
		.of_match_table	= hc16xx_pwm_of_match,
	},
};
module_platform_driver(hc16xx_pwm_driver);

MODULE_DESCRIPTION("HiChip HC16xx PWM");
MODULE_LICENSE("GPL");
