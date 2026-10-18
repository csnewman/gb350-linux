// SPDX-License-Identifier: GPL-2.0
/*
 * HiChip HC16xx display: DRM/KMS driver for the two graphics layers (GMA)
 * of the display engine, as used on the GB350 (640x480 MIPI-DSI panel).
 *
 * The bootloader brings the panel up (DSI, timings, display engine) and the
 * driver keeps its timing: the CRTC only scans out layers. Each layer is a plane
 * that the hardware scales (polyphase filter or pixel duplication) from
 * any source size into any window of the panel.
 *
 * Modes: the panel's native mode plus smaller "logical" modes (320x240,
 * 256x224, ...). The panel always runs natively; a logical mode is fitted
 * to it per the connector's "scaling mode" property (Full, Full aspect =
 * default, Center), and planes are scaled accordingly - a panel fitter.
 *
 * Layer n registers at base + 0x80 n (hcfb, HCRTOS SDK hc16xx_fb.c):
 *   +0x300 control: bit 0 on, bit 18 sharpness, bit 19 CSC bypass
 *   +0x304 physical address of the layer descriptor (in RAM)
 *   +0x308 global alpha [7:0], enable bit 16
 *   +0x350 bit 0: hold register updates (set, change, clear = commit)
 * and engine-wide: +0x3b8 [4:0] line buffer split (0xa layer 0 only, 0x12
 * layer 1 only, 0x2 both; one register, though the vendor writes it per
 * layer), +0x1e8 stacking order (2 bits per layer, higher = in front:
 * [1:0] video, [5:4] layer 1, [9:8] layer 0, [13:12] second video layer;
 * the bootloader's 0x3210 puts layer 0 above layer 1).
 * The descriptor (0x280 bytes, 32-byte aligned) is read by the hardware:
 * header words (see DESC_*), scale filter coefficients at 0x40, colour
 * enhancement/CSC coefficients at 0x240. Two per layer, alternated, in the
 * DT memory-region page (outside the kernel's allocator, so a kexec'ed
 * kernel finds them intact; offset 0 holds hc16xx-gma's boot descriptor).
 *
 * Buffers: CMA, mapped cached (map_noncoherent) and flushed on commit.
 *
 * Timing: the engine's interrupt (intc hwirq 40) is
 * +0x04 enable [5:0] & status [13:8] (write 1 to clear); +0x08 [27:16] is
 * the current line, 1..594 (vsync 1-31, active 65-544). Layer registers are
 * copied into read-only active copies (+0x800) at about line 60 - the
 * commit point - unless a layer's +0x350 hold bit is set then. The driver takes the
 * vblank interrupt from line compare 1 (bit 5, +0x30 [31:16]) just after
 * that, and complete a flip once the active copies show it.
 */
#include <linux/aperture.h>
#include <linux/delay.h>
#include <linux/dma-mapping.h>
#include <linux/io.h>
#include <linux/iopoll.h>
#include <linux/module.h>
#include <linux/of.h>
#include <linux/of_reserved_mem.h>
#include <linux/platform_device.h>

#include <drm/clients/drm_client_setup.h>
#include <drm/drm_atomic.h>
#include <drm/drm_atomic_helper.h>
#include <drm/drm_blend.h>
#include <drm/drm_connector.h>
#include <drm/drm_crtc.h>
#include <drm/drm_damage_helper.h>
#include <drm/drm_drv.h>
#include <drm/drm_edid.h>
#include <drm/drm_fb_dma_helper.h>
#include <drm/drm_fbdev_dma.h>
#include <drm/drm_fourcc.h>
#include <drm/drm_framebuffer.h>
#include <drm/drm_gem_dma_helper.h>
#include <drm/drm_gem_framebuffer_helper.h>
#include <drm/drm_managed.h>
#include <drm/drm_modes.h>
#include <drm/drm_probe_helper.h>
#include <drm/drm_simple_kms_helper.h>
#include <drm/drm_vblank.h>
#include <drm/hc16xx_drm.h>
#include <video/mipi_display.h>

/* Display engine: output timing */
#define DE_CTL			0x00
#define DE_CTL_UPDATE		BIT(2)	/* cleared around timing writes */
#define DE_TOTAL		0x0c	/* vtotal << 16 | 2 x htotal */
#define DE_HSYNC		0x10	/* 2 x hsync << 16 | 2 x htotal */
#define DE_VSYNC		0x18	/* vsync << 16 | vtotal */
#define DE_VTOTAL2		0x2c	/* vtotal << 16 | 0xfff */
#define DE_BG			0x188	/* background Y << 16 | Cb << 8 | Cr */
/* Rates the panel showed stable (2026-10-17): 30 Hz flickers below. */
#define REFRESH_MIN		30
#define REFRESH_MAX		86
#define PORCH_MIN		2	/* front porches, pixels / lines */

/* 2D engine (GE), direct register ("IO") mode */
#define GE_CFG			0x000
#define GE_CFG_QUEUE		BIT(31)
#define GE_CFG_HQ_INT		BIT(17)
#define GE_CFG_IO_INT		BIT(20)
#define GE_START		0x004
#define GE_START_IO		1
#define GE_STATUS		0x008
#define GE_STATUS_IO_DONE	BIT(2)
#define GE_STATUS_BUSY		BIT(31)
#define GE_FUNC			0x030
#define GE_FUNC_BLIT		0x00a00009	/* BITBLT, pattern bitmap, DFB rop */
#define GE_FUNC_AFFINE		0x00a03009	/* + tile scan, pattern cache */
#define GE_DST_BA		0x034
#define GE_DST_TYPE		0x038
#define GE_DST_XY		0x03c
#define GE_DST_WH		0x040
#define GE_SRC_BA		0x044
#define GE_SRC_TYPE		0x048
#define GE_SRC_XY		0x04c
#define GE_PTN_BA		0x050
#define GE_PTN_TYPE		0x054
#define GE_PTN_XY		0x058
#define GE_PTN_WH		0x05c
#define GE_ROP			0x084
#define GE_ROP_COPY		0x4000	/* DFB, pattern bypass */
#define GE_DFB_COLOR		0x0a8
#define GE_LAT_CFG		0x0f0
#define GE_MTX_CTL		0x100
#define GE_MTX(i)		(0x108 + 4 * (i))	/* m00 m01 m02 m10 m11 m12 */
#define GE_TYPE_FMT(f)		((f) << 12)
#define GE_TYPE_BGR		(1 << 17)
#define GE_TYPE_HFLIP		BIT(20)
#define GE_TYPE_VFLIP		BIT(21)
#define GE_TYPE_AFFINE		BIT(30)
#define GE_ONE			0x00010000	/* +1.0: sign bit 31, 16.16 magnitude */
#define GE_MINUS_ONE		0x80010000
/* System controller */
#define SYS_CLK_GATE		0x64	/* bit set = clock off */
#define SYS_CLK_GATE_GE		BIT(4)
#define SYS_CLK_SEL		0x7c
#define SYS_CLK_SEL_GE		GENMASK(19, 18)	/* 3 = 238 MHz */
#define SYS_RESET		0x80
#define SYS_RESET_GE		BIT(4)

/* MIPI-DSI host (DesignWare 1.01): panel commands */
#define DSI_GEN_HDR		0x34
#define DSI_CMD_STATUS		0x3c
#define DSI_CMD_EMPTY		BIT(0)
#define DCS_SHORT_WRITE		0x05

#define NUM_LAYERS		2
#define LAYER(n)		((n) * 0x80)
#define GMA_CTL			0x300
#define GMA_CTL_ON		BIT(0)
#define GMA_DMBA		0x304
#define GMA_K			0x308
#define GMA_MASK		0x350
#define GMA_LINEBUF		0x3b8	/* engine-wide */
#define DE_ORDER		0x1e8
#define DE_ORDER_MASK		GENMASK(13, 0)
#define DE_ORDER_PLANES		0x3120	/* video, layer 0, layer 1, aux video */
#define GMA_ACTIVE		0x800	/* hardware copy of +0x300.. */
#define DE_INT			0x04
#define DE_INT_STATUS		GENMASK(13, 8)
#define DE_INT_ALL		GENMASK(5, 0)
#define DE_IRQ_LINE1		BIT(5)	/* line compare 1 */
#define DE_LINE			0x08	/* [27:16] current line */
#define DE_LINE_CMP		0x30	/* [31:16] line compare 1 (fires at +1) */
#define VBLANK_LINE		62	/* just after the ~line 60 register latch */
#define ACTIVE_FIRST_LINE	65

