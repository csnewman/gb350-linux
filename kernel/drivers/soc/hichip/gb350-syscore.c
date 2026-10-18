// SPDX-License-Identifier: GPL-2.0
/*
 * GB350 second core ("syscore"): Linux side of the link to the gb350-syscore firmware on
 * the SoC's second CPU (a MIPS 24KE, not cache-coherent with Linux's 74Kc).
 * Firmware: avp/syscore (Rust), booted by the NOR bootloader as
 * cubegm/avp.uImage or (re)loaded here at runtime.
 *
 * Shared memory (physical 0x0bd00000, 64 KiB, uncached on both sides; layout
 * in avp/syscore/src/shm.rs): header, a Linux->core and a core->Linux ring
 * of 64-byte messages, and a log ring printed here as "syscore: ...".
 *
 * Doorbells (mailbox): channel 0 = Linux -> core (Linux sets bit 0
 * of its block +0x00; busy until the core releases it), channel 4 = core ->
 * Linux (Linux's block +0x08 bit 8 pending, raises hwirq 66 with +0x08 bit 0
 * set; acked by writing bit 8 back, released via +0x04 bit 0).
 *
 * Linux's sending block's +0x08 bit 1 must be set (0x10002, as the vendor) for
 * doorbells to reach the core; it also enables release interrupts, acked
 * here (bit 16).
 *
 * Reload: a running firmware is asked to park (RESTART message) - it then
 * spins in a few instructions it copied into shared memory, outside the
 * image, until the driver writes the new entry to H_PARK_GO; it parks the same way
 * after a crash. Fallback (probed 2026-10-16): 0x18800220 bit 1 holds the
 * second core in reset while clear; set, it starts at the trampoline in
 * 0x18800280..28c - but this did not seem to work on a core sleeping in
 * `wait`.
 *
 * Phase 2 (firmware 0.2): the core scans the buttons (input driver
 * gb350-syscore-keys), supervises power (power-button hold, card removal,
 * critical battery: POWER_REQUEST -> orderly_poweroff() here, the core cuts
 * power itself after a grace period) and drops the power latch for Linux
 * (power-off handler below). GPIO registers are shared with the core: the
 * hwspinlocks registered here (Peterson's algorithm in shared memory, with
 * the firmware's shm::lock) guard their read-modify-writes.
 *
 * sysfs: state, info, ping (write a count, read round-trip times),
 * firmware (write a name: request_firmware() it and restart the core),
 * call (write "kind word...", read the reply: for experiments).
 * In-kernel: gb350_syscore_send() and a notifier for incoming messages.
 */
#include <linux/completion.h>
#include <linux/delay.h>
#include <linux/firmware.h>
#include <linux/hwspinlock.h>
#include <linux/interrupt.h>
#include <linux/io.h>
#include <linux/math64.h>
#include <linux/module.h>
#include <linux/notifier.h>
#include <linux/of.h>
#include <linux/platform_device.h>
#include <linux/reboot.h>
#include <linux/slab.h>
#include <linux/soc/hichip/gb350-syscore.h>
#include <linux/unaligned.h>
#include <linux/workqueue.h>

#include "../../hwspinlock/hwspinlock_internal.h"	/* provider API */

/* Shared memory */
#define SC_MAGIC		0x43535953	/* "SYSC" */
#define SC_ABI			1
#define H_MAGIC			0x00
#define H_ABI			0x04
#define H_VERSION		0x08
#define H_STATE			0x0c
#define H_TICKS			0x10
#define H_CRASH			0x18
#define H_INFO			0x30
#define H_COUNT_PER_MS		0x40
#define H_EXCEPTIONS		0x44
#define H_LAST_CAUSE		0x48
#define H_LAST_STATUS		0x4c
#define H_LOOPS			0x50
#define H_PARK_GO		0x60
#define H_PARKED		0x64
#define H_BUILD			0x80
#define H_INPUT			0xc0
#define H_SCANS			0xc8
#define H_SCAN_US		0xcc
#define H_BATTERY_MV		0xd0
#define H_CHARGER_MV		0xd4
#define H_CARD			0xd8
#define H_SUPERVISOR		0xdc
#define H_SPEAKER		0xe0
#define H_IDLE			0xe4	/* firmware >= 0.2.3: idle count cycles */
#define H_CYCLES		0xe8	/* count when H_IDLE was written */
#define STATE_RUNNING		1
#define STATE_CRASHED		0xdead
#define RING_SLOTS		64
#define SLOT_SIZE		64
#define TO_CORE_HEAD		0x0100
#define TO_CORE_TAIL		0x0140
#define TO_CORE_SLOTS		0x0200
#define TO_LINUX_HEAD		0x1200
#define TO_LINUX_TAIL		0x1240
#define TO_LINUX_SLOTS		0x1300
#define LOG_WR			0x4000
#define LOG_RD			0x4004
#define LOG_DATA		0x4100
#define LOG_SIZE		0x4000
#define LOCKS			0x3f00	/* 16 bytes each: Linux flag, core flag, turn */
#define NUM_LOCKS		4
#define TURN_CORE		1

