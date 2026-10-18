// SPDX-License-Identifier: GPL-2.0
/*
 * HiChip HC16xx I2S output ("i2so") with its ring DMA, as wired on the GB350
 * to a CS4344 DAC (left-justified, no control interface). Register map and
 * stock sequence: from the AVP firmware (HCRTOS SDK libauddrv).
 *
 * Audio block SND0 (0x1880a000), i2so part:
 *   +0x04  bits 0, 8: output/clock enable; bit 16: DMA run
 *   +0x08  interrupt enables [15:0], status [31:16] (write 1 to clear);
 *          bit 0 / 16 = period (every +0x5c frames)
 *   +0x0c  more enables/status; bit 0 / 8 = buffer empty (rd reached wr)
 *   +0x30  ring buffer physical address (32-byte aligned)
 *   +0x34  [15:0] ring size in 16-byte units, [17:16] channels (1 = 2),
 *          [21:20] container (1 = 16 bit), 24 + 27 raw PCM, 25 aligned,
 *          26 unsigned 8-bit, 28 pause
 *   +0x38  write pointer, +0x3a hardware read pointer (16-byte units)
 *   +0x50  [1:0] format (1 = left-justified), [5:4] slot width (0 = 16 bit)
 *   +0x5c  period in frames
 *   +0x91  gain fade speed, +0x92 target gain 0..255, +0x93 current gain
 * The DMA stops when the read pointer reaches the write pointer. It advances
 * in 32-byte steps, so an odd write pointer is never reached: the ring then
 * plays cyclically, which is what ALSA expects (it detects underruns itself).
 *
 * Clocks (system control 0x18800000): audio PLL at +0x470..+0x47b (294.912
 * MHz for the 48 kHz family, 270.9504 MHz for 44.1 kHz), dividers at +0x148
 * ([31:23] MCLK, [9:0] BCLK): MCLK = 384 fs, BCLK = 32 fs (16-bit slots).
 *
 * Mixer: "Master" = the hardware gain (it ramps, so changes do not click),
 * and "Speaker" = the speaker amplifier policy, which the second-core
 * firmware applies (it also mutes the speaker for headphones).
 */
#include <linux/delay.h>
#include <linux/dma-mapping.h>
#include <linux/interrupt.h>
#include <linux/io.h>
#include <linux/module.h>
#include <linux/platform_device.h>
#include <linux/soc/hichip/gb350-syscore.h>
#include <sound/control.h>
#include <sound/core.h>
#include <sound/initval.h>
#include <sound/pcm.h>

/* SND0 */
#define I2SO_CTRL		0x04
#define I2SO_CTRL_OUT		(BIT(0) | BIT(8))
#define I2SO_CTRL_RUN		BIT(16)
#define I2SO_INT		0x08
#define I2SO_INT_PERIOD		BIT(0)
#define I2SO_INT_STATUS		GENMASK(31, 16)
#define I2SO_INT2		0x0c
#define I2SO_INT2_STATUS	(GENMASK(15, 8) | GENMASK(31, 24))
#define I2SO_BUF		0x30
#define I2SO_CFG		0x34
#define I2SO_CFG_KEEP		0xe0cc0000
#define I2SO_CFG_STEREO		(1 << 16)
#define I2SO_CFG_16BIT		(1 << 20)
#define I2SO_CFG_RAW		(BIT(24) | BIT(27))
#define I2SO_CFG_ALIGNED	BIT(25)
#define I2SO_WR			0x38
#define I2SO_RD			0x3a
#define I2SO_FMT		0x50
#define I2SO_FMT_LJ		1
#define I2SO_PERIOD		0x5c
#define I2SO_FADE		0x91
#define I2SO_GAIN		0x92
#define I2SO_FADE2		0x98

/* System control */
#define SYS_GATE		0x064	/* bit 15 clear = audio clock on */
#define SYS_GATE_AUDIO		BIT(15)
#define SYS_HDMI_RX_BYPASS	0x06c	/* bit 7: i2so from HDMI-RX */
#define SYS_RESET		0x080	/* bit 5: audio block reset */
#define SYS_PAD_CLOCKS		0x08c	/* bits 16..26 clear = on */
#define SYS_EJTAG		0x094	/* bit 16: EJTAG off (pins shared with I2S) */
#define SYS_DIV			0x148
#define SYS_AUDIO_PAD		0x154	/* bit 14 */
#define SYS_PLL_CTRL		0x470
#define SYS_PLL_FRAC		0x474
#define SYS_PLL_INT		0x478	/* [31:16] N, [7:0] 7 */