/* Descriptor */
#define DESC_SIZE		0x280
#define DESC_OFFSET		0x400	/* in the memory-region page */
#define DESC_BOOT		0x000	/* hc16xx-gma's, enhancement copied from it */
#define DESC_COEF		0x40
#define DESC_ENHANCE		0x240
#define DESC_ENHANCE_WORDS	13
#define D0_LAST			BIT(0)
#define D0_SCALE		BIT(2)
#define D0_EDGE_PRESERVE	BIT(3)
#define D0_FORMAT(f)		((f) << 4)
#define D0_PER_PIXEL_ALPHA	BIT(8)	/* "color by color": no global alpha */
#define D0_CLUT_DMA		BIT(9)
#define D0_PREMULTIPLIED	BIT(11)
#define D0_CSC_BT709		BIT(12)
#define D0_EP_THRESHOLDS	(32 << 16 | 170 << 24)	/* vendor defaults */
#define D8_DUPLICATE		BIT(0)	/* nearest neighbour instead of filter */

/* Hardware colour formats */
#define FMT_XRGB8888		0x0
#define FMT_ARGB8888		0x1
#define FMT_XRGB4444		0x2
#define FMT_ARGB4444		0x3
#define FMT_XRGB1555		0x4
#define FMT_ARGB1555		0x5
#define FMT_RGB565		0x6

#define FILTER_PHASES		16
#define H_TAPS			4
#define V_TAPS			3

struct hc16xx_drm {
	struct drm_device drm;
	void __iomem *regs;
	void __iomem *desc;		/* memory-region page, uncached */
	phys_addr_t desc_phys;
	u32 enhance[DESC_ENHANCE_WORDS];
	struct drm_display_mode native;
	u32 width_mm, height_mm;
	struct drm_crtc crtc;
	struct drm_encoder encoder;
	struct drm_connector connector;
	struct hc16xx_plane *planes[NUM_LAYERS];
	/* What was last programmed per layer, to see when it is latched. */
	u32 dmba[NUM_LAYERS];
	bool on[NUM_LAYERS];
	struct drm_pending_vblank_event *event;	/* under drm.event_lock */
	unsigned int event_age;			/* vblanks it has waited */
	void __iomem *ge, *dsi, *sys;		/* optional: NULL if not in the DT */
	struct mutex ge_lock;
	struct mutex panel_lock;
	bool panel_off;
};

struct hc16xx_plane {
	struct drm_plane base;
	unsigned int layer;
	unsigned int next_desc;		/* 0/1: the block written next */
};

static struct hc16xx_drm *to_hc(struct drm_device *drm)
{
	return container_of(drm, struct hc16xx_drm, drm);
}

static struct hc16xx_plane *to_hc_plane(struct drm_plane *plane)
{
	return container_of(plane, struct hc16xx_plane, base);
}

/*
 * BGR variants use the same formats with the descriptor's RGB order field
 * set to ABGR (XBGR1555 is the PlayStation's native framebuffer layout, so
 * emulators can hand frames over unconverted).
 */
static const u32 hc16xx_formats_primary[] = {
	DRM_FORMAT_XRGB8888, DRM_FORMAT_RGB565, DRM_FORMAT_XRGB1555, DRM_FORMAT_XRGB4444,
	DRM_FORMAT_XBGR8888, DRM_FORMAT_BGR565, DRM_FORMAT_XBGR1555, DRM_FORMAT_XBGR4444,
};

static const u32 hc16xx_formats_overlay[] = {
	DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888, DRM_FORMAT_RGB565,
	DRM_FORMAT_ARGB1555, DRM_FORMAT_XRGB1555, DRM_FORMAT_ARGB4444, DRM_FORMAT_XRGB4444,
	DRM_FORMAT_ABGR8888, DRM_FORMAT_XBGR8888, DRM_FORMAT_BGR565,
	DRM_FORMAT_ABGR1555, DRM_FORMAT_XBGR1555, DRM_FORMAT_ABGR4444, DRM_FORMAT_XBGR4444,
};

/* Descriptor word 5 [13:12]: component order. */
#define D5_ORDER_ABGR		(1 << 12)

static bool hc16xx_bgr(u32 fourcc)
{
	switch (fourcc) {
	case DRM_FORMAT_ABGR8888: case DRM_FORMAT_XBGR8888: case DRM_FORMAT_BGR565:
	case DRM_FORMAT_ABGR1555: case DRM_FORMAT_XBGR1555:
	case DRM_FORMAT_ABGR4444: case DRM_FORMAT_XBGR4444:
		return true;
	}
	return false;
}

static int hc16xx_hw_format(u32 fourcc)
{
	switch (fourcc) {
	case DRM_FORMAT_XRGB8888: case DRM_FORMAT_XBGR8888: return FMT_XRGB8888;
	case DRM_FORMAT_ARGB8888: case DRM_FORMAT_ABGR8888: return FMT_ARGB8888;
	case DRM_FORMAT_XRGB4444: case DRM_FORMAT_XBGR4444: return FMT_XRGB4444;
	case DRM_FORMAT_ARGB4444: case DRM_FORMAT_ABGR4444: return FMT_ARGB4444;
	case DRM_FORMAT_XRGB1555: case DRM_FORMAT_XBGR1555: return FMT_XRGB1555;
	case DRM_FORMAT_ARGB1555: case DRM_FORMAT_ABGR1555: return FMT_ARGB1555;
	case DRM_FORMAT_RGB565: case DRM_FORMAT_BGR565: return FMT_RGB565;
	}
	return -EINVAL;
}

/* Panel fitter: where the logical mode lands on the panel. */
struct hc16xx_fit {
	int x, y, w, h;			/* panel pixels */
	int mode_w, mode_h;		/* logical */
};

static void hc16xx_fit(const struct hc16xx_drm *hc, const struct drm_display_mode *mode,
		       unsigned int scaling, struct hc16xx_fit *fit)
{
	int pw = hc->native.hdisplay, ph = hc->native.vdisplay;
	int w = mode->hdisplay, h = mode->vdisplay;

	fit->mode_w = w;
	fit->mode_h = h;
	switch (scaling) {
	case DRM_MODE_SCALE_FULLSCREEN:
		fit->w = pw;
		fit->h = ph;
		break;
	case DRM_MODE_SCALE_CENTER:
	case DRM_MODE_SCALE_NONE:
		fit->w = w;
		fit->h = h;
		break;
	default:	/* aspect */
		if (w * ph > h * pw) {
			fit->w = pw;
			fit->h = h * pw / w;
		} else {
			fit->h = ph;
			fit->w = w * ph / h;
		}
	}
	fit->x = (pw - fit->w) / 2;
	fit->y = (ph - fit->h) / 2;
}

static unsigned int hc16xx_scaling(struct hc16xx_drm *hc, struct drm_atomic_commit *state)
{
	struct drm_connector_state *cs = drm_atomic_get_new_connector_state(state, &hc->connector);

	return cs ? cs->scaling_mode : hc->connector.state->scaling_mode;
}

/* A plane's window on the panel, from its (clipped) logical destination. */
static void hc16xx_plane_window(const struct hc16xx_fit *fit, const struct drm_rect *dst,
				struct drm_rect *win)
{
	win->x1 = fit->x + dst->x1 * fit->w / fit->mode_w;
	win->x2 = fit->x + dst->x2 * fit->w / fit->mode_w;
	win->y1 = fit->y + dst->y1 * fit->h / fit->mode_h;
	win->y2 = fit->y + dst->y2 * fit->h / fit->mode_h;
}

/*
 * Polyphase scale filter, in the layout the hardware reads (as the vendor
 * driver lays it out): 16 phases of 128 bits each, horizontal (4 taps:
 * 8-, 9-, 9-, 8-bit signed) then vertical (3 taps, 9-bit). Coefficients sum
 * to 256 per phase. Here: a triangle (bilinear) kernel, widened when
 * scaling down, sampled from a prototype of FILTER_PHASES * taps + 1 points
 * centred like the vendor's, so phase conventions match.
 */
