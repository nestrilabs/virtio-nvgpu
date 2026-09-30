// SPDX-License-Identifier: Apache-2.0
/*
 * rmlog.so -- LD_PRELOAD: log the NVIDIA RM controls a program makes that
 * fail, and every NV2080_CTRL_CMD_FIFO_DISABLE_CHANNELS with its parameters,
 * the same natively and in a guest. Output: $RMLOG (appended), else stderr.
 * RMLOG_REFUSE=0x2080110b[,...] answers those controls natively as a guest's
 * RM allowlist does, without RM seeing them (NV_ERR_NOT_SUPPORTED, or the
 * status in RMLOG_REFUSE_STATUS). RMLOG_ONLY_SCHED=1 sends DISABLE_CHANNELS
 * on with bOnlyDisableScheduling set. RMLOG_ALL=1 logs every RM control
 * (and the index/value pairs of NV2080_CTRL_CMD_FB_GET_INFO and _V2), every
 * RM_ALLOC's class and every VID_HEAP_CONTROL's function, served or not:
 * which controls a program reads memory sizes from.
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

/* RMLOG_REFUSE: RM control numbers, comma-separated, answered as the
 * backend's RM allowlist answers a refused one (device/src/rmallow.rs): the
 * ioctl succeeds with status NV_ERR_NOT_SUPPORTED, and RM never sees it. */
#define NV_ERR_NOT_SUPPORTED 0x56
static int refused(uint32_t cmd) {
  const char *s = getenv("RMLOG_REFUSE");
  while (s && *s) {
    char *end;
    unsigned long v = strtoul(s, &end, 0);
    if (end == s)
      break;
    if (v == cmd)
      return 1;
    s = *end == ',' ? end + 1 : end;
  }
  return 0;
}

struct nvos64 { /* NVOS64_PARAMETERS */
  uint32_t hRoot, hObjectParent, hObjectNew, hClass;
  uint64_t pAllocParms, pRightsRequested;
  uint32_t paramsSize, flags, status, pad;
};

/* RMLOG_ALL: every control, allocation and heap call, after RM ran. */
static void log_all(unsigned long req, void *arg, int ret) {
  if (_IOC_TYPE(req) != 'F')
    return;
  unsigned nr = _IOC_NR(req);
  if (nr == 0x2a && _IOC_SIZE(req) == sizeof(struct nvos54)) {
    struct nvos54 *p = arg;
    say("ALL control %#x ret %d status 0x%x size %u hObject %#x", p->cmd, ret, p->status,
        p->paramsSize, p->hObject);
    /* NV2080_CTRL_CMD_FB_GET_INFO_V2: fbInfoListSize, then (index, data). */
    if (p->cmd == 0x20801303 && p->params && p->paramsSize >= 4) {
      const uint32_t *w = (const uint32_t *)(uintptr_t)p->params;
      uint32_t n = w[0];
      for (uint32_t i = 0; i < n && 8 + 8 * i <= p->paramsSize && i < 128; i++)
        say("ALL   fbinfo index %#x data %u", w[1 + 2 * i], w[2 + 2 * i]);
    }
    /* NV2080_CTRL_CMD_FB_GET_INFO: fbInfoListSize, then a pointer to the list. */
    if (p->cmd == 0x20801301 && p->params && p->paramsSize >= 16) {
      const uint32_t *w = (const uint32_t *)(uintptr_t)p->params;
      const uint32_t *l = (const uint32_t *)(uintptr_t) * (const uint64_t *)(w + 2);
      for (uint32_t i = 0; l && i < w[0] && i < 128; i++)
        say("ALL   fbinfo(v1) index %#x data %u", l[2 * i], l[2 * i + 1]);
    }
  } else if (nr == 0x2b && _IOC_SIZE(req) == sizeof(struct nvos64)) {
    struct nvos64 *p = arg;
    say("ALL alloc class %#x ret %d status 0x%x size %u", p->hClass, ret, p->status,
        p->paramsSize);
  } else if (nr == 0x4a) {
    const uint32_t *w = arg; /* NVOS32: hRoot, hObjectParent, function, ... status at 20 */
    say("ALL vidheap function %u ret %d status 0x%x", w[2], ret, w[5]);
  }
}

int ioctl(int fd, unsigned long req, ...) {
  va_list ap;
  va_start(ap, req);
  void *arg = va_arg(ap, void *);
  va_end(ap);
  int is_ctl = _IOC_TYPE(req) == 'F' && _IOC_NR(req) == 0x2a &&
               _IOC_SIZE(req) == sizeof(struct nvos54);
  int ret;
  if (is_ctl && refused(((struct nvos54 *)arg)->cmd)) {
    /* RMLOG_REFUSE_STATUS: another answer, such as 0 (success, not done). */
    const char *st = getenv("RMLOG_REFUSE_STATUS");
    ((struct nvos54 *)arg)->status = st ? (uint32_t)strtoul(st, NULL, 0) : NV_ERR_NOT_SUPPORTED;
    ret = 0;
  } else {
    /* RMLOG_ONLY_SCHED=1: a DISABLE_CHANNELS that disables goes to RM with
     * bOnlyDisableScheduling set, so RM stops scheduling the channels but
     * preempts nothing off the GPU. */
    struct nvos54 *p = arg;
    if (is_ctl && p->cmd == 0x2080110b && p->params &&
        p->paramsSize >= sizeof(struct disable_channels) && getenv("RMLOG_ONLY_SCHED")) {
      struct disable_channels *d = (void *)(uintptr_t)p->params;
      if (d->bDisable)
        d->bOnlyDisableScheduling = 1;
    }
    ret = real_ioctl(fd, req, arg);
  }
  if (getenv("RMLOG_ALL")) {
    log_all(req, arg, ret);
  } else if (is_ctl) {
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