/* Mailbox: Linux's blocks, 0x20 per channel */
#define DB_OUT			0	/* Linux -> core */
#define DB_IN			4	/* core -> Linux */
#define MB(ch, off)		((ch) * 0x20 + (off))

/* System control */
#define SYS_RUN			0x220	/* bit 1: second core out of reset */
#define SYS_TRAMPOLINE		0x280
#define SYS_HANDSHAKE		0x06b

struct syscore {
	struct device *dev;
	void __iomem *shm, *fw, *mbox, *sys, *cmbox;	/* cmbox: the core's blocks */
	resource_size_t fw_phys, fw_size;
	spinlock_t tx_lock;
	struct mutex req_lock;		/* one synchronous request at a time */
	struct work_struct rx_work;
	struct mutex rx_lock;		/* the interrupt thread or the work */
	struct delayed_work poll_work;
	u32 seq;
	u32 wait_seq;
	struct gb350_syscore_msg reply;
	struct completion reply_done;
	char log_line[200];
	unsigned int log_len;
	bool crash_reported;
	s64 rtt_ns[3];			/* last ping run: min, avg, max */
	unsigned int rtt_count;
	struct gb350_syscore_msg call_reply;	/* sysfs call */
	int call_ret;
	struct hwspinlock_device *locks;	/* NUM_LOCKS entries follow */
};

static struct syscore *the_syscore;
static BLOCKING_NOTIFIER_HEAD(syscore_notifier);
static void (*wake_hook)(void);

static u32 shm_rd(struct syscore *sc, u32 off)
{
	return readl(sc->shm + off);
}

static void shm_wr(struct syscore *sc, u32 off, u32 v)
{
	writel(v, sc->shm + off);
}

static bool syscore_running(struct syscore *sc)
{
	return shm_rd(sc, H_MAGIC) == SC_MAGIC && shm_rd(sc, H_ABI) == SC_ABI &&
	       shm_rd(sc, H_STATE) == STATE_RUNNING;
}

int gb350_syscore_send(u16 kind, const void *data, u16 len, u32 *seq_out)
{
	struct syscore *sc = the_syscore;
	unsigned long flags;
	u32 head, slot, seq, w[SLOT_SIZE / 4] = {};
	int i;

	if (!sc || !syscore_running(sc))
		return -ENODEV;
	if (len > GB350_SYSCORE_MSG_DATA)
		return -EINVAL;

	spin_lock_irqsave(&sc->tx_lock, flags);
	head = shm_rd(sc, TO_CORE_HEAD);
	/* (gb350_syscore_kick rings without a message: same lock.) */
	if (head - shm_rd(sc, TO_CORE_TAIL) >= RING_SLOTS) {
		spin_unlock_irqrestore(&sc->tx_lock, flags);
		return -EBUSY;
	}
	seq = ++sc->seq;
	w[0] = kind | (u32)len << 16;
	w[1] = seq;
	memcpy(&w[2], data, len);
	slot = TO_CORE_SLOTS + (head % RING_SLOTS) * SLOT_SIZE;
	for (i = 0; i < SLOT_SIZE / 4; i++)
		writel(w[i], sc->shm + slot + 4 * i);
	wmb();
	shm_wr(sc, TO_CORE_HEAD, head + 1);
	/* Ring unless the core has not yet taken the previous doorbell. */
	if (!(readl(sc->mbox + MB(DB_OUT, 0)) & 1))
		writel(readl(sc->mbox + MB(DB_OUT, 0)) | 1, sc->mbox + MB(DB_OUT, 0));
	spin_unlock_irqrestore(&sc->tx_lock, flags);
	if (seq_out)
		*seq_out = seq;
	return 0;
}
EXPORT_SYMBOL_GPL(gb350_syscore_send);