#define PLL_48K			294912000U
#define PLL_44K1		270950400U
#define GAIN_DEFAULT		32	/* quiet; stock "volume 8" was 21 */
#define FADE_SPEED		10

struct hc16xx_i2s {
	struct device *dev;
	void __iomem *regs;
	void __iomem *sys;
	struct snd_card *card;
	struct snd_pcm *pcm;
	spinlock_t lock;
	u8 gain;
	bool mute;
	unsigned int speaker;	/* 0 auto, 1 off, 2 on (syscore policy) */
	unsigned int pll;	/* family programmed, 0 = none yet */
};

static void sys_update(struct hc16xx_i2s *a, unsigned int reg, u32 mask, u32 bits)
{
	writel((readl(a->sys + reg) & ~mask) | bits, a->sys + reg);
}

static void hc16xx_apply_gain(struct hc16xx_i2s *a)
{
	writeb(a->mute ? 0 : a->gain, a->regs + I2SO_GAIN);
}

static void hc16xx_set_pll(struct hc16xx_i2s *a, unsigned int pll)
{
	u32 n = pll == PLL_48K ? 0x158 : 0x13c;
	u32 k = pll == PLL_48K ? 0x106000 : 0x1b6000;

	if (a->pll == pll)
		return;
	/* F = 6 MHz * (N + K / 2^24) / 7 (stock values) */
	writel((readl(a->sys + SYS_PLL_FRAC) & 0xff000000) | k, a->sys + SYS_PLL_FRAC);
	writel(n << 16 | (readl(a->sys + SYS_PLL_INT) & 0xff00) | 7, a->sys + SYS_PLL_INT);
	sys_update(a, SYS_PLL_CTRL, 0x00ff0008, BIT(16) | BIT(21));
	usleep_range(1000, 2000);
	a->pll = pll;
}

static const unsigned int hc16xx_rates[] = { 16000, 22050, 24000, 32000, 44100, 48000 };

static const struct snd_pcm_hw_constraint_list hc16xx_rate_list = {
	.count	= ARRAY_SIZE(hc16xx_rates),
	.list	= hc16xx_rates,
};

static const struct snd_pcm_hardware hc16xx_pcm_hw = {
	.info			= SNDRV_PCM_INFO_MMAP | SNDRV_PCM_INFO_MMAP_VALID |
				  SNDRV_PCM_INFO_INTERLEAVED | SNDRV_PCM_INFO_BLOCK_TRANSFER,
	.formats		= SNDRV_PCM_FMTBIT_S16_LE,
	.rates			= SNDRV_PCM_RATE_KNOT,
	.rate_min		= 16000,
	.rate_max		= 48000,
	.channels_min		= 2,
	.channels_max		= 2,
	.buffer_bytes_max	= 256 * 1024,
	.period_bytes_min	= 256,
	.period_bytes_max	= 64 * 1024,
	.periods_min		= 2,
	.periods_max		= 256,
};

static int hc16xx_pcm_open(struct snd_pcm_substream *ss)
{
	int ret;

	ss->runtime->hw = hc16xx_pcm_hw;
	ret = snd_pcm_hw_constraint_list(ss->runtime, 0, SNDRV_PCM_HW_PARAM_RATE,
					 &hc16xx_rate_list);
	/* The DMA moves 32-byte blocks: periods and the ring are multiples. */
	if (!ret)
		ret = snd_pcm_hw_constraint_step(ss->runtime, 0,
						 SNDRV_PCM_HW_PARAM_PERIOD_BYTES, 32);
	if (!ret)
		ret = snd_pcm_hw_constraint_step(ss->runtime, 0,
						 SNDRV_PCM_HW_PARAM_BUFFER_BYTES, 32);
	if (!ret)
		ret = snd_pcm_hw_constraint_integer(ss->runtime, SNDRV_PCM_HW_PARAM_PERIODS);
	return ret;
}

static int hc16xx_pcm_close(struct snd_pcm_substream *ss)
{
	return 0;
}

