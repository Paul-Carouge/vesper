// gltest <glx|glx-dlopen|egl|egl-es-srgb> <frames | seconds"s"> [WxH]
// egl-es-srgb: OpenGL ES 3 on an sRGB EGL surface (writes are encoded:
// the background green 0.2 is stored as about 124/255).
// Clears with a cycling color and draws a moving white box with the scissor
// test, leaving the scissor ON at swap (checks the interposer restores it).
// Prints the average frame time. No vsync (swap interval 0).
#define _GNU_SOURCE
#include <X11/Xlib.h>
#include <GL/gl.h>
#include <GL/glx.h>
#include <EGL/egl.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

typedef void (*swap_fn)(Display *, GLXDrawable);

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "glx";
    int frames = argc > 2 ? atoi(argv[2]) : 600;
    double dur = argc > 2 && strchr(argv[2], 's') ? atof(argv[2]) : 0;
    int W = 1280, H = 720;
    if (argc > 3) sscanf(argv[3], "%dx%d", &W, &H);
    Display *dpy = XOpenDisplay(NULL);
    if (!dpy) { fprintf(stderr, "no display\n"); return 1; }
    Window root = DefaultRootWindow(dpy);
    XSetWindowAttributes swa = {0};
    Window win;
    EGLDisplay edpy = 0; EGLSurface esurf = 0;
    swap_fn swap = NULL;
    int es = strcmp(mode, "egl-es-srgb") == 0;
    if (es || strcmp(mode, "egl") == 0) {
        win = XCreateWindow(dpy, root, 0, 0, W, H, 0, CopyFromParent, InputOutput, CopyFromParent, 0, &swa);
        XMapWindow(dpy, win);
        edpy = eglGetDisplay((EGLNativeDisplayType)dpy);
        eglInitialize(edpy, NULL, NULL);
        eglBindAPI(es ? EGL_OPENGL_ES_API : EGL_OPENGL_API);
        EGLint attr[] = {EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_SURFACE_TYPE, EGL_WINDOW_BIT, EGL_RENDERABLE_TYPE, es ? EGL_OPENGL_ES3_BIT : EGL_OPENGL_BIT, EGL_NONE};
        EGLConfig cfg; EGLint n;
        eglChooseConfig(edpy, attr, &cfg, 1, &n);
        EGLint cattr[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE};
        EGLContext ctx = eglCreateContext(edpy, cfg, EGL_NO_CONTEXT, es ? cattr : NULL);
        EGLint sattr[] = {EGL_GL_COLORSPACE, EGL_GL_COLORSPACE_SRGB, EGL_NONE};
        esurf = eglCreateWindowSurface(edpy, cfg, (EGLNativeWindowType)win, es ? sattr : NULL);
        eglMakeCurrent(edpy, esurf, esurf, ctx);
        eglSwapInterval(edpy, 0);
    } else {
        int attr[] = {GLX_RGBA, GLX_DOUBLEBUFFER, GLX_RED_SIZE, 8, GLX_GREEN_SIZE, 8, GLX_BLUE_SIZE, 8, None};
        XVisualInfo *vi = glXChooseVisual(dpy, DefaultScreen(dpy), attr);
        swa.colormap = XCreateColormap(dpy, root, vi->visual, AllocNone);
        win = XCreateWindow(dpy, root, 0, 0, W, H, 0, vi->depth, InputOutput, vi->visual, CWColormap, &swa);
        XMapWindow(dpy, win);
        GLXContext ctx = glXCreateContext(dpy, vi, NULL, True);
        glXMakeCurrent(dpy, win, ctx);
        if (strcmp(mode, "glx-dlopen") == 0) {
            // Like SDL: a private handle, then dlsym on it.
            void *h = dlopen("libGL.so.1", RTLD_NOW | RTLD_LOCAL);
            swap = (swap_fn)dlsym(h, "glXSwapBuffers");
        } else {
            swap = glXSwapBuffers;
        }
    }
    printf("mode %s, GL %s, swap %p\n", mode, glGetString(GL_VERSION), (void *)swap);
    double t0 = now(), worst = 0, prev = t0;
    int i;
    for (i = 0; dur ? now() - t0 < dur : i < frames; i++) {
        float t = i / 120.0f;
        glDisable(GL_SCISSOR_TEST);
        glClearColor(0.5f + 0.5f * (i % 120) / 120.0f, 0.2f, 0.6f - 0.4f * (i % 120) / 120.0f, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        glEnable(GL_SCISSOR_TEST);
        // top-left marker (GL is bottom-up): a fixed green square near the top
        glScissor(20, H - 120, 100, 100);
        glClearColor(0, 1, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        int x = (int)((W - 160) * (0.5f + 0.5f * __builtin_sinf(t * 3)));
        glScissor(x, H / 2 - 80, 160, 160);
        glClearColor(1, 1, 1, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        // scissor stays enabled across the swap
        if (esurf) eglSwapBuffers(edpy, esurf);
        else swap(dpy, win);
        double n = now();
        if (n - prev > worst) worst = n - prev;
        prev = n;
        while (XPending(dpy)) { XEvent e; XNextEvent(dpy, &e); }
    }
    double el = now() - t0;
    printf("frames %d avg %.3f ms worst %.3f ms\n", i, el * 1000 / i, worst * 1000);
    return 0;
}
