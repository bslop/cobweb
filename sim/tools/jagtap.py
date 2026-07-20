#!/usr/bin/env python3
"""jagtap — split the USB capture of the real Jaguar so the human AND Claude
watch the same feed at the same time.

A V4L2 device allows one consumer. Point everything at jagtap instead: it
opens the device once (MJPEG passthrough, no transcode) and serves:

    /            live view page for the human (open in a browser)
    /stream.mjpg the raw multipart MJPEG stream (mpv/ffplay/more tabs work too)
    /frame.jpg   the latest single frame — Claude curls this and Reads it
    /status      one JSON object: fps, frame count, clients, device

plus, optionally:

    --snap FILE      atomically rewrite FILE with the latest frame every
                     --snap-secs (Claude can Read a stable path, no HTTP)
    --audio ALSADEV  also capture the box's audio into a 2-file WAV ring
                     (--audio-dir, 5s segments) — `jagemu audiocheck` reads
                     them, so the SAME analyzer serves silicon and simulator

Stdlib only; the one external tool is ffmpeg. Ctrl-C stops everything.

Typical session:
    tools/jagtap.py --device /dev/video2 &
    # human:  open http://localhost:8471
    # claude: curl -s localhost:8471/frame.jpg -o /tmp/jag.jpg  (then Read)

Self-test without hardware: --device testsrc
"""

import argparse
import json
import os
import signal
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BOUNDARY = "jagtapframe"

INDEX_HTML = """<!doctype html><html><head><title>Jaguar live</title>
<style>
 body {{ margin:0; background:#111; color:#ddd; font:13px monospace; }}
 header {{ padding:6px 10px; display:flex; gap:16px; align-items:baseline; }}
 img {{ display:block; margin:0 auto; image-rendering:pixelated;
       width:min(100vw, 160vh); }}
</style></head><body>
<header><b>jagtap</b> — {device} &middot; <span id=s>connecting…</span></header>
<img src="/stream.mjpg" alt="Jaguar live feed">
<script>
 setInterval(async () => {{
   try {{
     const r = await fetch('/status'); const j = await r.json();
     document.getElementById('s').textContent =
       j.fps.toFixed(1) + ' fps · ' + j.frames + ' frames · ' +
       j.clients + ' viewer(s)';
   }} catch (e) {{ document.getElementById('s').textContent = 'feed down'; }}
 }}, 1000);
</script></body></html>"""


class Tap:
    """Reads one MJPEG byte stream (ffmpeg stdout) and fans frames out."""

    def __init__(self):
        self.latest = None  # bytes of the newest complete JPEG
        self.frames = 0
        self.cond = threading.Condition()
        self.clients = 0
        self.t0 = time.monotonic()
        self.alive = True

    def feed(self, pipe):
        """Split the ffmpeg MJPEG stream on JPEG SOI/EOI markers."""
        buf = bytearray()
        while self.alive:
            chunk = pipe.read(65536)
            if not chunk:
                break
            buf += chunk
            while True:
                soi = buf.find(b"\xff\xd8")
                if soi < 0:
                    del buf[:-1]
                    break
                eoi = buf.find(b"\xff\xd9", soi + 2)
                if eoi < 0:
                    if soi:
                        del buf[:soi]
                    break
                frame = bytes(buf[soi : eoi + 2])
                del buf[: eoi + 2]
                with self.cond:
                    self.latest = frame
                    self.frames += 1
                    self.cond.notify_all()
        self.alive = False
        with self.cond:
            self.cond.notify_all()

    def wait_frame(self, after):
        """Block until a frame newer than `after` exists; return (n, jpeg)."""
        with self.cond:
            self.cond.wait_for(lambda: self.frames > after or not self.alive, timeout=5)
            return self.frames, self.latest

    def fps(self):
        dt = time.monotonic() - self.t0
        return self.frames / dt if dt > 0 else 0.0