static int hc16xx_pcm_prepare(struct snd_pcm_substream *ss)
{
	struct hc16xx_i2s *a = snd_pcm_substream_chip(ss);
	struct snd_pcm_runtime *rt = ss->runtime;
	unsigned int pll = rt->rate % 4000 ? PLL_44K1 : PLL_48K;
	u32 mclk = pll / (384 * rt->rate), bclk = pll / (32 * rt->rate);
	size_t bytes = snd_pcm_lib_buffer_bytes(ss);

	if (rt->dma_addr & 31 || bytes & 31)
		return -EINVAL;

	hc16xx_set_pll(a, pll);
	sys_update(a, SYS_DIV, 0xff800000 | 0x3ff, mclk << 23 | bclk);
	sys_update(a, SYS_AUDIO_PAD, 0, BIT(14));
	sys_update(a, SYS_HDMI_RX_BYPASS, BIT(7), 0);

	writel(readl(a->regs + I2SO_CTRL) & ~I2SO_CTRL_RUN, a->regs + I2SO_CTRL);
	writel((readl(a->regs + I2SO_FMT) & ~(0x33 | 0xc00 | BIT(29))) | I2SO_FMT_LJ,
	       a->regs + I2SO_FMT);
	writel(readl(a->regs + I2SO_CTRL) | I2SO_CTRL_OUT, a->regs + I2SO_CTRL);
	writel((readl(a->regs + I2SO_CFG) & I2SO_CFG_KEEP) | I2SO_CFG_STEREO | I2SO_CFG_16BIT |
	       I2SO_CFG_RAW | I2SO_CFG_ALIGNED | bytes >> 4, a->regs + I2SO_CFG);
	writel(rt->dma_addr, a->regs + I2SO_BUF);
	writel(rt->period_size, a->regs + I2SO_PERIOD);
	writew(0, a->regs + I2SO_WR);
	return 0;
}

static int hc16xx_pcm_trigger(struct snd_pcm_substream *ss, int cmd)
{
	struct hc16xx_i2s *a = snd_pcm_substream_chip(ss);
	void __iomem *r = a->regs;

	guard(spinlock_irqsave)(&a->lock);
	switch (cmd) {
	case SNDRV_PCM_TRIGGER_START:
	case SNDRV_PCM_TRIGGER_RESUME:
		/* Odd write pointer: never reached, so the ring plays round. */
		writew(1, r + I2SO_WR);
		writel(I2SO_INT_STATUS | I2SO_INT_PERIOD, r + I2SO_INT);
		writel(readl(r + I2SO_CFG) & ~BIT(28), r + I2SO_CFG);
		hc16xx_apply_gain(a);
		writel(readl(r + I2SO_CTRL) | I2SO_CTRL_RUN, r + I2SO_CTRL);
		return 0;
	case SNDRV_PCM_TRIGGER_STOP:
	case SNDRV_PCM_TRIGGER_SUSPEND:
		writel(readl(r + I2SO_CTRL) & ~I2SO_CTRL_RUN, r + I2SO_CTRL);
		writel(I2SO_INT_STATUS, r + I2SO_INT);
		writew(0, r + I2SO_WR);
		return 0;
	}
	return -EINVAL;
}

static snd_pcm_uframes_t hc16xx_pcm_pointer(struct snd_pcm_substream *ss)
{
	struct hc16xx_i2s *a = snd_pcm_substream_chip(ss);
	unsigned int pos = readw(a->regs + I2SO_RD) << 4;

	pos %= snd_pcm_lib_buffer_bytes(ss);
	return bytes_to_frames(ss->runtime, pos);
}

static const struct snd_pcm_ops hc16xx_pcm_ops = {
	.open		= hc16xx_pcm_open,
	.close		= hc16xx_pcm_close,
	.prepare	= hc16xx_pcm_prepare,
	.trigger	= hc16xx_pcm_trigger,
	.pointer	= hc16xx_pcm_pointer,
};

static irqreturn_t hc16xx_i2s_irq(int irq, void *data)
{
	struct hc16xx_i2s *a = data;
	u32 st = readl(a->regs + I2SO_INT), st2 = readl(a->regs + I2SO_INT2);
	irqreturn_t ret = IRQ_NONE;
	struct snd_pcm_substream *ss = a->pcm->streams[SNDRV_PCM_STREAM_PLAYBACK].substream;

	/* Ack whatever is latched; enable bits written back unchanged. */
	if (st & I2SO_INT_STATUS) {
		writel(st, a->regs + I2SO_INT);
		ret = IRQ_HANDLED;
	}
	if (st2 & I2SO_INT2_STATUS) {
		writel(st2, a->regs + I2SO_INT2);
		ret = IRQ_HANDLED;
	}
	if (st & (I2SO_INT_PERIOD << 16) && ss && ss->runtime)
		snd_pcm_period_elapsed(ss);
	return ret;
}

