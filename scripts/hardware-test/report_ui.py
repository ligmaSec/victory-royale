"""Keyboard-driven hardware report screens for Windows and POSIX terminals."""
import contextlib
import os
import shutil
import sys
import textwrap

WARNING = "WARNING: LOGS MAY CONTAIN SENSITIVE INFORMATION"


def terminal_available():
    if not sys.stdin.isatty() or not sys.stdout.isatty() or os.environ.get("TERM") == "dumb":
        return False
    if sys.platform != "win32":
        return True
    import ctypes
    import msvcrt
    from ctypes import wintypes
    handle = wintypes.HANDLE(msvcrt.get_osfhandle(sys.stdout.fileno()))
    mode = wintypes.DWORD()
    kernel = ctypes.windll.kernel32
    return bool(kernel.GetConsoleMode(handle, ctypes.byref(mode))
                and kernel.SetConsoleMode(handle, mode.value | 4))


@contextlib.contextmanager
def session():
    if sys.platform != "win32":
        import termios
        import tty
        fd = sys.stdin.fileno()
        saved = termios.tcgetattr(fd)
        tty.setcbreak(fd)
    try:
        sys.stdout.write("\x1b[?1049h\x1b[?25l")
        sys.stdout.flush()
        yield
    finally:
        if sys.platform != "win32":
            termios.tcsetattr(fd, termios.TCSADRAIN, saved)
        sys.stdout.write("\x1b[0m\x1b[?25h\x1b[?1049l")
        sys.stdout.flush()


def read_key():
    if sys.platform == "win32":
        import msvcrt
        import time
        if not msvcrt.kbhit():
            time.sleep(0.06)
            return ""
        ch = msvcrt.getwch()
        if ch in ("\x00", "\xe0"):
            return {"H": "up", "P": "down", "I": "pageup", "Q": "pagedown"}.get(msvcrt.getwch(), "")
    else:
        import select
        fd = sys.stdin.fileno()
        if not select.select([fd], [], [], 0.2)[0]:
            return ""
        data = os.read(fd, 1)
        if not data:
            return "escape"
        ch = data.decode("ascii", "ignore")
        if ch == "\x1b" and select.select([fd], [], [], 0.05)[0]:
            # Stop at the end of this escape sequence. Leave the next key for the next screen.
            seq = os.read(fd, 1)
            if seq in (b"[", b"O"):
                while len(seq) < 12 and select.select([fd], [], [], 0.05)[0]:
                    seq += os.read(fd, 1)
                    if 64 <= seq[-1] <= 126:
                        break
            return {b"[A": "up", b"[B": "down", b"OA": "up", b"OB": "down",
                    b"[5~": "pageup", b"[6~": "pagedown"}.get(seq, "")
    if ch == "\x03":
        raise KeyboardInterrupt
    return {"\r": "enter", "\n": "enter", "\x1b": "escape", "\x04": "escape"}.get(ch, ch.lower())


def clean(text):
    return "".join(ch if ch.isprintable() else " " for ch in text)


