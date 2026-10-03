// Drains nescapture's video socket and writes the elementary stream out.
//
// The guest image has no python, no socat and no nc, and this is the same
// framing verify-chain.sh reads: a 20-byte header beginning "NSTR", a zero at
// byte 4, and a little-endian u32 payload length at byte 16.
//
// Usage: nesrecv <socket> <out-file> <seconds>
// Prints "frames=<n> bytes=<n>" and, beneath it, how evenly they arrived.
//
// The arrival spacing is there because a frame count alone cannot tell a
// pipeline that is uniformly slower from one that is on time and occasionally
// stalls. At 60 Hz those look identical in a total -- 548 frames instead of 618
// -- and they have entirely different causes: the first is per-frame cost, the
// second is a missed deadline that costs a whole 16.7 ms slot.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/time.h>
#include <sys/stat.h>

#define BUF (8 << 20)

static int cmp_double(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y;
}

int main(int argc, char **argv) {
    if (argc != 4) { fprintf(stderr, "usage: %s <socket> <out> <seconds>\n", argv[0]); return 2; }
    const char *path = argv[1], *out = argv[2];
    double secs = atof(argv[3]);

    unlink(path);
    int s = socket(AF_UNIX, SOCK_DGRAM, 0);
    if (s < 0) { perror("socket"); return 1; }

    struct sockaddr_un a;
    memset(&a, 0, sizeof a);
    a.sun_family = AF_UNIX;
    strncpy(a.sun_path, path, sizeof a.sun_path - 1);
    if (bind(s, (struct sockaddr *)&a, sizeof a) < 0) { perror("bind"); return 1; }
    chmod(path, 0777);

    int rcv = BUF;
    setsockopt(s, SOL_SOCKET, SO_RCVBUF, &rcv, sizeof rcv);
    struct timeval tv = { .tv_sec = 1, .tv_usec = 0 };
    setsockopt(s, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);

    FILE *f = fopen(out, "wb");
    if (!f) { perror("fopen"); return 1; }

    unsigned char *buf = malloc(BUF);
    struct timeval now, end;
    gettimeofday(&end, NULL);
    end.tv_sec += (long)secs;

    long frames = 0, bytes = 0;
    double *gaps = malloc(sizeof(double) * 100000);
    long ngaps = 0;
    struct timeval prev = { 0, 0 };
    for (;;) {
        gettimeofday(&now, NULL);
        if (now.tv_sec >= end.tv_sec) break;
        ssize_t n = recv(s, buf, BUF, 0);
        if (n < 20) continue;
        if (memcmp(buf, "NSTR", 4) != 0 || buf[4] != 0) continue;
        unsigned int dl;
        memcpy(&dl, buf + 16, 4);
        if ((ssize_t)(20 + dl) > n) dl = (unsigned int)(n - 20);
        fwrite(buf + 20, 1, dl, f);
        frames++;
        bytes += dl;

        struct timeval at;
        gettimeofday(&at, NULL);
        if (prev.tv_sec && ngaps < 100000)
            gaps[ngaps++] = (at.tv_sec - prev.tv_sec) * 1000.0 +
                            (at.tv_usec - prev.tv_usec) / 1000.0;
        prev = at;
    }
    fclose(f);
    printf("frames=%ld bytes=%ld\n", frames, bytes);

    if (ngaps > 1) {
        qsort(gaps, ngaps, sizeof(double), cmp_double);
        long over25 = 0, over50 = 0;
        double sum = 0;
        for (long i = 0; i < ngaps; i++) {
            sum += gaps[i];
            // A 60 Hz slot is 16.7 ms. Past 25 ms a frame missed its slot;
            // past 50 ms it missed at least two.
            if (gaps[i] > 25.0) over25++;
            if (gaps[i] > 50.0) over50++;
        }
        printf("gap_ms mean %.2f p50 %.2f p99 %.2f max %.2f  over25=%ld over50=%ld of %ld\n",
               sum / ngaps, gaps[ngaps / 2], gaps[(long)(ngaps * 0.99)],
               gaps[ngaps - 1], over25, over50, ngaps);
    }
    return 0;
}