/* ---- mixer ---- */

static int hc16xx_vol_info(struct snd_kcontrol *kc, struct snd_ctl_elem_info *ui)
{
	ui->type = SNDRV_CTL_ELEM_TYPE_INTEGER;
	ui->count = 1;
	ui->value.integer.min = 0;
	ui->value.integer.max = 255;
	return 0;
}

static int hc16xx_vol_get(struct snd_kcontrol *kc, struct snd_ctl_elem_value *uv)
{
	uv->value.integer.value[0] = ((struct hc16xx_i2s *)snd_kcontrol_chip(kc))->gain;
	return 0;
}

static int hc16xx_vol_put(struct snd_kcontrol *kc, struct snd_ctl_elem_value *uv)
{
	struct hc16xx_i2s *a = snd_kcontrol_chip(kc);
	long v = clamp_val(uv->value.integer.value[0], 0, 255);

	if (v == a->gain)
		return 0;
	a->gain = v;
	hc16xx_apply_gain(a);
	return 1;
}

static int hc16xx_switch_get(struct snd_kcontrol *kc, struct snd_ctl_elem_value *uv)
{
	uv->value.integer.value[0] = !((struct hc16xx_i2s *)snd_kcontrol_chip(kc))->mute;
	return 0;
}

static int hc16xx_switch_put(struct snd_kcontrol *kc, struct snd_ctl_elem_value *uv)
{
	struct hc16xx_i2s *a = snd_kcontrol_chip(kc);
	bool mute = !uv->value.integer.value[0];

	if (mute == a->mute)
		return 0;
	a->mute = mute;
	hc16xx_apply_gain(a);
	return 1;
}

static const char * const hc16xx_speaker_modes[] = { "Auto", "Off", "On" };

static int hc16xx_speaker_info(struct snd_kcontrol *kc, struct snd_ctl_elem_info *ui)
{
	return snd_ctl_enum_info(ui, 1, ARRAY_SIZE(hc16xx_speaker_modes), hc16xx_speaker_modes);
}

static int hc16xx_speaker_get(struct snd_kcontrol *kc, struct snd_ctl_elem_value *uv)
{
	uv->value.enumerated.item[0] = ((struct hc16xx_i2s *)snd_kcontrol_chip(kc))->speaker;
	return 0;
}

/* The firmware on the second core drives the amplifier (GPIO L19). */
static int hc16xx_speaker_send(unsigned int mode)
{
	u32 cfg[2] = { 8 /* speaker policy */, mode };

	return gb350_syscore_send(GB350_SYSCORE_CONFIG, cfg, sizeof(cfg), NULL);
}

static int hc16xx_speaker_put(struct snd_kcontrol *kc, struct snd_ctl_elem_value *uv)
{
	struct hc16xx_i2s *a = snd_kcontrol_chip(kc);
	unsigned int mode = uv->value.enumerated.item[0];
	int ret;

	if (mode >= ARRAY_SIZE(hc16xx_speaker_modes))
		return -EINVAL;
	if (mode == a->speaker)
		return 0;
	ret = hc16xx_speaker_send(mode);
	if (ret)
		return ret;
	a->speaker = mode;
	return 1;
}

static const struct snd_kcontrol_new hc16xx_controls[] = {
	{
		.iface	= SNDRV_CTL_ELEM_IFACE_MIXER,
		.name	= "Master Playback Volume",
		.info	= hc16xx_vol_info,
		.get	= hc16xx_vol_get,
		.put	= hc16xx_vol_put,
	}, {
		.iface	= SNDRV_CTL_ELEM_IFACE_MIXER,
		.name	= "Master Playback Switch",
		.info	= snd_ctl_boolean_mono_info,
		.get	= hc16xx_switch_get,
		.put	= hc16xx_switch_put,
	}, {
		.iface	= SNDRV_CTL_ELEM_IFACE_MIXER,
		.name	= "Speaker Mode",
		.info	= hc16xx_speaker_info,
		.get	= hc16xx_speaker_get,
		.put	= hc16xx_speaker_put,
	},
};

/* ---- probe ---- */

