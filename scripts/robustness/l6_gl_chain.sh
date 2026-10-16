# L6: the OpenGL interposer chains with another preloaded interposer (Steam's
# overlay, MangoHud...) in both LD_PRELOAD orders: each swap reaches the fake
# libGL exactly once, never recursing. No GPU or display needed.
. $LABROOT/lib.sh
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
cat > $B/main.c <<'C'
#include <stdio.h>
void glXSwapBuffers(void *d, unsigned long w);
int main(void) { glXSwapBuffers(0, 0); return 0; }
C
gcc -shared -fPIC -o $B/lib/libGL.so.1 $B/gl.c && ln -sf libGL.so.1 $B/lib/libGL.so
gcc -shared -fPIC -o $B/libother.so $B/other.c
gcc -o $B/main $B/main.c -L$B/lib -lGL
V=$R/target/debug/libvesper_glcapture.so
for order in "$B/libother.so:$V" "$V:$B/libother.so"; do
  out=$(LD_LIBRARY_PATH=$B/lib LD_PRELOAD=$order timeout 5 $B/main | tr '\n' ' ')
  case "$out" in
    "other swap libGL swap ") echo "ok: ${order//$B\//}" ;;
    *) echo "FAIL: ${order//$B\//}: $out" ;;
  esac
done