static void hc16xx_filter(int taps, int src, int dst, s16 *out /* [taps][16] */)
{
	int len = FILTER_PHASES * taps + 1, centre = len / 2;
	/* kernel half-width in 1/16 pixel units: 1 pixel, or src/dst if bigger */
	int width = src > dst ? DIV_ROUND_UP(FILTER_PHASES * src, dst) : FILTER_PHASES;
	int p, t;

	width = min(width, FILTER_PHASES * taps / 2);
	for (p = 0; p < FILTER_PHASES; p++) {
		int sum = 0, best = 0, k[H_TAPS];

		for (t = 0; t < taps; t++) {
			int x = abs(t * FILTER_PHASES + p - centre);

			k[t] = max(width - x, 0);
			sum += k[t];
		}
		if (!sum) {
			k[taps / 2] = sum = 1;
		}
		for (t = 0; t < taps; t++) {
			out[t * FILTER_PHASES + p] = DIV_ROUND_CLOSEST(k[t] * 256, sum);
			if (out[t * FILTER_PHASES + p] > out[best * FILTER_PHASES + p])
				best = t;
		}
		sum = 0;
		for (t = 0; t < taps; t++)
			sum += out[t * FILTER_PHASES + p];
		out[best * FILTER_PHASES + p] += 256 - sum;
	}
}

static void hc16xx_write_coef(void __iomem *d, int src_w, int dst_w, int src_h, int dst_h)
{
	s16 h[H_TAPS * FILTER_PHASES], v[V_TAPS * FILTER_PHASES];
	u16 c[FILTER_PHASES * 2 * 8] = {};
	int i;

	hc16xx_filter(H_TAPS, src_w, dst_w, h);
	hc16xx_filter(V_TAPS, src_h, dst_h, v);
	for (i = 0; i < FILTER_PHASES; i++) {
		u16 *ph = &c[i * 8], *pv = &c[(FILTER_PHASES + i) * 8];

		ph[0] = h[3 * FILTER_PHASES + i] & 0xff;
		ph[1] = h[2 * FILTER_PHASES + i] & 0x1ff;
		ph[2] = h[1 * FILTER_PHASES + i] & 0x1ff;
		ph[3] = h[0 * FILTER_PHASES + i] & 0xff;
		pv[0] = v[2 * FILTER_PHASES + i] & 0x1ff;
		pv[1] = v[1 * FILTER_PHASES + i] & 0x1ff;
		pv[2] = v[0 * FILTER_PHASES + i] & 0x1ff;
	}
	for (i = 0; i < ARRAY_SIZE(c); i += 2)
		writel(c[i] | (u32)c[i + 1] << 16, d + DESC_COEF + i * 2);
}

static void hc16xx_layer_commit(struct hc16xx_drm *hc, unsigned int layer, u32 ctl_set,
				u32 ctl_clear, s64 desc_phys)
{
	void __iomem *r = hc->regs + LAYER(layer);
	u32 ctl = readl(r + GMA_CTL);

	ctl = (ctl & ~ctl_clear) | ctl_set;
	writel(readl(r + GMA_MASK) | 1, r + GMA_MASK);
	if (desc_phys >= 0) {
		writel(desc_phys, r + GMA_DMBA);
		hc->dmba[layer] = desc_phys;
	}
	writel(ctl, r + GMA_CTL);
	hc->on[layer] = ctl & GMA_CTL_ON;
	writel(readl(r + GMA_MASK) & ~1, r + GMA_MASK);
}

/* True once the hardware has latched what was last programmed. */
static bool hc16xx_latched(struct hc16xx_drm *hc)
{
	int i;

	for (i = 0; i < NUM_LAYERS; i++) {
		void __iomem *a = hc->regs + LAYER(i) + GMA_ACTIVE;
		bool on = readl(a + GMA_CTL) & GMA_CTL_ON;

		if (on != hc->on[i] || (on && readl(a + GMA_DMBA) != hc->dmba[i]))
			return false;
	}
	return true;
}

/* ---- planes ---- */

static int hc16xx_plane_atomic_check(struct drm_plane *plane, struct drm_atomic_commit *state)
{
	struct hc16xx_drm *hc = to_hc(plane->dev);
	struct drm_plane_state *ps = drm_atomic_get_new_plane_state(state, plane);
	struct drm_crtc_state *cs;
	struct drm_connector_state *conn;
	struct hc16xx_fit fit;
	struct drm_rect win;
	int ret, src_w, src_h;

	if (!ps->crtc)
		return 0;
	cs = drm_atomic_get_new_crtc_state(state, ps->crtc);
	if (WARN_ON(!cs))
		return -EINVAL;
	/* The fitter depends on the connector's scaling mode. */
	conn = drm_atomic_get_connector_state(state, &hc->connector);
	if (IS_ERR(conn))
		return PTR_ERR(conn);

	ret = drm_atomic_helper_check_plane_state(ps, cs, 1, INT_MAX, true, true);
	if (ret || !ps->visible)
		return ret;

	src_w = drm_rect_width(&ps->src) >> 16;
	src_h = drm_rect_height(&ps->src) >> 16;
	hc16xx_fit(hc, &cs->mode, conn->scaling_mode, &fit);
	hc16xx_plane_window(&fit, &ps->dst, &win);
	if (src_w < 1 || src_h < 1 || src_w > 4095 || src_h > 4095 ||
	    drm_rect_width(&win) < 1 || drm_rect_height(&win) < 1 ||
	    ps->fb->pitches[0] > 16383)
		return -EINVAL;
	/* Increments are 4.12 fixed point: at most ~16x down. */
	if (src_w * 4096 / drm_rect_width(&win) > 0xffff ||
	    src_h * 4096 / drm_rect_height(&win) > 0xffff)
		return -ERANGE;

	drm_atomic_helper_check_plane_damage(state, ps);
	return 0;
}

static void hc16xx_plane_atomic_update(struct drm_plane *plane, struct drm_atomic_commit *state)
{
	struct hc16xx_drm *hc = to_hc(plane->dev);
	struct hc16xx_plane *p = to_hc_plane(plane);
	struct drm_plane_state *ps = drm_atomic_get_new_plane_state(state, plane);
	struct drm_plane_state *old = drm_atomic_get_old_plane_state(state, plane);
	struct drm_crtc_state *cs;
	struct drm_framebuffer *fb = ps->fb;
	unsigned int off = DESC_OFFSET + (p->layer * 2 + p->next_desc) * DESC_SIZE;
	void __iomem *d = hc->desc + off;
	struct hc16xx_fit fit;
	struct drm_rect win;
	int src_w, src_h, win_w, win_h, i;
	bool scale, nearest;
	u32 w0;

	if (!ps->visible || !fb) {
		hc16xx_layer_commit(hc, p->layer, 0, GMA_CTL_ON, -1);
		return;
	}
	drm_fb_dma_sync_non_coherent(plane->dev, old, ps);

	cs = drm_atomic_get_new_crtc_state(state, ps->crtc) ?: ps->crtc->state;
	hc16xx_fit(hc, &cs->mode, hc16xx_scaling(hc, state), &fit);
	hc16xx_plane_window(&fit, &ps->dst, &win);
	src_w = drm_rect_width(&ps->src) >> 16;
	src_h = drm_rect_height(&ps->src) >> 16;
	win_w = drm_rect_width(&win);
	win_h = drm_rect_height(&win);
	scale = src_w != win_w || src_h != win_h;
	nearest = ps->scaling_filter == DRM_SCALING_FILTER_NEAREST_NEIGHBOR;

	w0 = D0_LAST | D0_FORMAT(hc16xx_hw_format(fb->format->format)) | D0_PER_PIXEL_ALPHA |
	     D0_CLUT_DMA | D0_CSC_BT709 | D0_EP_THRESHOLDS;
	if (fb->format->has_alpha && ps->pixel_blend_mode == DRM_MODE_BLEND_PREMULTI)
		w0 |= D0_PREMULTIPLIED;
	if (scale) {
		w0 |= D0_SCALE;
		/* Edge preservation when filtering up, as the vendor does. */
		if (!nearest && (win_w > src_w || win_h > src_h))
			w0 |= D0_EDGE_PRESERVE;
		hc16xx_write_coef(d, src_w, win_w, src_h, win_h);
	}
	writel(w0, d + 0x00);
	writel(0, d + 0x04);
	writel(win.x1 | (win.x2 - 1) << 16, d + 0x08);
	writel(win.y1 | (win.y2 - 1) << 16, d + 0x0c);
	writel(src_w | src_h << 16, d + 0x10);
	writel(0xff | fb->pitches[0] << 16 | (hc16xx_bgr(fb->format->format) ? D5_ORDER_ABGR : 0),
	       d + 0x14);
	writel(0, d + 0x18);
	writel(drm_fb_dma_get_gem_addr(fb, ps, 0), d + 0x1c);
	writel(scale && nearest ? D8_DUPLICATE : 0, d + 0x20);
	writel((src_w * 4096 / win_w) | (src_h * 4096 / win_h) << 16, d + 0x24);
	for (i = 0; i < DESC_ENHANCE_WORDS; i++)
		writel(hc->enhance[i], d + DESC_ENHANCE + 4 * i);
	wmb();

	hc16xx_layer_commit(hc, p->layer, GMA_CTL_ON, 0, hc->desc_phys + off);
	p->next_desc ^= 1;
}

