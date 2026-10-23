#!/usr/bin/env bash
# Builds dist/Mouse.app (settings app: window + menu bar icon) with the daemon
# dist/Mouse.app/Contents/Library/LoginItems/MouseAgent.app (no icon, started at
# login); both ad-hoc signed with fixed identifiers.
# --debug: test build with the `debug-access` feature (`mouse debug ...`,
# app.sock); never copy that one to /Applications.
set -euo pipefail

FEATURES=()
case "${1:-}" in
  "") ;;
  --debug) FEATURES=(--features debug-access) ;;
  *)
    echo "usage: bundle.sh [--debug]" >&2
    exit 1
    ;;
esac

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/dist/Mouse.app"
BUNDLE_ID="com.zubrworks.mouse"
AGENT_ID="com.zubrworks.mouse.agent"
AGENT="$APP/Contents/Library/LoginItems/MouseAgent.app"
VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
# Build number: commit count (falls back to 0 outside a git checkout).
BUILD="$(git -C "$ROOT" rev-list --count HEAD 2>/dev/null || echo 0)"

for tool in cargo python3 sips iconutil codesign; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "bundle.sh: required tool not found: $tool" >&2
    exit 1
  fi
done
if [[ -z "$VERSION" ]]; then
  echo "bundle.sh: no version = \"...\" line found in $ROOT/Cargo.toml" >&2
  exit 1
fi

cd "$ROOT"
# Keep the user name out of the binaries: paths under $HOME (project,
# ~/.cargo, ~/.rustup) are embedded in panic messages and debug info.
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=/home"
cargo build --release --locked ${FEATURES[@]+"${FEATURES[@]}"}

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$AGENT/Contents/MacOS" "$AGENT/Contents/Resources"
cp "$ROOT/target/release/mouse" "$APP/Contents/MacOS/mouse-bin"
# The same program; its file name tells it to run the daemon.
cp "$ROOT/target/release/mouse" "$AGENT/Contents/MacOS/mouse-agent"
# Mouse.app runs mouse-bin directly (no launcher script): a script that `exec`s
# the binary hides the menu bar icon on macOS 26.
# mouse-bin with no arguments opens the settings app and starts the daemon
# (MouseAgent.app) if none is running.

# Icon: simple generated mouse shape (no external tools beyond python3, sips, iconutil).
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
python3 -I - "$TMP/icon.png" <<'PY'
import sys, zlib, struct
N = 1024
rows = []
for y in range(N):
    row = bytearray([0])
    for x in range(N):
        # background rounded square
        m = 90
        inside = m <= x < N - m and m <= y < N - m
        r = 180
        cx = min(max(x, m + r), N - m - r); cy = min(max(y, m + r), N - m - r)
        if inside and (x - cx) ** 2 + (y - cy) ** 2 > r * r:
            inside = False
        px = (0, 0, 0, 0)
        if inside:
            px = (40, 44, 52, 255)
            # mouse body: ellipse
            ex, ey = (x - N / 2) / 190.0, (y - 540) / 300.0
            if ex * ex + ey * ey <= 1:
                px = (235, 237, 240, 255)
                if abs(x - N // 2) < 6 and y < 520:
                    px = (40, 44, 52, 255)  # button divider
        row += bytes(px)
    rows.append(bytes(row))
raw = b"".join(rows)
def chunk(t, d):
    c = struct.pack(">I", len(d)) + t + d
    return c + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", N, N, 8, 6, 0, 0, 0)) \
    + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")
open(sys.argv[1], "wb").write(png)
PY
SET="$TMP/Mouse.iconset"
mkdir -p "$SET"
for s in 16 32 128 256 512; do
  sips -z $s $s "$TMP/icon.png" --out "$SET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s*2)) $((s*2)) "$TMP/icon.png" --out "$SET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$SET" -o "$APP/Contents/Resources/Mouse.icns"
cp "$APP/Contents/Resources/Mouse.icns" "$AGENT/Contents/Resources/Mouse.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Mouse</string>
  <key>CFBundleDisplayName</key><string>Mouse</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>mouse-bin</string>
  <key>CFBundleIconFile</key><string>Mouse</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$BUILD</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

cat > "$AGENT/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>MouseAgent</string>
  <key>CFBundleDisplayName</key><string>MouseAgent</string>
  <key>CFBundleIdentifier</key><string>$AGENT_ID</string>
  <key>CFBundleExecutable</key><string>mouse-agent</string>
  <key>CFBundleIconFile</key><string>Mouse</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$BUILD</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <!-- One malloc magazine instead of one per CPU core: the daemon allocates
       little, and spread over a dozen magazines that little keeps several
       times its size in partly used pages. -->
  <key>LSEnvironment</key>
  <dict>
    <key>MallocMaxMagazines</key><string>1</string>
  </dict>
</dict>
</plist>
PLIST

# Ad-hoc signatures default to a cdhash requirement, which changes on every rebuild
# and makes macOS forget Input Monitoring / Accessibility grants. An explicit
# identifier-based designated requirement keeps the grants across rebuilds.
# The inner bundle is signed first, then the outer one.
codesign -s - --force --identifier "$AGENT_ID" -r="designated => identifier \"$AGENT_ID\"" "$AGENT"
codesign -s - --force --identifier "$BUNDLE_ID" -r="designated => identifier \"$BUNDLE_ID\"" "$APP"
codesign --verify --verbose "$AGENT"
codesign --verify --verbose "$APP"
codesign --verify --deep --strict --verbose "$APP"
if (( ${#FEATURES[@]} )); then
  echo "Built $APP (with $AGENT), TEST BUILD with debug access"
else
  echo "Built $APP (with $AGENT)"
fi