def layout(title, context, lines, options, selected, offset, width, height, color):
    """Render one viewport. Keep actions and navigation visible as the details scroll."""
    width = max(1, width - 1)
    inner = max(1, width - 4)
    def ink(text, style):
        return f"\x1b[{style}m{text}\x1b[0m" if color else text

    def wrap(line):
        return textwrap.wrap(clean(line), inner, replace_whitespace=False, drop_whitespace=True) or [""]

    header = [ink(("  VIctory  /  " + context)[:width], "1;96"),
              ink(("  " + title)[:width], "1;97")]
    header.extend(ink(("  " + part).ljust(width)[:width], "1;97;41") for part in wrap(WARNING))
    detail = ["  " + part for part in wrap(options[selected][2])]
    footer = "↑↓ Choose   Enter Select   Esc Back"
    # A wide window keeps results beside the actions. At smaller sizes commands
    # move below the content, with the active choice always visible.
    wide = width >= 88 and height >= len(header) + len(options) + len(detail) + 4
    right_width = 32
    left_width = width - right_width - 1
    body_width = left_width - 4 if wide else inner
    body = [part for line in lines for part in
            (textwrap.wrap(clean(line), body_width, replace_whitespace=False) or [""])]
    show_all = wide or height >= len(header) + len(options) + len(detail) + 7
    choices = []
    for i, (_, label, _) in enumerate(options):
        if show_all or i == selected:
            prefix = "› " if i == selected else "  "
            suffix = "" if show_all else f"  ({i + 1}/{len(options)})"
            row = (prefix + label + suffix)[:right_width - 4 if wide else inner]
            choices.append(ink(row.ljust(right_width - 4 if wide else inner), "1;30;106") if i == selected else row)
    room = max(0, height - len(header) - len(detail) - 4 - (0 if wide else len(choices)))
    offset = min(offset, max(0, len(body) - max(1, room)))
    page = body[offset:offset + room]
    if wide:
        def top(label, size):
            return ink("╭─ " + label + " " + "─" * (size - len(label) - 5) + "╮", "36")
        bar = ink("│", "36")
        rows = header + [top("Report summary" if title == "Hardware test results" else "Details", left_width)
                         + " " + top("Actions", right_width)]
        for i in range(room):
            text = page[i] if i < len(page) else ""
            choice = choices[i] if i < len(choices) else ""
            style = "1;93" if "timings unavailable" in text else "1;97" if text.startswith(("Main ", "Candidate ")) else ""
            cell = text.ljust(body_width)
            if style:
                cell = ink(cell, style)
            # ANSI colors occupy no cells, so pad against the underlying label.
            option_width = min(right_width - 4, 2 + len(options[i][1])) if i < len(choices) else 0
            if i == selected:
                option_width = right_width - 4
            rows.append(bar + " " + cell + " " + bar + " " + bar + " " + choice
                        + " " * (right_width - 4 - option_width) + " " + bar)
        rows.append(ink("╰" + "─" * (left_width - 2) + "╯ ╰" + "─" * (right_width - 2) + "╯", "36"))
    else:
        rows = header + [""]
        rows.extend("  " + part for part in page)
        rows.extend([""] * (room - len(page)))
        rows.append(ink("─" * width, "36"))
        rows.extend("  " + choice for choice in choices)
    if len(body) > room:
        footer += "   PgUp/PgDn Scroll"
    # Use all available rows for the guide, including after terminal resizes.
    rows.extend([""] * max(0, height - len(rows) - len(detail) - 1))
    rows.extend(detail)
    rows.append("  " + ink(footer[:inner], "96"))
    return "\r\n".join(rows[:height]), offset, max(1, room)



def choose(title, context, lines, options, default, cancel):
    """Return an action key. The same choices are available in a plain terminal."""
    keys = [key for key, _, _ in options]
    if not terminal_available():
        print(f"\n{title}  |  {context}\n{WARNING}")
        print("\n".join(lines))
        for key, label, hint in options:
            print(f"  [{key}] {label} — {hint}")
        response = input(f"Choose an action (Enter: {options[keys.index(default)][1]}): ").strip().lower()
        return response if response in keys else default if not response else cancel
    selected, offset, last = keys.index(default), 0, None
    with session():
        while True:
            width, height = shutil.get_terminal_size()
            state = (width, height, selected, offset)
            if state != last:
                page, offset, step = layout(title, context, lines, options, selected, offset,
                                            width, height, "NO_COLOR" not in os.environ)
                sys.stdout.write("\x1b[H" + "\x1b[K\r\n".join(page.split("\r\n")) + "\x1b[J")
                sys.stdout.flush()
                last = (width, height, selected, offset)
            key = read_key()
            if key == "up":
                selected = (selected - 1) % len(options)
            elif key in ("down", "\t"):
                selected = (selected + 1) % len(options)
            elif key == "pageup":
                offset = max(0, offset - step)
            elif key == "pagedown":
                offset += step
            elif key == "escape":
                return cancel
            elif key == "enter":
                return keys[selected]
            elif key in keys:
                return key
