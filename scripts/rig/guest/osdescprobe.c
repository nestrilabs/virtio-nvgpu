// Whether a guest can hand RM an address of its own.
//
// RM takes a CPU address and a length and pins what is there for the GPU. The
// address is read in the *caller's* address space, and the caller is the
// backend: an address written here names the backend's memory, not this
// guest's. Three ioctls carry one, and this asks all three.
//
// The positive side -- that an ordinary heap allocation still works -- is not
// here, because it needs a client, a device and a heap. It is covered by the
// draw probe, which makes 180 VID_HEAP_CONTROL calls and has none refused.
//
//   cc -O2 -static -o osdescprobe osdescprobe.c
//   osdescprobe
//
// Prints one PASS/FAIL line per route and exits non-zero if any failed.
// Layouts are NVOS02_PARAMETERS, NVOS32_PARAMETERS and
// NV_OS_DESC_MEMORY_ALLOCATION_PARAMS from NVIDIA's nvos.h. The offsets used
// are the same on every release this project supports; the backend reads its
// own from the release and does not trust these.
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#define NV_IOCTL_MAGIC 'F'
#define NV_ESC_RM_ALLOC_MEMORY 0x27
#define NV_ESC_RM_ALLOC 0x2b
#define NV_ESC_RM_VID_HEAP_CONTROL 0x4a
#define NV_ESC_CHECK_VERSION_STR 0xd2

#define NV01_MEMORY_SYSTEM_OS_DESCRIPTOR 0x71
#define NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR 27
#define NV_ERR_NOT_SUPPORTED 0x56

// An address the guest owns, so the only thing stopping this is the backend.
static char buffer[2 * 1024 * 1024] __attribute__((aligned(4096)));

struct rm_api_version {
    uint32_t cmd;
    uint32_t reply;
    char version[64];
};

struct nvos64 { // NVOS64_PARAMETERS
    uint32_t hRoot, hObjectParent, hObjectNew, hClass;
    uint64_t pAllocParms, pRightsRequested;
    uint32_t paramsSize, flags, status, pad;
};

struct os_desc_params { // NV_OS_DESC_MEMORY_ALLOCATION_PARAMS
    uint32_t type, flags, attr, attr2;
    uint64_t descriptor;
    uint64_t limit;
    uint32_t descriptorType, tag;
};

// nv_ioctl_nvos02_parameters_with_fd: NVOS02_PARAMETERS, which is 48 bytes
// because `status` at 40 is followed by padding to the struct's 8-byte
// alignment, and only then the descriptor. Written without the padding this
// comes to 48 and the guest module reads the descriptor off the end of it --
// which is how this probe failed the first time it ran.
struct nvos02_with_fd {
    uint32_t hRoot, hObjectParent, hObjectNew, hClass, flags;
    uint32_t pad0;
    uint64_t pMemory;
    uint64_t limit;
    uint32_t status;
    uint32_t pad1;
    int32_t fd;
    uint32_t pad2;
};

// Only the fields this probe sets; the block is sent at its full size.
#define NVOS32_SIZE 184
#define NVOS32_FUNCTION_AT 8
#define NVOS32_STATUS_AT 20
#define NVOS32_DESCRIPTOR_AT 64
#define NVOS32_LIMIT_AT 72

static int failures;

static void check(int ok, const char *what, const char *fmt, ...) {
    va_list ap;
    printf("%-6s %s", ok ? "PASS" : "FAIL", what);
    if (fmt && *fmt) {
        va_start(ap, fmt);
        printf("  ");
        vprintf(fmt, ap);
        va_end(ap);
    }
    putchar('\n');
    if (!ok)
        failures++;
}

static void put32(void *p, size_t at, uint32_t v) { memcpy((char *)p + at, &v, 4); }
static void put64(void *p, size_t at, uint64_t v) { memcpy((char *)p + at, &v, 8); }
static uint32_t get32(void *p, size_t at) {
    uint32_t v;
    memcpy(&v, (char *)p + at, 4);
    return v;
}

int main(void) {
    _Static_assert(sizeof(struct nvos02_with_fd) == 56, "NVOS02 with fd is 56 bytes");
    _Static_assert(offsetof(struct nvos02_with_fd, fd) == 48, "the descriptor sits at 48");
    struct rm_api_version ver;
    struct os_desc_params desc;
    struct nvos64 alloc;
    struct nvos02_with_fd nvos02;
    char heap[NVOS32_SIZE];
    uint64_t addr = (uint64_t)(uintptr_t)buffer;
    int fd, rc;

    fd = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        perror("osdescprobe: open /dev/nvidiactl");
        return 1;
    }

    // RM refuses a client from a caller that has not checked the version, and
    // a run that stopped there would say nothing about the routes below.
    memset(&ver, 0, sizeof ver);
    ver.cmd = '2';
    if (ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_CHECK_VERSION_STR, ver), &ver) < 0) {
        perror("osdescprobe: CHECK_VERSION_STR");
        return 1;
    }

    // Route 1: RM_ALLOC of the class, the address in the allocation parameters.
    memset(&desc, 0, sizeof desc);
    desc.descriptor = addr;
    desc.limit = sizeof buffer - 1;
    memset(&alloc, 0, sizeof alloc);
    alloc.hClass = NV01_MEMORY_SYSTEM_OS_DESCRIPTOR;
    alloc.pAllocParms = (uint64_t)(uintptr_t)&desc;
    alloc.paramsSize = sizeof desc;
    rc = ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_ALLOC, alloc), &alloc);
    check(rc == 0 && alloc.status == NV_ERR_NOT_SUPPORTED, "RM_ALLOC by address is refused",
          "rc=%d status=0x%x", rc, alloc.status);

    // Route 2: the older escape, class inside the parameters.
    memset(&nvos02, 0, sizeof nvos02);
    nvos02.hClass = NV01_MEMORY_SYSTEM_OS_DESCRIPTOR;
    nvos02.pMemory = addr;
    nvos02.limit = sizeof buffer - 1;
    nvos02.fd = -1;
    rc = ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_ALLOC_MEMORY, nvos02), &nvos02);
    check(rc == 0 && nvos02.status == NV_ERR_NOT_SUPPORTED,
          "RM_ALLOC_MEMORY by address is refused", "rc=%d status=0x%x", rc, nvos02.status);

    // Route 3: the heap ioctl, where only one function names an address.
    memset(heap, 0, sizeof heap);
    put32(heap, NVOS32_FUNCTION_AT, NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR);
    put64(heap, NVOS32_DESCRIPTOR_AT, addr);
    put64(heap, NVOS32_LIMIT_AT, sizeof buffer - 1);
    rc = ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_VID_HEAP_CONTROL, heap), heap);
    check(rc == 0 && get32(heap, NVOS32_STATUS_AT) == NV_ERR_NOT_SUPPORTED,
          "VID_HEAP_CONTROL by address is refused", "rc=%d status=0x%x", rc,
          get32(heap, NVOS32_STATUS_AT));

    printf("osdescprobe: %d failed\n", failures);
    close(fd);
    return failures ? 1 : 0;
}
