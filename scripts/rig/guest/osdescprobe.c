// Whether a guest can hand RM memory of its own.
//
// RM takes a CPU address and a length and pins what is there for the GPU. The
// address is read in the *caller's* address space, and the caller is the
// backend: an address written here names the backend's memory, not this
// guest's. Three ioctls carry one, and this asks all three.
//
// What should happen on the two routes that work is that each one succeeds and
// registers *this* guest's pages: the guest driver pins them and says where
// they are, and the backend builds an address of its own that aliases exactly
// those. What must never happen is that the address goes through untranslated,
// and the only honest way to tell the two apart from here is RM's own answer.
//
// The third route, RM_ALLOC of the class, does not work and never did --
// not through this backend and not on bare metal. NVIDIA's own
// osCreateMemFromOsDescriptor answers a user virtual address with
// NV_ERR_NOT_SUPPORTED on every Unix release (osmemdesc.c, `case
// NVOS32_DESCRIPTOR_TYPE_VIRTUAL_ADDRESS`), because the only path that can
// pin user pages is the escape layer's own RmCreateOsDescriptor, which the
// other two routes go through. So this asks for it and expects RM's refusal,
// which is also what it gets if the backend refuses first. The two are not
// distinguishable from here; the backend's own test suite is what tells them
// apart, and the backend log says which answered.
//
// The negative side is here too: a descriptor that does not name a user
// address is not something a guest can pin, and is refused. That one matters,
// because those are the descriptor types RM *does* serve -- a file handle
// among them, which from a guest would name whatever the backend has open at
// that number. Two things refuse it, the guest's own driver and the backend,
// and the guest's gets there first, so what this check sees is an errno. The
// backend's half is held up by its own test suite, where the guest cannot
// stand in the way.
//
//   cc -O2 -static -o osdescprobe osdescprobe.c
//   osdescprobe
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.
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
#define NV_ESC_RM_FREE 0x29
#define NV_ESC_RM_ALLOC 0x2b
#define NV_ESC_RM_VID_HEAP_CONTROL 0x4a
#define NV_ESC_CHECK_VERSION_STR 0xd2
#define NV_ESC_REGISTER_FD 0xc9

#define NV01_ROOT_CLIENT 0x41
#define NV01_DEVICE_0 0x80
#define NV01_MEMORY_SYSTEM_OS_DESCRIPTOR 0x71
#define NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR 27
#define NVOS32_TYPE_IMAGE 0
#define NV_ERR_NOT_SUPPORTED 0x56
#define NV_OK 0

// NVOS32_ATTR: sysmem, scattered, cached -- what memory of the guest's own is.
#define OSDESC_ATTR ((1u << 25) | (1u << 27) | (1u << 29))

// Descriptor types, from NVOS32_DESCRIPTOR_TYPE_*. 0 is a user virtual
// address; 1 is a kernel one, which no guest can pin and nothing here serves.
#define DESC_TYPE_VIRTUAL_ADDRESS 0
#define DESC_TYPE_KERNEL_VIRTUAL_ADDRESS 1

// NVOS02_FLAGS, which RmAllocOsDescriptor checks before it will do anything:
// LOCATION_PCI (0, bits 11:8), MAPPING_NO_MAP (1, bits 31:30), and a coherency
// it recognises -- CACHED (1, bits 15:12). PHYSICALITY_NONCONTIGUOUS (1, bits
// 7:4) because guest pages are not contiguous in the host.
#define OS02_FLAGS ((1u << 30) | (1u << 12) | (1u << 4))

// An address the guest owns, so the only thing that decides is the backend.
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

struct nvos00 { // NVOS00_PARAMETERS
    uint32_t hRoot, hObjectParent, hObjectOld, status;
};

struct device_params { // NV0080_ALLOC_PARAMETERS
    uint32_t deviceId, hClientShare, hTargetClient, hTargetDevice, flags;
    uint32_t pad;
    uint64_t vaSpaceSize, vaStartInternal, vaLimitInternal;
    uint32_t vaMode, pad2;
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
#define NVOS32_HROOT_AT 0
#define NVOS32_HPARENT_AT 4
#define NVOS32_FUNCTION_AT 8
#define NVOS32_STATUS_AT 20
#define NVOS32_HMEMORY_AT 40
#define NVOS32_TYPE_AT 44
#define NVOS32_FLAGS_AT 48
#define NVOS32_ATTR_AT 52
#define NVOS32_ATTR2_AT 56
#define NVOS32_DESCRIPTOR_AT 64
#define NVOS32_LIMIT_AT 72
#define NVOS32_DESCTYPE_AT 80

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

static int fd;
static uint32_t client, device;

/* Whether these ioctls are being answered by the backend or by a real driver. */
static int in_guest(void) { return access("/sys/module/virtio_gpu_nv", F_OK) == 0; }

/* RM_ALLOC, returning the handle RM made or 0, with its status out. */
static uint32_t rm_alloc(uint32_t parent, uint32_t cls, void *params, uint32_t size,
                         uint32_t *status, int *rc_out) {
    struct nvos64 a;
    int rc;

    memset(&a, 0, sizeof a);
    a.hRoot = client;
    a.hObjectParent = parent;
    a.hClass = cls;
    a.pAllocParms = (uint64_t)(uintptr_t)params;
    a.paramsSize = size;
    rc = ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_ALLOC, a), &a);
    if (rc_out)
        *rc_out = rc;
    *status = a.status;
    return (rc == 0 && a.status == NV_OK) ? a.hObjectNew : 0;
}

