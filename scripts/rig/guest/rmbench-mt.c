// Several threads making RM controls at once, each checking its own answers.
//
// Each thread allocates its own RM client and calls
// NV0000_CTRL_CMD_SYSTEM_GET_FEATURES on it in a loop. RM echoes the
// parameter block back, so a thread that wakes with someone else's response,
// or with a response the device has not written yet, sees a client handle
// that is not its own. That was possible while the guest driver shared one
// completion and one response buffer between all callers.
//
//   cc -O2 -static -pthread -o rmbench-mt rmbench-mt.c
//   rmbench-mt [threads] [calls per thread]     default 8 x 20000
//
// Prints "rmbench-mt threads=T calls=N wrong=W failed=F per_call_us=U" and
// exits non-zero if any answer was wrong or any call failed.
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#define NV_IOCTL_MAGIC 'F'
#define NV_ESC_RM_CONTROL 0x2a
#define NV_ESC_RM_ALLOC 0x2b
#define NV_ESC_CHECK_VERSION_STR 0xd2
#define NV01_ROOT_CLIENT 0x41
#define NV0000_CTRL_CMD_SYSTEM_GET_FEATURES 0x1f0

struct rm_api_version {
    uint32_t cmd, reply;
    char version[64];
};
struct nvos64 {
    uint32_t hRoot, hObjectParent, hObjectNew, hClass;
    uint64_t pAllocParms, pRightsRequested;
    uint32_t paramsSize, flags, status, pad;
};
struct nvos54 {
    uint32_t hClient, hObject, cmd, flags;
    uint64_t params;
    uint32_t paramsSize, status;
};

static long calls = 20000;
static int fd;

struct result {
    long wrong, failed;
};

static void *worker(void *arg) {
    struct result *r = arg;
    struct nvos64 a = {.hClass = NV01_ROOT_CLIENT};
    if (ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_ALLOC, struct nvos64), &a) < 0 || a.status) {
        r->failed = calls;
        return NULL;
    }
    uint32_t client = a.hObjectNew;
    uint32_t features;
    unsigned long req = _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_CONTROL, struct nvos54);
    for (long i = 0; i < calls; i++) {
        struct nvos54 c = {
            .hClient = client, .hObject = client, .cmd = NV0000_CTRL_CMD_SYSTEM_GET_FEATURES,
            .params = (uintptr_t)&features, .paramsSize = sizeof features,
        };
        if (ioctl(fd, req, &c) < 0 || c.status)
            r->failed++;
        else if (c.hClient != client || c.hObject != client ||
                 c.cmd != NV0000_CTRL_CMD_SYSTEM_GET_FEATURES || c.paramsSize != sizeof features)
            r->wrong++;
    }
    return NULL;
}

int main(int argc, char **argv) {
    int threads = argc > 1 ? atoi(argv[1]) : 8;
    if (argc > 2)
        calls = atol(argv[2]);
    fd = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        perror("rmbench-mt: open /dev/nvidiactl");
        return 1;
    }
    struct rm_api_version v = {.cmd = '2'};
    if (ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_CHECK_VERSION_STR, struct rm_api_version), &v) < 0) {
        perror("rmbench-mt: CHECK_VERSION_STR");
        return 1;
    }
    pthread_t t[64];
    struct result r[64] = {0};
    if (threads < 1 || threads > 64)
        threads = 8;
    struct timespec t0, t1;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    for (int i = 0; i < threads; i++)
        pthread_create(&t[i], NULL, worker, &r[i]);
    long wrong = 0, failed = 0;
    for (int i = 0; i < threads; i++) {
        pthread_join(t[i], NULL);
        wrong += r[i].wrong;
        failed += r[i].failed;
    }
    clock_gettime(CLOCK_MONOTONIC, &t1);
    double us = (t1.tv_sec - t0.tv_sec) * 1e6 + (t1.tv_nsec - t0.tv_nsec) / 1e3;
    printf("rmbench-mt threads=%d calls=%ld wrong=%ld failed=%ld per_call_us=%.2f\n", threads,
           calls * threads, wrong, failed, us / (calls * threads));
    return wrong || failed;
}