static void hc16xx_plane_atomic_disable(struct drm_plane *plane, struct drm_atomic_commit *state)
{
	hc16xx_layer_commit(to_hc(plane->dev), to_hc_plane(plane)->layer, 0, GMA_CTL_ON, -1);
}

static const struct drm_plane_helper_funcs hc16xx_plane_helper_funcs = {
	.atomic_check	= hc16xx_plane_atomic_check,
	.atomic_update	= hc16xx_plane_atomic_update,
	.atomic_disable	= hc16xx_plane_atomic_disable,
};

static const struct drm_plane_funcs hc16xx_plane_funcs = {
	.update_plane		= drm_atomic_helper_update_plane,
	.disable_plane		= drm_atomic_helper_disable_plane,
	.reset			= drm_atomic_helper_plane_reset,
	.atomic_duplicate_state	= drm_atomic_helper_plane_duplicate_state,
	.atomic_destroy_state	= drm_atomic_helper_plane_destroy_state,
};

static int hc16xx_plane_init(struct hc16xx_drm *hc, unsigned int layer)
{
	bool primary = layer == 0;
	struct hc16xx_plane *p;
	int ret;

	p = drmm_universal_plane_alloc(&hc->drm, struct hc16xx_plane, base, 1, &hc16xx_plane_funcs,
				       primary ? hc16xx_formats_primary : hc16xx_formats_overlay,
				       primary ? ARRAY_SIZE(hc16xx_formats_primary) :
						 ARRAY_SIZE(hc16xx_formats_overlay),
				       NULL, primary ? DRM_PLANE_TYPE_PRIMARY : DRM_PLANE_TYPE_OVERLAY,
				       NULL);
	if (IS_ERR(p))
		return PTR_ERR(p);
	p->layer = layer;
	drm_plane_helper_add(&p->base, &hc16xx_plane_helper_funcs);
	drm_plane_enable_fb_damage_clips(&p->base);
	ret = drm_plane_create_zpos_immutable_property(&p->base, layer);
	if (!ret)
		ret = drm_plane_create_scaling_filter_property(&p->base,
				BIT(DRM_SCALING_FILTER_DEFAULT) |
				BIT(DRM_SCALING_FILTER_NEAREST_NEIGHBOR));
	if (!ret && !primary)
		ret = drm_plane_create_blend_mode_property(&p->base,
				BIT(DRM_MODE_BLEND_PREMULTI) | BIT(DRM_MODE_BLEND_COVERAGE));
	hc->planes[layer] = p;
	return ret;
}

/* ---- output timing ---- */

/*
 * Other refresh rates at the same 33 MHz clock: only the totals change, the
 * front porches take the difference. Exact rates for the emulated systems.
 */
static const u16 hc16xx_rates[][2] = {	/* htotal, vtotal */
	{ 914, 722 },	/* 50.007 Hz: PAL */
	{ 927, 596 },	/* 59.729: Game Boy, GBA */
	{ 935, 589 },	/* 59.922: Mega Drive */
	{ 945, 582 },	/* 60.000 */
	{ 937, 586 },	/* 60.099: NES */
	{ 736, 594 },	/* 75.483: WonderSwan (75.47) */
};

static void hc16xx_mode_set_totals(struct drm_display_mode *m, int htotal, int vtotal)
{
	int dh = htotal - m->htotal, dv = vtotal - m->vtotal;

	m->hsync_start += dh;
	m->hsync_end += dh;
	m->htotal = htotal;
	m->vsync_start += dv;
	m->vsync_end += dv;
	m->vtotal = vtotal;
}

/* Totals the panel takes: shortest porches, rate range, field widths. */
static bool hc16xx_totals_ok(const struct hc16xx_drm *hc, int htotal, int vtotal)
{
	const struct drm_display_mode *n = &hc->native;
	int hmin = n->htotal - (n->hsync_start - n->hdisplay) + PORCH_MIN;
	int vmin = n->vtotal - (n->vsync_start - n->vdisplay) + PORCH_MIN;
	u64 hz100 = div_u64((u64)n->clock * 100000, (u32)htotal * vtotal);

	return htotal >= hmin && htotal <= 0x7fff && vtotal >= vmin && vtotal <= 0xfff &&
	       hz100 >= REFRESH_MIN * 100 && hz100 <= REFRESH_MAX * 100;
}

/*
 * Programs the totals: +0x00 bit 2 cleared around the writes (the engine
 * takes them at once, no blanking), vtotal in all three places it lives.
 */
static void hc16xx_set_timing(struct hc16xx_drm *hc, int htotal, int vtotal)
{
	void __iomem *r = hc->regs;
	u32 ctl = readl(r + DE_CTL), ht = htotal * 2;

	if (readl(r + DE_TOTAL) == (vtotal << 16 | ht))
		return;
	writel(ctl & ~DE_CTL_UPDATE, r + DE_CTL);
	writel(vtotal << 16 | ht, r + DE_TOTAL);
	writel((readl(r + DE_HSYNC) & 0xffff0000) | ht, r + DE_HSYNC);
	writel((readl(r + DE_VSYNC) & 0xffff0000) | vtotal, r + DE_VSYNC);
	writel((readl(r + DE_VTOTAL2) & 0xffff) | vtotal << 16, r + DE_VTOTAL2);
	writel(ctl | DE_CTL_UPDATE, r + DE_CTL);
}

/* ---- CRTC ---- */

static enum drm_mode_status hc16xx_crtc_mode_valid(struct drm_crtc *crtc,
						   const struct drm_display_mode *mode)
{
	struct hc16xx_drm *hc = to_hc(crtc->dev);

	if (mode->hdisplay < 16 || mode->vdisplay < 16 ||
	    mode->hdisplay > hc->native.hdisplay || mode->vdisplay > hc->native.vdisplay)
		return MODE_BAD;
	/* The pixel clock is fixed; the totals set the rate. */
	if (mode->clock != hc->native.clock)
		return MODE_CLOCK_RANGE;
	if (!hc16xx_totals_ok(hc, mode->htotal, mode->vtotal))
		return MODE_BAD;
	return MODE_OK;
}

static int hc16xx_crtc_atomic_check(struct drm_crtc *crtc, struct drm_atomic_commit *state)
{
	struct drm_crtc_state *cs = drm_atomic_get_new_crtc_state(state, crtc);

	/*
	 * The panel always runs its native active area and porches; a mode
	 * only picks the totals (the refresh rate). Smaller logical modes are
	 * fitted onto the panel by the planes.
	 */
	if (cs->enable) {
		drm_mode_copy(&cs->adjusted_mode, &to_hc(crtc->dev)->native);
		hc16xx_mode_set_totals(&cs->adjusted_mode, cs->mode.htotal, cs->mode.vtotal);
		drm_mode_set_crtcinfo(&cs->adjusted_mode, 0);
	}
	return 0;
}

