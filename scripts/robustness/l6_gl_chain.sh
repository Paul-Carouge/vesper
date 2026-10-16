# L6: the OpenGL interposer chains with another preloaded interposer (Steam's
# overlay, MangoHud...) in both LD_PRELOAD orders: each swap reaches the fake
# libGL exactly once, never recursing. No GPU or display needed.
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
B=$L/chain
mkdir -p $B/lib
cat > $B/gl.c <<'C'
#include <stdio.h>
void glXSwapBuffers(void *d, unsigned long w) { puts("libGL swap"); }
C
cat > $B/other.c <<'C'
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <unistd.h>
static int depth;
void glXSwapBuffers(void *d, unsigned long w) {
  static void (*real)(void *, unsigned long);
  if (!real) real = dlsym(RTLD_NEXT, "glXSwapBuffers");
  if (++depth > 5) { puts("RECURSION"); _exit(3); }
  puts("other swap");
  if (real) real(d, w);
  depth--;
}
C
# main [linked | default | null | lib]: calls the swap directly, or through
# dlsym(RTLD_DEFAULT), dlsym(dlopen(NULL)), dlsym(dlopen("libGL.so.1")).
cat > $B/main.c <<'C'
#define _GNU_SOURCE
#include <dlfcn.h>
#include <string.h>
void glXSwapBuffers(void *d, unsigned long w);
int main(int argc, char **argv) {
  const char *m = argc > 1 ? argv[1] : "linked";
  void (*s)(void *, unsigned long) = glXSwapBuffers;
  if (!strcmp(m, "default")) s = dlsym(RTLD_DEFAULT, "glXSwapBuffers");
  if (!strcmp(m, "null")) s = dlsym(dlopen(NULL, RTLD_LAZY), "glXSwapBuffers");
  if (!strcmp(m, "lib")) s = dlsym(dlopen("libGL.so.1", RTLD_LAZY | RTLD_LOCAL), "glXSwapBuffers");
  s(0, 0);
  return 0;
}
C
gcc -shared -fPIC -o $B/lib/libGL.so.1 $B/gl.c && ln -sf libGL.so.1 $B/lib/libGL.so
gcc -shared -fPIC -o $B/libother.so $B/other.c
gcc -o $B/main $B/main.c -L$B/lib -lGL
V=$R/target/debug/libvesper_glcapture.so
# Each swap reaches libGL exactly once; a private dlopen of libGL bypasses
# the other interposer (as it would without us).
for order in "$B/libother.so:$V" "$V:$B/libother.so"; do
  for mode in linked default null lib; do
    out=$(LD_LIBRARY_PATH=$B/lib LD_PRELOAD=$order timeout 5 $B/main $mode | tr '\n' ' ')
    want="other swap libGL swap "
    [ $mode = lib ] && want="libGL swap "
    if [ "$out" = "$want" ]; then r=ok; else r=FAIL; fi
    echo "$r: ${order//$B\//} $mode: $out"
  done
done
