// SPDX-License-Identifier: Apache-2.0
/*
 * libnvgpu-shim.so -- LD_PRELOAD glue so stock KMS tools can drive a lease.
 *
 * A DRM lease reaches a guest program as a descriptor (nvgpu-lease hands it
 * down), but kmscube, modetest and drm_info only know how to open a path.
 * Opening the guest card node again would give a fresh card file, not the
 * lease. So with NVGPU_LEASE_FD=N in the environment, opening the path
 * NVGPU_LEASE_PATH (default /dev/dri/lease) returns dup(N) instead.
 *
 * Second, modetest's -D takes a *bus id*, not a path: libdrm's drmOpen() hands
 * the string to drmOpenByBusid(), which walks /dev/dri/card* comparing bus
 * ids, so `modetest -D /dev/dri/card0` (as TESTING.md and kms-smoke.sh write
 * it) finds nothing. drmOpen()/drmOpenWithType() here treat a bus id that
 * starts with '/' as a path and open it.
 *
 *   NVGPU_LEASE_FD=5 LD_PRELOAD=/opt/nvgpu/libnvgpu-shim.so kmscube -D /dev/dri/lease
 *   LD_PRELOAD=/opt/nvgpu/libnvgpu-shim.so modetest -D /dev/dri/card0 -c
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* -2: not ours, carry on with the real open. Otherwise the result. */
static int lease_open(const char *path, int flags)
{
	const char *want = getenv("NVGPU_LEASE_PATH");
	const char *fds = getenv("NVGPU_LEASE_FD");
	char *end;
	long fd;

	if (!want || !*want)
		want = "/dev/dri/lease";
	if (!path || !fds || strcmp(path, want) != 0)
		return -2;
	fd = strtol(fds, &end, 10);
	if (*fds == '\0' || *end != '\0' || fd < 0) {
		errno = EBADF;
		return -1;
	}
	return fcntl((int)fd, (flags & O_CLOEXEC) ? F_DUPFD_CLOEXEC : F_DUPFD, 0);
}

static mode_t mode_arg(int flags, va_list ap)
{
	if (flags & (O_CREAT | O_TMPFILE))
		return (mode_t)va_arg(ap, int);
	return 0;
}

#define REAL(name, type) \
	static type real_##name; \
	if (!real_##name) \
		real_##name = (type)dlsym(RTLD_NEXT, #name)

typedef int (*open_fn)(const char *, int, ...);
typedef int (*openat_fn)(int, const char *, int, ...);
typedef int (*open2_fn)(const char *, int);
typedef int (*openat2_fn)(int, const char *, int);

int open(const char *path, int flags, ...)
{
	va_list ap;
	mode_t m;
	int r = lease_open(path, flags);

	if (r != -2)
		return r;
	va_start(ap, flags);
	m = mode_arg(flags, ap);
	va_end(ap);
	REAL(open, open_fn);
	return real_open(path, flags, m);
}

int open64(const char *path, int flags, ...)
{
	va_list ap;
	mode_t m;
	int r = lease_open(path, flags);

	if (r != -2)
		return r;
	va_start(ap, flags);
	m = mode_arg(flags, ap);
	va_end(ap);
	REAL(open64, open_fn);
	return real_open64(path, flags, m);
}

int openat(int dirfd, const char *path, int flags, ...)
{
	va_list ap;
	mode_t m;
	int r = lease_open(path, flags);

	if (r != -2)
		return r;
	va_start(ap, flags);
	m = mode_arg(flags, ap);
	va_end(ap);
	REAL(openat, openat_fn);
	return real_openat(dirfd, path, flags, m);
}

int openat64(int dirfd, const char *path, int flags, ...)
{
	va_list ap;
	mode_t m;
	int r = lease_open(path, flags);

	if (r != -2)
		return r;
	va_start(ap, flags);
	m = mode_arg(flags, ap);
	va_end(ap);
	REAL(openat64, openat_fn);
	return real_openat64(dirfd, path, flags, m);
}

/* _FORTIFY_SOURCE builds call these. */
int __open_2(const char *path, int flags)
{
	int r = lease_open(path, flags);

	if (r != -2)
		return r;
	REAL(__open_2, open2_fn);
	return real___open_2(path, flags);
}

int __open64_2(const char *path, int flags)
{
	int r = lease_open(path, flags);

	if (r != -2)
		return r;
	REAL(__open64_2, open2_fn);
	return real___open64_2(path, flags);
}

int __openat_2(int dirfd, const char *path, int flags)
{
	int r = lease_open(path, flags);

	if (r != -2)
		return r;
	REAL(__openat_2, openat2_fn);
	return real___openat_2(dirfd, path, flags);
}

int __openat64_2(int dirfd, const char *path, int flags)
{
	int r = lease_open(path, flags);

	if (r != -2)
		return r;
	REAL(__openat64_2, openat2_fn);
	return real___openat64_2(dirfd, path, flags);
}

/* libdrm: a "bus id" that is a path is opened as one. */
typedef int (*drmopen_fn)(const char *, const char *);
typedef int (*drmopentype_fn)(const char *, const char *, int);

int drmOpenWithType(const char *name, const char *busid, int type)
{
	if (busid && busid[0] == '/')
		return open(busid, O_RDWR | O_CLOEXEC);
	REAL(drmOpenWithType, drmopentype_fn);
	if (!real_drmOpenWithType) {
		errno = ENOSYS;
		return -1;
	}
	return real_drmOpenWithType(name, busid, type);
}

int drmOpen(const char *name, const char *busid)
{
	if (busid && busid[0] == '/')
		return open(busid, O_RDWR | O_CLOEXEC);
	REAL(drmOpen, drmopen_fn);
	if (!real_drmOpen) {
		errno = ENOSYS;
		return -1;
	}
	return real_drmOpen(name, busid);
}
