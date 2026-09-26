/* A stand-in for nesbox and for a stale process: stays up (pause) when its
 * first argument names "other" (another VM's process), exits at once otherwise. */
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
    for (int i = 1; i < argc; i++)
        if (strstr(argv[i], "other"))
            for (;;) pause();
    return 0;
}
