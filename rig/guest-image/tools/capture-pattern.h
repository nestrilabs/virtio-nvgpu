/* SPDX-License-Identifier: Apache-2.0 */
/*
 * The picture the capture test paints on the host (rig/rig-tools/
 * nvgpu-inject-test.c) and checks in the guest (nvgpu-capture-import.c):
 * every pixel a function of its position and the frame number, so a guest
 * that reads any pixel knows what it must be, and a checksum over the whole
 * image says the two sides agree.
 *
 * Rows are in memory order: row 0 is the first row of the buffer, which is
 * gl_FragCoord.y 0.5 when the buffer is a GL framebuffer's attachment and
 * the first row a Vulkan copy writes.
 */
#ifndef CAPTURE_PATTERN_H
#define CAPTURE_PATTERN_H

#include <stdint.h>

static inline void cap_pixel(uint32_t x, uint32_t y, uint32_t frame,
                             uint8_t rgba[4])
{
	rgba[0] = (uint8_t)(x + 3u * frame);
	rgba[1] = (uint8_t)(y + 5u * frame);
	rgba[2] = (uint8_t)((x ^ y) + 7u * frame);
	rgba[3] = 255;
}

/* The same in GLSL ES 3.00, for a fragment shader with `uniform uint frame`. */
#define CAP_PATTERN_GLSL                                                       \
	"uvec2 p = uvec2(gl_FragCoord.xy);\n"                                  \
	"uint r = (p.x + 3u * frame) & 255u;\n"                                \
	"uint g = (p.y + 5u * frame) & 255u;\n"                                \
	"uint b = ((p.x ^ p.y) + 7u * frame) & 255u;\n"                        \
	"o = vec4(float(r), float(g), float(b), 255.0) / 255.0;\n"

/* FNV-1a over an image's RGBA bytes, row by row, alpha taken as 255 (an
 * XRGB buffer's X is undefined). */
static inline uint32_t cap_fnv(const uint8_t *rgba, uint32_t w, uint32_t h,
                               uint32_t stride)
{
	uint32_t hsh = 2166136261u;
	for (uint32_t y = 0; y < h; y++) {
		const uint8_t *row = rgba + (uint64_t)y * stride;
		for (uint32_t x = 0; x < w; x++) {
			for (int c = 0; c < 4; c++) {
				hsh ^= c == 3 ? 255 : row[x * 4 + c];
				hsh *= 16777619u;
			}
		}
	}
	return hsh;
}

/* The checksum the pattern itself has, for a w x h image of frame f. */
static inline uint32_t cap_expected_fnv(uint32_t w, uint32_t h, uint32_t f)
{
	uint32_t hsh = 2166136261u;
	uint8_t px[4];
	for (uint32_t y = 0; y < h; y++)
		for (uint32_t x = 0; x < w; x++) {
			cap_pixel(x, y, f, px);
			for (int c = 0; c < 4; c++) {
				hsh ^= px[c];
				hsh *= 16777619u;
			}
		}
	return hsh;
}

/* Pixels of an RGBA image that are not frame f's (alpha not compared). */
static inline uint64_t cap_mismatches(const uint8_t *rgba, uint32_t w,
                                      uint32_t h, uint32_t stride, uint32_t f)
{
	uint64_t bad = 0;
	uint8_t px[4];
	for (uint32_t y = 0; y < h; y++) {
		const uint8_t *row = rgba + (uint64_t)y * stride;
		for (uint32_t x = 0; x < w; x++) {
			cap_pixel(x, y, f, px);
			if (row[x * 4] != px[0] || row[x * 4 + 1] != px[1] ||
			    row[x * 4 + 2] != px[2])
				bad++;
		}
	}
	return bad;
}

#endif
