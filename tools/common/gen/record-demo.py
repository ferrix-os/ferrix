#!/usr/bin/env python3
"""Record Ferrix's demo video: boot the desktop, drive it, cut, label.

    record-demo.py record OUT --tree TREE [--vnc HOST:PORT] [--ssh PORT] -- XTASK...
    record-demo.py edit OUT [--keep NAME=HEAD,TAIL]... [--loop START LENGTH]

`record` starts `cargo xtask XTASK...` in TREE -- a `run-compositor ...
--vnc HOST:PORT --ssh PORT` boot -- in a process group of its own. As soon
as the served screen answers with an RFB greeting it writes the screen to
OUT/raw.mkv at a fixed frame rate, in real time, while a second connection
types and clicks through `SCENE`. Every step is stamped in OUT/marks.json
with the second of the recording it happened at, and xtask's output (the
serial console with it) goes to OUT/boot.log. The boot is stopped at the end
by killing its process group, never by a pattern.

`edit` writes OUT/demo.mp4 (H.264, 1280 wide) and OUT/demo-loop.gif and
.webp from them. A span `SCENE` marks with `cut` (the boot, rustc's run,
the page's load) loses everything but its first HEAD and last TAIL
seconds, when there is more than half a second between them -- it is cut,
never sped up -- and the next frames say on screen how much was cut. A
span that short is kept whole. Every frame carries the label
(docs/brand/screenshots/CAPTIONS.md says how the published copy was made).

Standard library only, plus ffmpeg (libx264, drawtext, libwebp) and the
guest's ssh key that xtask keeps. The VNC side speaks RFB 3.8 with raw
encoding, as QEMU serves it, plus DesktopSize (the guest changes mode while
it boots) and the rich cursor: the compositor puts the pointer on a cursor
plane, which QEMU sends apart from the screen, so it is drawn into each
frame here, where the driver put it.

The guest is only asked, over ssh, when to go on (is Chrome's window up, has
rustc finished, has the page loaded) and once told to stop Steam, which
`--everything` starts and whose first-run update and sign-in windows would
sit over the middle of the screen with a live sign-in QR code.
"""
import argparse
import json
import os
import shutil
import signal
import socket
import struct
import subprocess
import threading
import time
from pathlib import Path

KEYS = {
    "ctrl": 0xFFE3, "shift": 0xFFE1, "alt": 0xFFE9, "super": 0xFFEB,
    "Return": 0xFF0D, "BackSpace": 0xFF08, "Escape": 0xFF1B, "Tab": 0xFF09,
    "Delete": 0xFFFF, "Next": 0xFF56, "Prior": 0xFF55, "space": 0x20,
}
SHIFTED = '~!@#$%^&*()_+{}|:"<>?'
RAW = "raw.mkv"
MP4 = "demo.mp4"

URL = "https://en.wikipedia.org/wiki/Rust_(programming_language)"
# Run over ssh at every poll until the desktop is up, so Steam is stopped
# before its window opens. The terminal's home is on the home disk, which
# run-compositor keeps from one boot to the next: an earlier take's binary
# would end the rustc wait before this take's compiler has run.
SETUP = ("ps -o pid,args | grep -e '/[s]team/' -e '[u]buntu12_32' -e '[s]teamwebhelper'"
         " | while read pid rest; do kill -9 $pid; done;"
         " rm -f /home/ferrix/hello /home/ferrix/hello.rs; true")

# What the video shows, in order. `cut NAME` to `uncut` is a span the
# editor may shorten; `wait` blocks until the guest says the thing is so.
# Windows are found by class through hyprctl, so no coordinate is fixed;
# a click lands at a fraction of the window's width and height.
SCENE = [
    ("cut", "boot"),
    ("wait", "desktop", 300),
    ("uncut",),
    ("sleep", 4),
    ("click", "term", 0.5, 0.5),
    ("sleep", 1),
    ("type", "uname -sm\n", 0.08),
    ("sleep", 1.5),
    ("type", "rustc --version\n", 0.08),
    ("sleep", 2),
    # zinc's own editor takes the quoted lines (`quote>`); `cat > file`
    # would echo them through the tty, whose echo ends a line without a
    # carriage return (2026-10-18), so the code would step to the right.
    ("type", "echo 'fn main() {\n", 0.07),
    ("type", '    println!("Hello from rustc on Ferrix!");\n', 0.06),
    ("type", "}' > hello.rs\n", 0.07),
    ("sleep", 1),
    ("type", "rustc hello.rs && ./hello\n", 0.08),
    ("cut", "rustc"),
    ("wait", "hello", 180),
    ("sleep", 1),
    ("uncut",),
    ("sleep", 3),
    # The tab strip, not the page: the start page's tiles are links.
    ("click", "chromium", 0.6, 0.015),
    ("sleep", 0.8),
    ("key", ["ctrl", "l"]),
    ("sleep", 0.6),
    ("type", URL + "\n", 0.05),
    ("cut", "page"),
    ("wait", "page", 120),
    ("uncut",),
    ("sleep", 4),
    ("wheel", "chromium", 5),
    ("sleep", 3.5),
    ("wheel", "chromium", 5),
    ("sleep", 6),
]


