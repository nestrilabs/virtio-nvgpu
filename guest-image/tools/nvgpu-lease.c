/*
 * nvgpu-lease -- take a DRM lease from the Wayland compositor and hand it to a
 * KMS program.
 *
 * The lease stage (TESTING.md stage 4) needs "a lease client": a Wayland
 * client of nvgpu-wl-guest that speaks wp_drm_lease_device_v1, receives the
 * lease fd (which the guest kernel adopts as a guest DRM file) and gives it to
 * kmscube / modetest / lease-flip. None of those speak the protocol, so this
 * does, and runs them as a child with the fd inherited:
 *
 *   nvgpu-lease --list                          connectors on offer, then exit
 *   nvgpu-lease [--connector NAME] [--timeout S] [--hold S] [-- CMD ARGS...]
 *
 * With CMD, the child gets the lease fd as NVGPU_LEASE_FD (and every "{fd}"
 * in its arguments is replaced by the number); NVGPU_LEASE_CONNECTOR and
 * NVGPU_LEASE_CONNECTOR_ID name what was leased. Put libnvgpu-shim.so in the
 * child's LD_PRELOAD to let path-only tools open /dev/dri/lease as the fd.
 * The lease lives as long as this process: when the child exits, the lease is
 * destroyed and the compositor gets its output back; the exit status is the
 * child's. Without CMD, the lease is held for --hold seconds (default: until
 * SIGINT/SIGTERM).
 *
 * Exit status: the child's; 2 usage; 3 no lease device / no connector;
 * 4 the lease was refused (finished before lease_fd) or timed out.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <wayland-client.h>

#include "drm-lease-v1-client-protocol.h"

#define MAX_CONN 32

struct conn {
	struct wp_drm_lease_connector_v1 *obj;
	char name[64];
	char desc[128];
	uint32_t id;
	int withdrawn;
};

static struct wp_drm_lease_device_v1 *device;
static int device_done;
static int device_released;
static struct conn conns[MAX_CONN];
static int nconns;
static int lease_fd = -1;
static int lease_finished;
static volatile sig_atomic_t stop_sig;

static void die(int code, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
static void die(int code, const char *fmt, ...)
{
	va_list ap;

	va_start(ap, fmt);
	fputs("nvgpu-lease: ", stderr);
	vfprintf(stderr, fmt, ap);
	fputc('\n', stderr);
	va_end(ap);
	exit(code);
}

static double now_s(void)
{
	struct timespec t;

	clock_gettime(CLOCK_MONOTONIC, &t);
	return t.tv_sec + t.tv_nsec / 1e9;
}

/* ---- wp_drm_lease_connector_v1 ---- */
static struct conn *conn_of(struct wp_drm_lease_connector_v1 *c)
{
	for (int i = 0; i < nconns; i++)
		if (conns[i].obj == c)
			return &conns[i];
	return NULL;
}

static void c_name(void *d, struct wp_drm_lease_connector_v1 *c, const char *name)
{
	struct conn *k = conn_of(c);
	(void)d;
	if (k)
		snprintf(k->name, sizeof k->name, "%s", name);
}

static void c_desc(void *d, struct wp_drm_lease_connector_v1 *c, const char *desc)
{
	struct conn *k = conn_of(c);
	(void)d;
	if (k)
		snprintf(k->desc, sizeof k->desc, "%s", desc);
}

static void c_id(void *d, struct wp_drm_lease_connector_v1 *c, uint32_t id)
{
	struct conn *k = conn_of(c);
	(void)d;
	if (k)
		k->id = id;
}

static void c_done(void *d, struct wp_drm_lease_connector_v1 *c)
{
	(void)d;
	(void)c;
}

static void c_withdrawn(void *d, struct wp_drm_lease_connector_v1 *c)
{
	struct conn *k = conn_of(c);
	(void)d;
	if (k) {
		k->withdrawn = 1;
		fprintf(stderr, "nvgpu-lease: connector %s withdrawn\n", k->name);
	}
}

static const struct wp_drm_lease_connector_v1_listener conn_listener = {
	.name = c_name,
	.description = c_desc,
	.connector_id = c_id,
	.done = c_done,
	.withdrawn = c_withdrawn,
};

/* ---- wp_drm_lease_device_v1 ---- */
static void d_drm_fd(void *d, struct wp_drm_lease_device_v1 *dev, int32_t fd)
{
	(void)d;
	(void)dev;
	/* A non-master fd for the device; nothing here needs it. */
	fprintf(stderr, "nvgpu-lease: device drm_fd received (fd %d)\n", fd);
	close(fd);
}

static void d_connector(void *d, struct wp_drm_lease_device_v1 *dev,
			struct wp_drm_lease_connector_v1 *c)
{
	(void)d;
	(void)dev;
	if (nconns >= MAX_CONN) {
		wp_drm_lease_connector_v1_destroy(c);
		return;
	}
	memset(&conns[nconns], 0, sizeof conns[nconns]);
	conns[nconns].obj = c;
	nconns++;
	wp_drm_lease_connector_v1_add_listener(c, &conn_listener, NULL);
}

