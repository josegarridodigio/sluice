#!/usr/bin/env python3
"""Replay a `script` typescript + timing file into an animated GIF."""
import re
import sys

from PIL import Image, ImageDraw, ImageFont

COLS, ROWS, SCALE = 80, 24, 2
FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
BG, FG, BAR, DIM = (22, 24, 30), (214, 220, 230), (40, 44, 54), (130, 138, 150)
MAX_GAP, HOLD, MIN_MS = 2.0, 3.0, 60

ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]")


class Term:
    def __init__(self):
        self.rows = [[" "] * COLS for _ in range(ROWS)]
        self.r = self.c = 0

    def scroll(self):
        self.rows.pop(0)
        self.rows.append([" "] * COLS)

    def newline(self):
        if self.r == ROWS - 1:
            self.scroll()
        else:
            self.r += 1

    def feed(self, text):
        for ch in ANSI.sub("", text):
            if ch == "\n":
                self.newline()
                self.c = 0
            elif ch == "\r":
                self.c = 0
            elif ch == "\b":
                self.c = max(0, self.c - 1)
            elif ch == "\t":
                self.c = min(COLS - 1, (self.c // 8 + 1) * 8)
            elif ch >= " ":
                if self.c >= COLS:
                    self.newline()
                    self.c = 0
                self.rows[self.r][self.c] = ch
                self.c += 1

    def lines(self):
        return ["".join(r).rstrip() for r in self.rows]



def main():
    ts_path, timing_path, out = sys.argv[1:4]
    raw = open(ts_path, "rb").read()
    nl = raw.index(b"\n") + 1  # skip header line
    body = raw[nl:]
    events, pos = [], 0
    for line in open(timing_path):
        parts = line.split()
        if len(parts) == 3 and parts[0] == "O":
            parts = parts[1:]
        elif len(parts) != 2:
            continue
        n = int(parts[1])
        events.append((float(parts[0]), body[pos:pos + n].decode("utf-8", "replace")))
        pos += n

    font = ImageFont.truetype(FONT, 13 * SCALE)
    cw = int(round(font.getlength("M")))
    ch = 13 * SCALE + 4 * SCALE
    pad, bar = 10 * SCALE, 22 * SCALE
    W, H = COLS * cw + 2 * pad, ROWS * ch + 2 * pad + bar

    def render(lines):
        im = Image.new("RGB", (W, H), BG)
        d = ImageDraw.Draw(im)
        d.rectangle([0, 0, W, bar], fill=BAR)
        for i, col in enumerate([(255, 95, 86), (255, 189, 46), (39, 201, 63)]):
            x = pad + i * 16 * SCALE
            d.ellipse([x, 6 * SCALE, x + 10 * SCALE, 16 * SCALE], fill=col)
        d.text((W // 2, bar // 2), "sluice demo", font=font, fill=DIM, anchor="mm")
        for i, t in enumerate(lines):
            d.text((pad, bar + pad + i * ch), t, font=font, fill=FG)
        return im

    term = Term()
    frames, durs, last = [], [], None
    for delay, text in events:
        delay = min(delay, MAX_GAP)
        if frames:
            durs[-1] += delay
        term.feed(text)
        lines = term.lines()
        if lines == last:
            continue
        last = lines
        frames.append(render(lines))
        durs.append(0.0)
    if not frames:
        frames.append(render([""] * ROWS))
        durs.append(0.0)
    durs[-1] += HOLD

    # Merge frames that would be shorter than the minimum duration.
    keep, kd = [], []
    for im, d in zip(frames, durs):
        if keep and kd[-1] < MIN_MS / 1000:
            keep[-1], kd[-1] = im, kd[-1] + d
        else:
            keep.append(im)
            kd.append(d)
    ms = [max(MIN_MS, int(round(d * 1000 / 10.0)) * 10) for d in kd]

    pal = [im.quantize(16, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE)
           for im in keep]
    base = keep[-1].quantize(16, dither=Image.Dither.NONE)
    pal = [im.quantize(palette=base, dither=Image.Dither.NONE) for im in keep]
    pal[0].save(out, save_all=True, append_images=pal[1:], duration=ms, loop=0,
                optimize=True, disposal=1)
    import os
    print(f"{out}: {os.path.getsize(out) / 1024:.0f} KiB, {len(pal)} frames, "
          f"{sum(ms) / 1000:.1f} s")


main()
