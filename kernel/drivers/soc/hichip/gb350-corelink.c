// SPDX-License-Identifier: GPL-2.0
/*
 * GB350 corelink: memory shared with the second core (avp/syscore) and
 * rings feeding services there - /dev/corelink. See
 * include/uapi/linux/gb350-corelink.h for the interface.
 *
 * Buffers: physically contiguous, allocated on demand (CMA via the DMA
 * API), owned by the file that allocated them and reference counted (the
 * file, each mapping, each ring attached on the core), so nothing the core
 * may still read is freed: closing the file detaches its rings on the core
 * first. A buffer's name is its physical address, which is also its mmap
 * offset: the main CPU's data cache aliases (32 KiB, 4-way, VIPT), and a
 * mapping at the physical page's colour keeps cache maintenance through the
 * kernel's KSEG0 addresses exact.
 */
#include <linux/delay.h>
#include <linux/dma-mapping.h>
#include <linux/fs.h>
#include <linux/io.h>
#include <linux/kref.h>
#include <linux/miscdevice.h>
#include <linux/mm.h>
#include <linux/module.h>
#include <linux/of.h>
#include <linux/platform_device.h>
#include <linux/slab.h>
#include <linux/soc/hichip/gb350-syscore.h>
#include <linux/uaccess.h>
#include <linux/wait.h>
#include <uapi/linux/gb350-corelink.h>

#include <asm/cpu-features.h>

#define CORELINK_VERSION	1
#define MAX_BUFFER		(8 << 20)

/* GB350_SYSCORE_CORELINK ops (payload le32s: op, then arguments). */
#define OP_ATTACH		1	/* service, header, data, size -> status, channel */
#define OP_DETACH		2	/* channel -> status */

struct cl_buf {
	struct kref ref;
	struct list_head node;		/* in the file's list */
	void *vaddr;			/* kernel: KSEG0 (cached) or KSEG1 */
	dma_addr_t phys;
	size_t size;
	u32 caching;
};

struct cl_chan {
	struct list_head node;
	u32 channel;
	struct cl_buf *header, *data;
};

struct cl_file {
	struct mutex lock;
	struct list_head bufs, chans;
};

static struct device *cl_dev;
static DECLARE_WAIT_QUEUE_HEAD(cl_wait);
static atomic_t cl_allocated = ATOMIC_INIT(0);
static atomic_t cl_channels = ATOMIC_INIT(0);

static void cl_wake(void)
{
	wake_up_all(&cl_wait);
}

static void cl_buf_release(struct kref *ref)
{
	struct cl_buf *b = container_of(ref, struct cl_buf, ref);

	if (b->caching == CORELINK_CACHED)
		dma_free_noncoherent(cl_dev, b->size, b->vaddr, b->phys, DMA_BIDIRECTIONAL);
	else
		dma_free_coherent(cl_dev, b->size, b->vaddr, b->phys);
	atomic_sub(b->size, &cl_allocated);
	kfree(b);
}

static void cl_buf_put(struct cl_buf *b)
{
	kref_put(&b->ref, cl_buf_release);
}

/* The file's buffer containing [phys, phys + len), or NULL. */
static struct cl_buf *cl_find(struct cl_file *f, u32 phys, u32 len)
{
	struct cl_buf *b;

	list_for_each_entry(b, &f->bufs, node)
		if (phys >= b->phys && len <= b->size && phys - b->phys <= b->size - len)
			return b;
	return NULL;
}

static int cl_detach(struct cl_chan *c)
{
	struct gb350_syscore_msg reply;
	__le32 req[2] = { cpu_to_le32(OP_DETACH), cpu_to_le32(c->channel) };
	int ret = gb350_syscore_call(GB350_SYSCORE_CORELINK, req, sizeof(req), &reply);

	if (ret || le32_to_cpup((__le32 *)reply.data)) {
		/*
		 * The core may still use the buffers: keep them (leaked) rather
		 * than hand the memory to someone else under it.
		 */
		dev_err(cl_dev, "channel %u: detach failed (%d): keeping its buffers\n",
			c->channel, ret ?: (int)le32_to_cpup((__le32 *)reply.data));
		return ret ?: -EIO;
	}
	atomic_dec(&cl_channels);
	cl_buf_put(c->header);
	cl_buf_put(c->data);
	return 0;
}

/* ---- ioctls ---- */