def readn(sock, n):
    data = bytearray()
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise ConnectionError("the VNC server closed the connection")
        data += chunk
    return bytes(data)


def handshake(host, port, encodings):
    """An RFB 3.8 session in 32-bit BGRX: (socket, width, height)."""
    s = socket.create_connection((host, port), timeout=30)
    if not readn(s, 12).startswith(b"RFB "):
        raise ConnectionError("not an RFB server")
    s.sendall(b"RFB 003.008\n")
    kinds = readn(s, readn(s, 1)[0])
    if 1 not in kinds:
        raise ConnectionError(f"the server wants authentication {list(kinds)}")
    s.sendall(bytes([1]))
    if struct.unpack(">I", readn(s, 4))[0] != 0:
        raise ConnectionError("the security handshake failed")
    s.sendall(bytes([1]))  # shared: the recorder and the driver both connect
    w, h = struct.unpack(">HH", readn(s, 4))
    readn(s, 16)
    readn(s, struct.unpack(">I", readn(s, 4))[0])
    s.sendall(struct.pack(">BBBBBBBBHHHBBBBBB", 0, 0, 0, 0, 32, 24, 0, 1,
                          255, 255, 255, 16, 8, 0, 0, 0, 0))
    s.sendall(struct.pack(">BBH", 2, 0, len(encodings))
              + b"".join(struct.pack(">i", e) for e in encodings))
    return s, w, h