static void d_done(void *d, struct wp_drm_lease_device_v1 *dev)
{
	(void)d;
	(void)dev;
	device_done = 1;
}

static void d_released(void *d, struct wp_drm_lease_device_v1 *dev)
{
	(void)d;
	(void)dev;
	device_released = 1;
	fprintf(stderr, "nvgpu-lease: lease device released by the compositor\n");
}

static const struct wp_drm_lease_device_v1_listener device_listener = {
	.drm_fd = d_drm_fd,
	.connector = d_connector,
	.done = d_done,
	.released = d_released,
};

/* ---- wp_drm_lease_v1 ---- */
static void l_fd(void *d, struct wp_drm_lease_v1 *l, int32_t fd)
{
	(void)d;
	(void)l;
	lease_fd = fd;
}

static void l_finished(void *d, struct wp_drm_lease_v1 *l)
{
	(void)d;
	(void)l;
	lease_finished = 1;
}

static const struct wp_drm_lease_v1_listener lease_listener = {
	.lease_fd = l_fd,
	.finished = l_finished,
};

/* ---- registry ---- */
static void r_global(void *d, struct wl_registry *reg, uint32_t name,
		     const char *iface, uint32_t version)
{
	(void)d;
	if (!device && !strcmp(iface, wp_drm_lease_device_v1_interface.name)) {
		device = wl_registry_bind(reg, name, &wp_drm_lease_device_v1_interface, 1);
		wp_drm_lease_device_v1_add_listener(device, &device_listener, NULL);
		fprintf(stderr, "nvgpu-lease: bound %s v%u (of %u)\n", iface, 1u, version);
	}
}

static void r_global_remove(void *d, struct wl_registry *reg, uint32_t name)
{
	(void)d;
	(void)reg;
	(void)name;
}

static const struct wl_registry_listener registry_listener = {
	.global = r_global,
	.global_remove = r_global_remove,
};

/* Dispatch whatever arrives within `ms`; -1 on a dead connection. */
static int pump(struct wl_display *dpy, int ms)
{
	struct pollfd p = {.fd = wl_display_get_fd(dpy), .events = POLLIN};
	int r;

	while (wl_display_prepare_read(dpy) != 0)
		if (wl_display_dispatch_pending(dpy) < 0)
			return -1;
	if (wl_display_flush(dpy) < 0 && errno != EAGAIN) {
		wl_display_cancel_read(dpy);
		return -1;
	}
	r = poll(&p, 1, ms);
	if (r > 0 && (p.revents & POLLIN)) {
		if (wl_display_read_events(dpy) < 0)
			return -1;
	} else {
		wl_display_cancel_read(dpy);
		if (r > 0 && (p.revents & (POLLERR | POLLHUP)))
			return -1;
	}
	return wl_display_dispatch_pending(dpy) < 0 ? -1 : 0;
}

static void on_signal(int s)
{
	stop_sig = s;
}

static void usage(void)
{
	fprintf(stderr,
		"usage: nvgpu-lease --list\n"
		"       nvgpu-lease [--connector NAME] [--timeout SECS] [--hold SECS] [-- CMD ARGS...]\n");
	exit(2);
}