static long cl_alloc(struct cl_file *f, struct corelink_alloc __user *arg)
{
	struct corelink_alloc a;
	struct cl_buf *b;

	if (copy_from_user(&a, arg, sizeof(a)))
		return -EFAULT;
	if (!a.size || a.size > MAX_BUFFER || a.caching > CORELINK_WRITECOMBINE)
		return -EINVAL;
	b = kzalloc(sizeof(*b), GFP_KERNEL);
	if (!b)
		return -ENOMEM;
	b->size = PAGE_ALIGN(a.size);
	b->caching = a.caching;
	/*
	 * Coherent memory is uncached for the kernel here (and its cache
	 * lines dropped); noncoherent is cached, synced explicitly.
	 */
	if (a.caching == CORELINK_CACHED)
		b->vaddr = dma_alloc_noncoherent(cl_dev, b->size, &b->phys, DMA_BIDIRECTIONAL,
						 GFP_KERNEL);
	else
		b->vaddr = dma_alloc_coherent(cl_dev, b->size, &b->phys, GFP_KERNEL);
	if (!b->vaddr) {
		kfree(b);
		return -ENOMEM;
	}
	if (a.caching == CORELINK_CACHED) {
		memset(b->vaddr, 0, b->size);
		dma_sync_single_for_device(cl_dev, b->phys, b->size, DMA_TO_DEVICE);
	} else {
		memset_io((void __iomem *)b->vaddr, 0, b->size);
	}
	kref_init(&b->ref);
	atomic_add(b->size, &cl_allocated);
	mutex_lock(&f->lock);
	list_add(&b->node, &f->bufs);
	mutex_unlock(&f->lock);
	a.phys = b->phys;
	return copy_to_user(arg, &a, sizeof(a)) ? -EFAULT : 0;
}

static long cl_free(struct cl_file *f, u32 phys)
{
	struct cl_buf *b;

	guard(mutex)(&f->lock);
	b = cl_find(f, phys, 1);
	if (!b || b->phys != phys)
		return -ENOENT;
	list_del(&b->node);
	cl_buf_put(b);		/* mappings and attached rings keep their refs */
	return 0;
}

static long cl_sync(struct cl_file *f, struct corelink_sync __user *arg)
{
	struct corelink_sync s;
	struct cl_buf *b;

	if (copy_from_user(&s, arg, sizeof(s)))
		return -EFAULT;
	guard(mutex)(&f->lock);
	b = cl_find(f, s.phys, s.len);
	if (!b || b->caching != CORELINK_CACHED || !s.len)
		return -EINVAL;
	/*
	 * All three are cache operations "for the device": for the CPU, MIPS
	 * only invalidates on CPUs that prefetch speculatively (not the 74K),
	 * so FROM_CORE through dma_sync_single_for_cpu() did nothing. Ranges
	 * of at least the cache's size become whole-cache operations.
	 */
	if (s.dir == CORELINK_TO_CORE)
		dma_sync_single_for_device(cl_dev, s.phys, s.len, DMA_TO_DEVICE);
	else if (s.dir == CORELINK_FROM_CORE)
		dma_sync_single_for_device(cl_dev, s.phys, s.len, DMA_FROM_DEVICE);
	else if (s.dir == CORELINK_BOTH)
		dma_sync_single_for_device(cl_dev, s.phys, s.len, DMA_BIDIRECTIONAL);
	else
		return -EINVAL;
	return 0;
}

static long cl_wait_fence(struct cl_file *f, struct corelink_wait __user *arg)
{
	struct corelink_wait w;
	struct cl_buf *b;
	u32 __iomem *fence;
	long left;

	if (copy_from_user(&w, arg, sizeof(w)))
		return -EFAULT;
	mutex_lock(&f->lock);
	b = cl_find(f, w.fence, 8);
	if (b && (b->caching != CORELINK_UNCACHED || (w.fence & 3)))
		b = NULL;
	if (b)
		kref_get(&b->ref);
	mutex_unlock(&f->lock);
	if (!b)
		return -EINVAL;
	fence = (u32 __iomem *)((char *)b->vaddr + (w.fence - b->phys));

#define FENCE_DONE() ((s32)(readl(fence) - w.target) >= 0)
	writel(w.target, fence + 1);	/* wanted: the core rings Linux */
	wmb();
	left = msecs_to_jiffies(w.timeout_ms ?: 1);
	if (!FENCE_DONE()) {
		/* Make sure the core looks (it may be asleep with work queued). */
		gb350_syscore_kick();
		left = wait_event_interruptible_timeout(cl_wait, FENCE_DONE(), left);
	}
	cl_buf_put(b);
	if (left < 0)
		return left;
	return FENCE_DONE() ? 0 : -ETIMEDOUT;
#undef FENCE_DONE
}

