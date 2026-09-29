// SPDX-License-Identifier: Apache-2.0
/* A stand-in for nesbox and for a stale process: stays up (pause) when an
 * argument names "other" (another VM's process), exits at once otherwise.
 *
 * Built -nostdlib -static (x86_64 Linux system calls, no libc): a jail the
 * root launcher builds from it needs no library, which in the dry run's
 * user namespace would be real root's, uid 65534 there, and refused. Built
 * with libc as well (stubvmm-dyn), for the check that refuses such a
 * library. */
#ifdef WITH_LIBC
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
    for (int i = 1; i < argc; i++)
        if (strstr(argv[i], "other"))
            for (;;) pause();
    return 0;
}
#else
static long sys1(long n, long a) {
    long r;
    __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a) : "rcx", "r11", "memory");
    return r;
}

static int names_other(const char *s) {
    for (; *s; s++)
        if (s[0] == 'o' && s[1] == 't' && s[2] == 'h' && s[3] == 'e' && s[4] == 'r')
            return 1;
    return 0;
}

__attribute__((used)) void stub_main(long *sp) {
    long argc = sp[0];
    char **argv = (char **)(sp + 1);
    for (long i = 1; i < argc; i++)
        if (names_other(argv[i]))
            for (;;) sys1(34 /* pause */, 0);
    sys1(60 /* exit */, 0);
}

__attribute__((naked)) void _start(void) {
    __asm__ volatile("mov %rsp, %rdi\n\tand $-16, %rsp\n\tcall stub_main\n\thlt");
}
#endif
