/*
 * vptr -- one virtual pointer that lives as long as the program, driven from
 * stdin, for the rig's headless compositor (scripts/rig-app-check.sh).
 *
 * wlrctl makes a virtual pointer per command and destroys it at once, and a
 * seat whose only pointer comes and goes that fast gives no client a
 * wl_pointer that ever sees a motion. This keeps one for the whole run.
 *
 * Commands, one per line:
 *   abs X Y         move to (X, Y) in output pixels (1920x1080 extent)
 *   move DX DY      relative motion
 *   click [BTN]     press and release (default 272, BTN_LEFT)
 *   wheel N         N vertical wheel steps
 *   sleep MS
 */
#define _GNU_SOURCE
#include <linux/input-event-codes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>
#include "wlr-virtual-pointer-unstable-v1-client-protocol.h"

static struct zwlr_virtual_pointer_manager_v1 *mgr;
static struct wl_seat *seat;

static void global(void *d, struct wl_registry *r, uint32_t name, const char *iface, uint32_t v)
{
	(void)d;
	if (!strcmp(iface, zwlr_virtual_pointer_manager_v1_interface.name))
		mgr = wl_registry_bind(r, name, &zwlr_virtual_pointer_manager_v1_interface, 1);
	else if (!strcmp(iface, wl_seat_interface.name) && !seat)
		seat = wl_registry_bind(r, name, &wl_seat_interface, v < 7 ? v : 7);
}
static void global_remove(void *d, struct wl_registry *r, uint32_t n) { (void)d; (void)r; (void)n; }
static const struct wl_registry_listener reg = { global, global_remove };

static uint32_t now(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

int main(void)
{
	struct wl_display *dpy = wl_display_connect(NULL);
	if (!dpy) { perror("wl_display_connect"); return 1; }
	struct wl_registry *r = wl_display_get_registry(dpy);
	wl_registry_add_listener(r, &reg, NULL);
	wl_display_roundtrip(dpy);
	if (!mgr) { fprintf(stderr, "vptr: no zwlr_virtual_pointer_manager_v1\n"); return 1; }
	struct zwlr_virtual_pointer_v1 *p = zwlr_virtual_pointer_manager_v1_create_virtual_pointer(mgr, seat);
	wl_display_roundtrip(dpy);
	fprintf(stderr, "vptr: ready\n");

	char line[256];
	while (fgets(line, sizeof line, stdin)) {
		int a = 0, b = 0;
		if (sscanf(line, "abs %d %d", &a, &b) == 2) {
			zwlr_virtual_pointer_v1_motion_absolute(p, now(), a, b, 1920, 1080);
		} else if (sscanf(line, "move %d %d", &a, &b) == 2) {
			zwlr_virtual_pointer_v1_motion(p, now(), wl_fixed_from_int(a), wl_fixed_from_int(b));
		} else if (!strncmp(line, "click", 5)) {
			a = BTN_LEFT;
			sscanf(line, "click %d", &a);
			zwlr_virtual_pointer_v1_button(p, now(), a, WL_POINTER_BUTTON_STATE_PRESSED);
			zwlr_virtual_pointer_v1_frame(p);
			wl_display_flush(dpy);
			usleep(50000);
			zwlr_virtual_pointer_v1_button(p, now(), a, WL_POINTER_BUTTON_STATE_RELEASED);
		} else if (sscanf(line, "wheel %d", &a) == 1) {
			zwlr_virtual_pointer_v1_axis_source(p, WL_POINTER_AXIS_SOURCE_WHEEL);
			zwlr_virtual_pointer_v1_axis_discrete(p, now(), WL_POINTER_AXIS_VERTICAL_SCROLL,
							      wl_fixed_from_int(15 * a), a);
		} else if (sscanf(line, "sleep %d", &a) == 1) {
			wl_display_flush(dpy);
			usleep(a * 1000);
			continue;
		} else {
			continue;
		}
		zwlr_virtual_pointer_v1_frame(p);
		wl_display_roundtrip(dpy);
	}
	/* Stay until killed: the pointer is the seat's for as long as we live. */
	while (wl_display_dispatch(dpy) != -1)
		;
	return 0;
}
