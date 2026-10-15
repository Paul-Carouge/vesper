#!/usr/bin/python3 -I
"""Resize storm: XResizeWindow on the game's top-level windows, `count` times.
Usage: xresize.py [count] [delay_s]  (DISPLAY must point at gamescope's Xwayland)"""
import ctypes, ctypes.util, sys, time

x = ctypes.CDLL(ctypes.util.find_library("X11"))
x.XOpenDisplay.restype = ctypes.c_void_p
x.XDefaultRootWindow.restype = ctypes.c_ulong
x.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
x.XQueryTree.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong),
                         ctypes.POINTER(ctypes.POINTER(ctypes.c_ulong)), ctypes.POINTER(ctypes.c_uint)]
x.XGetGeometry.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_int),
                           ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_uint), ctypes.POINTER(ctypes.c_uint),
                           ctypes.POINTER(ctypes.c_uint), ctypes.POINTER(ctypes.c_uint)]
x.XResizeWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_uint, ctypes.c_uint]
x.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]

dpy = x.XOpenDisplay(None)
if not dpy:
    sys.exit("no display")


def geometry(win):
    r = ctypes.c_ulong()
    xx, yy = ctypes.c_int(), ctypes.c_int()
    w, h, b, d = (ctypes.c_uint() for _ in range(4))
    x.XGetGeometry(dpy, win, ctypes.byref(r), ctypes.byref(xx), ctypes.byref(yy), ctypes.byref(w), ctypes.byref(h), ctypes.byref(b), ctypes.byref(d))
    return w.value, h.value


def children(win):
    root, parent = ctypes.c_ulong(), ctypes.c_ulong()
    kids = ctypes.POINTER(ctypes.c_ulong)()
    n = ctypes.c_uint()
    if not x.XQueryTree(dpy, win, ctypes.byref(root), ctypes.byref(parent), ctypes.byref(kids), ctypes.byref(n)):
        return []
    return [kids[i] for i in range(n.value)]


root = x.XDefaultRootWindow(dpy)
wins = [w for w in children(root) if geometry(w)[0] >= 300]
print("windows:", [(hex(w), geometry(w)) for w in wins], flush=True)
count = int(sys.argv[1]) if len(sys.argv) > 1 else 200
delay = float(sys.argv[2]) if len(sys.argv) > 2 else 0.05
sizes = [(1280, 720), (800, 600), (1024, 768), (640, 400), (1500, 800)]
for i in range(count):
    w, h = sizes[i % len(sizes)]
    for win in wins:
        x.XResizeWindow(dpy, win, w, h)
    x.XSync(dpy, 0)
    time.sleep(delay)
print("resized", count, "times; now", [geometry(w) for w in wins], flush=True)
