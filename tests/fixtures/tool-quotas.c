#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/prctl.h>

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    if (strcmp(argv[1], "memory") == 0) {
        if (prctl(PR_SET_NAME, "arany-oom-ready") != 0) return 8;
        if (raise(SIGSTOP) != 0) return 9;
        volatile unsigned char *bytes = malloc(600UL * 1024 * 1024);
        if (!bytes) return 3;
        for (size_t i = 0; i < 600UL * 1024 * 1024; i += 4096) bytes[i] = 1;
        return 4;
    }
    if (strcmp(argv[1], "pids") == 0) {
        unsigned count = 0;
        for (; count < 128; count++) {
            pid_t pid = fork();
            if (pid < 0) {
                if (errno != EAGAIN || count < 2 || count >= 64) return 5;
                printf("descendant quota enforced\n");
                return 0;
            }
            if (pid == 0) {
                close(STDIN_FILENO);
                close(STDOUT_FILENO);
                close(STDERR_FILENO);
                for (;;) pause();
            }
        }
        return 6;
    }
    return 7;
}
