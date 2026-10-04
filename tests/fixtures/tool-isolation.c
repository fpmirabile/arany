#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <arpa/inet.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/un.h>

int main(int argc, char **argv) {
    if (argc != 4) return 2;
    if (getenv("OPENAI_API_KEY") || getenv("ANTHROPIC_API_KEY") ||
        getenv("DBUS_SESSION_BUS_ADDRESS") || getenv("SSH_AUTH_SOCK")) return 3;
    if (access(argv[3], F_OK) == 0 || access("/run/user", F_OK) == 0 ||
        access("/sys/fs/cgroup", F_OK) == 0 || access("/home", F_OK) == 0) return 4;
    int fd = open("/proc/2/mem", O_RDWR);
    if (fd >= 0) { close(fd); return 5; }
    errno = 0;
    if (syscall(SYS_ptrace, 0, 0, 0, 0) != -1 || errno != EPERM) return 6;
    errno = 0;
    if (syscall(SYS_process_vm_writev, 2, 0, 0, 0, 0, 0) != -1 || errno != EPERM) return 7;
    errno = 0;
    if (syscall(SYS_unshare, 0) != -1 || errno != EPERM) return 8;
    errno = 0;
    if (syscall(SYS_clone3, 0, 0) != -1 || errno != ENOSYS) return 9;
    errno = 0;
    int tcp = socket(AF_INET, SOCK_STREAM, 0);
    if (tcp != -1 || errno != EPERM) return 10;
    errno = 0;
    int udp = socket(AF_INET, SOCK_DGRAM, 0);
    if (udp != -1 || errno != EPERM) return 11;
    errno = 0;
    int local = socket(AF_UNIX, SOCK_STREAM, 0);
    if (local != -1 || errno != EPERM) return 12;
    errno = 0;
    int datagram = socket(AF_UNIX, SOCK_DGRAM, 0);
    if (datagram != -1 || errno != EPERM) return 13;
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_DGRAM, 0, pair) != 0) return 14;
    char observed;
    if (send(pair[0], "x", 1, 0) != 1 || read(pair[1], &observed, 1) != 1 || observed != 'x') return 15;
    struct sockaddr_un local_address = { .sun_family = AF_UNIX, .sun_path = "/scratch/host-socket" };
    errno = 0;
    if (connect(pair[0], (struct sockaddr *)&local_address, sizeof(local_address)) != -1 || errno != EPERM) return 16;
    errno = 0;
    if (sendto(pair[0], "x", 1, 0, (struct sockaddr *)&local_address, sizeof(local_address)) != -1 || errno != EPERM) return 17;
    struct iovec message_buffer = { .iov_base = "x", .iov_len = 1 };
    struct msghdr message = { .msg_name = &local_address, .msg_namelen = sizeof(local_address), .msg_iov = &message_buffer, .msg_iovlen = 1 };
    errno = 0;
    if (sendmsg(pair[0], &message, 0) != -1 || errno != EPERM) return 18;
    close(pair[0]);
    close(pair[1]);
    puts("isolation and inherited syscall restrictions passed");
    return 0;
}
