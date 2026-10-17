#define _GNU_SOURCE
#include <stdlib.h>
#include <unistd.h>
#include <sys/prctl.h>
#include <sys/types.h>

int main(void) {
    if (fork() == 0) {
        if (setsid() < 0) return 2;
        if (fork() == 0) {
            if (prctl(PR_SET_NAME, "arany-test-leaf") != 0) return 3;
            for (;;) pause();
        }
        _exit(0);
    }
    if (prctl(PR_SET_NAME, "arany-test-root") != 0) return 4;
    for (;;) pause();
}
