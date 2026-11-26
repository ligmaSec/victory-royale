"""Draw a solid-color, PID-labeled X11 window for the private screenshot regression."""
import ctypes as c
import os
from pathlib import Path
import sys
import time

x = c.CDLL("libX11.so.6")
x.XOpenDisplay.restype = c.c_void_p
display = c.c_void_p(x.XOpenDisplay(None))
if not display:
    sys.exit("X11 fixture display unavailable")
x.XDefaultRootWindow.restype = c.c_ulong
x.XCreateSimpleWindow.restype = c.c_ulong
root = c.c_ulong(x.XDefaultRootWindow(display))
window = c.c_ulong(x.XCreateSimpleWindow(display, root, 0, 0, 160, 120, 0, 0, int(sys.argv[1], 16)))
atom = c.c_ulong(x.XInternAtom(display, b"_NET_WM_PID", 0))
pid = c.c_ulong(os.getpid())
x.XChangeProperty(display, window, atom, c.c_ulong(6), 32, 0, c.byref(pid), 1)
x.XMapWindow(display, window)
x.XSync(display, 0)
Path(sys.argv[2]).touch()
time.sleep(60)
