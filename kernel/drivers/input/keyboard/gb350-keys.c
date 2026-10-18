// SPDX-License-Identifier: GPL-2.0
/*
 * GB350 button shift register.
 *
 * Twelve buttons (d-pad, ABXY, L1/R1, Select/Start) sit behind a 16-bit
 * parallel-in shift register read over two GPIOs. The data line doubles as
 * the load input. Protocol (stock cubevol joystick_key_task):
 *   1. drive DATA low as an output for ~4 us (load),
 *   2. release DATA to input, ~2 us, read bit 0,
 *   3. 15x: CLK low, ~2 us, CLK high, read the next bit from DATA.
 * Buttons read 0 while pressed. CLK idles high.
 *
 * Timing matters: the stock code asks for usleep(4)/usleep(2), which with
 * the default 50 us timer slack really gives ~60 us pulses, and the chip
 * only answers at about that speed - both true 2-4 us pulses and ~10 ms
 * ones read all ones (tested 2026-10-16). So every step sleeps ~60 us
 * (needs CONFIG_HIGH_RES_TIMERS; the scan runs from the polling work).
 *
 * DT: clk-gpios, data-gpios, linux,keycodes (one per bit, from bit 0;
 * KEY_RESERVED = unused), optional poll-interval (ms).
 */
#include <linux/delay.h>
#include <linux/gpio/consumer.h>
#include <linux/input.h>
#include <linux/module.h>
#include <linux/platform_device.h>
#include <linux/property.h>

#define GB350_KEYS_BITS		16
#define GB350_KEYS_STEP_US	55

struct gb350_keys {
	struct input_dev *input;
	struct gpio_desc *clk;
	struct gpio_desc *data;
	u32 keycodes[GB350_KEYS_BITS];
	u16 last;
};

static u16 gb350_keys_scan(struct gb350_keys *k)
{
	u16 bits;
	int i;

	gpiod_direction_output(k->data, 0);
	usleep_range(GB350_KEYS_STEP_US, GB350_KEYS_STEP_US + 20);
	gpiod_direction_input(k->data);
	usleep_range(GB350_KEYS_STEP_US, GB350_KEYS_STEP_US + 20);
	bits = gpiod_get_value(k->data);
	for (i = 1; i < GB350_KEYS_BITS; i++) {
		gpiod_set_value(k->clk, 0);
		usleep_range(GB350_KEYS_STEP_US, GB350_KEYS_STEP_US + 20);
		gpiod_set_value(k->clk, 1);
		bits |= gpiod_get_value(k->data) << i;
	}
	return ~bits;	/* 1 = pressed */
}

static void gb350_keys_poll(struct input_dev *input)
{
	struct gb350_keys *k = input_get_drvdata(input);
	u16 now = gb350_keys_scan(k), changed = now ^ k->last;
	int i;

	if (!changed)
		return;
	for (i = 0; i < GB350_KEYS_BITS; i++)
		if (changed & BIT(i) && k->keycodes[i] != KEY_RESERVED)
			input_report_key(input, k->keycodes[i], now & BIT(i));
	input_sync(input);
	k->last = now;
}

static int gb350_keys_probe(struct platform_device *pdev)
{
	struct device *dev = &pdev->dev;
	struct gb350_keys *k;
	u32 poll_ms = 10;
	int n, i, ret;

	k = devm_kzalloc(dev, sizeof(*k), GFP_KERNEL);
	if (!k)
		return -ENOMEM;

	k->clk = devm_gpiod_get(dev, "clk", GPIOD_OUT_HIGH);
	if (IS_ERR(k->clk))
		return dev_err_probe(dev, PTR_ERR(k->clk), "clk gpio\n");
	k->data = devm_gpiod_get(dev, "data", GPIOD_IN);
	if (IS_ERR(k->data))
		return dev_err_probe(dev, PTR_ERR(k->data), "data gpio\n");

	n = device_property_count_u32(dev, "linux,keycodes");
	if (n <= 0 || n > GB350_KEYS_BITS)
		return dev_err_probe(dev, -EINVAL, "linux,keycodes: need 1-%d entries\n",
				     GB350_KEYS_BITS);
	ret = device_property_read_u32_array(dev, "linux,keycodes", k->keycodes, n);
	if (ret)
		return ret;
	device_property_read_u32(dev, "poll-interval", &poll_ms);

	k->input = devm_input_allocate_device(dev);
	if (!k->input)
		return -ENOMEM;
	k->input->name = "gb350-buttons";
	k->input->phys = "gb350-buttons/input0";
	k->input->id.bustype = BUS_HOST;
	input_set_drvdata(k->input, k);
	for (i = 0; i < n; i++)
		if (k->keycodes[i] != KEY_RESERVED)
			input_set_capability(k->input, EV_KEY, k->keycodes[i]);

	ret = input_setup_polling(k->input, gb350_keys_poll);
	if (ret)
		return ret;
	input_set_poll_interval(k->input, poll_ms);
	return input_register_device(k->input);
}

static const struct of_device_id gb350_keys_of_match[] = {
	{ .compatible = "hichip,gb350-keys" },
	{ }
};
MODULE_DEVICE_TABLE(of, gb350_keys_of_match);

static struct platform_driver gb350_keys_driver = {
	.probe	= gb350_keys_probe,
	.driver	= {
		.name		= "gb350-keys",
		.of_match_table	= gb350_keys_of_match,
	},
};
module_platform_driver(gb350_keys_driver);

MODULE_DESCRIPTION("GB350 button shift register");
MODULE_LICENSE("GPL");
