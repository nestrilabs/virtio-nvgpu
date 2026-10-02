// What the backend lets through to /dev/nvidia-uvm, and what it does not.
//
// UVM is the second ioctl interface and nothing in it carries a privilege
// flag, so the backend checks shape: a command the host release defines, at
// the release's own size, with any descriptor inside it translated and
// checked to be a file this VM opened of the kind the call wants. Each of
// those is one check here, and each is checked by being broken as well as by
// being satisfied -- a refusal that never fires is indistinguishable from a
// refusal that does not work.
//
//   cc -O2 -static -o uvmprobe uvmprobe.c
//   uvmprobe
//
// Prints one PASS/FAIL line per check and exits non-zero if any failed.
//
// Layouts are from nvidia-uvm/uvm_ioctl.h. The four commands used are the
// same size on every release this project supports (535.129.03 through
// 615.71.09), which is why they are the ones used.
#include <errno.h>
#include <stdarg.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

// UVM_IOCTL_BASE(i) is i on Linux; UVM_INITIALIZE and UVM_DEINITIALIZE are
// numbered separately in uvm_linux_ioctl.h.
#define UVM_REGISTER_GPU 0x25
#define UVM_PAGEABLE_MEM_ACCESS 0x27
#define UVM_INITIALIZE 0x30000001
#define UVM_DEINITIALIZE 0x30000002
// In the gap between UVM_SET_STREAM_STOPPED (7) and UVM_ADD_SESSION (10).
#define UVM_NOT_A_COMMAND 0x08

struct uvm_initialize { // UVM_INITIALIZE_PARAMS
    uint64_t flags;
    uint32_t rmStatus;
    uint32_t pad;
};

struct uvm_pageable { // UVM_PAGEABLE_MEM_ACCESS_PARAMS
    uint8_t pageableMemAccess;
    uint8_t pad[3];
    uint32_t rmStatus;
};

struct uvm_register_gpu { // UVM_REGISTER_GPU_PARAMS
    uint8_t gpu_uuid[16];
    uint8_t numaEnabled;
    uint8_t pad[3];
    int32_t numaNodeId;
    int32_t rmCtrlFd;
    uint32_t hClient;
    uint32_t hSmcPartRef;
    uint32_t rmStatus;
};

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

int main(void) {
    struct uvm_initialize init;
    struct uvm_pageable pageable;
    struct uvm_register_gpu reg;
    int ctl, uvm, rc;

    ctl = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
    if (ctl < 0) {
        perror("uvmprobe: open /dev/nvidiactl");
        return 1;
    }
    uvm = open("/dev/nvidia-uvm", O_RDWR | O_CLOEXEC);
    if (uvm < 0) {
        perror("uvmprobe: open /dev/nvidia-uvm");
        return 1;
    }

    // The flags are the backend's whatever is asked for here, because every
    // guest process's UVM file is opened over there: a VA space tied to the
    // caller's mm would be tied to the backend's.
    memset(&init, 0, sizeof init);
    init.flags = 0; // HMM on, no sharing mode -- what the backend overrides
    rc = ioctl(uvm, UVM_INITIALIZE, &init);
    check(rc == 0 && init.rmStatus == 0, "UVM_INITIALIZE", "rc=%d rmStatus=0x%x",
          rc, init.rmStatus);

    // The one observable consequence of those flags. On 615.71.09 the backend
    // asks for pageable access to be off; on releases with no bit for it the
    // backend asks this question itself and refuses the file if the answer is
    // yes -- so getting an answer at all already means it was no.
    memset(&pageable, 0, sizeof pageable);
    rc = ioctl(uvm, UVM_PAGEABLE_MEM_ACCESS, &pageable);
    check(rc == 0 && pageable.rmStatus == 0 && pageable.pageableMemAccess == 0,
          "pageable memory access is off", "rc=%d rmStatus=0x%x pageable=%u", rc,
          pageable.rmStatus, pageable.pageableMemAccess);

    // A descriptor inside the parameters. The number written here means
    // nothing in the backend's process, so the guest module resolves it and
    // the backend puts its own in. The UUID is zeroes, so UVM answers an RM
    // error -- which is the point: an answer means the call was carried, and
    // only a translated descriptor gets that far.
    memset(&reg, 0, sizeof reg);
    reg.rmCtrlFd = ctl;
    rc = ioctl(uvm, UVM_REGISTER_GPU, &reg);
    check(rc == 0, "a control descriptor is carried through",
          "rc=%d errno=%d rmStatus=0x%x", rc, rc ? errno : 0, reg.rmStatus);

    // The same call with a UVM file where it wants the control file. Both are
    // this VM's, so ownership alone would let it through; the kind is what
    // stops it.
    memset(&reg, 0, sizeof reg);
    reg.rmCtrlFd = uvm;
    rc = ioctl(uvm, UVM_REGISTER_GPU, &reg);
    check(rc < 0, "a descriptor of the wrong kind is refused", "rc=%d errno=%d",
          rc, rc ? errno : 0);

    // And one that is not a descriptor of this VM's at all.
    memset(&reg, 0, sizeof reg);
    reg.rmCtrlFd = 4096;
    rc = ioctl(uvm, UVM_REGISTER_GPU, &reg);
    check(rc < 0, "a descriptor this VM never opened is refused",
          "rc=%d errno=%d", rc, rc ? errno : 0);

    // A number no release defines. Refused by the guest module, which knows
    // only what the backend told it the host takes.
    {
        char junk[64];
        memset(junk, 0, sizeof junk);
        rc = ioctl(uvm, UVM_NOT_A_COMMAND, junk);
        check(rc < 0, "a command no release defines is refused", "rc=%d errno=%d",
              rc, rc ? errno : 0);
    }

    rc = ioctl(uvm, UVM_DEINITIALIZE, 0);
    check(rc == 0, "UVM_DEINITIALIZE", "rc=%d errno=%d", rc, rc ? errno : 0);

    printf("uvmprobe: %d failed\n", failures);
    close(uvm);
    close(ctl);
    return failures ? 1 : 0;
}