static void hc16xx_crtc_atomic_flush(struct drm_crtc *crtc, struct drm_atomic_commit *state)
{
	struct hc16xx_drm *hc = to_hc(crtc->dev);
	struct drm_crtc_state *cs = drm_atomic_get_new_crtc_state(state, crtc);
	struct drm_plane_state *p0 = hc->planes[0]->base.state, *p1 = hc->planes[1]->base.state;
	bool on0 = p0 && p0->visible, on1 = p1 && p1->visible;
	u32 split = on0 && on1 ? 0x2 : on1 ? 0x12 : 0xa;

	/* Line buffer split between the layers, as the vendor driver. */
	writel((readl(hc->regs + GMA_LINEBUF) & ~0x1f) | split, hc->regs + GMA_LINEBUF);

	/* Completed by the vblank interrupt once the hardware has latched it. */
	if (cs->event) {
		spin_lock_irq(&crtc->dev->event_lock);
		if (drm_crtc_vblank_get(crtc) == 0) {
			WARN_ON(hc->event);
			hc->event = cs->event;
			hc->event_age = 0;
		} else {
			drm_crtc_send_vblank_event(crtc, cs->event);
		}
		cs->event = NULL;
		spin_unlock_irq(&crtc->dev->event_lock);
	}
}

static void hc16xx_crtc_atomic_disable(struct drm_crtc *crtc, struct drm_atomic_commit *state)
{
	struct hc16xx_drm *hc = to_hc(crtc->dev);
	int i;

	for (i = 0; i < NUM_LAYERS; i++)
		hc16xx_layer_commit(hc, i, 0, GMA_CTL_ON, -1);
	drm_crtc_vblank_off(crtc);
	spin_lock_irq(&crtc->dev->event_lock);
	if (hc->event) {
		drm_crtc_send_vblank_event(crtc, hc->event);
		hc->event = NULL;
	}
	spin_unlock_irq(&crtc->dev->event_lock);
}

static void hc16xx_crtc_atomic_enable(struct drm_crtc *crtc, struct drm_atomic_commit *state)
{
	struct drm_display_mode *m = &crtc->state->adjusted_mode;

	hc16xx_set_timing(to_hc(crtc->dev), m->crtc_htotal, m->crtc_vtotal);
	drm_crtc_vblank_on(crtc);
}

/* Lines counted from the start of active video; negative in blanking. */
static bool hc16xx_crtc_get_scanout_position(struct drm_crtc *crtc, bool in_vblank_irq,
					     int *vpos, int *hpos, ktime_t *stime,
					     ktime_t *etime, const struct drm_display_mode *mode)
{
	struct hc16xx_drm *hc = to_hc(crtc->dev);
	int line;

	if (stime)
		*stime = ktime_get();
	line = (readl(hc->regs + DE_LINE) >> 16) & 0xfff;
	if (etime)
		*etime = ktime_get();
	if (line >= ACTIVE_FIRST_LINE + mode->crtc_vdisplay)
		line -= mode->crtc_vtotal;
	*vpos = line - ACTIVE_FIRST_LINE;
	*hpos = 0;
	return true;
}

static int hc16xx_enable_vblank(struct drm_crtc *crtc)
{
	struct hc16xx_drm *hc = to_hc(crtc->dev);

	/* Status bits written as 0: leave latched events alone. */
	writel((readl(hc->regs + DE_INT) & ~DE_INT_STATUS) | DE_IRQ_LINE1, hc->regs + DE_INT);
	return 0;
}

static void hc16xx_disable_vblank(struct drm_crtc *crtc)
{
	struct hc16xx_drm *hc = to_hc(crtc->dev);

	writel(readl(hc->regs + DE_INT) & ~(DE_INT_STATUS | DE_IRQ_LINE1), hc->regs + DE_INT);
}

static irqreturn_t hc16xx_irq(int irq, void *arg)
{
	struct hc16xx_drm *hc = arg;
	u32 v = readl(hc->regs + DE_INT), st = (v & DE_INT_STATUS) >> 8;

	if (!(st & v & DE_INT_ALL))
		return IRQ_NONE;
	writel((v & ~DE_INT_STATUS) | st << 8, hc->regs + DE_INT);	/* ack */
	if (st & DE_IRQ_LINE1) {
		drm_crtc_handle_vblank(&hc->crtc);
		spin_lock(&hc->drm.event_lock);
		/*
		 * Normally latched within a frame; if something else changed the
		 * layer registers it never matches - complete the flip anyway.
		 */
		if (hc->event && (hc16xx_latched(hc) || ++hc->event_age > 4)) {
			if (hc->event_age > 4)
				dev_warn_ratelimited(hc->drm.dev, "flip not latched after 4 frames\n");
			drm_crtc_send_vblank_event(&hc->crtc, hc->event);
			hc->event = NULL;
			drm_crtc_vblank_put(&hc->crtc);
		}
		spin_unlock(&hc->drm.event_lock);
	}
	return IRQ_HANDLED;
}

static const struct drm_crtc_helper_funcs hc16xx_crtc_helper_funcs = {
	.mode_valid	= hc16xx_crtc_mode_valid,
	.atomic_check	= hc16xx_crtc_atomic_check,
	.atomic_flush	= hc16xx_crtc_atomic_flush,
	.atomic_enable	= hc16xx_crtc_atomic_enable,
	.atomic_disable	= hc16xx_crtc_atomic_disable,
	.get_scanout_position = hc16xx_crtc_get_scanout_position,
};

static const struct drm_crtc_funcs hc16xx_crtc_funcs = {
	.set_config		= drm_atomic_helper_set_config,
	.page_flip		= drm_atomic_helper_page_flip,
	.reset			= drm_atomic_helper_crtc_reset,
	.atomic_duplicate_state	= drm_atomic_helper_crtc_duplicate_state,
	.atomic_destroy_state	= drm_atomic_helper_crtc_destroy_state,
	.enable_vblank		= hc16xx_enable_vblank,
	.disable_vblank		= hc16xx_disable_vblank,
	.get_vblank_timestamp	= drm_crtc_vblank_helper_get_vblank_timestamp,
};

/* ---- connector ---- */

/* Logical modes offered besides the native one (any size fits, though). */
static const u16 hc16xx_extra_modes[][2] = {
	{ 160, 144 }, { 240, 160 }, { 256, 192 }, { 256, 224 }, { 256, 240 },
	{ 320, 200 }, { 320, 224 }, { 320, 240 }, { 384, 224 }, { 400, 240 },
	{ 480, 272 }, { 512, 384 },
};

static int hc16xx_connector_get_modes(struct drm_connector *connector)
{
	struct hc16xx_drm *hc = to_hc(connector->dev);
	struct drm_display_mode *mode;
	int i, n = 0;

	mode = drm_mode_duplicate(connector->dev, &hc->native);
	if (!mode)
		return 0;
	mode->type = DRM_MODE_TYPE_DRIVER | DRM_MODE_TYPE_PREFERRED;
	drm_mode_probed_add(connector, mode);
	n++;
	/* Native size at other rates (pick by vrefresh / clock and totals). */
	for (i = 0; i < ARRAY_SIZE(hc16xx_rates); i++) {
		mode = drm_mode_duplicate(connector->dev, &hc->native);
		if (!mode)
			break;
		hc16xx_mode_set_totals(mode, hc16xx_rates[i][0], hc16xx_rates[i][1]);
		mode->type = DRM_MODE_TYPE_DRIVER;
		drm_mode_probed_add(connector, mode);
		n++;
	}
	for (i = 0; i < ARRAY_SIZE(hc16xx_extra_modes); i++) {
		mode = drm_mode_duplicate(connector->dev, &hc->native);
		if (!mode)
			break;
		/* Same timing (so the same refresh rate), smaller active area. */
		mode->hdisplay = hc16xx_extra_modes[i][0];
		mode->vdisplay = hc16xx_extra_modes[i][1];
		mode->type = DRM_MODE_TYPE_DRIVER;
		drm_mode_set_name(mode);
		drm_mode_probed_add(connector, mode);
		n++;
	}
	connector->display_info.width_mm = hc->width_mm;
	connector->display_info.height_mm = hc->height_mm;
	return n;
}

static int hc16xx_connector_atomic_check(struct drm_connector *connector,
					 struct drm_atomic_commit *state)
{
	struct drm_connector_state *old = drm_atomic_get_old_connector_state(state, connector);
	struct drm_connector_state *new = drm_atomic_get_new_connector_state(state, connector);
	struct drm_crtc_state *cs;

