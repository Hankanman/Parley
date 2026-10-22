#!/usr/bin/env python3
"""Synthesise the promo's backing track (original, so no licensing questions).

    python3 music/make-music.py   # -> src/assets/music.mp3

120 BPM in A minor (Am F C G), so one beat is exactly 15 frames at 30 fps
and the hook's word swaps land on the kick. Needs numpy, scipy and ffmpeg.
"""

import os
import subprocess
import sys

import numpy as np
from scipy.signal import butter, lfilter

SR = 44100
BPM = 120
BEAT = 60 / BPM
LENGTH = 42.4  # seconds; matches PROMO_DURATION in src/ParleyPromo.tsx
OUTRO = 1150 / 30  # where the outro card starts
HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "..", "src", "assets", "music.mp3")

rng = np.random.default_rng(7)
n = int(LENGTH * SR)
left = np.zeros(n)
right = np.zeros(n)


def lp(x, hz, order=2):
    b, a = butter(order, hz / (SR / 2), "low")
    return lfilter(b, a, x)


def hp(x, hz, order=2):
    b, a = butter(order, hz / (SR / 2), "high")
    return lfilter(b, a, x)


def add(sig, t, gain=1.0, pan=0.0):
    i = int(t * SR)
    if i >= n:
        return
    sig = sig[: n - i]
    left[i : i + len(sig)] += sig * gain * (1 - max(0, pan))
    right[i : i + len(sig)] += sig * gain * (1 + min(0, pan))


def env(length, attack, decay):
    t = np.arange(int(length * SR)) / SR
    return np.minimum(1, t / max(attack, 1e-4)) * np.exp(-t / decay)


def saw(freq, length, detune=0.0):
    t = np.arange(int(length * SR)) / SR
    f = freq * (1 + detune)
    return 2 * ((t * f) % 1) - 1


def note(name):
    names = {"C": -9, "D": -7, "E": -5, "F": -4, "G": -2, "A": 0, "B": 2}
    return 440 * 2 ** ((names[name[0]] + 12 * (int(name[1]) - 4)) / 12)


# --- drums -----------------------------------------------------------------
def kick():
    t = np.arange(int(0.45 * SR)) / SR
    pitch = 45 + 110 * np.exp(-t / 0.035)
    body = np.sin(2 * np.pi * np.cumsum(pitch) / SR) * np.exp(-t / 0.18)
    click = rng.standard_normal(len(t)) * np.exp(-t / 0.004) * 0.3
    return np.tanh(1.6 * (body + click))


def clap():
    noise = hp(rng.standard_normal(int(0.3 * SR)), 900)
    e = env(0.3, 0.001, 0.09)
    # three quick re-triggers make it a clap rather than a snare
    for d in (0.010, 0.022):
        k = int(d * SR)
        e[k:] += env(0.3, 0.001, 0.09)[: len(e) - k] * 0.6
    return lp(noise, 6000) * e * 0.5


def hat(open_=False):
    noise = hp(rng.standard_normal(int(0.25 * SR)), 7000)
    return noise * env(0.25, 0.0005, 0.08 if open_ else 0.025) * 0.35


K, C, H, HO = kick(), clap(), hat(), hat(True)

# --- harmony -----------------------------------------------------------------
CHORDS = [("A2", ["A3", "C4", "E4"]), ("F2", ["F3", "A3", "C4"]), ("C3", ["C4", "E4", "G4"]), ("G2", ["G3", "B3", "D4"])]
BAR = 4 * BEAT


def pad(notes, length):
    out_l = np.zeros(int(length * SR))
    out_r = np.zeros(int(length * SR))
    for nm in notes:
        f = note(nm)
        out_l += saw(f, length, -0.004) + saw(f, length, 0.007)
        out_r += saw(f, length, 0.004) + saw(f, length, -0.007)
    e = np.minimum(1, np.arange(len(out_l)) / (0.25 * SR)) * np.minimum(1, (len(out_l) - np.arange(len(out_l))) / (0.2 * SR))
    return lp(out_l, 1400) * e * 0.05, lp(out_r, 1400) * e * 0.05


