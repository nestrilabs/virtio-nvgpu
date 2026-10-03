// What one forwarded RM control costs, end to end.
//
// Allocates an RM client on /dev/nvidiactl, then times N calls of
// NV0000_CTRL_CMD_SYSTEM_GET_FEATURES, a control with a four-byte parameter
// block that RM answers without touching the GPU. Run the same binary in a
// guest and on the host: the difference is the forwarding path.
//
//   cc -O2 -static -o rmbench rmbench.c
//   rmbench [calls]          default 100000
//
// Prints one line: "rmbench calls=N total_ms=T per_call_us=U".
// Layouts are NVOS54_PARAMETERS and NVOS64_PARAMETERS from NVIDIA's nvos.h;
// both have been stable since well before the oldest release this project
// supports.
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#define NV_IOCTL_MAGIC 'F'
#define NV_ESC_RM_CONTROL 0x2a
#define NV_ESC_RM_ALLOC 0x2b
#define NV_ESC_CHECK_VERSION_STR 0xd2
#define NV01_ROOT_CLIENT 0x41
#define NV0000_CTRL_CMD_SYSTEM_GET_FEATURES 0x1f0

struct rm_api_version { // nv_ioctl_rm_api_version_t
    uint32_t cmd;
    uint32_t reply;
    char version[64];
};

struct nvos64 { // NVOS64_PARAMETERS
    uint32_t hRoot, hObjectParent, hObjectNew, hClass;
    uint64_t pAllocParms, pRightsRequested;
    uint32_t paramsSize, flags, status, pad;
};

struct nvos54 { // NVOS54_PARAMETERS
    uint32_t hClient, hObject, cmd, flags;
    uint64_t params;
    uint32_t paramsSize, status;
};

static double now_us(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e6 + t.tv_nsec / 1e3;
}

int main(int argc, char **argv) {
    long calls = argc > 1 ? atol(argv[1]) : 100000;
    int fd = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
    if (fd < 0) { perror("rmbench: open /dev/nvidiactl"); return 1; }

    // RM refuses a client from a caller that has not checked the version.
    // Cmd '2' asks it to report the version rather than compare one.
    struct rm_api_version v = { .cmd = '2' };
    if (ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_CHECK_VERSION_STR, struct rm_api_version), &v) < 0) {
        perror("rmbench: CHECK_VERSION_STR");
        return 1;
    }

    struct nvos64 a = { .hClass = NV01_ROOT_CLIENT };
    if (ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_ALLOC, struct nvos64), &a) < 0 || a.status) {
        fprintf(stderr, "rmbench: client alloc failed, status %#x\n", a.status);
        return 1;
    }
    uint32_t client = a.hObjectNew;

    uint32_t features = 0;
    struct nvos54 c = {
        .hClient = client, .hObject = client, .cmd = NV0000_CTRL_CMD_SYSTEM_GET_FEATURES,
        .params = (uintptr_t)&features, .paramsSize = sizeof features,
    };
    unsigned long req = _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_CONTROL, struct nvos54);

    for (int i = 0; i < 1000; i++) ioctl(fd, req, &c); // warm up

    double t0 = now_us();
    for (long i = 0; i < calls; i++) {
        c.status = 0;
        if (ioctl(fd, req, &c) < 0 || c.status) {
            fprintf(stderr, "rmbench: call %ld failed, status %#x\n", i, c.status);
            return 1;
        }
    }
    double t1 = now_us();
    printf("rmbench calls=%ld total_ms=%.1f per_call_us=%.2f\n", calls, (t1 - t0) / 1e3,
           (t1 - t0) / calls);
    return 0;
}
