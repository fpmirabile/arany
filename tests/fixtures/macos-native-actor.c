#include <errno.h>
#include <limits.h>
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc != 2) return 1;
    char cwd[PATH_MAX];
    int workspace = strcmp(argv[1], "workspace") == 0;
    if (workspace && (getcwd(cwd, sizeof(cwd)) == NULL || cwd[0] != '/')) return 6;
    int denied = strcmp(argv[1], "deny") == 0;
    char *arguments[] = {"/usr/bin/true", NULL};
    char *environment[] = {NULL};
    pid_t child = 0;
    int status = 0;
    int result = posix_spawn(&child, arguments[0], NULL, NULL, arguments, environment);
    if (denied ? result != EPERM : result != 0) return 2;
    if (!result && (waitpid(child, &status, 0) != child || status != 0)) return 3;
    child = fork();
    if (child == 0) _exit(0);
    if (denied ? child != -1 || errno != EPERM : child == -1) return 4;
    if (child > 0 && (waitpid(child, &status, 0) != child || status != 0)) return 5;
    puts("native actor success");
    return 0;
}
