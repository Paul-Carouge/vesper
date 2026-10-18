// LD_PRELOAD slow disk (lab round 3, r3_slowdisk.sh): write()/pwrite() to a *.mkv file
// stall SLOW_MS (800) every SLOW_EVERY_S (5; 0: once) from SLOW_AFTER_S (12)
// after the process started. While a stall lasts, every other *.mkv write
// waits too, as on a real disk. Each stall is logged on stderr. Build:
//   gcc -O2 -shared -fPIC -o slowdisk.so slowdisk.c -ldl -lpthread
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static ssize_t (*real_write)(int, const void *, size_t);
static ssize_t (*real_pwrite)(int, const void *, size_t, off_t);
static ssize_t (*real_pwrite64)(int, const void *, size_t, off_t);
static pthread_mutex_t disk = PTHREAD_MUTEX_INITIALIZER;
static double t0, next_stall, every, ms;

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static double env(const char *k, double def) {
    const char *v = getenv(k);
    return v && *v ? atof(v) : def;
}

__attribute__((constructor)) static void init(void) {
    t0 = now();
    ms = env("SLOW_MS", 800);
    every = env("SLOW_EVERY_S", 5);
    next_stall = t0 + env("SLOW_AFTER_S", 12);
}

static int is_mkv(int fd) {
    char link[64], path[512];
    snprintf(link, sizeof link, "/proc/self/fd/%d", fd);
    ssize_t n = readlink(link, path, sizeof path - 1);
    return n > 4 && memcmp(path + n - 4, ".mkv", 4) == 0;
}

// Stalls when one is due; otherwise waits out a stall in progress.
static void disk_wait(int fd) {
    if (ms <= 0 || !is_mkv(fd)) return;
    pthread_mutex_lock(&disk);
    double t = now();
    if (next_stall > 0 && t >= next_stall) {
        next_stall = every > 0 ? t + every : 0;
        long whole = (long)(ms / 1000);
        struct timespec d = {whole, (long)((ms - whole * 1000.0) * 1e6)};
        while (nanosleep(&d, &d) == -1 && errno == EINTR) {}
        fprintf(stderr, "slowdisk: stalled %.0f ms at t=%.1f s\n", ms, t - t0);
    }
    pthread_mutex_unlock(&disk);
}

// Resolved on first use: a write may come before the constructor runs.
ssize_t write(int fd, const void *buf, size_t n) {
    if (!real_write) real_write = dlsym(RTLD_NEXT, "write");
    disk_wait(fd);
    return real_write(fd, buf, n);
}

ssize_t pwrite(int fd, const void *buf, size_t n, off_t off) {
    if (!real_pwrite) real_pwrite = dlsym(RTLD_NEXT, "pwrite");
    disk_wait(fd);
    return real_pwrite(fd, buf, n, off);
}

ssize_t pwrite64(int fd, const void *buf, size_t n, off_t off) {
    if (!real_pwrite64) real_pwrite64 = dlsym(RTLD_NEXT, "pwrite64");
    disk_wait(fd);
    return real_pwrite64(fd, buf, n, off);
}
