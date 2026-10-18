/*
 * ps1gpu: PCSX-ReARMed's unai rasteriser outside the emulator, behind seven
 * C entry points - for running PS1 drawing on the GB350's second core (fed
 * through corelink) and for replaying captures (gpulib patch 0004). Built
 * against out/build/pcsx_rearmed (GPL, as the core), new renderer only
 * (GPU_UNAI_NO_OLD), integer maths.
 */
#include <stdint.h>
#include <string.h>
#include "plugins/gpulib/gpu.h"
#include "frontend/plugin_lib.h"

/* The gpulib state unai reads: vram, status, screen, state.downscale. */
struct psx_gpu gpu;

/* gpulib_if.cpp's hooks for the emulator's split drawing (libretro.c there):
 * none here. */
extern "C" const void *gb350_split = NULL;
extern "C" {
struct gb350_split_shared { uint32_t area_unmarked; int32_t x0, y0, x1, y1; } gb350_split_shared;
}

/* From gpulib/gpu.c: GP0 command lengths (extra words). */
extern "C" const unsigned char cmd_lengths[256] =
{
	0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	3, 3, 3, 3, 6, 6, 6, 6, 4, 4, 4, 4, 8, 8, 8, 8, // 20
	5, 5, 5, 5, 8, 8, 8, 8, 7, 7, 7, 7, 11, 11, 11, 11,
	2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 3, // 40
	3, 3, 3, 3, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4, 4,
	2, 2, 2, 2, 3, 3, 3, 3, 1, 1, 1, 1, 0, 0, 0, 0, // 60
	1, 1, 1, 1, 2, 2, 2, 2, 1, 1, 1, 1, 2, 2, 2, 2,
	3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, // 80
	3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
	2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, // a0
	2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
	2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, // c0
	2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
	0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // e0
	0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
};

/* From gpulib/gpu.c. */
void cpy_mask(uint16_t *dst, const uint16_t *src, int l, uint32_t r6)
{
  int i;
  if (r6 == 1) {
    for (i = 0; i < l; i++)
      dst[i] = src[i] | 0x8000;
  } else {
    uint16_t msb = r6 << 15;
    for (i = 0; i < l; i++) {
      uint16_t mask = (int16_t)dst[i] >> 15;
      dst[i] = (dst[i] & mask) | ((src[i] | msb) & ~mask);
    }
  }
}

static void vram_copy(uint16_t *vram, const uint32_t *ex_regs, const uint32_t *params)
{
  const uint32_t sx = params[1] & 0x3FF, sy = (params[1] >> 16) & 0x1FF;
  const uint32_t dx = params[2] & 0x3FF, dy = (params[2] >> 16) & 0x1FF;
  uint32_t w = ((params[3] - 1) & 0x3FF) + 1;
  uint32_t h = (((params[3] >> 16) - 1) & 0x1FF) + 1;
  uint16_t msb = ex_regs[6] << 15;
  uint32_t x, y;

  for (y = 0; y < h; y++) {
    uint16_t *src = vram + ((sy + y) & 0x1ff) * 1024;
    uint16_t *dst = vram + ((dy + y) & 0x1ff) * 1024;
    if ((sx < dx && dx < sx + w) || sx + w > 1024 || dx + w > 1024 || msb) {
      uint16_t lbuf[1024];
      for (x = 0; x < w; x++)
        lbuf[x] = src[(sx + x) & 0x3ff];
      for (x = 0; x < w; x++)
        dst[(dx + x) & 0x3ff] = lbuf[x] | msb;
    } else {
      memcpy(dst + dx, src + sx, w * 2);
    }
  }
}

extern "C" {

/* The emulator's log (psx_gpu prints its configuration). */
void SysPrintf(const char *fmt, ...)
{
}

/* vram: 1024 x 512 16-bit pixels. Drawing options as the GB350 emulator's. */
void ps1gpu_init(uint16_t *vram)
{
  struct rearmed_cbs cbs;

  memset(&gpu, 0, sizeof(gpu));
  gpu.vram = vram;
  gpu.screen.hres = gpu.screen.w = 320;
  gpu.screen.vres = gpu.screen.h = 240;
  renderer_init();
  memset(&cbs, 0, sizeof(cbs));
  cbs.gpu_unai.lighting = 1;
  cbs.gpu_unai.fast_lighting = 1;
  cbs.gpu_unai.blending = 1;
  cbs.dithering = 0;
  renderer_set_config(&cbs);
}

void renderer_set_split(int shift, unsigned skip);
int renderer_set_split_state(const uint32_t *words, int n);

/* Two cores sharing the drawing: VRAM rows in stripes of 2^shift rows;
 * skip the stripes whose bit (stripe & 31) is set in skip (0: draw all). */
void ps1gpu_split(int shift, unsigned skip)
{
#ifndef PS1GPU_NO_UNAI
  renderer_set_split(shift, skip);
#endif
}

/* The main core's unai options (renderer_split_state there). */
void ps1gpu_state(const uint32_t *words, int n)
{
#ifndef PS1GPU_NO_UNAI
  renderer_set_split_state(words, n);
#endif
}

/* GP0 E1..E6 as words [0..8) (E1 at [1]). */
void ps1gpu_ex_regs(const uint32_t *ex)
{
  memcpy(gpu.ex_regs, ex, sizeof(gpu.ex_regs));
  renderer_sync_ecmds(gpu.ex_regs);
}

/* A command list (drawing, fills, E-commands); returns words consumed. */
int ps1gpu_cmd(uint32_t *list, int n)
{
  int sum = 0, last = 0, cmd = 0;
  return renderer_do_cmd_list(list, n, gpu.ex_regs, &sum, &last, &cmd);
}

void ps1gpu_vram_write(int x, int y, const uint16_t *pixels, int l, uint32_t r6)
{
  uint16_t *dst = gpu.vram + (y & 511) * 1024 + x;
  if (r6)
    cpy_mask(dst, pixels, l, r6);
  else
    memcpy(dst, pixels, l * 2);
}

void ps1gpu_vram_copy(const uint32_t *params)
{
  vram_copy(gpu.vram, gpu.ex_regs, params);
}

}
