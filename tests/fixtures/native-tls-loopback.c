#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dlfcn.h>
#include <errno.h>
#include <netdb.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>

int getaddrinfo(const char *name, const char *service,
                const struct addrinfo *hints, struct addrinfo **result) {
    int (*resolve)(const char *, const char *, const struct addrinfo *, struct addrinfo **) =
        dlsym(RTLD_NEXT, "getaddrinfo");
    if (!resolve || !name ||
        (strcmp(name, "api.openai.com") && strcmp(name, "api.anthropic.com")))
        return EAI_NONAME;
    return resolve("127.0.0.1", service, hints, result);
}

int connect(int socket, const struct sockaddr *address, socklen_t length) {
    int (*dial)(int, const struct sockaddr *, socklen_t) = dlsym(RTLD_NEXT, "connect");
    if (!dial || !address) { errno = EACCES; return -1; }
    if (address->sa_family == AF_UNIX) return dial(socket, address, length);
    if (address->sa_family != AF_INET || length != sizeof(struct sockaddr_in)) {
        errno = EACCES;
        return -1;
    }
    const struct sockaddr_in *original = (const struct sockaddr_in *) address;
    const char *selected = getenv("ARANY_TEST_TLS_PORT");
    char *end = NULL;
    long port = selected ? strtol(selected, &end, 10) : 0;
    if (!selected || *end || port < 1024 || port > 65535 ||
        original->sin_addr.s_addr != htonl(INADDR_LOOPBACK) ||
        original->sin_port != htons(443)) {
        errno = EACCES;
        return -1;
    }
    struct sockaddr_in target = *original;
    target.sin_port = htons((unsigned short) port);
    return dial(socket, (const struct sockaddr *) &target, sizeof(target));
}