int gb350_syscore_kick(void)
{
	struct syscore *sc = the_syscore;
	unsigned long flags;

	if (!sc || !syscore_running(sc))
		return -ENODEV;
	spin_lock_irqsave(&sc->tx_lock, flags);
	if (!(readl(sc->mbox + MB(DB_OUT, 0)) & 1))
		writel(readl(sc->mbox + MB(DB_OUT, 0)) | 1, sc->mbox + MB(DB_OUT, 0));
	spin_unlock_irqrestore(&sc->tx_lock, flags);
	return 0;
}
EXPORT_SYMBOL_GPL(gb350_syscore_kick);

void gb350_syscore_set_wake_hook(void (*hook)(void))
{
	WRITE_ONCE(wake_hook, hook);
}
EXPORT_SYMBOL_GPL(gb350_syscore_set_wake_hook);

/* False until the link driver has probed: clients defer until then. */
bool gb350_syscore_present(void)
{
	return READ_ONCE(the_syscore);
}
EXPORT_SYMBOL_GPL(gb350_syscore_present);

int gb350_syscore_register_notifier(struct notifier_block *nb)
{
	return blocking_notifier_chain_register(&syscore_notifier, nb);
}
EXPORT_SYMBOL_GPL(gb350_syscore_register_notifier);

void gb350_syscore_unregister_notifier(struct notifier_block *nb)
{
	blocking_notifier_chain_unregister(&syscore_notifier, nb);
}
EXPORT_SYMBOL_GPL(gb350_syscore_unregister_notifier);

/* Sends a request and waits for the reply with the same sequence number. */
static int syscore_request(struct syscore *sc, u16 kind, const void *data, u16 len,
			   struct gb350_syscore_msg *reply)
{
	u32 seq;
	int ret;

	mutex_lock(&sc->req_lock);
	reinit_completion(&sc->reply_done);
	WRITE_ONCE(sc->wait_seq, 0);
	ret = gb350_syscore_send(kind, data, len, &seq);
	if (!ret) {
		WRITE_ONCE(sc->wait_seq, seq);
		/* A reply may already be in: the rx work checks wait_seq. */
		schedule_work(&sc->rx_work);
		if (!wait_for_completion_timeout(&sc->reply_done, msecs_to_jiffies(500)))
			ret = -ETIMEDOUT;
		else
			*reply = sc->reply;
	}
	WRITE_ONCE(sc->wait_seq, 0);
	mutex_unlock(&sc->req_lock);
	return ret;
}

int gb350_syscore_call(u16 kind, const void *data, u16 len, struct gb350_syscore_msg *reply)
{
	if (!the_syscore)
		return -ENODEV;
	return syscore_request(the_syscore, kind, data, len, reply);
}
EXPORT_SYMBOL_GPL(gb350_syscore_call);

static void syscore_drain_log(struct syscore *sc)
{
	u32 rd = shm_rd(sc, LOG_RD), wr = shm_rd(sc, LOG_WR);

	if (wr - rd > LOG_SIZE)
		rd = wr - LOG_SIZE;	/* should not happen; resync */
	while (rd != wr) {
		char c = readb(sc->shm + LOG_DATA + rd % LOG_SIZE);

		rd++;
		if (c == '\n' || sc->log_len == sizeof(sc->log_line) - 1) {
			sc->log_line[sc->log_len] = 0;
			dev_info(sc->dev, "%s\n", sc->log_line);
			sc->log_len = 0;
		} else {
			sc->log_line[sc->log_len++] = c;
		}
	}
	shm_wr(sc, LOG_RD, rd);
}

static void syscore_check_crash(struct syscore *sc)
{
	if (shm_rd(sc, H_MAGIC) != SC_MAGIC || shm_rd(sc, H_STATE) != STATE_CRASHED) {
		sc->crash_reported = false;
		return;
	}
	if (sc->crash_reported)
		return;
	sc->crash_reported = true;
	dev_err(sc->dev, "firmware crashed: vector %#x cause %08x epc %08x badvaddr %08x status %08x\n",
		shm_rd(sc, H_CRASH), shm_rd(sc, H_CRASH + 4), shm_rd(sc, H_CRASH + 8),
		shm_rd(sc, H_CRASH + 12), shm_rd(sc, H_CRASH + 16));
}

static void syscore_power_request(struct syscore *sc, const struct gb350_syscore_msg *msg)
{
	static const char * const reasons[] = { "?", "SD card removed", "battery critical", "power button held" };
	u32 reason = get_unaligned_le32(msg->data), grace = get_unaligned_le32(msg->data + 4);

	dev_crit(sc->dev, "%s: powering off (the core cuts power in %u s)\n",
		 reasons[reason < ARRAY_SIZE(reasons) ? reason : 0], grace / 1000);
	orderly_poweroff(true);
}

