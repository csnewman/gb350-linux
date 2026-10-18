// SPDX-License-Identifier: GPL-2.0
/*
 * GB350 battery: level and charge status from two ADC channels.
 *
 * io-channels "battery" (DT /panel adc-bat-level) and "charger"
 * (adc-bat-charging), both in mV at the ADC pin. The battery is behind an
 * unknown divider, so there is no voltage_now; the level follows the stock
 * cubevol battery_monitor defaults (DT bat-voltage-threshold-* absent):
 * bars step at 1070/1140/1210/1280 mV, low warning below 1020 mV, charging
 * while the charge pin is above 400 mV. capacity is a linear estimate over
 * 1020-1300 mV (a fresh full battery reads ~1320-1380 mV).
 */
#include <linux/iio/consumer.h>
#include <linux/module.h>
#include <linux/platform_device.h>
#include <linux/power_supply.h>
#include <linux/property.h>
#include <linux/workqueue.h>

#define GB350_BAT_EMPTY_MV	1020
#define GB350_BAT_FULL_MV	1300
#define GB350_CHARGING_MV	400
#define GB350_POLL		(10 * HZ)

struct gb350_battery {
	struct iio_channel *battery;
	struct iio_channel *charger;
	struct power_supply *psy;
	struct delayed_work work;
	int last_status;
	int last_level;
};

static int gb350_read_mv(struct iio_channel *ch)
{
	int mv, ret;

	ret = iio_read_channel_processed(ch, &mv);
	return ret < 0 ? ret : mv;
}

static int gb350_status(struct gb350_battery *b)
{
	int mv = gb350_read_mv(b->charger);

	if (mv < 0)
		return POWER_SUPPLY_STATUS_UNKNOWN;
	return mv > GB350_CHARGING_MV ? POWER_SUPPLY_STATUS_CHARGING
				      : POWER_SUPPLY_STATUS_DISCHARGING;
}

static int gb350_level(int mv)
{
	if (mv < GB350_BAT_EMPTY_MV)
		return POWER_SUPPLY_CAPACITY_LEVEL_CRITICAL;
	if (mv < 1070)
		return POWER_SUPPLY_CAPACITY_LEVEL_LOW;
	if (mv >= 1280)
		return POWER_SUPPLY_CAPACITY_LEVEL_FULL;
	if (mv >= 1210)
		return POWER_SUPPLY_CAPACITY_LEVEL_HIGH;
	return POWER_SUPPLY_CAPACITY_LEVEL_NORMAL;
}

static int gb350_get_property(struct power_supply *psy, enum power_supply_property psp,
			      union power_supply_propval *val)
{
	struct gb350_battery *b = power_supply_get_drvdata(psy);
	int mv;

	switch (psp) {
	case POWER_SUPPLY_PROP_STATUS:
		val->intval = gb350_status(b);
		return 0;
	case POWER_SUPPLY_PROP_PRESENT:
		val->intval = 1;
		return 0;
	case POWER_SUPPLY_PROP_TECHNOLOGY:
		val->intval = POWER_SUPPLY_TECHNOLOGY_LION;
		return 0;
	case POWER_SUPPLY_PROP_CAPACITY:
	case POWER_SUPPLY_PROP_CAPACITY_LEVEL:
		mv = gb350_read_mv(b->battery);
		if (mv < 0)
			return mv;
		if (psp == POWER_SUPPLY_PROP_CAPACITY_LEVEL)
			val->intval = gb350_level(mv);
		else
			val->intval = clamp((mv - GB350_BAT_EMPTY_MV) * 100 /
					    (GB350_BAT_FULL_MV - GB350_BAT_EMPTY_MV), 0, 100);
		return 0;
	default:
		return -EINVAL;
	}
}

static const enum power_supply_property gb350_props[] = {
	POWER_SUPPLY_PROP_STATUS,
	POWER_SUPPLY_PROP_PRESENT,
	POWER_SUPPLY_PROP_TECHNOLOGY,
	POWER_SUPPLY_PROP_CAPACITY,
	POWER_SUPPLY_PROP_CAPACITY_LEVEL,
};

static const struct power_supply_desc gb350_battery_desc = {
	.name		= "battery",
	.type		= POWER_SUPPLY_TYPE_BATTERY,
	.properties	= gb350_props,
	.num_properties	= ARRAY_SIZE(gb350_props),
	.get_property	= gb350_get_property,
};

/* Nothing interrupts on a change, so poll and notify userspace. */
static void gb350_battery_work(struct work_struct *work)
{
	struct gb350_battery *b = container_of(work, struct gb350_battery, work.work);
	int status = gb350_status(b), mv = gb350_read_mv(b->battery);
	int level = mv < 0 ? -1 : gb350_level(mv);

	if (status != b->last_status || level != b->last_level) {
		b->last_status = status;
		b->last_level = level;
		power_supply_changed(b->psy);
	}
	schedule_delayed_work(&b->work, GB350_POLL);
}

static void gb350_battery_stop(void *data)
{
	struct gb350_battery *b = data;

	cancel_delayed_work_sync(&b->work);
}

static int gb350_battery_probe(struct platform_device *pdev)
{
	struct device *dev = &pdev->dev;
	struct power_supply_config cfg = {};
	struct gb350_battery *b;

	b = devm_kzalloc(dev, sizeof(*b), GFP_KERNEL);
	if (!b)
		return -ENOMEM;
	b->battery = devm_iio_channel_get(dev, "battery");
	if (IS_ERR(b->battery))
		return dev_err_probe(dev, PTR_ERR(b->battery), "battery channel\n");
	b->charger = devm_iio_channel_get(dev, "charger");
	if (IS_ERR(b->charger))
		return dev_err_probe(dev, PTR_ERR(b->charger), "charger channel\n");

	cfg.drv_data = b;
	cfg.fwnode = dev_fwnode(dev);
	b->psy = devm_power_supply_register(dev, &gb350_battery_desc, &cfg);
	if (IS_ERR(b->psy))
		return dev_err_probe(dev, PTR_ERR(b->psy), "register\n");

	b->last_status = -1;
	INIT_DELAYED_WORK(&b->work, gb350_battery_work);
	schedule_delayed_work(&b->work, 0);
	return devm_add_action_or_reset(dev, gb350_battery_stop, b);
}

static const struct of_device_id gb350_battery_of_match[] = {
	{ .compatible = "hichip,gb350-battery" },
	{ }
};
MODULE_DEVICE_TABLE(of, gb350_battery_of_match);

static struct platform_driver gb350_battery_driver = {
	.probe	= gb350_battery_probe,
	.driver	= {
		.name		= "gb350-battery",
		.of_match_table	= gb350_battery_of_match,
	},
};
module_platform_driver(gb350_battery_driver);

MODULE_DESCRIPTION("GB350 battery (ADC level and charge status)");
MODULE_LICENSE("GPL");
