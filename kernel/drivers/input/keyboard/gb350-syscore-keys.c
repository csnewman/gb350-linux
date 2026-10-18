// SPDX-License-Identifier: GPL-2.0
/*
 * GB350 buttons and switches, scanned by the gb350-syscore firmware on the second core
 * (avp/syscore, src/input.rs) and delivered as GB350_SYSCORE_INPUT messages
 * through the gb350-syscore link: a 64-bit state, 1 = pressed/active, bits
 * 0-15 the button shift register, bit 32 + n GPIO L<n>. The firmware
 * debounces and only reports changes (plus its initial state); the driver
 * asks for the current state at probe.
 *
 * DT: one child node per input: reg = state bit, linux,code, optional
 * linux,input-type (default EV_KEY) and label.
 */
#include <linux/input.h>
#include <linux/module.h>
#include <linux/platform_device.h>
#include <linux/property.h>
#include <linux/soc/hichip/gb350-syscore.h>
#include <linux/unaligned.h>

struct gb350_sc_key {
	u32 bit, type, code;
};

struct gb350_sc_keys {
	struct input_dev *input;
	struct notifier_block nb;
	struct gb350_sc_key *keys;
	unsigned int nkeys;
	u64 state;
};

static int gb350_sc_keys_event(struct notifier_block *nb, unsigned long kind, void *data)
{
	struct gb350_sc_keys *k = container_of(nb, struct gb350_sc_keys, nb);
	const struct gb350_syscore_msg *msg = data;
	u64 state, changed;
	unsigned int i;

	if (kind != GB350_SYSCORE_INPUT || msg->len < 8)
		return NOTIFY_DONE;
	state = get_unaligned_le32(msg->data) | (u64)get_unaligned_le32(msg->data + 4) << 32;
	changed = state ^ k->state;
	if (!changed)
		return NOTIFY_OK;
	for (i = 0; i < k->nkeys; i++)
		if (changed & BIT_ULL(k->keys[i].bit))
			input_event(k->input, k->keys[i].type, k->keys[i].code,
				    !!(state & BIT_ULL(k->keys[i].bit)));
	input_sync(k->input);
	k->state = state;
	return NOTIFY_OK;
}

static void gb350_sc_keys_unregister(void *data)
{
	gb350_syscore_unregister_notifier(data);
}

static int gb350_sc_keys_probe(struct platform_device *pdev)
{
	struct device *dev = &pdev->dev;
	struct gb350_sc_keys *k;
	unsigned int i = 0;
	int ret;

	if (!gb350_syscore_present())
		return -EPROBE_DEFER;
	k = devm_kzalloc(dev, sizeof(*k), GFP_KERNEL);
	if (!k)
		return -ENOMEM;
	k->nkeys = device_get_child_node_count(dev);
	if (!k->nkeys)
		return dev_err_probe(dev, -EINVAL, "no keys\n");
	k->keys = devm_kcalloc(dev, k->nkeys, sizeof(*k->keys), GFP_KERNEL);
	k->input = devm_input_allocate_device(dev);
	if (!k->keys || !k->input)
		return -ENOMEM;

	device_for_each_child_node_scoped(dev, child) {
		struct gb350_sc_key *key = &k->keys[i++];

		key->type = EV_KEY;
		fwnode_property_read_u32(child, "linux,input-type", &key->type);
		if (fwnode_property_read_u32(child, "reg", &key->bit) ||
		    key->bit >= GB350_SYSCORE_INPUT_BITS ||
		    fwnode_property_read_u32(child, "linux,code", &key->code))
			return dev_err_probe(dev, -EINVAL, "%pfwP: need reg < %d and linux,code\n",
					     child, GB350_SYSCORE_INPUT_BITS);
		input_set_capability(k->input, key->type, key->code);
	}

	k->input->name = "gb350-buttons";
	k->input->phys = "gb350-syscore/input0";
	k->input->id.bustype = BUS_HOST;
	ret = input_register_device(k->input);
	if (ret)
		return ret;

	k->nb.notifier_call = gb350_sc_keys_event;
	ret = gb350_syscore_register_notifier(&k->nb);
	if (ret)
		return ret;
	ret = devm_add_action_or_reset(dev, gb350_sc_keys_unregister, &k->nb);
	if (ret)
		return ret;
	/* The reply arrives as an INPUT message like any change. */
	ret = gb350_syscore_send(GB350_SYSCORE_GET_INPUT, NULL, 0, NULL);
	if (ret)
		dev_warn(dev, "no state from the second core yet (%d)\n", ret);
	return 0;
}

static const struct of_device_id gb350_sc_keys_of_match[] = {
	{ .compatible = "hichip,gb350-syscore-keys" },
	{ }
};
MODULE_DEVICE_TABLE(of, gb350_sc_keys_of_match);

static struct platform_driver gb350_sc_keys_driver = {
	.probe	= gb350_sc_keys_probe,
	.driver	= {
		.name		= "gb350-syscore-keys",
		.of_match_table	= gb350_sc_keys_of_match,
	},
};
module_platform_driver(gb350_sc_keys_driver);

MODULE_DESCRIPTION("GB350 buttons via the second core");
MODULE_LICENSE("GPL");
