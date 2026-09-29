// SPDX-License-Identifier: Apache-2.0
/*
 * rmlog.so -- LD_PRELOAD: log the NVIDIA RM controls a program makes that
 * fail, and every NV2080_CTRL_CMD_FIFO_DISABLE_CHANNELS with its parameters,
 * the same natively and in a guest. Output: $RMLOG (appended), else stderr.
 *
 *   cc -O2 -shared -fPIC rmlog.c -o rmlog.so -ldl
 *   RMLOG=/tmp/rm.log LD_PRELOAD=$PWD/rmlog.so supertuxkart ...
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

struct nvos54 { /* NVOS54_PARAMETERS */
  uint32_t hClient, hObject, cmd, flags;
  uint64_t params;
  uint32_t paramsSize, status;
};

struct disable_channels { /* NV2080_CTRL_FIFO_DISABLE_CHANNELS_PARAMS */
  uint8_t bDisable;
  uint8_t pad0[3];
  uint32_t numChannels;
  uint8_t bOnlyDisableScheduling;
  uint8_t bRewindGpPut;
  uint8_t pad1[6];
  uint64_t pRunlistPreemptEvent;
  uint32_t hClientList[64];
  uint32_t hChannelList[64];
};

static int (*real_ioctl)(int, unsigned long, ...);
static int logfd = 2;

static void say(const char *fmt, ...) {
  char b[1024];
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  int n = snprintf(b, sizeof b, "%ld.%06ld tid %ld ", (long)t.tv_sec, t.tv_nsec / 1000,
                   (long)syscall(SYS_gettid));
  va_list ap;
  va_start(ap, fmt);
  n += vsnprintf(b + n, sizeof b - n, fmt, ap);
  va_end(ap);
  if (n > (int)sizeof b - 2)
    n = sizeof b - 2;
  b[n++] = '\n';
  (void)!write(logfd, b, n);
}

__attribute__((constructor)) static void init(void) {
  real_ioctl = (int (*)(int, unsigned long, ...))dlsym(RTLD_NEXT, "ioctl");
  const char *p = getenv("RMLOG");
  if (p) {
    int fd = open(p, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0644);
    if (fd >= 0)
      logfd = fd;
  }
  say("rmlog: pid %d", getpid());
}

int ioctl(int fd, unsigned long req, ...) {
  va_list ap;
  va_start(ap, req);
  void *arg = va_arg(ap, void *);
  va_end(ap);
  int ret = real_ioctl(fd, req, arg);
  if (_IOC_TYPE(req) == 'F' && _IOC_NR(req) == 0x2a && _IOC_SIZE(req) == sizeof(struct nvos54)) {
    struct nvos54 *p = arg;
    if (p->cmd == 0x2080110b && p->params && p->paramsSize >= sizeof(struct disable_channels)) {
      struct disable_channels *d = (void *)(uintptr_t)p->params;
      say("DISABLE_CHANNELS ret %d status 0x%x size %u bDisable %u n %u onlySched %u rewind %u event %#llx "
          "client0 %#x chan0 %#x client1 %#x chan1 %#x (hClient %#x)",
          ret, p->status, p->paramsSize, d->bDisable, d->numChannels, d->bOnlyDisableScheduling,
          d->bRewindGpPut, (unsigned long long)d->pRunlistPreemptEvent, d->hClientList[0],
          d->hChannelList[0], d->hClientList[1], d->hChannelList[1], p->hClient);
    } else if (ret != 0 || p->status != 0) {
      say("control %#x ret %d status 0x%x size %u", p->cmd, ret, p->status, p->paramsSize);
    }
  }
  return ret;
}