static long cl_attach(struct cl_file *f, struct corelink_attach __user *arg)
{
	struct gb350_syscore_msg reply;
	struct corelink_attach a;
	struct cl_buf *h, *d;
	struct cl_chan *c;
	__le32 req[5];
	int ret;

	if (copy_from_user(&a, arg, sizeof(a)))
		return -EFAULT;
	if (a.size < 64 || (a.size & (a.size - 1)) || (a.header & 63) || (a.data & 15))
		return -EINVAL;
	c = kzalloc(sizeof(*c), GFP_KERNEL);
	if (!c)
		return -ENOMEM;
	mutex_lock(&f->lock);
	h = cl_find(f, a.header, CORELINK_RING_HEADER_SIZE);
	d = cl_find(f, a.data, a.size);
	if (!h || !d || h->caching != CORELINK_UNCACHED) {
		mutex_unlock(&f->lock);
		kfree(c);
		return -EINVAL;
	}
	kref_get(&h->ref);
	kref_get(&d->ref);
	mutex_unlock(&f->lock);

	req[0] = cpu_to_le32(OP_ATTACH);
	req[1] = cpu_to_le32(a.service);
	req[2] = cpu_to_le32(a.header);
	req[3] = cpu_to_le32(a.data);
	req[4] = cpu_to_le32(a.size);
	ret = gb350_syscore_call(GB350_SYSCORE_CORELINK, req, sizeof(req), &reply);
	if (!ret && le32_to_cpup((__le32 *)reply.data))
		ret = -(int)le32_to_cpup((__le32 *)reply.data);	/* the core's errno */
	if (ret) {
		cl_buf_put(h);
		cl_buf_put(d);
		kfree(c);
		return ret;
	}
	c->channel = le32_to_cpup((__le32 *)reply.data + 1);
	c->header = h;
	c->data = d;
	atomic_inc(&cl_channels);
	mutex_lock(&f->lock);
	list_add(&c->node, &f->chans);
	mutex_unlock(&f->lock);
	a.channel = c->channel;
	return copy_to_user(arg, &a, sizeof(a)) ? -EFAULT : 0;
}

static long cl_detach_ioctl(struct cl_file *f, u32 channel)
{
	struct cl_chan *c;
	int ret;

	guard(mutex)(&f->lock);
	list_for_each_entry(c, &f->chans, node) {
		if (c->channel != channel)
			continue;
		ret = cl_detach(c);
		list_del(&c->node);
		kfree(c);
		return ret;
	}
	return -ENOENT;
}

static long cl_ioctl(struct file *file, unsigned int cmd, unsigned long arg)
{
	struct cl_file *f = file->private_data;
	void __user *p = (void __user *)arg;
	u32 v;

	switch (cmd) {
	case CORELINK_IOC_INFO: {
		struct corelink_info i = {
			.version = CORELINK_VERSION,
			.allocated = atomic_read(&cl_allocated),
			.channels = atomic_read(&cl_channels),
		};

		return copy_to_user(p, &i, sizeof(i)) ? -EFAULT : 0;
	}
	case CORELINK_IOC_ALLOC:
		return cl_alloc(f, p);
	case CORELINK_IOC_FREE:
		return get_user(v, (u32 __user *)p) ?: cl_free(f, v);
	case CORELINK_IOC_SYNC:
		return cl_sync(f, p);
	case CORELINK_IOC_KICK:
		return gb350_syscore_kick();
	case CORELINK_IOC_WAIT:
		return cl_wait_fence(f, p);
	case CORELINK_IOC_ATTACH:
		return cl_attach(f, p);
	case CORELINK_IOC_DETACH:
		return get_user(v, (u32 __user *)p) ?: cl_detach_ioctl(f, v);
	}
	return -ENOTTY;
}

/* ---- mappings ---- */

static void cl_vm_open(struct vm_area_struct *vma)
{
	kref_get(&((struct cl_buf *)vma->vm_private_data)->ref);
}

static void cl_vm_close(struct vm_area_struct *vma)
{
	cl_buf_put(vma->vm_private_data);
}

