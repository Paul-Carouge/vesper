// LD_PRELOAD syscall-wrapper counter, per thread. Dumps to $SHIM_OUT at exit.
// Counts libc wrapper calls (what strace would mostly see), not raw syscalls
// made inside libc (e.g. pthread futexes).
#define _GNU_SOURCE
#include <dlfcn.h>
#include <poll.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

enum { IOCTL, READ, WRITE, WRITEV, SENDMSG, RECVMSG, POLL, PPOLL, NANOSLEEP,
       CLOCK_NANOSLEEP, LSEEK, CLOSE, DUP, FCNTL, FSTAT, FSTATFS, READLINK, SYSCALL_FUTEX,
       SYSCALL_OTHER, MMAP, MUNMAP, EPOLL_WAIT, NCALLS };
static const char *names[NCALLS] = {"ioctl", "read", "write", "writev", "sendmsg", "recvmsg",
    "poll", "ppoll", "nanosleep", "clock_nanosleep", "lseek", "close", "dup", "fcntl", "fstat",
    "fstatfs", "readlink", "futex(syscall)", "syscall(other)", "mmap", "munmap", "epoll_wait"};

#define MAXT 512
struct slot { int tid; char name[16]; unsigned long n[NCALLS]; };
static struct slot slots[MAXT];
static int nslots;
static __thread struct slot *me;

static struct slot *self(void) {
    if (!me) {
        int i = __atomic_fetch_add(&nslots, 1, __ATOMIC_SEQ_CST);
        if (i >= MAXT) return NULL;
        me = &slots[i];
        me->tid = (int)syscall(SYS_gettid);
    }
    return me;
}
#define COUNT(k) do { struct slot *s = self(); if (s) { s->n[k]++; if ((s->n[k] & 1023) == 1) prctl(PR_GET_NAME, s->name); } } while (0)
#define REAL(ret, name, ...) static ret (*real)(__VA_ARGS__); if (!real) real = dlsym(RTLD_NEXT, #name)

int ioctl(int fd, unsigned long req, ...) {
    va_list ap; va_start(ap, req); void *arg = va_arg(ap, void *); va_end(ap);
    REAL(int, ioctl, int, unsigned long, ...);
    COUNT(IOCTL); return real(fd, req, arg);
}
ssize_t read(int fd, void *b, size_t n) { REAL(ssize_t, read, int, void *, size_t); COUNT(READ); return real(fd, b, n); }
#include <execinfo.h>
#include <fcntl.h>
ssize_t write(int fd, const void *b, size_t n) {
    REAL(ssize_t, write, int, const void *, size_t); COUNT(WRITE);
    struct slot *s = self();
    if (s && s->tid == getpid() && s->n[WRITE] == 200 && getenv("SHIM_BT")) {
        void *bt[64]; int k = backtrace(bt, 64);
        int o = open(getenv("SHIM_BT"), O_WRONLY | O_CREAT | O_APPEND, 0644);
        if (o >= 0) { char m[64]; int l = snprintf(m, sizeof m, "write fd=%d n=%zu\n", fd, n); real(o, m, l); backtrace_symbols_fd(bt, k, o); close(o); }
    }
    return real(fd, b, n);
}
ssize_t writev(int fd, const struct iovec *v, int c) { REAL(ssize_t, writev, int, const struct iovec *, int); COUNT(WRITEV); return real(fd, v, c); }
ssize_t sendmsg(int fd, const struct msghdr *m, int f) { REAL(ssize_t, sendmsg, int, const struct msghdr *, int); COUNT(SENDMSG); return real(fd, m, f); }
ssize_t recvmsg(int fd, struct msghdr *m, int f) { REAL(ssize_t, recvmsg, int, struct msghdr *, int); COUNT(RECVMSG); return real(fd, m, f); }
int poll(struct pollfd *p, nfds_t n, int t) { REAL(int, poll, struct pollfd *, nfds_t, int); COUNT(POLL); return real(p, n, t); }
int ppoll(struct pollfd *p, nfds_t n, const struct timespec *t, const sigset_t *s) { REAL(int, ppoll, struct pollfd *, nfds_t, const struct timespec *, const sigset_t *); COUNT(PPOLL); return real(p, n, t, s); }
int nanosleep(const struct timespec *a, struct timespec *b) { REAL(int, nanosleep, const struct timespec *, struct timespec *); COUNT(NANOSLEEP); return real(a, b); }
int clock_nanosleep(clockid_t c, int f, const struct timespec *a, struct timespec *b) { REAL(int, clock_nanosleep, clockid_t, int, const struct timespec *, struct timespec *); COUNT(CLOCK_NANOSLEEP); return real(c, f, a, b); }
off_t lseek(int fd, off_t o, int w) { REAL(off_t, lseek, int, off_t, int); COUNT(LSEEK); return real(fd, o, w); }
off_t lseek64(int fd, off_t o, int w) { REAL(off_t, lseek64, int, off_t, int); COUNT(LSEEK); return real(fd, o, w); }
int close(int fd) { REAL(int, close, int); COUNT(CLOSE); return real(fd); }
int dup(int fd) { REAL(int, dup, int); COUNT(DUP); return real(fd); }
int fcntl(int fd, int cmd, ...) { va_list ap; va_start(ap, cmd); void *a = va_arg(ap, void *); va_end(ap); REAL(int, fcntl, int, int, ...); COUNT(FCNTL); return real(fd, cmd, a); }
int fcntl64(int fd, int cmd, ...) { va_list ap; va_start(ap, cmd); void *a = va_arg(ap, void *); va_end(ap); REAL(int, fcntl64, int, int, ...); COUNT(FCNTL); return real(fd, cmd, a); }
int fstat(int fd, struct stat *st) { REAL(int, fstat, int, struct stat *); COUNT(FSTAT); return real(fd, st); }

int statx(int d, const char *p, int f, unsigned m, struct statx *s) { REAL(int, statx, int, const char *, int, unsigned, struct statx *); COUNT(FSTAT); return real(d, p, f, m, s); }
ssize_t readlink(const char *p, char *b, size_t n) { REAL(ssize_t, readlink, const char *, char *, size_t); COUNT(READLINK); return real(p, b, n); }
int epoll_wait(int e, void *ev, int m, int t) { REAL(int, epoll_wait, int, void *, int, int); COUNT(EPOLL_WAIT); return real(e, ev, m, t); }
long syscall(long nr, ...) {
    va_list ap; va_start(ap, nr);
    long a = va_arg(ap, long), b = va_arg(ap, long), c = va_arg(ap, long), d = va_arg(ap, long), e = va_arg(ap, long), f = va_arg(ap, long);
    va_end(ap);
    REAL(long, syscall, long, ...);
    if (nr == SYS_futex) COUNT(SYSCALL_FUTEX); else if (nr != SYS_gettid) COUNT(SYSCALL_OTHER);
    return real(nr, a, b, c, d, e, f);
}

__attribute__((destructor)) static void dump(void) {
    const char *out = getenv("SHIM_OUT");
    if (!out) return;
    FILE *f = fopen(out, "w");
    if (!f) return;
    int n = nslots < MAXT ? nslots : MAXT;
    for (int i = 0; i < n; i++) {
        fprintf(f, "%d\t%s", slots[i].tid, slots[i].name);
        for (int k = 0; k < NCALLS; k++)
            if (slots[i].n[k]) fprintf(f, "\t%s=%lu", names[k], slots[i].n[k]);
        fputc('\n', f);
    }
    fclose(f);
}
