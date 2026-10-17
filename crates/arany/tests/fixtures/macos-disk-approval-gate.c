#include <CoreFoundation/CoreFoundation.h>
#include <DiskArbitration/DiskArbitration.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static DADissenterRef approval(DADiskRef disk, void *context) {
    (void)context;
    const char *device = DADiskGetBSDName(disk);
    if (!device) _exit(3);
    printf("BLOCKED /dev/%s\n", device);
    fflush(stdout);
    if (getchar() != 'G') _exit(4);
    CFRunLoopStop(CFRunLoopGetCurrent());
    return NULL;
}

int main(int argc, char **argv) {
    if (argc != 3) return 1;
    alarm(15);
    DASessionRef session = DASessionCreate(kCFAllocatorDefault);
    CFStringRef name = CFStringCreateWithCString(kCFAllocatorDefault, argv[2], kCFStringEncodingUTF8);
    if (!session || !name) return 2;
    const void *keys[] = { kDADiskDescriptionVolumeNameKey };
    const void *values[] = { name };
    CFDictionaryRef match = CFDictionaryCreate(kCFAllocatorDefault, keys, values, 1,
        &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    if (!match) return 2;
    if (!strcmp(argv[1], "mount")) {
        DARegisterDiskMountApprovalCallback(session, match, approval, NULL);
    } else if (!strcmp(argv[1], "unmount")) {
        DARegisterDiskUnmountApprovalCallback(session, match, approval, NULL);
    } else return 1;
    DASessionScheduleWithRunLoop(session, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
    puts("READY");
    fflush(stdout);
    CFRunLoopRun();
    DASessionUnscheduleFromRunLoop(session, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
    CFRelease(match);
    CFRelease(name);
    CFRelease(session);
    return 0;
}
