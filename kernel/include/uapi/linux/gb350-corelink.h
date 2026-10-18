/* SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note */
/*
 * GB350 corelink: memory shared with the second core (avp/syscore), for
 * work handed to it (services fed through rings), /dev/corelink.
 *
 * Buffers are allocated on demand from Linux's CMA pool (physically
 * contiguous; nothing is set aside while no one uses corelink) and freed
 * with the file that allocated them. A buffer is named by its physical
 * address, which is also its mmap offset (MAP_SHARED): the main CPU's data
 * cache aliases, and mapping at the physical page's colour keeps the
 * kernel's cache maintenance exact.
 *
 * The two cores' caches are not coherent:
 *  - CACHED buffers: SYNC TO_CORE (write back) before the core reads what
 *    Linux wrote, SYNC FROM_CORE (invalidate) before Linux reads what the
 *    core wrote;
 *  - UNCACHED buffers (headers, counters): plain loads and stores;
 *  - WRITECOMBINE buffers (streams Linux writes, the core reads): stores
 *    merge in the CPU's write buffer (uncached accelerated) - no SYNC.
 */
#ifndef _UAPI_LINUX_GB350_CORELINK_H
#define _UAPI_LINUX_GB350_CORELINK_H

#include <linux/ioctl.h>
#include <linux/types.h>

#define CORELINK_UNCACHED	0
#define CORELINK_CACHED		1
#define CORELINK_WRITECOMBINE	2

struct corelink_info {
	__u32 version;
	__u32 allocated;	/* bytes allocated by everyone */
	__u32 channels;		/* rings attached on the core */
	__u32 pad;
};

struct corelink_alloc {
	__u32 size;		/* in: bytes (rounded up to pages) */
	__u32 caching;		/* in: CORELINK_* */
	__u32 phys;		/* out: physical address = mmap offset = id */
	__u32 pad;
};

#define CORELINK_TO_CORE	1	/* write back */
#define CORELINK_FROM_CORE	2	/* invalidate */
#define CORELINK_BOTH		3	/* write back and invalidate */

struct corelink_sync {
	__u32 phys;		/* anywhere inside an own CACHED buffer */
	__u32 len;
	__u32 dir;
	__u32 pad;
};

/*
 * A fence: two u32s in an UNCACHED buffer: value (the core advances it),
 * wanted (Linux writes it before sleeping; the core rings Linux's doorbell
 * once value reaches it). WAIT returns when (s32)(value - target) >= 0.
 */
struct corelink_wait {
	__u32 fence;		/* physical address of the fence */
	__u32 target;
	__u32 timeout_ms;
	__u32 pad;
};

/*
 * Attach a ring to a service on the core: the ring's header (UNCACHED,
 * 64 bytes: head, tail, idle, fence value, fence wanted) and data buffer.
 */
struct corelink_attach {
	__u32 service;
	__u32 header;		/* physical address */
	__u32 data;		/* physical address */
	__u32 size;		/* data bytes, a power of two */
	__u32 channel;		/* out: the core's channel number */
	__u32 pad[3];
};

#define CORELINK_IOC_INFO	_IOR('C', 0, struct corelink_info)
#define CORELINK_IOC_ALLOC	_IOWR('C', 1, struct corelink_alloc)
#define CORELINK_IOC_FREE	_IOW('C', 2, __u32)
#define CORELINK_IOC_SYNC	_IOW('C', 3, struct corelink_sync)
#define CORELINK_IOC_KICK	_IO('C', 4)
#define CORELINK_IOC_WAIT	_IOW('C', 5, struct corelink_wait)
#define CORELINK_IOC_ATTACH	_IOWR('C', 6, struct corelink_attach)
#define CORELINK_IOC_DETACH	_IOW('C', 7, __u32)

/* Ring header layout (byte offsets). */
#define CORELINK_RING_HEAD	0x00	/* producer: bytes written, free running */
#define CORELINK_RING_TAIL	0x04	/* consumer: bytes consumed */
#define CORELINK_RING_IDLE	0x08	/* consumer: 1 = asleep, kick after writing */
#define CORELINK_RING_FENCE	0x10	/* consumer: records completed; +4 wanted */
#define CORELINK_RING_ERROR	0x18	/* consumer: last error (service specific) */
#define CORELINK_RING_HEADER_SIZE 64

/*
 * Records: u32 tag << 16 | flags, u32 payload bytes, payload, padded to 8.
 * A record never wraps: the producer writes a PAD record (tag 0) up to the
 * end of the buffer instead.
 */
#define CORELINK_TAG_PAD	0

#endif
