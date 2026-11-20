#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static int (*next_clock)(clockid_t, struct timespec *);
static int output = -1;
static pid_t main_thread;

__attribute__((constructor)) static void setup(void) {
    next_clock = dlsym(RTLD_NEXT, "clock_gettime");
    main_thread = getpid();
    const char *path = getenv("FELL_CLOCK_PROBE");
    if (path != NULL)
        output = open(path, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0600);
}

static long long nanoseconds(const struct timespec *value) {
    return (long long)value->tv_sec * 1000000000LL + value->tv_nsec;
}

int clock_gettime(clockid_t clock, struct timespec *value) {
    if (next_clock == NULL)
        return syscall(SYS_clock_gettime, clock, value);
    int result = next_clock(clock, value);
    if (result != 0 || output < 0 || clock != CLOCK_THREAD_CPUTIME_ID ||
        (pid_t)syscall(SYS_gettid) != main_thread)
        return result;
    struct timespec wall;
    struct rusage counters;
    if (next_clock(CLOCK_MONOTONIC, &wall) == 0 &&
        getrusage(RUSAGE_THREAD, &counters) == 0) {
        char row[160];
        int length = snprintf(row, sizeof(row), "%lld\t%lld\t%ld\t%ld\n",
                              nanoseconds(&wall), nanoseconds(value),
                              counters.ru_nvcsw, counters.ru_nivcsw);
        if (length > 0 && length < (int)sizeof(row))
            write(output, row, (size_t)length);
    }
    return result;
}