TAP = Tap()
ARGS = None


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):  # quiet
        pass

    def do_GET(self):
        if self.path == "/" or self.path.startswith("/index"):
            body = INDEX_HTML.format(device=ARGS.device).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        elif self.path.startswith("/frame.jpg"):
            n, frame = TAP.wait_frame(0)
            if not frame:
                self.send_error(503, "no frame yet")
                return
            self.send_response(200)
            self.send_header("Content-Type", "image/jpeg")
            self.send_header("Content-Length", str(len(frame)))
            self.send_header("X-Frame-Number", str(n))
            self.end_headers()
            self.wfile.write(frame)
        elif self.path.startswith("/status"):
            body = json.dumps(
                {
                    "ok": TAP.alive,
                    "device": ARGS.device,
                    "frames": TAP.frames,
                    "fps": round(TAP.fps(), 2),
                    "clients": TAP.clients,
                }
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        elif self.path.startswith("/stream.mjpg"):
            self.send_response(200)
            self.send_header(
                "Content-Type", f"multipart/x-mixed-replace; boundary={BOUNDARY}"
            )
            self.end_headers()
            TAP.clients += 1
            seen = 0
            try:
                while TAP.alive:
                    seen, frame = TAP.wait_frame(seen)
                    if not frame:
                        continue
                    self.wfile.write(
                        b"--%s\r\nContent-Type: image/jpeg\r\n"
                        b"Content-Length: %d\r\n\r\n" % (BOUNDARY.encode(), len(frame))
                    )
                    self.wfile.write(frame)
                    self.wfile.write(b"\r\n")
            except (BrokenPipeError, ConnectionResetError):
                pass
            finally:
                TAP.clients -= 1
        else:
            self.send_error(404)


def ffmpeg_video_cmd(args):
    if args.device == "testsrc":
        # hardware-free self-test: a moving test pattern (realtime-paced)
        src = ["-re", "-f", "lavfi", "-i", f"testsrc=size={args.size}:rate={args.rate}"]
        codec = ["-c:v", "mjpeg", "-q:v", "4"]
    else:
        # the capture box emits MJPEG natively — pass it through untouched
        src = [
            "-f", "v4l2",
            "-input_format", "mjpeg",
            "-video_size", args.size,
            "-framerate", str(args.rate),
            "-i", args.device,
        ]
        codec = ["-c:v", "copy"]
    return (
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin"]
        + src
        + codec
        + ["-f", "mjpeg", "-"]
    )


def ffmpeg_audio_cmd(args):
    os.makedirs(args.audio_dir, exist_ok=True)
    pattern = os.path.join(args.audio_dir, "jagtap_audio_%d.wav")
    return [
        "ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin",
        "-f", "alsa", "-i", args.audio,
        "-ac", "2", "-ar", "48000", "-c:a", "pcm_s16le",
        "-f", "segment", "-segment_time", str(args.audio_secs),
        "-segment_wrap", "2", "-reset_timestamps", "1",
        pattern,
    ]


def snap_loop(args):
    """Atomically rewrite --snap with the newest frame every --snap-secs."""
    tmp = args.snap + ".tmp"
    while TAP.alive:
        time.sleep(args.snap_secs)
        with TAP.cond:
            frame = TAP.latest
        if frame:
            with open(tmp, "wb") as f:
                f.write(frame)
            os.replace(tmp, args.snap)


def main():
    global ARGS
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--device", default="/dev/video2", help="V4L2 device, or 'testsrc'")
    p.add_argument("--size", default="720x480", help="capture size (Jaguar NTSC: 720x480)")
    p.add_argument("--rate", type=int, default=30, help="capture framerate")
    p.add_argument("--port", type=int, default=8471, help="HTTP port")
    p.add_argument("--snap", help="also write the latest frame to this file")
    p.add_argument("--snap-secs", type=float, default=1.0)
    p.add_argument("--audio", help="ALSA device of the capture box (e.g. hw:2,0)")
    p.add_argument("--audio-dir", default="/tmp/jagtap", help="WAV ring directory")
    p.add_argument("--audio-secs", type=int, default=5, help="WAV segment length")
    ARGS = args = p.parse_args()

    vcmd = ffmpeg_video_cmd(args)
    try:
        vproc = subprocess.Popen(vcmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    except FileNotFoundError:
        sys.exit("jagtap: ffmpeg not found")
    aproc = None
    if args.audio:
        aproc = subprocess.Popen(ffmpeg_audio_cmd(args), stderr=subprocess.DEVNULL)

    threading.Thread(target=TAP.feed, args=(vproc.stdout,), daemon=True).start()
    if args.snap:
        threading.Thread(target=snap_loop, args=(args,), daemon=True).start()

    srv = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"jagtap: {args.device} -> http://localhost:{args.port}", flush=True)
    print(f"jagtap:   human view : http://localhost:{args.port}/", flush=True)
    print(f"jagtap:   claude view: curl -s localhost:{args.port}/frame.jpg -o f.jpg", flush=True)
    if args.snap:
        print(f"jagtap:   snapshots  : {args.snap} every {args.snap_secs}s", flush=True)
    if args.audio:
        print(f"jagtap:   audio ring : {args.audio_dir}/jagtap_audio_{{0,1}}.wav "
              f"({args.audio_secs}s segments; analyze the one not being written)",
              flush=True)

    def stop(*_):
        TAP.alive = False
        for proc in (vproc, aproc):
            if proc:
                proc.terminate()
        srv.shutdown()

    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)
    threading.Thread(target=srv.serve_forever, daemon=True).start()

    # exit with a clear message if ffmpeg dies (device busy is the common one)
    rc = vproc.wait()
    TAP.alive = False
    if rc not in (0, -15):
        err = vproc.stderr.read().decode(errors="replace").strip()
        sys.exit(f"jagtap: ffmpeg exited ({rc}): {err}\n"
                 f"jagtap: if the device is busy, close whatever else has "
                 f"{args.device} open — jagtap replaces it for everyone.")
    srv.shutdown()


if __name__ == "__main__":
    main()