/*
 * Takes in everything the core sent: crash state, log, messages. Normally
 * from the doorbell's interrupt thread (real-time priority, so replies and
 * button events are not held up by a busy system), else from a work item
 * (polling fallback, requests).
 */
static void syscore_rx(struct syscore *sc)
{
	struct gb350_syscore_msg msg;
	u32 head, tail, slot, w[SLOT_SIZE / 4];
	int i;

	void (*hook)(void) = READ_ONCE(wake_hook);

	if (hook)
		hook();
	guard(mutex)(&sc->rx_lock);
	syscore_check_crash(sc);
	if (shm_rd(sc, H_MAGIC) != SC_MAGIC)
		return;
	syscore_drain_log(sc);

	head = shm_rd(sc, TO_LINUX_HEAD);
	tail = shm_rd(sc, TO_LINUX_TAIL);
	while (tail != head) {
		rmb();
		slot = TO_LINUX_SLOTS + (tail % RING_SLOTS) * SLOT_SIZE;
		for (i = 0; i < SLOT_SIZE / 4; i++)
			w[i] = readl(sc->shm + slot + 4 * i);
		shm_wr(sc, TO_LINUX_TAIL, ++tail);

		msg.kind = w[0] & 0xffff;
		msg.len = min_t(u16, w[0] >> 16, GB350_SYSCORE_MSG_DATA);
		msg.seq = w[1];
		memcpy(msg.data, &w[2], sizeof(msg.data));
		if (msg.kind == GB350_SYSCORE_POWER_REQUEST)
			syscore_power_request(sc, &msg);
		if (msg.seq && msg.seq == READ_ONCE(sc->wait_seq)) {
			sc->reply = msg;
			WRITE_ONCE(sc->wait_seq, 0);
			complete(&sc->reply_done);
		} else {
			blocking_notifier_call_chain(&syscore_notifier, msg.kind, &msg);
		}
	}
}

static void syscore_rx_work(struct work_struct *work)
{
	syscore_rx(container_of(work, struct syscore, rx_work));
}

static irqreturn_t syscore_irq_thread(int irq, void *data)
{
	syscore_rx(data);
	return IRQ_HANDLED;
}

static irqreturn_t syscore_irq(int irq, void *data)
{
	struct syscore *sc = data;
	u32 out = readl(sc->mbox + MB(DB_OUT, 8)), v = readl(sc->mbox + MB(DB_IN, 8));

	if (out & 0x10000)	/* the core took Linux's doorbell */
		writel((out & 0xfffefeff) | 0x10000, sc->mbox + MB(DB_OUT, 8));
	if (!(v & 0x100))
		return out & 0x10000 ? IRQ_HANDLED : IRQ_NONE;
	writel((v & 0xfffefeff) | 0x100, sc->mbox + MB(DB_IN, 8));	/* ack */
	writel(readl(sc->mbox + MB(DB_IN, 4)) | 1, sc->mbox + MB(DB_IN, 4)); /* release */
	return IRQ_WAKE_THREAD;
}

/* Fallback in case a doorbell is missed, and crash/log reporting. */
static void syscore_poll_work(struct work_struct *work)
{
	struct syscore *sc = container_of(to_delayed_work(work), struct syscore, poll_work);

	/* The core's doorbell busy without a request for Linux: release it. */
	if (readl(sc->cmbox + MB(DB_IN, 0)) & 1 && !(readl(sc->mbox + MB(DB_IN, 8)) & 0x100)) {
		dev_warn_ratelimited(sc->dev, "doorbell from the core stuck: releasing it\n");
		writel(readl(sc->mbox + MB(DB_IN, 4)) | 1, sc->mbox + MB(DB_IN, 4));
	}
	schedule_work(&sc->rx_work);
	schedule_delayed_work(&sc->poll_work, HZ);
}

/*
 * Clearing a pending request without releasing it would leave the sender
 * busy for good (it never rings again), so release as well.
 */
static void syscore_mbox_init(struct syscore *sc)
{
	writel(0x101, sc->mbox + MB(DB_IN, 8));		/* request irq on, clear */
	writel(readl(sc->mbox + MB(DB_IN, 4)) | 1, sc->mbox + MB(DB_IN, 4));
	writel(0x10002, sc->mbox + MB(DB_OUT, 8));	/* sender enable, clear */
}

