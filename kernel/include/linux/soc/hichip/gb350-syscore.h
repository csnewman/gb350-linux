/* SPDX-License-Identifier: GPL-2.0 */
/*
 * GB350 second-core ("syscore") link: messages to and from the gb350-syscore firmware on
 * the second CPU (avp/syscore). drivers/soc/hichip/gb350-syscore.c
 */
#ifndef __LINUX_SOC_HICHIP_GB350_SYSCORE_H
#define __LINUX_SOC_HICHIP_GB350_SYSCORE_H

#include <linux/notifier.h>
#include <linux/types.h>

#define GB350_SYSCORE_MSG_DATA	56

struct gb350_syscore_msg {
	u16 kind;
	u16 len;
	u32 seq;
	u8 data[GB350_SYSCORE_MSG_DATA];
};

/* Message kinds (firmware: avp/syscore/src/main.rs, mod kind). */
enum {
	GB350_SYSCORE_PING = 1,		/* Linux -> core, payload echoed */
	GB350_SYSCORE_PONG = 2,		/* core -> Linux */
	GB350_SYSCORE_GET_INFO = 3,	/* Linux -> core */
	GB350_SYSCORE_INFO = 4,		/* core -> Linux */
	GB350_SYSCORE_RESTART = 5,	/* Linux -> core: park for a reload */
	GB350_SYSCORE_INPUT = 6,	/* core -> Linux: le32 state lo, hi, tick */
	GB350_SYSCORE_GET_INPUT = 7,	/* Linux -> core: INPUT reply */
	GB350_SYSCORE_POWER_OFF = 8,	/* Linux -> core: drop the power latch */
	GB350_SYSCORE_POWER_REQUEST = 9, /* core -> Linux: le32 reason, grace ms */
	GB350_SYSCORE_CONFIG = 10,	/* Linux -> core: key, value (~0 = read) */
	GB350_SYSCORE_CORELINK = 11,	/* Linux -> core: corelink op (attach, detach) */
};

/*
 * Input state (GB350_SYSCORE_INPUT), 1 = pressed/active: bits 0-15 the
 * button shift register, bit 32 + n GPIO L<n> (direct buttons, switches).
 */
#define GB350_SYSCORE_INPUT_BITS	64

bool gb350_syscore_present(void);

/* Queues a message for the core; -ENODEV if no firmware is running. */
int gb350_syscore_send(u16 kind, const void *data, u16 len, u32 *seq_out);

/*
 * Incoming messages that are not replies to the driver's own requests are
 * passed to these notifiers: action = kind, data = struct gb350_syscore_msg *.
 */
/* Sends a request and waits (500 ms) for the reply with its sequence number. */
int gb350_syscore_call(u16 kind, const void *data, u16 len, struct gb350_syscore_msg *reply);

/* Rings the core's doorbell (it then looks at its rings and corelink). */
int gb350_syscore_kick(void);

/*
 * Called (from the doorbell's interrupt thread) on every doorbell from the
 * core: corelink wakes its fence waiters. One hook; NULL removes it.
 */
void gb350_syscore_set_wake_hook(void (*hook)(void));

int gb350_syscore_register_notifier(struct notifier_block *nb);
void gb350_syscore_unregister_notifier(struct notifier_block *nb);

#endif
