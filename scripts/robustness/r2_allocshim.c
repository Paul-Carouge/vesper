// LD_PRELOAD malloc counter per thread (audit round 2). Build:
//   gcc -O2 -shared -fPIC -o $SHIM r2_allocshim.c
// SIGUSR2 rewrites $SHIM_OUT with "tid<TAB>mallocs<TAB>bytes<TAB>frees" per
// thread. Counts glibc malloc/calloc/realloc/free calls (Rust's default allocator).
#define _GNU_SOURCE
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

extern void *__libc_malloc(size_t), *__libc_calloc(size_t, size_t), *__libc_realloc(void *, size_t);
extern void __libc_free(void *);

#define MAXT 1024
struct slot { int tid; unsigned long n, bytes, frees; };
static struct slot slots[MAXT];
static int nslots;
static __thread struct slot *me;
static __thread int busy;

static struct slot *self(void) {
    if (!me) {
        int i = __atomic_fetch_add(&nslots, 1, __ATOMIC_SEQ_CST);
        if (i >= MAXT) return NULL;
        me = &slots[i];
        me->tid = (int)syscall(SYS_gettid);
    }
    return me;
}
#define COUNT(sz) do { struct slot *s = self(); if (s) { s->n++; s->bytes += (sz); } } while (0)

void *malloc(size_t n) { COUNT(n); return __libc_malloc(n); }
void *calloc(size_t a, size_t b) { COUNT(a * b); return __libc_calloc(a, b); }
void *realloc(void *p, size_t n) { COUNT(n); return __libc_realloc(p, n); }
void free(void *p) { if (p) { struct slot *s = self(); if (s) s->frees++; } __libc_free(p); }

static void dump(int sig) {
    (void)sig;
    const char *out = getenv("SHIM_OUT");
    if (!out || busy) return;
    busy = 1;
    char tmp[512];
    snprintf(tmp, sizeof tmp, "%s.tmp", out);
    int fd = open(tmp, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { busy = 0; return; }
    int n = nslots < MAXT ? nslots : MAXT;
    char line[256];
    for (int i = 0; i < n; i++) {
        int l = snprintf(line, sizeof line, "%d\t%lu\t%lu\t%lu\n", slots[i].tid, slots[i].n, slots[i].bytes, slots[i].frees);
        if (write(fd, line, l) < 0) break;
    }
    close(fd);
    rename(tmp, out);
    busy = 0;
}
__attribute__((constructor)) static void init(void) { signal(SIGUSR2, dump); }