static void hc16xx_i2s_hw_init(struct hc16xx_i2s *a)
{
	/* Clocks on, block reset, EJTAG off the I2S pins. */
	sys_update(a, SYS_GATE, SYS_GATE_AUDIO, 0);
	sys_update(a, SYS_PAD_CLOCKS, GENMASK(26, 16), 0);
	sys_update(a, SYS_RESET, 0, BIT(5));
	usleep_range(1000, 2000);
	sys_update(a, SYS_RESET, BIT(5), 0);
	sys_update(a, SYS_EJTAG, 0, BIT(16));

	/* Idle: DMA stopped, interrupts off and clear, silent. */
	writel(readl(a->regs + I2SO_CTRL) & ~I2SO_CTRL_RUN, a->regs + I2SO_CTRL);
	writel(I2SO_INT_STATUS, a->regs + I2SO_INT);
	writel(I2SO_INT2_STATUS, a->regs + I2SO_INT2);
	writew(0, a->regs + I2SO_WR);
	writeb(FADE_SPEED, a->regs + I2SO_FADE);
	writeb(FADE_SPEED, a->regs + I2SO_FADE2);
	writeb(0, a->regs + I2SO_GAIN);
}

static int hc16xx_i2s_probe(struct platform_device *pdev)
{
	struct device *dev = &pdev->dev;
	struct resource *res;
	struct hc16xx_i2s *a;
	struct snd_card *card;
	struct snd_pcm *pcm;
	int irq, ret, i;

	ret = snd_devm_card_new(dev, SNDRV_DEFAULT_IDX1, "GB350", THIS_MODULE, sizeof(*a), &card);
	if (ret)
		return ret;
	a = card->private_data;
	a->dev = dev;
	a->card = card;
	a->gain = GAIN_DEFAULT;
	spin_lock_init(&a->lock);

	a->regs = devm_platform_ioremap_resource(pdev, 0);
	if (IS_ERR(a->regs))
		return PTR_ERR(a->regs);
	/* System control registers: shared with other drivers, not claimed. */
	res = platform_get_resource(pdev, IORESOURCE_MEM, 1);
	if (!res)
		return -EINVAL;
	a->sys = devm_ioremap(dev, res->start, resource_size(res));
	if (!a->sys)
		return -ENOMEM;
	irq = platform_get_irq(pdev, 0);
	if (irq < 0)
		return irq;

	ret = dma_set_mask_and_coherent(dev, DMA_BIT_MASK(32));
	if (ret)
		return ret;
	hc16xx_i2s_hw_init(a);
	ret = devm_request_irq(dev, irq, hc16xx_i2s_irq, 0, dev_name(dev), a);
	if (ret)
		return ret;

	strscpy(card->driver, "hc16xx-i2s");
	strscpy(card->shortname, "GB350");
	strscpy(card->longname, "GB350 I2S (HiChip HC16xx) + CS4344");
	ret = snd_pcm_new(card, "I2S", 0, 1, 0, &pcm);
	if (ret)
		return ret;
	pcm->private_data = a;
	a->pcm = pcm;
	strscpy(pcm->name, "GB350 I2S");
	snd_pcm_set_ops(pcm, SNDRV_PCM_STREAM_PLAYBACK, &hc16xx_pcm_ops);
	snd_pcm_set_managed_buffer_all(pcm, SNDRV_DMA_TYPE_DEV, dev, 64 * 1024,
				       hc16xx_pcm_hw.buffer_bytes_max);

	for (i = 0; i < ARRAY_SIZE(hc16xx_controls); i++) {
		ret = snd_ctl_add(card, snd_ctl_new1(&hc16xx_controls[i], a));
		if (ret)
			return ret;
	}
	ret = snd_card_register(card);
	if (ret)
		return ret;
	platform_set_drvdata(pdev, a);
	dev_info(dev, "I2S playback, gain %u/255\n", a->gain);
	return 0;
}

static const struct of_device_id hc16xx_i2s_of_match[] = {
	{ .compatible = "hichip,hc16xx-i2so" },
	{ }
};
MODULE_DEVICE_TABLE(of, hc16xx_i2s_of_match);

static struct platform_driver hc16xx_i2s_driver = {
	.probe	= hc16xx_i2s_probe,
	.driver	= {
		.name		= "hc16xx-i2s",
		.of_match_table	= hc16xx_i2s_of_match,
	},
};
module_platform_driver(hc16xx_i2s_driver);

MODULE_DESCRIPTION("HiChip HC16xx I2S output (GB350 audio)");
MODULE_LICENSE("GPL");
