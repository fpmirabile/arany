#include <unistd.h>
#include <mach/vm_page_size.h>

typedef struct {
    const void *replacement;
    const void *replacee;
} arany_interpose;

static long arany_sysconf(int name) {
    if (name == _SC_PAGESIZE) return (long)vm_page_size;
    return sysconf(name);
}

static int arany_getpagesize(void) {
    return (int)vm_page_size;
}

__attribute__((used, section("__DATA,__interpose")))
static const arany_interpose arany_interposes[] = {
    { (const void *)&arany_sysconf, (const void *)&sysconf },
    { (const void *)&arany_getpagesize, (const void *)&getpagesize },
};
