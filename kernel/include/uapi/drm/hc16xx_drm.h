/* SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note */
/*
 * HiChip HC16xx display (hc16xx-drm) driver-private ioctls.
 */
#ifndef _UAPI_HC16XX_DRM_H_
#define _UAPI_HC16XX_DRM_H_

#include "drm.h"

#if defined(__cplusplus)
extern "C" {
#endif

/*
 * DRM_IOCTL_HC16XX_BLIT: copy a rectangle between two dumb buffers with the
 * SoC's 2D engine, optionally turned (DRM_MODE_ROTATE_0/90/180/270,
 * counter-clockwise) and converted between formats. The destination is
 * width x height, or height x width when turned by 90 / 270. Waits for the
 * engine (about 1.5 ms for 256x224 turned).
 */
struct drm_hc16xx_blit {
	__u32 src_handle;	/* GEM handle */
	__u32 src_offset;	/* bytes to the rectangle's first pixel */
	__u32 src_pitch;	/* bytes per row */
	__u32 src_format;	/* DRM_FORMAT_* */
	__u32 width;		/* source rectangle */
	__u32 height;
	__u32 dst_handle;
	__u32 dst_offset;
	__u32 dst_pitch;
	__u32 dst_format;
	__u32 rotation;		/* DRM_MODE_ROTATE_* */
	__u32 pad;
};

#define DRM_HC16XX_BLIT		0x00

#define DRM_IOCTL_HC16XX_BLIT	DRM_IOW(DRM_COMMAND_BASE + DRM_HC16XX_BLIT, struct drm_hc16xx_blit)

#if defined(__cplusplus)
}
#endif

#endif
