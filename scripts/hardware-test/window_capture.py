"""Capture one of the supplied Unix game process IDs from its own X11 pixmap."""
import ctypes as c
import os
from pathlib import Path
import struct
import sys
import zlib


class Image(c.Structure):
    _fields_ = [(key, c.c_int) for key in ("width", "height", "xoffset", "format")]
    _fields_ += [("data", c.c_void_p)]
    _fields_ += [(key, c.c_int) for key in ("byte_order", "bitmap_unit", "bitmap_bit_order",
                                             "bitmap_pad", "depth", "stride", "bits")]
    _fields_ += [(key, c.c_ulong) for key in ("red", "green", "blue")]


def png(image):
    if (image.byte_order, image.bits, image.red, image.green, image.blue) != (0, 32, 0xff0000, 0xff00, 0xff):
        raise ValueError("unsupported X11 pixel format")
    step = max(1, (image.width + 959) // 960, (image.height + 539) // 540)
    pixels = bytearray()
    for y in range(0, image.height, step):
        row = c.string_at(image.data + y * image.stride, image.width * 4)
        rgb = bytearray(3 * len(row[::4 * step]))
        for channel in range(3):
            rgb[channel::3] = row[2 - channel::4 * step]
        pixels.extend(b"\0" + rgb)

    def chunk(tag, data):
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data))

    header = struct.pack(">IIBBBBB", (image.width + step - 1) // step,
                         (image.height + step - 1) // step, 8, 2, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(pixels, 3)) + chunk(b"IEND", b"")


def capture(pids):
    x = c.CDLL("libX11.so.6")
    composite = c.CDLL("libXcomposite.so.1")
    ptr, xid, num = c.c_void_p, c.c_ulong, c.c_int
    x.XOpenDisplay.argtypes = [c.c_char_p]
    x.XOpenDisplay.restype = ptr
    display = x.XOpenDisplay(None)
    if not display:
        raise ValueError("no X11 display")
    # Give Xlib correctly sized XIDs and pointers on 64-bit hosts. All server resources
    # are released when this bounded, single-shot child disconnects.
    d = ptr(display)
    x.XDefaultRootWindow.restype = xid
    x.XInternAtom.restype = xid
    pid_atom = xid(x.XInternAtom(d, b"_NET_WM_PID", 1))
    if not pid_atom.value:
        raise ValueError("no window process IDs")
    root = x.XDefaultRootWindow(d)
    pending = [root]
    seen = set()
    matches = []
    while pending and len(seen) < 8192:
        w = pending.pop()
        if w in seen:
            continue
        seen.add(w)
        owner = ptr()
        kind, count, remaining, fmt = xid(), xid(), xid(), num()
        x.XGetWindowProperty(d, xid(w), pid_atom, c.c_long(0), c.c_long(1), 0, xid(6),
                             c.byref(kind), c.byref(fmt), c.byref(count), c.byref(remaining), c.byref(owner))
        if owner:
            owned = (kind.value == 6 and fmt.value == 32 and count.value == 1
                     and c.cast(owner, c.POINTER(xid))[0] in pids)
            x.XFree(owner)
            if owned and w != root:
                parent = xid()
                fields = [num() for _ in range(6)]
                if x.XGetGeometry(d, xid(w), c.byref(parent), *(c.byref(v) for v in fields)):
                    width, height = fields[2].value, fields[3].value
                    if 64 <= width <= 8192 and 64 <= height <= 8192 and width * height <= 34000000:
                        matches.append((width * height, w, width, height))
        children = c.POINTER(xid)()
        parent, root_id, n = xid(), xid(), c.c_uint()
        x.XQueryTree(d, xid(w), c.byref(root_id), c.byref(parent), c.byref(children), c.byref(n))
        if children:
            pending.extend(children[:min(n.value, 8192)])
            x.XFree(children)
    if not matches:
        raise ValueError("game window unavailable")
    _, window, width, height = max(matches)
    composite.XCompositeRedirectWindow(d, xid(window), 0)
    composite.XCompositeNameWindowPixmap.restype = xid
    pixmap = composite.XCompositeNameWindowPixmap(d, xid(window))
    x.XGetImage.restype = c.POINTER(Image)
    image = x.XGetImage(d, xid(pixmap), 0, 0, width, height, xid(-1), 2)
    if not image:
        raise ValueError("game window has no readable pixmap")
    # Pixmaps do not carry a visual. Query the window's color masks without using
    # its potentially obscured pixels for the screenshot.
    sample = x.XGetImage(d, xid(window), 0, 0, 1, 1, xid(-1), 2)
    if sample:
        for channel in ("red", "green", "blue"):
            setattr(image.contents, channel, getattr(sample.contents, channel))
    return png(image.contents)


if __name__ == "__main__":
    try:
        if len(sys.argv) < 3 or not os.environ.get("DISPLAY"):
            sys.exit(1)
        Path(sys.argv[1]).write_bytes(capture({int(pid) for pid in sys.argv[2:]}))
    except (OSError, ValueError):
        sys.exit(1)
