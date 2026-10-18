// SPDX-License-Identifier: GPL-2.0
/*
 * HiChip HC16xx SAR ADC ("check ADC").
 *
 * 8-bit SAR ADC at 0x18818400 sampling continuously; the NOR bootloader
 * already configures it (channels 1 and 5 enabled, as the vendor 4.4
 * hc_checkadc driver would), so this driver only reads it:
 *   +0x8c bit (16 + ch)   channel enable
 *   +0xd0 + ch            latest sample, ch 0-3; ch 4/5 at +0xd6/+0xd7
 * Channel 0 is the calibration reference (DT adjust_channel = 0): the vendor
 * reports sample * 241 / ref, capped at 255 (hc_checkadc_read), and stock
 * cubevol converts that to mV at the pin with * 1890 / 241. Here in_voltageN
 * raw is the calibrated value and scale is 1890/241 mV.
 *
 * GB350: channel 1 = battery level, channel 5 = charge status.
 */
#include <linux/iio/iio.h>
#include <linux/io.h>
#include <linux/module.h>
#include <linux/platform_device.h>
#include <linux/property.h>

#define HC_ADC_CH_ENABLE	0x8c
#define HC_ADC_SAMPLE		0xd0
#define HC_ADC_REF_CH		0

struct hc16xx_adc {
	void __iomem *base;
};

static u8 hc16xx_adc_sample(struct hc16xx_adc *adc, unsigned int ch)
{
	return readb(adc->base + HC_ADC_SAMPLE + ch + (ch > 3 ? 2 : 0));
}

static int hc16xx_adc_read_raw(struct iio_dev *indio, struct iio_chan_spec const *chan,
			       int *val, int *val2, long mask)
{
	struct hc16xx_adc *adc = iio_priv(indio);
	unsigned int ref;

	switch (mask) {
	case IIO_CHAN_INFO_RAW:
		if (!(readl(adc->base + HC_ADC_CH_ENABLE) & BIT(16 + chan->channel)))
			return -ENODATA;
		ref = hc16xx_adc_sample(adc, HC_ADC_REF_CH);
		if (!ref)
			return -EIO;
		*val = min(hc16xx_adc_sample(adc, chan->channel) * 241U / ref, 255U);
		return IIO_VAL_INT;
	case IIO_CHAN_INFO_SCALE:
		*val = 1890;
		*val2 = 241;
		return IIO_VAL_FRACTIONAL;
	default:
		return -EINVAL;
	}
}

/* DT io-channels name the hardware channel number, not an array index. */
static int hc16xx_adc_fwnode_xlate(struct iio_dev *indio,
				   const struct fwnode_reference_args *spec)
{
	int i;

	for (i = 0; i < indio->num_channels; i++)
		if (indio->channels[i].channel == spec->args[0])
			return i;
	return -EINVAL;
}

static const struct iio_info hc16xx_adc_info = {
	.read_raw = hc16xx_adc_read_raw,
	.fwnode_xlate = hc16xx_adc_fwnode_xlate,
};

#define HC16XX_ADC_CHAN(n) {						\
	.type = IIO_VOLTAGE,						\
	.indexed = 1,							\
	.channel = (n),							\
	.info_mask_separate = BIT(IIO_CHAN_INFO_RAW),			\
	.info_mask_shared_by_type = BIT(IIO_CHAN_INFO_SCALE),		\
}

static const struct iio_chan_spec hc16xx_adc_channels[] = {
	HC16XX_ADC_CHAN(1), HC16XX_ADC_CHAN(2), HC16XX_ADC_CHAN(3),
	HC16XX_ADC_CHAN(4), HC16XX_ADC_CHAN(5),
};

static int hc16xx_adc_probe(struct platform_device *pdev)
{
	struct iio_dev *indio;
	struct hc16xx_adc *adc;

	indio = devm_iio_device_alloc(&pdev->dev, sizeof(*adc));
	if (!indio)
		return -ENOMEM;
	adc = iio_priv(indio);
	adc->base = devm_platform_ioremap_resource(pdev, 0);
	if (IS_ERR(adc->base))
		return PTR_ERR(adc->base);

	indio->name = "hc16xx-adc";
	indio->info = &hc16xx_adc_info;
	indio->modes = INDIO_DIRECT_MODE;
	indio->channels = hc16xx_adc_channels;
	indio->num_channels = ARRAY_SIZE(hc16xx_adc_channels);
	return devm_iio_device_register(&pdev->dev, indio);
}

static const struct of_device_id hc16xx_adc_of_match[] = {
	{ .compatible = "hichip,hc16xx-adc" },
	{ }
};
MODULE_DEVICE_TABLE(of, hc16xx_adc_of_match);

static struct platform_driver hc16xx_adc_driver = {
	.probe	= hc16xx_adc_probe,
	.driver	= {
		.name		= "hc16xx-adc",
		.of_match_table	= hc16xx_adc_of_match,
	},
};
module_platform_driver(hc16xx_adc_driver);

MODULE_DESCRIPTION("HiChip HC16xx SAR ADC");
MODULE_LICENSE("GPL");