	/* A new scaling mode moves the planes: re-check and update them. */
	if (!new->crtc || old->scaling_mode == new->scaling_mode)
		return 0;
	cs = drm_atomic_get_crtc_state(state, new->crtc);
	if (IS_ERR(cs))
		return PTR_ERR(cs);
	return drm_atomic_add_affected_planes(state, new->crtc);
}

static void hc16xx_connector_reset(struct drm_connector *connector)
{
	drm_atomic_helper_connector_reset(connector);
	if (connector->state)
		connector->state->scaling_mode = DRM_MODE_SCALE_ASPECT;
}

static const struct drm_connector_helper_funcs hc16xx_connector_helper_funcs = {
	.get_modes	= hc16xx_connector_get_modes,
	.atomic_check	= hc16xx_connector_atomic_check,
};

static const struct drm_connector_funcs hc16xx_connector_funcs = {
	.reset			= hc16xx_connector_reset,
	.fill_modes		= drm_helper_probe_single_connector_modes,
	.atomic_duplicate_state	= drm_atomic_helper_connector_duplicate_state,
	.atomic_destroy_state	= drm_atomic_helper_connector_destroy_state,
};

/* ---- 2D engine ---- */

static void hc16xx_ge_init(struct hc16xx_drm *hc)
{
	void __iomem *sys = hc->sys;

	/* Clock first: any GE register access with it gated hangs the bus. */
	writel(readl(sys + SYS_CLK_GATE) & ~SYS_CLK_GATE_GE, sys + SYS_CLK_GATE);
	writel(readl(sys + SYS_RESET) | SYS_RESET_GE, sys + SYS_RESET);
	udelay(100);
	writel(readl(sys + SYS_RESET) & ~SYS_RESET_GE, sys + SYS_RESET);
	writel(readl(sys + SYS_CLK_SEL) | SYS_CLK_SEL_GE, sys + SYS_CLK_SEL);	/* 238 MHz */
	writel(0, hc->ge + GE_LAT_CFG);
	writel((readl(hc->ge + GE_CFG) & ~(GE_CFG_QUEUE | GE_CFG_HQ_INT)) | GE_CFG_IO_INT,
	       hc->ge + GE_CFG);
}

/* GE colour format (with the component-order bit) and bytes per pixel. */
static int hc16xx_ge_format(u32 fourcc, int *bpp)
{
	int f = hc16xx_hw_format(fourcc);	/* same codes as the layers */

	if (f < 0 || f == FMT_XRGB4444 || f == FMT_ARGB4444)
		return -EINVAL;
	*bpp = f <= FMT_ARGB8888 ? 4 : 2;
	return GE_TYPE_FMT(f) | (hc16xx_bgr(fourcc) ? GE_TYPE_BGR : 0);
}

/* A buffer's DMA address for an access of rows x bytes at offset/pitch. */
static int hc16xx_ge_buffer(struct drm_file *file, u32 handle, u32 offset, u32 pitch,
			    u32 rows, u32 bytes, dma_addr_t *addr, size_t *len,
			    struct drm_gem_object **obj)
{
	struct drm_gem_dma_object *dma;
	u64 end = (u64)offset + (u64)(rows - 1) * pitch + bytes;

	*obj = drm_gem_object_lookup(file, handle);
	if (!*obj)
		return -ENOENT;
	dma = to_drm_gem_dma_obj(*obj);
	if (end > (*obj)->size || dma->dma_addr + offset >= SZ_256M) {
		drm_gem_object_put(*obj);
		return -EINVAL;
	}
	*addr = dma->dma_addr + offset;
	*len = end - offset;
	return 0;
}

/*
 * Copy / turn / convert with the GE (tested on the device):
 * a turn is an affine pass whose inverse matrix maps destination to source
 * around the window centres; samples round to the nearest source pixel and
 * the mirrored axis needs a -1 offset to land exactly. 180 is a flipped
 * plain blit.
 */
static int hc16xx_blit_ioctl(struct drm_device *drm, void *data, struct drm_file *file)
{
	struct hc16xx_drm *hc = to_hc(drm);
	struct drm_hc16xx_blit *b = data;
	struct drm_gem_object *sobj, *dobj;
	dma_addr_t src, dst;
	size_t slen, dlen;
	int sfmt, dfmt, sbpp, dbpp, ret;
	bool turn = b->rotation == DRM_MODE_ROTATE_90 || b->rotation == DRM_MODE_ROTATE_270;
	u32 dw = turn ? b->height : b->width, dh = turn ? b->width : b->height;
	u32 m[6] = {}, st, ptn;

	if (!hc->ge)
		return -ENODEV;
	if (b->pad || !b->width || !b->height || b->width > 4095 || b->height > 4095 ||
	    (b->rotation & ~DRM_MODE_ROTATE_MASK) || hweight32(b->rotation) != 1)
		return -EINVAL;
	sfmt = hc16xx_ge_format(b->src_format, &sbpp);
	dfmt = hc16xx_ge_format(b->dst_format, &dbpp);
	if (sfmt < 0 || dfmt < 0 || b->src_pitch % sbpp || b->dst_pitch % dbpp ||
	    b->src_pitch / sbpp > 4095 || b->dst_pitch / dbpp > 4095 ||
	    b->src_pitch / sbpp < b->width || b->dst_pitch / dbpp < dw)
		return -EINVAL;
	ret = hc16xx_ge_buffer(file, b->src_handle, b->src_offset, b->src_pitch, b->height,
			       b->width * sbpp, &src, &slen, &sobj);
	if (ret)
		return ret;
	ret = hc16xx_ge_buffer(file, b->dst_handle, b->dst_offset, b->dst_pitch, dh,
			       dw * dbpp, &dst, &dlen, &dobj);
	if (ret)
		goto put_src;

	/* Dumb buffers are cached for the CPU: write the source back, drop
	 * the destination's lines (they must not be written over the result). */
	dma_sync_single_for_device(drm->dev, src, slen, DMA_TO_DEVICE);
	dma_sync_single_for_device(drm->dev, dst, dlen, DMA_FROM_DEVICE);

	ptn = (b->src_pitch / sbpp) | sfmt;
	switch (b->rotation) {
	case DRM_MODE_ROTATE_90:	/* counter-clockwise */
		m[1] = GE_MINUS_ONE;
		m[2] = GE_MINUS_ONE;
		m[3] = GE_ONE;
		break;
	case DRM_MODE_ROTATE_270:	/* clockwise */
		m[1] = GE_ONE;
		m[3] = GE_MINUS_ONE;
		m[5] = GE_MINUS_ONE;
		break;
	case DRM_MODE_ROTATE_180:
		ptn |= GE_TYPE_HFLIP | GE_TYPE_VFLIP;
		break;
	}
	if (turn)
		ptn |= GE_TYPE_AFFINE;

	mutex_lock(&hc->ge_lock);
	ret = readl_poll_timeout(hc->ge + GE_STATUS, st, !(st & GE_STATUS_BUSY), 20, 50000);
	if (ret) {
		hc16xx_ge_init(hc);	/* stuck: reset */
	}
	writel(turn ? GE_FUNC_AFFINE : GE_FUNC_BLIT, hc->ge + GE_FUNC);
	writel(dst, hc->ge + GE_DST_BA);
	writel((b->dst_pitch / dbpp) | dfmt, hc->ge + GE_DST_TYPE);
	writel(dst, hc->ge + GE_SRC_BA);		/* "src" = the destination surface */
	writel((b->dst_pitch / dbpp) | dfmt, hc->ge + GE_SRC_TYPE);
	writel(src, hc->ge + GE_PTN_BA);	/* "pattern" = the source */
	writel(ptn, hc->ge + GE_PTN_TYPE);
	writel(0, hc->ge + GE_DST_XY);
	writel(dh << 16 | dw, hc->ge + GE_DST_WH);
	writel(0, hc->ge + GE_SRC_XY);
	writel(0, hc->ge + GE_PTN_XY);
	writel(b->height << 16 | b->width, hc->ge + GE_PTN_WH);
	writel(GE_ROP_COPY, hc->ge + GE_ROP);
	writel(0, hc->ge + GE_DFB_COLOR);
	if (turn) {
		int i;

		writel(0, hc->ge + GE_MTX_CTL);	/* bilinear, window centres */
		for (i = 0; i < 6; i++)
			writel(m[i], hc->ge + GE_MTX(i));
	}
	writel(readl(hc->ge + GE_STATUS), hc->ge + GE_STATUS);
	writel((readl(hc->ge + GE_START) & ~3) | GE_START_IO, hc->ge + GE_START);
	ret = readl_poll_timeout(hc->ge + GE_STATUS, st, st & GE_STATUS_IO_DONE, 50, 100000);
	writel(st, hc->ge + GE_STATUS);
	writel(0, hc->ge + GE_PTN_TYPE);	/* the affine bit is sticky */
	mutex_unlock(&hc->ge_lock);
	if (ret)
		dev_warn_ratelimited(drm->dev, "2D engine timeout, status %#x\n", st);

	dma_sync_single_for_cpu(drm->dev, dst, dlen, DMA_FROM_DEVICE);
	drm_gem_object_put(dobj);
put_src:
	drm_gem_object_put(sobj);
	return ret;
}