static void rm_free(uint32_t parent, uint32_t object) {
    struct nvos00 f;

    memset(&f, 0, sizeof f);
    f.hRoot = client;
    f.hObjectParent = parent;
    f.hObjectOld = object;
    ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_FREE, f), &f);
}

int main(void) {
    _Static_assert(sizeof(struct nvos02_with_fd) == 56, "NVOS02 with fd is 56 bytes");
    _Static_assert(offsetof(struct nvos02_with_fd, fd) == 48, "the descriptor sits at 48");
    _Static_assert(sizeof(struct device_params) == 56, "NV0080_ALLOC_PARAMETERS is 56 bytes");
    _Static_assert(offsetof(struct os_desc_params, descriptor) == 16, "address at 16");
    struct rm_api_version ver;
    struct device_params dev;
    struct os_desc_params desc;
    struct nvos02_with_fd nvos02;
    char heap[NVOS32_SIZE];
    uint64_t addr = (uint64_t)(uintptr_t)buffer;
    uint32_t status, mem;
    int rc, gpu, ctl;

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

    // Something to hang the registrations off. Without a real client and a
    // real device RM refuses every one of them for reasons of its own, and the
    // run would say nothing about the translation.
    client = rm_alloc(0, NV01_ROOT_CLIENT, NULL, 0, &status, &rc);
    if (!client) {
        printf("osdescprobe: no client (rc=%d errno=%d status=0x%x)\n", rc, rc ? errno : 0, status);
        return 1;
    }

    // RM only lets a client make a device for a GPU the calling process has
    // open -- `osIsGpuAccessible`, which is what answers
    // NV_ERR_INSUFFICIENT_PERMISSIONS when it is not.
    gpu = open("/dev/nvidia0", O_RDWR | O_CLOEXEC);
    if (gpu < 0) {
        perror("osdescprobe: open /dev/nvidia0");
        return 1;
    }
    // RM keeps its clients per open file, so a client made on the control file
    // is not one the GPU file knows -- NV_ERR_INVALID_CLIENT -- until the two
    // are tied together. nv_ioctl_register_fd_t is the control fd and nothing
    // else.
    // A copy, because this ioctl writes its answer back over the descriptor it
    // was given -- the backend's handle, not an fd of ours -- and passing the
    // real one leaves every later call on a number that means nothing.
    ctl = fd;
    if (ioctl(gpu, _IOWR(NV_IOCTL_MAGIC, NV_ESC_REGISTER_FD, int), &ctl) < 0) {
        perror("osdescprobe: REGISTER_FD");
        return 1;
    }

    memset(&dev, 0, sizeof dev);
    device = rm_alloc(client, NV01_DEVICE_0, &dev, sizeof dev, &status, &rc);
    if (!device) {
        printf("osdescprobe: no device (rc=%d errno=%d status=0x%x)\n", rc, rc ? errno : 0, status);
        return 1;
    }

    // Route 1: RM_ALLOC of the class, the address in the allocation parameters.
    memset(&desc, 0, sizeof desc);
    desc.type = NVOS32_TYPE_IMAGE;
    desc.attr = OSDESC_ATTR;
    desc.descriptor = addr;
    desc.limit = sizeof buffer - 1;
    desc.descriptorType = DESC_TYPE_VIRTUAL_ADDRESS;
    mem = rm_alloc(device, NV01_MEMORY_SYSTEM_OS_DESCRIPTOR, &desc, sizeof desc, &status, &rc);
    check(rc == 0 && status == NV_ERR_NOT_SUPPORTED,
          "RM_ALLOC by address is answered, not served", "rc=%d status=0x%x", rc, status);
    if (mem)
        rm_free(device, mem);

    // Route 2: the older escape, class inside the parameters.
    memset(&nvos02, 0, sizeof nvos02);
    nvos02.hRoot = client;
    nvos02.hObjectParent = device;
    nvos02.hClass = NV01_MEMORY_SYSTEM_OS_DESCRIPTOR;
    // On this route the handle is the caller's to choose -- RmAllocOsDescriptor
    // passes NVOS32_ALLOC_FLAGS_MEMORY_HANDLE_PROVIDED -- so a zero here makes
    // an object there is no way to free.
    nvos02.hObjectNew = client + 0x100;
    nvos02.pMemory = addr;
    nvos02.limit = sizeof buffer - 1;
    nvos02.flags = OS02_FLAGS;
    nvos02.fd = -1;
    // NV_ACTUAL_DEVICE_ONLY: this escape is refused outright on the control
    // file, so it goes to the GPU node.
    rc = ioctl(gpu, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_ALLOC_MEMORY, nvos02), &nvos02);
    check(rc == 0 && nvos02.status == NV_OK, "RM_ALLOC_MEMORY registers the guest's own memory",
          "rc=%d status=0x%x handle=0x%x", rc, nvos02.status, nvos02.hObjectNew);
    if (rc == 0 && nvos02.status == NV_OK)
        rm_free(device, nvos02.hObjectNew);

    // Route 3: the heap ioctl, where only one function names an address.
    memset(heap, 0, sizeof heap);
    put32(heap, NVOS32_HROOT_AT, client);
    put32(heap, NVOS32_HPARENT_AT, device);
    put32(heap, NVOS32_FUNCTION_AT, NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR);
    put32(heap, NVOS32_TYPE_AT, NVOS32_TYPE_IMAGE);
    put32(heap, NVOS32_ATTR_AT, OSDESC_ATTR);
    put64(heap, NVOS32_DESCRIPTOR_AT, addr);
    put64(heap, NVOS32_LIMIT_AT, sizeof buffer - 1);
    put32(heap, NVOS32_DESCTYPE_AT, DESC_TYPE_VIRTUAL_ADDRESS);
    rc = ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_VID_HEAP_CONTROL, heap), heap);
    check(rc == 0 && get32(heap, NVOS32_STATUS_AT) == NV_OK,
          "VID_HEAP_CONTROL registers the guest's own memory", "rc=%d status=0x%x handle=0x%x", rc,
          get32(heap, NVOS32_STATUS_AT), get32(heap, NVOS32_HMEMORY_AT));
    if (rc == 0 && get32(heap, NVOS32_STATUS_AT) == NV_OK)
        rm_free(device, get32(heap, NVOS32_HMEMORY_AT));

    // And the other side of it: a descriptor that does not name a user address
    // is nothing a guest can pin, so it never reaches the host's RM. On the
    // heap route, where RM would otherwise serve it.
    memset(heap, 0, sizeof heap);
    put32(heap, NVOS32_HROOT_AT, client);
    put32(heap, NVOS32_HPARENT_AT, device);
    put32(heap, NVOS32_FUNCTION_AT, NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR);
    put32(heap, NVOS32_TYPE_AT, NVOS32_TYPE_IMAGE);
    put32(heap, NVOS32_ATTR_AT, OSDESC_ATTR);
    put64(heap, NVOS32_DESCRIPTOR_AT, addr);
    put64(heap, NVOS32_LIMIT_AT, sizeof buffer - 1);
    put32(heap, NVOS32_DESCTYPE_AT, DESC_TYPE_KERNEL_VIRTUAL_ADDRESS);
    rc = ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_VID_HEAP_CONTROL, heap), heap);
    // Only through the backend. On bare metal the escape layer pins the
    // caller's user pages whatever the block says the descriptor is, and
    // overwrites the type with OS_PAGE_ARRAY before RM proper sees it -- so
    // the host serves this, and rightly, because the pages it pinned are its
    // caller's either way. It is a guest saying it that has to be refused.
    if (in_guest())
        check(rc != 0 || get32(heap, NVOS32_STATUS_AT) != NV_OK,
              "a descriptor that is not a user address is refused",
              "rc=%d errno=%d status=0x%x", rc, rc ? errno : 0, get32(heap, NVOS32_STATUS_AT));
    else
        printf("SKIP   a descriptor that is not a user address is refused  "
               "(no backend here; rc=%d status=0x%x)\n",
               rc, get32(heap, NVOS32_STATUS_AT));
    if (rc == 0 && get32(heap, NVOS32_STATUS_AT) == NV_OK)
        rm_free(device, get32(heap, NVOS32_HMEMORY_AT));

    rm_free(client, device);
    rm_free(0, client);
    printf("osdescprobe: %d failed\n", failures);
    close(gpu);
    close(fd);
    return failures ? 1 : 0;
}
