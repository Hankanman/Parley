#!/bin/bash
# Regenerate the README / docs screenshots in docs/images/ from the demo
# data set (parley-core/examples/demo/seed.sql), never your own meetings.
#
# Each page is captured in a private, headless KWin session with its own
# D-Bus, so nothing appears on (or steals focus from) your desktop.
# Needs KDE Plasma's kwin_wayland and spectacle, plus python3 with Pillow.
#
#   scripts/showcase-screenshots.sh [dark|light]
set -euo pipefail

THEME="${1:-dark}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/docs/images"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
DATA="$WORK/share"
WAIT=10

cd "$ROOT"
cargo build -q -p parley-gpui
cargo run -q -p parley-core --example seed_demo -- "$DATA"
# Reuse the installed models so the app doesn't offer to download them.
REAL_MODELS="${XDG_DATA_HOME:-$HOME/.local/share}/io.github.hankanman.Parley/models"
[ -d "$REAL_MODELS" ] && ln -s "$REAL_MODELS" "$DATA/io.github.hankanman.Parley/models"
mkdir -p "$OUT"

# route ("" is the Record page) and output name
PAGES=(
  ":record"
  "meeting:demo-roadmap:meeting"
  "ask:ask"
  "actions:actions"
  "speakers:speakers"
  "settings:settings"
)

for page in "${PAGES[@]}"; do
  name="${page##*:}"
  route="${page%:*}"
  cat > "$WORK/inner.sh" <<INNER
#!/bin/bash
XDG_DATA_HOME="$DATA" XDG_STATE_HOME="$WORK/state" PARLEY_DEV_THEME="$THEME" PARLEY_DEV_ROUTE="$route" \
  timeout $((WAIT + 4)) "$ROOT/target/debug/parley" > "$WORK/$name.log" 2>&1 &
sleep $WAIT
spectacle -b -n -f -o "$WORK/raw-$name.png" > /dev/null 2>&1
wait
INNER
  chmod +x "$WORK/inner.sh"
  timeout $((WAIT + 25)) dbus-run-session -- kwin_wayland --virtual --no-lockscreen \
    --no-global-shortcuts --socket parley-showcase --width 1440 --height 900 \
    --exit-with-session "$WORK/inner.sh" > /dev/null 2>&1 || true
  echo "captured $name"
done

python3 - "$WORK" "$OUT" <<'PY'
import sys
from PIL import Image, ImageDraw, ImageFilter

work, out = sys.argv[1], sys.argv[2]
# The 1100x720 window sits centred on the 1440x900 output; keep its
# client area only (no window decoration).
BOX = (170, 101, 1270, 821)
for name in ["record", "meeting", "ask", "actions", "speakers", "settings"]:
    shot = Image.open(f"{work}/raw-{name}.png").convert("RGB").crop(BOX)
    shot.save(f"{out}/{name}.png", optimize=True)

# Hero: the meeting page as a rounded window on a dark-to-blue gradient.
shot = Image.open(f"{out}/meeting.png").convert("RGBA")
W, H, r = 1400, 940, 14
top, bot = (11, 14, 20), (16, 52, 104)
bg = Image.new("RGBA", (W, H))
px = bg.load()
for y in range(H):
    for x in range(W):
        t = min(1, x / W * 0.35 + y / H * 0.65)
        px[x, y] = tuple(int(top[i] + (bot[i] - top[i]) * t) for i in range(3)) + (255,)
w, h = shot.size
ox, oy = (W - w) // 2, (H - h) // 2
mask = Image.new("L", (w, h), 0)
ImageDraw.Draw(mask).rounded_rectangle((0, 0, w - 1, h - 1), r, fill=255)
shadow = Image.new("RGBA", (W, H), (0, 0, 0, 0))
ImageDraw.Draw(shadow).rounded_rectangle((ox, oy + 18, ox + w, oy + h + 18), r, fill=(0, 0, 0, 170))
bg = Image.alpha_composite(bg, shadow.filter(ImageFilter.GaussianBlur(28)))
bg.paste(shot, (ox, oy), mask)
ImageDraw.Draw(bg).rounded_rectangle((ox, oy, ox + w - 1, oy + h - 1), r, outline=(255, 255, 255, 28), width=1)
bg.convert("RGB").save(f"{out}/hero.png", optimize=True)
PY
echo "Screenshots written to $OUT"