int main(int argc, char **argv)
{
	const char *want = NULL;
	double timeout = 15, hold = -1;
	int list = 0, cmd_at = -1;
	struct wl_display *dpy;
	struct wl_registry *reg;
	struct conn *pick = NULL;
	struct wp_drm_lease_request_v1 *req;
	struct wp_drm_lease_v1 *lease;
	double t0;
	pid_t child = -1;
	int status = 0, rc;

	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--list"))
			list = 1;
		else if (!strcmp(argv[i], "--connector") && i + 1 < argc)
			want = argv[++i];
		else if (!strcmp(argv[i], "--timeout") && i + 1 < argc)
			timeout = atof(argv[++i]);
		else if (!strcmp(argv[i], "--hold") && i + 1 < argc)
			hold = atof(argv[++i]);
		else if (!strcmp(argv[i], "--")) {
			cmd_at = i + 1;
			break;
		} else
			usage();
	}
	if (cmd_at >= argc)
		usage();

	dpy = wl_display_connect(NULL);
	if (!dpy)
		die(3, "cannot connect to the Wayland display (WAYLAND_DISPLAY=%s): %s",
		    getenv("WAYLAND_DISPLAY") ? getenv("WAYLAND_DISPLAY") : "(unset)",
		    strerror(errno));
	reg = wl_display_get_registry(dpy);
	wl_registry_add_listener(reg, &registry_listener, NULL);
	wl_display_roundtrip(dpy);
	if (!device)
		die(3, "the compositor offers no wp_drm_lease_device_v1 "
		       "(host started without --wayland-lease, or no leasable output)");

	t0 = now_s();
	while (!device_done && now_s() - t0 < timeout)
		if (pump(dpy, 200) < 0)
			die(3, "Wayland connection lost while listing connectors");
	/* The connectors' own events follow the device's done in the same burst. */
	wl_display_roundtrip(dpy);
	if (!device_done)
		die(3, "lease device never sent done within %.0fs", timeout);

	for (int i = 0; i < nconns; i++) {
		struct conn *k = &conns[i];
		printf("connector %s id=%u%s \"%s\"\n", k->name, k->id,
		       k->withdrawn ? " (withdrawn)" : "", k->desc);
		if (!pick && !k->withdrawn && (!want || !strcmp(want, k->name)))
			pick = k;
	}
	fflush(stdout);
	if (list)
		return nconns ? 0 : 3;
	if (!pick)
		die(3, "no %s connector on offer (%d listed)", want ? want : "leasable", nconns);

	req = wp_drm_lease_device_v1_create_lease_request(device);
	wp_drm_lease_request_v1_request_connector(req, pick->obj);
	lease = wp_drm_lease_request_v1_submit(req);
	wp_drm_lease_v1_add_listener(lease, &lease_listener, NULL);
	fprintf(stderr, "nvgpu-lease: requested %s (id %u)\n", pick->name, pick->id);

	t0 = now_s();
	while (lease_fd < 0 && !lease_finished && now_s() - t0 < timeout)
		if (pump(dpy, 200) < 0)
			die(4, "Wayland connection lost waiting for the lease");
	if (lease_fd < 0)
		die(4, "%s", lease_finished ? "lease refused (finished before lease_fd)"
					   : "no lease_fd within the timeout");
	printf("lease-fd %d connector %s id=%u after %.3fs\n", lease_fd, pick->name,
	       pick->id, now_s() - t0);
	fflush(stdout);

	signal(SIGINT, on_signal);
	signal(SIGTERM, on_signal);
	signal(SIGHUP, on_signal);

	if (cmd_at > 0) {
		char fdbuf[16], idbuf[16];

		snprintf(fdbuf, sizeof fdbuf, "%d", lease_fd);
		snprintf(idbuf, sizeof idbuf, "%u", pick->id);
		child = fork();
		if (child < 0)
			die(1, "fork: %s", strerror(errno));
		if (child == 0) {
			char **av = calloc((size_t)(argc - cmd_at + 1), sizeof *av);

			signal(SIGINT, SIG_DFL);
			signal(SIGTERM, SIG_DFL);
			signal(SIGHUP, SIG_DFL);
			/* The lease fd crosses exec; the Wayland socket does not need to. */
			fcntl(lease_fd, F_SETFD, 0);
			fcntl(wl_display_get_fd(dpy), F_SETFD, FD_CLOEXEC);
			setenv("NVGPU_LEASE_FD", fdbuf, 1);
			setenv("NVGPU_LEASE_CONNECTOR", pick->name, 1);
			setenv("NVGPU_LEASE_CONNECTOR_ID", idbuf, 1);
			for (int i = cmd_at; i < argc; i++) {
				const char *a = argv[i];
				char *at = strstr(a, "{fd}");

				if (at) {
					size_t n = strlen(a) + strlen(fdbuf) + 1;
					char *s = malloc(n);

					snprintf(s, n, "%.*s%s%s", (int)(at - a), a, fdbuf, at + 4);
					av[i - cmd_at] = s;
				} else
					av[i - cmd_at] = (char *)a;
			}
			execvp(av[0], av);
			fprintf(stderr, "nvgpu-lease: exec %s: %s\n", av[0], strerror(errno));
			_exit(127);
		}
	}

	t0 = now_s();
	rc = 0;
	for (;;) {
		if (stop_sig) {
			if (child > 0)
				kill(child, stop_sig);
			else
				break;
			stop_sig = 0;
		}
		if (child > 0) {
			pid_t w = waitpid(child, &status, WNOHANG);

			if (w == child) {
				rc = WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
				fprintf(stderr, "nvgpu-lease: child exited %s %d\n",
					WIFEXITED(status) ? "with" : "on signal",
					WIFEXITED(status) ? WEXITSTATUS(status) : WTERMSIG(status));
				break;
			}
		} else if (hold >= 0 && now_s() - t0 >= hold)
			break;
		if (pump(dpy, 100) < 0) {
			fprintf(stderr, "nvgpu-lease: Wayland connection lost; the lease is gone\n");
			if (child > 0) {
				kill(child, SIGTERM);
				waitpid(child, &status, 0);
			}
			return 4;
		}
		if (lease_finished) {
			fprintf(stderr, "nvgpu-lease: the compositor finished the lease\n");
			lease_finished = 0;
			if (child < 0)
				break;
		}
	}

	wp_drm_lease_v1_destroy(lease);
	close(lease_fd);
	wl_display_roundtrip(dpy);
	fprintf(stderr, "nvgpu-lease: lease released\n");
	wl_display_disconnect(dpy);
	return rc;
}
