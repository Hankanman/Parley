#!/usr/bin/env python3
"""Render the ~35 s promo video from the demo screenshots in docs/images/.

    scripts/showcase-screenshots.sh     # refresh the screenshots first, if needed
    python3 scripts/promo-video.py      # -> docs/images/promo.mp4, promo-poster.png

Every frame is drawn with Pillow and piped to ffmpeg (libx264), so it needs
python3 with Pillow and numpy, ffmpeg, and the Inter font (fc-match is used to
find it; any sans font works as a fallback). The video is silent.
"""

import math
import os
import subprocess
import sys

import numpy as np
from PIL import Image, ImageDraw, ImageFilter, ImageFont

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
IMAGES = os.path.join(ROOT, "docs", "images")
ICON = os.path.join(ROOT, "parley-gpui", "packaging", "linux", "icon.png")
W, H, FPS = 1920, 1080, 30
FADE = 0.5  # crossfade between scenes, seconds

BG_TOP, BG_BOTTOM = np.array([11, 14, 20]), np.array([16, 52, 104])
FG, MUTED, LINK = (230, 234, 242), (138, 148, 166), (111, 176, 255)


def font(name, size):
    path = subprocess.run(["fc-match", "-f", "%{file}", name], capture_output=True, text=True).stdout
    try:
        return ImageFont.truetype(path, size)
    except OSError:
        return ImageFont.load_default(size)


HEAD = lambda s: font("Inter Display:weight=bold", s)
BODY = lambda s: font("Inter:weight=regular", s)
SEMI = lambda s: font("Inter:weight=semibold", s)


def ease(t):
    t = min(1.0, max(0.0, t))
    return t * t * (3 - 2 * t)


def ease_out(t):
    t = min(1.0, max(0.0, t))
    return 1 - (1 - t) ** 3


def lerp(a, b, t):
    return a + (b - a) * t


def background():
    y, x = np.mgrid[0:H, 0:W]
    t = np.clip(x / W * 0.35 + y / H * 0.65, 0, 1)[..., None]
    rgb = BG_TOP + (BG_BOTTOM - BG_TOP) * t
    # soft glow behind the window
    d = np.sqrt(((x - W / 2) / (W * 0.55)) ** 2 + ((y - H * 1.05) / (H * 0.6)) ** 2)[..., None]
    rgb = rgb + np.clip(1 - d, 0, 1) * np.array([10, 40, 90]) * 0.6
    return Image.fromarray(np.clip(rgb, 0, 255).astype(np.uint8), "RGB").convert("RGBA")


BG = background()

# Darkens the top of the frame so a window zoomed up under a caption stays readable.
_scrim = np.zeros((H, W, 4), np.uint8)
_scrim[..., :3] = BG_TOP
_scrim[..., 3] = (np.clip((300 - np.arange(H)) / 120, 0, 1) * 235).astype(np.uint8)[:, None]
SCRIM = Image.fromarray(_scrim, "RGBA")