static int syscore_load(struct syscore *sc, const char *name)
{
	const struct firmware *fw;
	u32 entry = (u32)(sc->fw_phys | 0xa0000000);	/* KSEG1 */
	bool parked;
	int ret, i;

	ret = request_firmware(&fw, name, sc->dev);
	if (ret)
		return ret;
	if (fw->size > sc->fw_size) {
		dev_err(sc->dev, "%s: %zu bytes, room for %pa\n", name, fw->size, &sc->fw_size);
		release_firmware(fw);
		return -EFBIG;
	}

	mutex_lock(&sc->req_lock);
	cancel_delayed_work_sync(&sc->poll_work);
	/* Ask a running firmware to park (a crashed one parks by itself). */
	if (syscore_running(sc) && !shm_rd(sc, H_PARKED)) {
		shm_wr(sc, H_PARKED, 0);
		gb350_syscore_send(GB350_SYSCORE_RESTART, NULL, 0, NULL);
		for (i = 0; i < 50 && !shm_rd(sc, H_PARKED); i++)
			msleep(2);
	}
	parked = shm_rd(sc, H_MAGIC) == SC_MAGIC && shm_rd(sc, H_PARKED) == 1;
	if (!parked) {
		dev_info(sc->dev, "core not parked: resetting it\n");
		writel(readl(sc->sys + SYS_RUN) & ~2, sc->sys + SYS_RUN);
		usleep_range(1000, 2000);
	}
	shm_wr(sc, H_MAGIC, 0);
	shm_wr(sc, H_STATE, 0);
	memcpy_toio(sc->fw, fw->data, fw->size);
	release_firmware(fw);
	wmb();
	/* lui t0, hi(entry); ori t0, lo(entry); jr t0; nop */
	writel(0x3c080000 | entry >> 16, sc->sys + SYS_TRAMPOLINE);
	writel(0x35080000 | (entry & 0xffff), sc->sys + SYS_TRAMPOLINE + 4);
	writel(0x01000008, sc->sys + SYS_TRAMPOLINE + 8);
	writel(0, sc->sys + SYS_TRAMPOLINE + 12);
	writeb(1, sc->sys + SYS_HANDSHAKE);
	syscore_mbox_init(sc);
	if (parked) {
		shm_wr(sc, H_PARK_GO, entry);	/* the park loop jumps there */
	} else {
		writel(readl(sc->sys + SYS_RUN) | 2, sc->sys + SYS_RUN);
	}

	/* As the vendor's boot stub: if it has not reported in, pulse again. */
	for (i = 0; i < 20 && readb(sc->sys + SYS_HANDSHAKE) != 2; i++)
		msleep(10);
	if (readb(sc->sys + SYS_HANDSHAKE) != 2) {
		dev_warn(sc->dev, "second core did not report in; pulsing reset again\n");
		writel(readl(sc->sys + SYS_RUN) & ~2, sc->sys + SYS_RUN);
		usleep_range(10000, 20000);
		writel(readl(sc->sys + SYS_RUN) | 2, sc->sys + SYS_RUN);
	}
	for (i = 0; i < 100 && !syscore_running(sc); i++)
		msleep(10);
	ret = syscore_running(sc) ? 0 : -ETIMEDOUT;
	if (ret)
		dev_err(sc->dev, "%s did not start (handshake %#x, state %#x)\n", name,
			readb(sc->sys + SYS_HANDSHAKE), shm_rd(sc, H_STATE));
	else
		dev_info(sc->dev, "%s started (version %#x)\n", name, shm_rd(sc, H_VERSION));
	sc->log_len = 0;
	sc->crash_reported = false;
	schedule_delayed_work(&sc->poll_work, 0);
	mutex_unlock(&sc->req_lock);
	return ret;
}

static ssize_t state_show(struct device *dev, struct device_attribute *attr, char *buf)
{
	struct syscore *sc = dev_get_drvdata(dev);

	if (shm_rd(sc, H_MAGIC) != SC_MAGIC)
		return sysfs_emit(buf, "absent\n");
	if (shm_rd(sc, H_STATE) == STATE_CRASHED)
		return sysfs_emit(buf, "crashed %#x %08x %08x %08x %08x\n", shm_rd(sc, H_CRASH),
				  shm_rd(sc, H_CRASH + 4), shm_rd(sc, H_CRASH + 8),
				  shm_rd(sc, H_CRASH + 12), shm_rd(sc, H_CRASH + 16));
	return sysfs_emit(buf, "%s\n", syscore_running(sc) ? "running" : "starting");
}
static DEVICE_ATTR_RO(state);

/*
 * load: "<idle cycles> <count> <count/ms>" - the core's idle time (in wait,
 * interrupts excluded) and its clock, both wrapping u32. Sampled twice, the
 * busy share is 1 - d(idle) / d(count).
 */