class Recorder:
    """Keeps a copy of the screen and writes it to ffmpeg `fps` times a second."""

    def __init__(self, host, port, out, size, fps):
        self.sock, self.w, self.h = handshake(host, port, [0, -223, -239])
        self.cw, self.ch = size
        self.canvas = bytearray(self.cw * self.ch * 4)
        self.lock = threading.Lock()
        self.pointer = None  # where the driver last put it
        self.cursor = None  # (hot x, hot y, w, h, pixels, mask)
        self.fps = fps
        self.frames = 0
        self.stopped = threading.Event()
        self.ffmpeg = subprocess.Popen(
            ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
             "-f", "rawvideo", "-pix_fmt", "bgr0", "-s", f"{self.cw}x{self.ch}",
             "-r", str(fps), "-i", "-",
             "-c:v", "libx264", "-preset", "veryfast", "-crf", "12",
             "-pix_fmt", "yuv444p", str(out)],
            stdin=subprocess.PIPE)

    def now(self):
        return self.frames / self.fps

    def offset(self):
        """Where a screen smaller than the canvas sits: in its middle."""
        return max(0, (self.cw - self.w) // 2), max(0, (self.ch - self.h) // 2)

    def paste(self, x, y, w, h, data):
        ox, oy = self.offset()
        x0, y0 = x + ox, y + oy
        if x0 >= self.cw or y0 >= self.ch:
            return
        cols = min(w, self.cw - x0)
        for row in range(min(h, self.ch - y0)):
            at = ((y0 + row) * self.cw + x0) * 4
            self.canvas[at:at + cols * 4] = data[row * w * 4:(row * w + cols) * 4]

    def rect(self):
        x, y, w, h, enc = struct.unpack(">HHHHi", readn(self.sock, 12))
        if enc == 0:
            data = readn(self.sock, w * h * 4)
            with self.lock:
                self.paste(x, y, w, h, data)
        elif enc == -223:  # the guest changed mode
            with self.lock:
                self.w, self.h = w, h
                self.canvas[:] = bytes(len(self.canvas))
        elif enc == -239:  # the pointer's shape, its hot spot at (x, y)
            pixels = readn(self.sock, w * h * 4)
            mask = readn(self.sock, (w + 7) // 8 * h)
            with self.lock:
                self.cursor = (x, y, w, h, pixels, mask) if w and h else None
        else:
            raise ConnectionError(f"an encoding not asked for: {enc}")

    def receive(self):
        s = self.sock
        s.settimeout(None)
        s.sendall(struct.pack(">BBHHHH", 3, 0, 0, 0, self.w, self.h))
        while not self.stopped.is_set():
            kind = readn(s, 1)[0]
            if kind == 2:  # bell
                continue
            if kind == 3:  # cut text
                readn(s, 3)
                readn(s, struct.unpack(">I", readn(s, 4))[0])
                continue
            if kind != 0:
                raise ConnectionError(f"an unexpected server message: {kind}")
            readn(s, 1)
            for _ in range(struct.unpack(">H", readn(s, 2))[0]):
                self.rect()
            s.sendall(struct.pack(">BBHHHH", 3, 1, 0, 0, self.w, self.h))

    def frame(self):
        with self.lock:
            out = bytearray(self.canvas)
            cursor, pointer = self.cursor, self.pointer
            ox, oy = self.offset()
        if not (cursor and pointer):
            return out
        hx, hy, w, h, pixels, mask = cursor
        stride = (w + 7) // 8
        for row in range(h):
            y = pointer[1] - hy + row + oy
            if not 0 <= y < self.ch:
                continue
            for col in range(w):
                x = pointer[0] - hx + col + ox
                if 0 <= x < self.cw and mask[row * stride + col // 8] & (0x80 >> (col % 8)):
                    at = (y * self.cw + x) * 4
                    out[at:at + 4] = pixels[(row * w + col) * 4:(row * w + col) * 4 + 4]
        return out

    def write(self):
        start = time.monotonic()
        while not self.stopped.is_set():
            self.ffmpeg.stdin.write(self.frame())
            self.frames += 1
            ahead = start + self.frames / self.fps - time.monotonic()
            if ahead > 0:
                time.sleep(ahead)

    def run(self):
        threading.Thread(target=self.receive, daemon=True).start()
        self.writer = threading.Thread(target=self.write, daemon=True)
        self.writer.start()

    def stop(self):
        self.stopped.set()
        self.writer.join()
        self.ffmpeg.stdin.close()
        self.ffmpeg.wait()


class Driver:
    """The pointer and keyboard, over a connection of their own."""

    def __init__(self, host, port, recorder):
        self.sock, _, _ = handshake(host, port, [0])
        self.recorder = recorder
        self.buttons = 0
        threading.Thread(target=self.drain, daemon=True).start()

    def drain(self):
        """Read what the server sends this connection, so it never blocks."""
        try:
            while self.sock.recv(65536):
                continue
        except OSError:
            return

    def pointer(self, x, y, buttons=None):
        if buttons is not None:
            self.buttons = buttons
        self.recorder.pointer = (x, y)
        self.sock.sendall(struct.pack(">BBHH", 5, self.buttons, x, y))
        time.sleep(0.02)

    def glide(self, x, y, steps=15):
        sx, sy = self.recorder.pointer or (960, 540)
        for k in range(1, steps + 1):
            self.pointer(sx + (x - sx) * k // steps, sy + (y - sy) * k // steps)
            time.sleep(0.02)

    def key(self, sym, down):
        self.sock.sendall(struct.pack(">BBHI", 4, 1 if down else 0, 0, sym))
        time.sleep(0.02)

    def chord(self, names):
        syms = [KEYS[n] if n in KEYS else ord(n) for n in names]
        for s in syms:
            self.key(s, True)
        for s in reversed(syms):
            self.key(s, False)

    def type(self, text, pace):
        for ch in text:
            if ch == "\n":
                self.chord(["Return"])
            elif ch.isupper() or ch in SHIFTED:
                self.chord(["shift", ch])
            else:
                self.chord([ch])
            time.sleep(pace)


class Guest:
    """What the driver asks the guest over ssh to know when to go on."""

    HYPR = "HYPRLAND_INSTANCE_SIGNATURE=ferrix XDG_RUNTIME_DIR=/run/user/1000 hyprctl -j"

    def __init__(self, port):
        key = Path.home() / ".local/share/ferrix/ssh/id_ed25519"
        self.ssh = ["ssh", "-q", "-i", str(key), "-p", str(port),
                    "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
                    "-o", "ConnectTimeout=5", "-o", "BatchMode=yes", "root@127.0.0.1"]
        self.origin = None

    def run(self, command):
        try:
            done = subprocess.run(self.ssh + [command], capture_output=True, text=True,
                                  timeout=30, stdin=subprocess.DEVNULL)
        except subprocess.TimeoutExpired:
            return ""
        return done.stdout

    def json(self, what):
        try:
            return json.loads(self.run(f"{self.HYPR} {what}"))
        except ValueError:
            return []

    def window(self, name):
        """hyprix's window whose class has `name` in it, at screen coordinates."""
        for c in self.json("clients"):
            if name in c.get("class", "").lower():
                if self.origin is None:
                    m = self.json("monitors")[0]
                    self.origin = (m["x"], m["y"])
                c["at"] = [c["at"][0] - self.origin[0], c["at"][1] - self.origin[1]]
                return c
        return None

    def ready(self, what):
        if what == "desktop":
            self.run(SETUP)
            return self.window("chromium") is not None and self.window("term") is not None
        if what == "hello":
            # rustc runs under glibc's loader, so its name is the loader's.
            return "done" in self.run("test -x /home/ferrix/hello && "
                                      "! ps -o args | grep -q '[r]ustc hello' && echo done")
        if what == "page":
            w = self.window("chromium")
            return w is not None and "Wikipedia" in w.get("title", "")
        raise SystemExit(f"no condition {what}")


def play(scene, recorder, driver, guest, marks):
    def mark(**what):
        marks.append({"at": round(recorder.now(), 2), **what})
        print(f"{recorder.now():7.1f}s  {what}", flush=True)

    for step in scene:
        op = step[0]
        if op == "cut":
            mark(cut=step[1])
        elif op == "uncut":
            mark(uncut=True)
        elif op == "sleep":
            time.sleep(step[1])
        elif op == "key":
            driver.chord(step[1])
        elif op == "type":
            driver.type(step[1], step[2])
        elif op == "ssh":
            guest.run(step[1])
            mark(ssh=step[1])
        elif op == "wait":
            began = time.monotonic()
            while not guest.ready(step[1]):
                if time.monotonic() - began > step[2]:
                    mark(timeout=step[1])
                    raise SystemExit(f"{step[1]}: not so after {step[2]} s")
                time.sleep(0.5)
            mark(seen=step[1])
        elif op in ("click", "wheel"):
            w = guest.window(step[1])
            if w is None:
                raise SystemExit(f"no {step[1]} window to {op} in")
            fx, fy = step[2:4] if op == "click" else (0.5, 0.5)
            x = w["at"][0] + int(w["size"][0] * fx)
            y = w["at"][1] + int(w["size"][1] * fy)
            driver.glide(x, y)
            presses = 1 if op == "click" else step[2]
            for _ in range(presses):
                driver.pointer(x, y, 1 if op == "click" else 16)
                driver.pointer(x, y, 0)
                time.sleep(0.1)
            mark(**{op: step[1]})


def record(args):
    out = Path(args.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    host, port = args.vnc.rsplit(":", 1)
    log = open(out / "boot.log", "w")
    boot = subprocess.Popen(["cargo", "xtask"] + args.xtask, cwd=args.tree, stdout=log,
                            stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
                            start_new_session=True)
    marks, recorder = [], None
    try:
        while recorder is None:  # the build comes first; the screen answers when QEMU runs
            if boot.poll() is not None:
                raise SystemExit(f"the boot ended ({boot.returncode}) first: {out}/boot.log")
            try:
                recorder = Recorder(host, int(port), out / RAW, (1920, 1080), args.fps)
            except OSError:
                time.sleep(0.2)
        recorder.run()
        play(SCENE, recorder, Driver(host, int(port), recorder), Guest(args.ssh), marks)
        marks.append({"at": round(recorder.now(), 2), "end": True})
    finally:
        if recorder is not None:
            recorder.stop()
        (out / "marks.json").write_text(json.dumps(marks, indent=1) + "\n")
        try:
            os.killpg(boot.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        boot.wait()


# Seconds of each cut span kept at its start and its end, unless `--keep`
# says otherwise. Under KVM the boot (firmware to Chrome's window) took
# about 7 s on 2026-10-18 and is kept whole; a slower one loses its middle.
KEEP = {"boot": (12.0, 3.0), "rustc": (3.0, 1.5), "page": (3.0, 1.0)}


def pieces(marks, keep):
    """The spans of the recording to keep: (start, end, seconds cut before it)."""
    out, at, cut, opened = [], 0.0, 0.0, None
    end = next(m["at"] for m in marks if m.get("end"))
    for m in marks:
        if "cut" in m:
            opened = m
        elif m.get("uncut") and opened is not None:
            head, tail = keep.get(opened["cut"], KEEP.get(opened["cut"], (2.0, 1.0)))
            a, b = opened["at"] + head, m["at"] - tail
            if b - a > 0.5:
                out.append((at, a, cut))
                at, cut = b, b - a
            opened = None
    out.append((at, end, cut))
    return out


def edit(args):
    out = Path(args.out)
    marks = json.loads((out / "marks.json").read_text())
    keep = {}
    for item in args.keep:
        name, ends = item.split("=")
        keep[name] = tuple(float(v) for v in ends.split(","))
    spans = pieces(marks, keep)
    font, label = args.font, args.label.replace(":", r"\:")
    box = "fontcolor=white:box=1:boxcolor=black@0.6"
    chains = []
    for i, (a, b, cut) in enumerate(spans):
        chain = f"[0:v]trim={a:.2f}:{b:.2f},setpts=PTS-STARTPTS"
        if cut:
            chain += (f",drawtext=fontfile={font}:text='{cut:.0f} s of waiting cut'"
                      f":fontsize=34:{box}:boxborderw=14:x=(w-tw)/2:y=h-th-90"
                      f":enable='lt(t,2.5)'")
        chains.append(f"{chain}[p{i}]")
    graph = ";".join(chains) + ";" + "".join(f"[p{i}]" for i in range(len(spans)))
    graph += (f"concat=n={len(spans)}:v=1:a=0[cat];[cat]scale=1280:-2:flags=lanczos,"
              f"drawtext=fontfile={font}:text='{label}':fontsize=17:{box}:boxborderw=6"
              f":x=w-tw-12:y=h-th-12[v]")
    subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", str(out / RAW),
                    "-filter_complex", graph, "-map", "[v]", "-c:v", "libx264",
                    "-preset", "slow", "-crf", str(args.crf), "-pix_fmt", "yuv420p",
                    "-movflags", "+faststart", str(out / MP4)], check=True)
    edited = [{"kept": [round(a, 2), round(b, 2)], "cut_before": round(c, 1)}
              for a, b, c in spans]
    (out / "cuts.json").write_text(json.dumps(edited, indent=1) + "\n")
    print(json.dumps(edited))

    start, length = args.loop
    loop = (f"trim={start}:{start + length},setpts=PTS-STARTPTS,fps={args.loop_fps},"
            f"scale={args.loop_width}:-2:flags=lanczos")
    subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", str(out / MP4),
                    "-vf", loop + ",split[a][b];[a]palettegen=max_colors=128:stats_mode=diff[p];"
                    "[b][p]paletteuse=dither=bayer:bayer_scale=4:diff_mode=rectangle",
                    "-loop", "0", str(out / "demo-loop.gif")], check=True)
    subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", str(out / MP4),
                    "-vf", loop, "-c:v", "libwebp_anim", "-quality", str(args.webp_quality),
                    "-compression_level", "6", "-loop", "0", str(out / "demo-loop.webp")],
                   check=True)
    for name in (MP4, "demo-loop.gif", "demo-loop.webp"):
        print(f"{name}: {(out / name).stat().st_size / 1e6:.2f} MB")


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = p.add_subparsers(dest="command", required=True)
    r = sub.add_parser("record")
    r.add_argument("out")
    r.add_argument("--tree", default=".")
    r.add_argument("--vnc", default="127.0.0.1:5991")
    r.add_argument("--ssh", type=int, default=2391)
    r.add_argument("--fps", type=int, default=20)
    r.add_argument("xtask", nargs="+")
    e = sub.add_parser("edit")
    e.add_argument("out")
    e.add_argument("--label", default="FERRIX · x86-64 · QEMU/KVM on Linux")
    e.add_argument("--font", default=str(Path(__file__).resolve().parents[3]
                                         / "assets/fonts/inter/InterVariable.ttf"))
    e.add_argument("--keep", action="append", default=[],
                   help="NAME=HEAD,TAIL: seconds of a cut span to keep at each end")
    e.add_argument("--crf", type=int, default=20)
    e.add_argument("--loop", type=float, nargs=2, default=[0, 12],
                   help="START LENGTH in seconds of the edited video")
    e.add_argument("--loop-fps", type=int, default=12)
    e.add_argument("--loop-width", type=int, default=960)
    e.add_argument("--webp-quality", type=int, default=70)
    args = p.parse_args()
    if shutil.which("ffmpeg") is None:
        raise SystemExit("ffmpeg is not on PATH")
    if args.command == "record":
        record(args)
    else:
        edit(args)


if __name__ == "__main__":
    main()