def bass(root, length):
    f = note(root)
    s = saw(f, length) * 0.6 + np.sin(2 * np.pi * f * np.arange(int(length * SR)) / SR)
    return lp(s, 380) * env(length, 0.003, 0.22) * 0.55


def pluck(nm, length=0.25):
    f = note(nm)
    s = saw(f, length) + saw(f, length, 0.01)
    return lp(s, 3200) * env(length, 0.002, 0.07) * 0.09


# --- arrangement -------------------------------------------------------------
beats = int(LENGTH / BEAT)
groove_start = 8 * BEAT  # the hook is kick only; the groove enters with the logo
for b in range(beats):
    t = b * BEAT
    if t >= OUTRO + BEAT:
        break
    add(K, t, 0.9)
    if t >= groove_start:
        if b % 4 in (1, 3):
            add(C, t, 0.8, pan=0.05)
        add(H, t + BEAT / 2, 0.8, pan=0.3)
        add(H, t + BEAT / 4, 0.25, pan=-0.3)
        add(H, t + 3 * BEAT / 4, 0.25, pan=-0.3)
        if b % 8 == 7:
            add(HO, t + BEAT / 2, 0.6, pan=0.3)

bars = int(LENGTH / BAR) + 1
for bar in range(bars):
    t = bar * BAR
    root, notes = CHORDS[bar % 4]
    if t >= OUTRO:
        break
    pl, pr = pad(notes, BAR)
    add(pl, t, 1.0, pan=-0.2)
    add(pr, t, 1.0, pan=0.2)
    if t >= groove_start:
        for e8 in range(8):
            add(bass(root, BEAT / 2), t + e8 * BEAT / 2, 1.0 if e8 % 2 == 0 else 0.6)
        # 16th-note arpeggio over the chord, an octave up, after the first groove bar
        if t >= groove_start + BAR:
            arp = [notes[0], notes[1], notes[2], notes[1]]
            for s in range(16):
                nm = arp[s % 4]
                up = nm[0] + str(int(nm[1]) + 1)
                add(pluck(up), t + s * BEAT / 4, 0.8 if s % 4 == 0 else 0.5, pan=0.35 if s % 2 else -0.35)

# outro: one big chord and a final kick, ringing out
pl, pr = pad(["A3", "C4", "E4", "A4"], LENGTH - OUTRO)
add(pl * 1.6, OUTRO, 1.0, pan=-0.2)
add(pr * 1.6, OUTRO, 1.0, pan=0.2)
add(bass("A2", 2.5) * 1.2, OUTRO)
add(K, OUTRO, 1.0)

# sidechain-style pump: duck everything except the kick on each beat
t = np.arange(n) / SR
phase = (t % BEAT) / BEAT
duck = 1 - 0.45 * np.exp(-phase * 9)
duck[t >= OUTRO] = 1
kick_track = np.zeros(n)
for b in range(beats):
    if b * BEAT < OUTRO + BEAT:
        i = int(b * BEAT * SR)
        seg = K[: n - i] * 0.9
        kick_track[i : i + len(seg)] += seg
left = (left - kick_track) * duck + kick_track
right = (right - kick_track) * duck + kick_track

# master: gentle fade in/out, soft clip, normalise
fade = np.minimum(1, t / 0.05) * np.minimum(1, (LENGTH - t) / 1.5)
stereo = np.stack([left, right], axis=1) * fade[:, None]
stereo = np.tanh(stereo * 1.2)
stereo /= np.abs(stereo).max() / 0.89

os.makedirs(os.path.dirname(OUT), exist_ok=True)
pcm = (stereo * 32767).astype(np.int16).tobytes()
proc = subprocess.run(
    ["ffmpeg", "-y", "-loglevel", "error", "-f", "s16le", "-ar", str(SR), "-ac", "2", "-i", "-", "-c:a", "libmp3lame", "-b:a", "192k", OUT],
    input=pcm,
)
if proc.returncode:
    sys.exit("ffmpeg failed")
print(f"Wrote {os.path.normpath(OUT)}")