static ssize_t load_show(struct device *dev, struct device_attribute *attr, char *buf)
{
	struct syscore *sc = dev_get_drvdata(dev);

	return sysfs_emit(buf, "%u %u %u\n", shm_rd(sc, H_IDLE), shm_rd(sc, H_CYCLES), shm_rd(sc, H_COUNT_PER_MS));
}
static DEVICE_ATTR_RO(load);

static ssize_t debug_show(struct device *dev, struct device_attribute *attr, char *buf)
{
	struct syscore *sc = dev_get_drvdata(dev);

	return sysfs_emit(buf,
			  "ticks %u exceptions %u last cause %08x status %08x loops %u parked %u count/ms %u\n"
			  "input %08x%08x scans %u (last %u us) battery %u mV charger %u mV card %u supervisor %08x speaker muted %u\n"
			  "mbox L0 %08x/%08x L4 %08x/%08x A0 %08x/%08x A4 %08x/%08x\n",
			  shm_rd(sc, H_TICKS), shm_rd(sc, H_EXCEPTIONS), shm_rd(sc, H_LAST_CAUSE),
			  shm_rd(sc, H_LAST_STATUS), shm_rd(sc, H_LOOPS), shm_rd(sc, H_PARKED),
			  shm_rd(sc, H_COUNT_PER_MS), shm_rd(sc, H_INPUT + 4), shm_rd(sc, H_INPUT),
			  shm_rd(sc, H_SCANS), shm_rd(sc, H_SCAN_US), shm_rd(sc, H_BATTERY_MV),
			  shm_rd(sc, H_CHARGER_MV), shm_rd(sc, H_CARD), shm_rd(sc, H_SUPERVISOR), shm_rd(sc, H_SPEAKER),
			  readl(sc->mbox + MB(DB_OUT, 0)), readl(sc->mbox + MB(DB_OUT, 8)),
			  readl(sc->mbox + MB(DB_IN, 0)), readl(sc->mbox + MB(DB_IN, 8)),
			  readl(sc->cmbox + MB(DB_OUT, 0)), readl(sc->cmbox + MB(DB_OUT, 8)),
			  readl(sc->cmbox + MB(DB_IN, 0)), readl(sc->cmbox + MB(DB_IN, 8)));
}
static DEVICE_ATTR_RO(debug);

static ssize_t info_show(struct device *dev, struct device_attribute *attr, char *buf)
{
	struct syscore *sc = dev_get_drvdata(dev);
	struct gb350_syscore_msg reply;
	char build[64];
	u32 w[7];
	int ret, i;

	for (i = 0; i < sizeof(build) - 1; i++)
		build[i] = readb(sc->shm + H_BUILD + i);
	build[sizeof(build) - 1] = 0;
	ret = syscore_request(sc, GB350_SYSCORE_GET_INFO, NULL, 0, &reply);
	if (ret)
		return sysfs_emit(buf, "%s: no reply (%d)\n", build, ret);
	memcpy(w, reply.data, sizeof(w));
	return sysfs_emit(buf,
			  "%s\nversion %#x abi %u ticks %u count/ms %u rx %u tx %u prid %08x\n",
			  build, w[0], w[1], w[2], w[3], w[4], w[5], w[6]);
}
static DEVICE_ATTR_RO(info);

static ssize_t ping_show(struct device *dev, struct device_attribute *attr, char *buf)
{
	struct syscore *sc = dev_get_drvdata(dev);

	return sysfs_emit(buf, "%u pings: rtt min %lld avg %lld max %lld us\n", sc->rtt_count,
			  div_s64(sc->rtt_ns[0], 1000), div_s64(sc->rtt_ns[1], 1000),
			  div_s64(sc->rtt_ns[2], 1000));
}

static ssize_t ping_store(struct device *dev, struct device_attribute *attr,
			  const char *buf, size_t count)
{
	struct syscore *sc = dev_get_drvdata(dev);
	struct gb350_syscore_msg reply;
	unsigned int n, i;
	s64 total = 0, min = S64_MAX, max = 0;
	int ret;

	if (kstrtouint(buf, 0, &n) || !n || n > 10000)
		return -EINVAL;
	for (i = 0; i < n; i++) {
		u8 payload[8];
		ktime_t t0 = ktime_get();
		s64 rtt;

		put_unaligned_le32(i, payload);
		put_unaligned_le32(~i, payload + 4);
		ret = syscore_request(sc, GB350_SYSCORE_PING, payload, sizeof(payload), &reply);
		if (ret)
			return ret;
		rtt = ktime_to_ns(ktime_sub(ktime_get(), t0));
		if (reply.kind != GB350_SYSCORE_PONG || reply.len != sizeof(payload) ||
		    memcmp(reply.data, payload, sizeof(payload)))
			return -EIO;
		total += rtt;
		min = min(min, rtt);
		max = max(max, rtt);
	}
	sc->rtt_ns[0] = min;
	sc->rtt_ns[1] = div_s64(total, n);
	sc->rtt_ns[2] = max;
	sc->rtt_count = n;
	return count;
}
static DEVICE_ATTR_RW(ping);