static const struct drm_ioctl_desc hc16xx_ioctls[] = {
	DRM_IOCTL_DEF_DRV(HC16XX_BLIT, hc16xx_blit_ioctl, 0),
};

/* ---- sysfs: colour, background, panel power ---- */

static void hc16xx_write_enhance(struct hc16xx_drm *hc)
{
	int d, i;

	/* Every descriptor: the hardware re-reads them each frame. */
	for (d = 0; d < NUM_LAYERS * 2; d++)
		for (i = 0; i < DESC_ENHANCE_WORDS; i++)
			writel(hc->enhance[i],
			       hc->desc + DESC_OFFSET + d * DESC_SIZE + DESC_ENHANCE + 4 * i);
}

/* enhance: the 13 layer colour words (RGB -> YCbCr x512 with brightness,
 * contrast, saturation, hue folded in; word 12 sharpness -5..5). */
static ssize_t enhance_show(struct device *dev, struct device_attribute *attr, char *buf)
{
	struct hc16xx_drm *hc = dev_get_drvdata(dev);
	int i, n = 0;

	for (i = 0; i < DESC_ENHANCE_WORDS; i++)
		n += sysfs_emit_at(buf, n, "%d%c", (s32)hc->enhance[i],
				   i == DESC_ENHANCE_WORDS - 1 ? '\n' : ' ');
	return n;
}

static ssize_t enhance_store(struct device *dev, struct device_attribute *attr,
			     const char *buf, size_t len)
{
	struct hc16xx_drm *hc = dev_get_drvdata(dev);
	u32 v[DESC_ENHANCE_WORDS];
	char *s, *dup, *tok;
	int i = 0, ret = 0;

	dup = s = kstrndup(buf, len, GFP_KERNEL);
	if (!dup)
		return -ENOMEM;
	while ((tok = strsep(&s, " \t\n")) && i < DESC_ENHANCE_WORDS) {
		if (!*tok)
			continue;
		ret = kstrtoint(tok, 0, (int *)&v[i++]);
		if (ret)
			break;
	}
	kfree(dup);
	if (ret || i != DESC_ENHANCE_WORDS)
		return ret ?: -EINVAL;
	memcpy(hc->enhance, v, sizeof(v));
	hc16xx_write_enhance(hc);
	return len;
}
static DEVICE_ATTR_RW(enhance);

/* background: RRGGBB shown around and under the layers. */
static ssize_t background_show(struct device *dev, struct device_attribute *attr, char *buf)
{
	struct hc16xx_drm *hc = dev_get_drvdata(dev);

	return sysfs_emit(buf, "ycbcr %06x\n", readl(hc->regs + DE_BG) & 0xffffff);
}

static ssize_t background_store(struct device *dev, struct device_attribute *attr,
				const char *buf, size_t len)
{
	struct hc16xx_drm *hc = dev_get_drvdata(dev);
	int r, g, b, y, cb, cr, ret;
	u32 rgb;

	ret = kstrtou32(buf, 16, &rgb);
	if (ret)
		return ret;
	r = rgb >> 16 & 0xff;
	g = rgb >> 8 & 0xff;
	b = rgb & 0xff;
	/* BT.601, limited range (the boot value 0x108080 is black). */
	y = 16 + ((66 * r + 129 * g + 25 * b + 128) >> 8);
	cb = 128 + ((-38 * r - 74 * g + 112 * b + 128) >> 8);
	cr = 128 + ((112 * r - 94 * g - 18 * b + 128) >> 8);
	writel(y << 16 | cb << 8 | cr, hc->regs + DE_BG);
	return len;
}
static DEVICE_ATTR_RW(background);

static int hc16xx_dcs(struct hc16xx_drm *hc, u8 cmd)
{
	u32 st;

	writel(cmd << 8 | DCS_SHORT_WRITE, hc->dsi + DSI_GEN_HDR);
	return readl_poll_timeout(hc->dsi + DSI_CMD_STATUS, st, st & DSI_CMD_EMPTY, 10, 20000);
}

/*
 * panel: "off" = display off + sleep in (the panel's own circuits down,
 * the picture black; the backlight is separate), "on" = sleep out + on.
 * Writes only: a DCS read hangs the link until a power cycle.
 */
static ssize_t panel_show(struct device *dev, struct device_attribute *attr, char *buf)
{
	struct hc16xx_drm *hc = dev_get_drvdata(dev);

	return sysfs_emit(buf, "%s\n", hc->panel_off ? "off" : "on");
}

static ssize_t panel_store(struct device *dev, struct device_attribute *attr,
			   const char *buf, size_t len)
{
	struct hc16xx_drm *hc = dev_get_drvdata(dev);
	bool off;
	int ret = 0;

	if (sysfs_streq(buf, "off"))
		off = true;
	else if (sysfs_streq(buf, "on"))
		off = false;
	else
		return -EINVAL;
	if (!hc->dsi)
		return -ENODEV;
	mutex_lock(&hc->panel_lock);
	if (off != hc->panel_off) {
		if (off) {
			ret = hc16xx_dcs(hc, MIPI_DCS_SET_DISPLAY_OFF);
			msleep(20);
			ret = ret ?: hc16xx_dcs(hc, MIPI_DCS_ENTER_SLEEP_MODE);
		} else {
			ret = hc16xx_dcs(hc, MIPI_DCS_EXIT_SLEEP_MODE);
			msleep(120);	/* the panel's wake-up time */
			ret = ret ?: hc16xx_dcs(hc, MIPI_DCS_SET_DISPLAY_ON);
		}
		if (!ret)
			hc->panel_off = off;
	}
	mutex_unlock(&hc->panel_lock);
	return ret ?: len;
}
static DEVICE_ATTR_RW(panel);

static struct attribute *hc16xx_attrs[] = {
	&dev_attr_enhance.attr,
	&dev_attr_background.attr,
	&dev_attr_panel.attr,
	NULL
};
ATTRIBUTE_GROUPS(hc16xx);

/* ---- device ---- */

static struct drm_gem_object *hc16xx_gem_create_object(struct drm_device *drm, size_t size)
{
	struct drm_gem_dma_object *obj = kzalloc_obj(*obj);

	if (!obj)
		return ERR_PTR(-ENOMEM);
	/* Cached CPU mapping, flushed on commit: far faster for CPU drawing. */
	obj->map_noncoherent = true;
	return &obj->base;
}

static const struct drm_mode_config_funcs hc16xx_mode_config_funcs = {
	.fb_create	= drm_gem_fb_create_with_dirty,
	.atomic_check	= drm_atomic_helper_check,
	.atomic_commit	= drm_atomic_helper_commit,
};

DEFINE_DRM_GEM_DMA_FOPS(hc16xx_fops);

static const struct drm_driver hc16xx_driver = {
	.driver_features	= DRIVER_MODESET | DRIVER_GEM | DRIVER_ATOMIC,
	.name			= "hc16xx",
	.desc			= "HiChip HC16xx display",
	.major			= 1,
	.minor			= 0,
	.fops			= &hc16xx_fops,
	.gem_create_object	= hc16xx_gem_create_object,
	.ioctls			= hc16xx_ioctls,
	.num_ioctls		= ARRAY_SIZE(hc16xx_ioctls),
	DRM_GEM_DMA_DRIVER_OPS,
	DRM_FBDEV_DMA_DRIVER_OPS,
};