def text_block(lines, align="center", width=W):
    """lines: [(text, font, colour, gap_after)] -> RGBA image of the block."""
    probe = ImageDraw.Draw(Image.new("RGBA", (1, 1)))
    boxes = [probe.textbbox((0, 0), t, font=f) for t, f, _, _ in lines]
    height = sum(b[3] + g for b, (_, _, _, g) in zip(boxes, lines))
    img = Image.new("RGBA", (width, height + 10), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    y = 0
    for (t, f, c, g), b in zip(lines, boxes):
        x = (width - (b[2] - b[0])) // 2 - b[0] if align == "center" else 0
        d.text((x, y), t, font=f, fill=c)
        y += b[3] + g
    return img


def with_alpha(img, a):
    if a >= 1:
        return img
    out = img.copy()
    out.putalpha(img.getchannel("A").point(lambda v: int(v * a)))
    return out


def paste(frame, img, xy, a=1.0):
    if a <= 0:
        return
    frame.alpha_composite(with_alpha(img, a), (int(xy[0]), int(xy[1])))


class Window:
    """A screenshot as a rounded app window with a drop shadow."""

    MAX = 1.75  # largest scale used; prepared once, downscaled per frame

    def __init__(self, name):
        shot = Image.open(os.path.join(IMAGES, f"{name}.png")).convert("RGBA")
        self.w, self.h = shot.size
        big = shot.resize((int(self.w * self.MAX), int(self.h * self.MAX)), Image.LANCZOS)
        r = int(14 * self.MAX)
        mask = Image.new("L", big.size, 0)
        ImageDraw.Draw(mask).rounded_rectangle((0, 0, big.width - 1, big.height - 1), r, fill=255)
        big.putalpha(mask)
        ImageDraw.Draw(big).rounded_rectangle(
            (0, 0, big.width - 1, big.height - 1), r, outline=(255, 255, 255, 34), width=2
        )
        self.card = big
        pad = 60
        sh = Image.new("RGBA", (self.w + pad * 2, self.h + pad * 2), (0, 0, 0, 0))
        ImageDraw.Draw(sh).rounded_rectangle((pad, pad + 16, pad + self.w, pad + self.h + 16), 14, fill=(0, 0, 0, 190))
        self.shadow = sh.filter(ImageFilter.GaussianBlur(26))
        self.pad = pad

    def draw(self, frame, x, y, s, a=1.0):
        sw = self.shadow.resize((int(self.shadow.width * s), int(self.shadow.height * s)), Image.BILINEAR)
        paste(frame, sw, (x - self.pad * s, y - self.pad * s), a)
        card = self.card.resize((int(self.w * s), int(self.h * s)), Image.BILINEAR)
        paste(frame, card, (x, y), a)


class Intro:
    def __init__(self, duration):
        self.duration = duration
        self.icon = Image.open(ICON).convert("RGBA")
        self.word = text_block([("Parley", HEAD(132), FG, 0)])
        self.tag = text_block([("Meeting notes that never leave your machine", BODY(46), MUTED, 0)])

    def render(self, frame, t):
        a = ease_out(t / 0.8)
        size = int(lerp(150, 190, ease_out(t / 1.2)))
        icon = self.icon.resize((size, size), Image.LANCZOS)
        paste(frame, icon, ((W - size) / 2, 250 - (size - 190) / 2), a)
        paste(frame, self.word, (0, lerp(500, 480, ease_out((t - 0.3) / 0.8))), ease((t - 0.3) / 0.6))
        paste(frame, self.tag, (0, lerp(680, 665, ease_out((t - 0.8) / 0.8))), ease((t - 0.8) / 0.6))


class Feature:
    """A caption over a screenshot, with a slow camera move onto `focus`."""

    def __init__(self, name, headline, sub, focus, zoom, duration):
        self.duration = duration
        self.win = Window(name)
        self.caption = text_block([(headline, HEAD(64), FG, 18), (sub, BODY(32), MUTED, 0)])
        self.focus, self.z0, self.z1 = focus, 1.2, zoom

    def render(self, frame, t):
        p = ease(t / self.duration)
        s = lerp(self.z0, self.z1, p)
        fx, fy = self.focus
        # where the focus point sits at the start (window centred, 300 px down) and the end
        ax0, ay0 = (W - self.win.w * self.z0) / 2 + fx * self.z0, 300 + fy * self.z0
        ax1, ay1 = W / 2, 690
        ax, ay = lerp(ax0, ax1, p), lerp(ay0, ay1, p)
        rise = (1 - ease_out(t / 0.9)) * 70
        # never let the window's top edge climb under the caption
        y = max(ay - fy * s, 262)
        self.win.draw(frame, ax - fx * s, y + rise, s, ease(t / 0.5))
        frame.alpha_composite(SCRIM)
        paste(frame, self.caption, (0, lerp(92, 72, ease_out((t - 0.2) / 0.8))), ease((t - 0.2) / 0.6))


class Promises:
    ITEMS = ["Runs on your device", "No cloud, no account", "No telemetry", "Open source, MIT"]

    def __init__(self, duration):
        self.duration = duration
        self.title = text_block([("Private by design", HEAD(80), FG, 0)])
        self.chips = []
        for label in self.ITEMS:
            f = SEMI(38)
            probe = ImageDraw.Draw(Image.new("RGBA", (1, 1))).textbbox((0, 0), label, font=f)
            cw, ch = probe[2] - probe[0] + 120, 92
            chip = Image.new("RGBA", (cw, ch), (0, 0, 0, 0))
            d = ImageDraw.Draw(chip)
            d.rounded_rectangle((0, 0, cw - 1, ch - 1), 46, fill=(18, 23, 34, 235), outline=(46, 56, 74, 255), width=2)
            d.ellipse((30, ch / 2 - 13, 56, ch / 2 + 13), fill=(47, 191, 113))
            d.line([(37, ch / 2), (42, ch / 2 + 6), (50, ch / 2 - 6)], fill=(255, 255, 255), width=4, joint="curve")
            d.text((76 - probe[0], (ch - (probe[3] - probe[1])) / 2 - probe[1]), label, font=f, fill=FG)
            self.chips.append(chip)

    def render(self, frame, t):
        paste(frame, self.title, (0, lerp(250, 230, ease_out(t / 0.8))), ease(t / 0.6))
        # two rows of two
        for i, chip in enumerate(self.chips):
            row, col = divmod(i, 2)
            row_w = self.chips[row * 2].width + self.chips[row * 2 + 1].width + 32
            x = (W - row_w) / 2 + (0 if col == 0 else self.chips[row * 2].width + 32)
            start = 0.5 + i * 0.3
            y = 470 + row * 124 + (1 - ease_out((t - start) / 0.6)) * 30
            paste(frame, chip, (x, y), ease((t - start) / 0.45))


class Outro:
    def __init__(self, duration):
        self.duration = duration
        self.icon = Image.open(ICON).convert("RGBA").resize((150, 150), Image.LANCZOS)
        self.word = text_block([("Parley", HEAD(110), FG, 0)])
        self.sub = text_block([("Free and open source, for Linux", BODY(42), MUTED, 0)])
        self.url = text_block([("meetparley.app", SEMI(48), LINK, 0)])

    def render(self, frame, t):
        a = ease(t / 0.6)
        paste(frame, self.icon, ((W - 150) / 2, 250), a)
        paste(frame, self.word, (0, lerp(440, 425, ease_out(t / 0.8))), a)
        paste(frame, self.sub, (0, 590), ease((t - 0.4) / 0.6))
        paste(frame, self.url, (0, lerp(700, 685, ease_out((t - 0.8) / 0.8))), ease((t - 0.8) / 0.6))


SCENES = [
    Intro(3.4),
    Feature("record", "Record both sides of any call", "Mic and call audio straight from PipeWire. No bots joining your meeting.",
            (560, 110), 1.55, 4.8),
    Feature("meeting", "The meeting, already written up", "Summary, decisions and chapters the moment you stop recording.",
            (930, 560), 1.6, 5.6),
    Feature("ask", "Ask across every meeting", "Answers that cite the exact moment they came from.",
            (680, 470), 1.5, 4.6),
    Feature("actions", "Action items, with owners", "Picked out of every meeting, collected in one list.",
            (680, 230), 1.5, 4.6),
    Feature("speakers", "Knows who said what", "Name a voice once and Parley recognises it next time.",
            (680, 520), 1.5, 4.6),
    Promises(4.2),
    Outro(4.0),
]


def render_frame(n):
    """Frame n of the timeline, crossfading where scenes overlap."""
    t = n / FPS
    start = 0.0
    layers = []
    for scene in SCENES:
        if start - 1e-9 <= t < start + scene.duration:
            frame = BG.copy()
            scene.render(frame, t - start)
            layers.append((frame, t - start, scene.duration))
        start += scene.duration - FADE
    frame, _, _ = layers[0]
    if len(layers) > 1:
        incoming, local, _ = layers[1]
        frame = Image.blend(frame, incoming, ease(local / FADE))
    return frame.convert("RGB")


def main():
    total = sum(s.duration for s in SCENES) - FADE * (len(SCENES) - 1)
    frames = int(total * FPS)
    out = os.path.join(IMAGES, "promo.mp4")
    ff = subprocess.Popen(
        ["ffmpeg", "-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", f"{W}x{H}",
         "-r", str(FPS), "-i", "-", "-c:v", "libx264", "-preset", "slow", "-crf", "20", "-tune", "animation",
         "-pix_fmt", "yuv420p", "-movflags", "+faststart", out],
        stdin=subprocess.PIPE,
    )
    for n in range(frames):
        frame = render_frame(n)
        ff.stdin.write(frame.tobytes())
        if n == int((3.4 + 4.8 + 2.2 - FADE * 2) * FPS):  # meeting scene, mid-move
            frame.save(os.path.join(IMAGES, "promo-poster.png"), optimize=True)
        if n % FPS == 0:
            print(f"\r{n // FPS}/{int(total)} s", end="", file=sys.stderr, flush=True)
    ff.stdin.close()
    if ff.wait() != 0:
        sys.exit("ffmpeg failed")
    print(f"\nWrote {out} ({total:.1f} s)", file=sys.stderr)


if __name__ == "__main__":
    main()