/* Debugging: write "kind [word ...]" (u32s), read "kind len word ..." back. */
static ssize_t call_show(struct device *dev, struct device_attribute *attr, char *buf)
{
	struct syscore *sc = dev_get_drvdata(dev);
	const struct gb350_syscore_msg *r = &sc->call_reply;
	int len, i;

	if (sc->call_ret)
		return sysfs_emit(buf, "error %d\n", sc->call_ret);
	len = sysfs_emit(buf, "%u %u", r->kind, r->len);
	for (i = 0; i + 4 <= r->len; i += 4)
		len += sysfs_emit_at(buf, len, " %#x", get_unaligned_le32(r->data + i));
	return len + sysfs_emit_at(buf, len, "\n");
}

static ssize_t call_store(struct device *dev, struct device_attribute *attr,
			  const char *buf, size_t count)
{
	struct syscore *sc = dev_get_drvdata(dev);
	char *args, *p, *tok;
	u8 data[GB350_SYSCORE_MSG_DATA];
	unsigned int kind, n = 0;
	u32 v;

	args = kstrndup(buf, count, GFP_KERNEL);
	if (!args)
		return -ENOMEM;
	p = strim(args);
	tok = strsep(&p, " ");
	if (kstrtouint(tok, 0, &kind) || kind > U16_MAX)
		goto inval;
	while ((tok = strsep(&p, " ")) != NULL) {
		if (!*tok)
			continue;
		if (n == GB350_SYSCORE_MSG_DATA || kstrtou32(tok, 0, &v))
			goto inval;
		put_unaligned_le32(v, data + n);
		n += 4;
	}
	kfree(args);
	sc->call_ret = syscore_request(sc, kind, data, n, &sc->call_reply);
	return sc->call_ret ?: count;
inval:
	kfree(args);
	return -EINVAL;
}
static DEVICE_ATTR_RW(call);

static ssize_t firmware_store(struct device *dev, struct device_attribute *attr,
			      const char *buf, size_t count)
{
	struct syscore *sc = dev_get_drvdata(dev);
	char name[64];
	int ret;

	if (strscpy(name, buf, sizeof(name)) < 0)
		return -EINVAL;
	strim(name);
	ret = syscore_load(sc, name);
	return ret ? ret : count;
}
static DEVICE_ATTR_WO(firmware);

static struct attribute *syscore_attrs[] = {
	&dev_attr_state.attr,
	&dev_attr_debug.attr,
	&dev_attr_load.attr,
	&dev_attr_info.attr,
	&dev_attr_ping.attr,
	&dev_attr_firmware.attr,
	&dev_attr_call.attr,
	NULL
};
ATTRIBUTE_GROUPS(syscore);

/* Linux's side of the firmware's shm::lock (Peterson's algorithm). */
static int syscore_hwlock_trylock(struct hwspinlock *lock)
{
	void __iomem *l = lock->priv;

	writel(1, l);
	writel(TURN_CORE, l + 8);
	mb();
	if (readl(l + 4) && readl(l + 8) == TURN_CORE) {
		writel(0, l);
		return 0;
	}
	return 1;
}

static void syscore_hwlock_unlock(struct hwspinlock *lock)
{
	mb();
	writel(0, lock->priv);
}

static const struct hwspinlock_ops syscore_hwlock_ops = {
	.trylock	= syscore_hwlock_trylock,
	.unlock		= syscore_hwlock_unlock,
};

static int syscore_register_locks(struct syscore *sc)
{
	int i;

	sc->locks = devm_kzalloc(sc->dev, struct_size(sc->locks, lock, NUM_LOCKS), GFP_KERNEL);
	if (!sc->locks)
		return -ENOMEM;
	for (i = 0; i < NUM_LOCKS; i++) {
		sc->locks->lock[i].priv = sc->shm + LOCKS + 16 * i;
		writel(0, sc->shm + LOCKS + 16 * i);	/* Linux's flag */
	}
	return devm_hwspin_lock_register(sc->dev, sc->locks, &syscore_hwlock_ops, 0, NUM_LOCKS);
}