static int hc16xx_drm_probe(struct platform_device *pdev)
{
	struct device *dev = &pdev->dev;
	struct device_node *np = dev->of_node, *timing, *mem;
	struct reserved_mem *rmem;
	struct hc16xx_drm *hc;
	struct drm_device *drm;
	int ret, i, irq;

	hc = devm_drm_dev_alloc(dev, &hc16xx_driver, struct hc16xx_drm, drm);
	if (IS_ERR(hc))
		return PTR_ERR(hc);
	drm = &hc->drm;
	platform_set_drvdata(pdev, hc);

	hc->regs = devm_platform_ioremap_resource(pdev, 0);
	if (IS_ERR(hc->regs))
		return PTR_ERR(hc->regs);
	/*
	 * Optional: the 2D engine, the DSI host (panel commands) and the
	 * system controller (GE clock/reset). Mapped without claiming: the
	 * system controller is shared (pinctrl owns most of it).
	 */
	for (i = 1; i <= 3; i++) {
		struct resource *res = platform_get_resource(pdev, IORESOURCE_MEM, i);
		void __iomem *p = res ? devm_ioremap(dev, res->start, resource_size(res)) : NULL;

		if (i == 1)
			hc->ge = p;
		else if (i == 2)
			hc->dsi = p;
		else
			hc->sys = p;
	}
	if (!hc->sys)
		hc->ge = NULL;
	mutex_init(&hc->ge_lock);
	mutex_init(&hc->panel_lock);
	if (hc->ge)
		hc16xx_ge_init(hc);
	mem = of_parse_phandle(np, "memory-region", 0);
	rmem = mem ? of_reserved_mem_lookup(mem) : NULL;
	of_node_put(mem);
	if (!rmem || rmem->size < DESC_OFFSET + NUM_LAYERS * 2 * DESC_SIZE)
		return dev_err_probe(dev, -EINVAL, "memory-region for descriptors\n");
	hc->desc_phys = rmem->base;
	hc->desc = devm_ioremap(dev, rmem->base, rmem->size);
	if (!hc->desc)
		return -ENOMEM;
	/* Colour enhancement/CSC as the bootloader set it up. */
	for (i = 0; i < DESC_ENHANCE_WORDS; i++)
		hc->enhance[i] = readl(hc->desc + DESC_BOOT + DESC_ENHANCE + 4 * i);

	timing = of_get_child_by_name(np, "panel-timing");
	ret = timing ? of_get_drm_panel_display_mode(np, &hc->native, NULL) : -ENOENT;
	of_node_put(timing);
	if (ret)
		return dev_err_probe(dev, ret, "panel-timing\n");
	hc->native.type = DRM_MODE_TYPE_DRIVER | DRM_MODE_TYPE_PREFERRED;
	drm_mode_set_crtcinfo(&hc->native, 0);	/* adjusted_mode is a copy */
	of_property_read_u32(np, "width-mm", &hc->width_mm);
	of_property_read_u32(np, "height-mm", &hc->height_mm);

	ret = drmm_mode_config_init(drm);
	if (ret)
		return ret;
	drm->mode_config.min_width = 16;
	drm->mode_config.min_height = 16;
	drm->mode_config.max_width = 4095;
	drm->mode_config.max_height = 4095;
	drm->mode_config.preferred_depth = 24;
	drm->mode_config.funcs = &hc16xx_mode_config_funcs;

	for (i = 0; i < NUM_LAYERS; i++) {
		ret = hc16xx_plane_init(hc, i);
		if (ret)
			return ret;
	}
	ret = drmm_crtc_init_with_planes(drm, &hc->crtc, &hc->planes[0]->base, NULL,
					 &hc16xx_crtc_funcs, NULL);
	if (ret)
		return ret;
	drm_crtc_helper_add(&hc->crtc, &hc16xx_crtc_helper_funcs);

	ret = drm_simple_encoder_init(drm, &hc->encoder, DRM_MODE_ENCODER_DSI);
	if (ret)
		return ret;
	hc->encoder.possible_crtcs = drm_crtc_mask(&hc->crtc);

	ret = drmm_connector_init(drm, &hc->connector, &hc16xx_connector_funcs,
				  DRM_MODE_CONNECTOR_DSI, NULL);
	if (ret)
		return ret;
	drm_connector_helper_add(&hc->connector, &hc16xx_connector_helper_funcs);
	hc->connector.status = connector_status_connected;
	ret = drm_connector_attach_scaling_mode_property(&hc->connector,
			BIT(DRM_MODE_SCALE_FULLSCREEN) | BIT(DRM_MODE_SCALE_ASPECT) |
			BIT(DRM_MODE_SCALE_CENTER));
	if (ret)
		return ret;
	ret = drm_connector_attach_encoder(&hc->connector, &hc->encoder);
	if (ret)
		return ret;

	ret = drm_vblank_init(drm, 1);
	if (ret)
		return ret;
	/*
	 * The bootloader leaves sources enabled and their status latched
	 * (nobody acked them): all off and cleared before taking the line.
	 */
	irq = platform_get_irq(pdev, 0);
	if (irq < 0)
		return irq;
	writel((readl(hc->regs + DE_INT) & ~(DE_INT_STATUS | DE_INT_ALL)) | DE_INT_STATUS,
	       hc->regs + DE_INT);
	writel((readl(hc->regs + DE_LINE_CMP) & 0xffff) | VBLANK_LINE << 16,
	       hc->regs + DE_LINE_CMP);
	/* Overlay (layer 1) in front of the primary (layer 0). */
	writel((readl(hc->regs + DE_ORDER) & ~DE_ORDER_MASK) | DE_ORDER_PLANES,
	       hc->regs + DE_ORDER);
	for (i = 0; i < NUM_LAYERS; i++) {
		void __iomem *r = hc->regs + LAYER(i);

		hc->on[i] = readl(r + GMA_CTL) & GMA_CTL_ON;
		hc->dmba[i] = readl(r + GMA_DMBA);
	}
	ret = devm_request_irq(dev, irq, hc16xx_irq, 0, dev_name(dev), hc);
	if (ret)
		return ret;
	drm_mode_config_reset(drm);

	/* Take over from simple-framebuffer (the boot framebuffer). */
	ret = aperture_remove_all_conflicting_devices(hc16xx_driver.name);
	if (ret)
		dev_warn(dev, "removing the boot framebuffer: %d\n", ret);

	ret = drm_dev_register(drm, 0);
	if (ret)
		return ret;
	drm_client_setup(drm, NULL);
	dev_info(dev, "%ux%u panel, %d layers\n", hc->native.hdisplay, hc->native.vdisplay,
		 NUM_LAYERS);
	return 0;
}

static void hc16xx_drm_remove(struct platform_device *pdev)
{
	struct hc16xx_drm *hc = platform_get_drvdata(pdev);

	drm_dev_unplug(&hc->drm);
	drm_atomic_helper_shutdown(&hc->drm);
}

/*
 * Layers off before a kexec (the next kernel reuses these buffers' memory),
 * and the boot timing back: the next kernel takes the panel as found.
 */
static void hc16xx_drm_shutdown(struct platform_device *pdev)
{
	struct hc16xx_drm *hc = platform_get_drvdata(pdev);

	drm_atomic_helper_shutdown(&hc->drm);
	hc16xx_set_timing(hc, hc->native.htotal, hc->native.vtotal);
	if (hc->panel_off && hc->dsi) {
		hc16xx_dcs(hc, MIPI_DCS_EXIT_SLEEP_MODE);
		msleep(120);
		hc16xx_dcs(hc, MIPI_DCS_SET_DISPLAY_ON);
	}
}

static const struct of_device_id hc16xx_drm_of_match[] = {
	{ .compatible = "hichip,hc16xx-display" },
	{ }
};
MODULE_DEVICE_TABLE(of, hc16xx_drm_of_match);

static struct platform_driver hc16xx_drm_platform_driver = {
	.probe		= hc16xx_drm_probe,
	.remove		= hc16xx_drm_remove,
	.shutdown	= hc16xx_drm_shutdown,
	.driver		= {
		.name		= "hc16xx-drm",
		.of_match_table	= hc16xx_drm_of_match,
		.dev_groups	= hc16xx_groups,
	},
};
module_platform_driver(hc16xx_drm_platform_driver);

MODULE_DESCRIPTION("HiChip HC16xx display (GMA layers) DRM driver");
MODULE_LICENSE("GPL");