static const struct vm_operations_struct cl_vm_ops = {
	.open	= cl_vm_open,
	.close	= cl_vm_close,
};

static int cl_mmap(struct file *file, struct vm_area_struct *vma)
{
	struct cl_file *f = file->private_data;
	unsigned long len = vma->vm_end - vma->vm_start;
	u32 phys = vma->vm_pgoff << PAGE_SHIFT;
	struct cl_buf *b;
	pgprot_t prot;
	int ret;

	if (!(vma->vm_flags & VM_SHARED))
		return -EINVAL;
	/* The user address must share the physical page's cache colour. */
	if (cpu_has_dc_aliases && ((vma->vm_start ^ phys) & shm_align_mask))
		return -EINVAL;
	mutex_lock(&f->lock);
	b = cl_find(f, phys, len);
	if (b)
		kref_get(&b->ref);
	mutex_unlock(&f->lock);
	if (!b)
		return -EINVAL;
	switch (b->caching) {
	case CORELINK_CACHED:
		prot = vma->vm_page_prot;
		break;
	case CORELINK_WRITECOMBINE:
		prot = pgprot_writecombine(vma->vm_page_prot);
		break;
	default:
		prot = pgprot_noncached(vma->vm_page_prot);
	}
	vm_flags_set(vma, VM_IO | VM_DONTEXPAND | VM_DONTDUMP);
	ret = remap_pfn_range(vma, vma->vm_start, phys >> PAGE_SHIFT, len, prot);
	if (ret) {
		cl_buf_put(b);
		return ret;
	}
	vma->vm_private_data = b;
	vma->vm_ops = &cl_vm_ops;
	return 0;
}

/* ---- file ---- */

static int cl_open(struct inode *inode, struct file *file)
{
	struct cl_file *f;

	if (!gb350_syscore_present())
		return -ENODEV;
	f = kzalloc(sizeof(*f), GFP_KERNEL);
	if (!f)
		return -ENOMEM;
	mutex_init(&f->lock);
	INIT_LIST_HEAD(&f->bufs);
	INIT_LIST_HEAD(&f->chans);
	file->private_data = f;
	return 0;
}

static int cl_release(struct inode *inode, struct file *file)
{
	struct cl_file *f = file->private_data;
	struct cl_chan *c, *cn;
	struct cl_buf *b, *bn;

	list_for_each_entry_safe(c, cn, &f->chans, node) {
		cl_detach(c);
		kfree(c);
	}
	list_for_each_entry_safe(b, bn, &f->bufs, node)
		cl_buf_put(b);
	kfree(f);
	return 0;
}

static const struct file_operations cl_fops = {
	.owner		= THIS_MODULE,
	.open		= cl_open,
	.release	= cl_release,
	.unlocked_ioctl	= cl_ioctl,
	.mmap		= cl_mmap,
	.get_unmapped_area = NULL,	/* MIPS colours MAP_SHARED by pgoff */
};

static struct miscdevice cl_misc = {
	.minor	= MISC_DYNAMIC_MINOR,
	.name	= "corelink",
	.fops	= &cl_fops,
	.mode	= 0600,
};

static int cl_probe(struct platform_device *pdev)
{
	int ret;

	if (!gb350_syscore_present())
		return -EPROBE_DEFER;
	cl_dev = &pdev->dev;
	ret = dma_set_mask_and_coherent(cl_dev, DMA_BIT_MASK(28));	/* the core's KSEG0/1 */
	if (ret)
		return ret;
	ret = misc_register(&cl_misc);
	if (ret)
		return ret;
	gb350_syscore_set_wake_hook(cl_wake);
	dev_info(cl_dev, "corelink v%u: /dev/corelink\n", CORELINK_VERSION);
	return 0;
}

static void cl_remove(struct platform_device *pdev)
{
	gb350_syscore_set_wake_hook(NULL);
	misc_deregister(&cl_misc);
}

static const struct of_device_id cl_of_match[] = {
	{ .compatible = "hichip,gb350-corelink" },
	{ }
};
MODULE_DEVICE_TABLE(of, cl_of_match);

static struct platform_driver cl_driver = {
	.probe	= cl_probe,
	.remove	= cl_remove,
	.driver	= {
		.name		= "gb350-corelink",
		.of_match_table	= cl_of_match,
	},
};
module_platform_driver(cl_driver);

MODULE_DESCRIPTION("GB350 memory and work rings shared with the second core");
MODULE_LICENSE("GPL");