/*
 * Power off through the core, which also drives GPIO bank L (button scan):
 * one core owns the power latch. If it does not act, the board handler
 * (hc16xx-board, lower priority) drops the latch directly.
 */
static int syscore_power_off(struct sys_off_data *data)
{
	struct syscore *sc = data->cb_data;

	if (!syscore_running(sc) || gb350_syscore_send(GB350_SYSCORE_POWER_OFF, NULL, 0, NULL))
		return NOTIFY_DONE;
	/* Power takes over 500 ms to collapse once the latch drops. */
	mdelay(2000);
	pr_emerg("gb350-syscore: still powered 2 s after POWER_OFF\n");
	return NOTIFY_DONE;
}

static void __iomem *syscore_map(struct platform_device *pdev, int i, resource_size_t *phys,
				 resource_size_t *size)
{
	struct resource *res = platform_get_resource(pdev, IORESOURCE_MEM, i);

	if (!res)
		return IOMEM_ERR_PTR(-EINVAL);
	if (phys)
		*phys = res->start;
	if (size)
		*size = resource_size(res);
	/* Shared with other drivers (system control) or outside RAM: no claim. */
	return devm_ioremap(&pdev->dev, res->start, resource_size(res)) ?: IOMEM_ERR_PTR(-ENOMEM);
}

static int syscore_probe(struct platform_device *pdev)
{
	struct device *dev = &pdev->dev;
	struct syscore *sc;
	int irq, ret;

	sc = devm_kzalloc(dev, sizeof(*sc), GFP_KERNEL);
	if (!sc)
		return -ENOMEM;
	sc->dev = dev;
	sc->shm = syscore_map(pdev, 0, NULL, NULL);
	sc->fw = syscore_map(pdev, 1, &sc->fw_phys, &sc->fw_size);
	sc->mbox = syscore_map(pdev, 2, NULL, NULL);
	sc->sys = syscore_map(pdev, 3, NULL, NULL);
	sc->cmbox = syscore_map(pdev, 4, NULL, NULL);
	if (IS_ERR(sc->shm) || IS_ERR(sc->fw) || IS_ERR(sc->mbox) || IS_ERR(sc->sys) ||
	    IS_ERR(sc->cmbox))
		return dev_err_probe(dev, -EINVAL, "missing reg\n");
	irq = platform_get_irq(pdev, 0);
	if (irq < 0)
		return irq;

	spin_lock_init(&sc->tx_lock);
	mutex_init(&sc->req_lock);
	init_completion(&sc->reply_done);
	INIT_WORK(&sc->rx_work, syscore_rx_work);
	mutex_init(&sc->rx_lock);
	INIT_DELAYED_WORK(&sc->poll_work, syscore_poll_work);
	platform_set_drvdata(pdev, sc);

	syscore_mbox_init(sc);
	ret = devm_request_threaded_irq(dev, irq, syscore_irq, syscore_irq_thread, 0,
					"gb350-syscore", sc);
	if (ret)
		return ret;
	ret = syscore_register_locks(sc);
	if (ret)
		return dev_err_probe(dev, ret, "hwspinlocks\n");
	ret = devm_register_sys_off_handler(dev, SYS_OFF_MODE_POWER_OFF, SYS_OFF_PRIO_FIRMWARE,
					    syscore_power_off, sc);
	if (ret)
		return ret;
	the_syscore = sc;
	if (syscore_running(sc))
		dev_info(dev, "firmware running (version %#x)\n", shm_rd(sc, H_VERSION));
	else
		dev_info(dev, "no syscore firmware running on the second core\n");
	schedule_delayed_work(&sc->poll_work, 0);
	return 0;
}

static void syscore_remove(struct platform_device *pdev)
{
	struct syscore *sc = platform_get_drvdata(pdev);

	cancel_delayed_work_sync(&sc->poll_work);
	cancel_work_sync(&sc->rx_work);
	the_syscore = NULL;
}

static const struct of_device_id syscore_of_match[] = {
	{ .compatible = "hichip,gb350-syscore" },
	{ }
};
MODULE_DEVICE_TABLE(of, syscore_of_match);

static struct platform_driver syscore_driver = {
	.probe	= syscore_probe,
	.remove	= syscore_remove,
	.driver	= {
		.name		= "gb350-syscore",
		.of_match_table	= syscore_of_match,
		.dev_groups	= syscore_groups,
	},
};
module_platform_driver(syscore_driver);

MODULE_DESCRIPTION("GB350 second-core (syscore) link");
MODULE_LICENSE("GPL");
