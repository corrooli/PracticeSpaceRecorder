#!/usr/bin/env bash
# PracticeSpaceRecorder installer
#
#   curl -fsSL https://raw.githubusercontent.com/corrooli/PracticeSpaceRecorder/main/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/corrooli/PracticeSpaceRecorder/main/install.sh | bash -s -- --autostart
#
# Downloads the latest release for your machine, installs it, and (with
# --autostart) makes it open every time you log in.

set -euo pipefail

REPO="corrooli/PracticeSpaceRecorder"
APP="PracticeSpaceRecorder"
AUTOSTART=0
for a in "$@"; do
  case "$a" in
    --autostart) AUTOSTART=1 ;;
    -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
    *) echo "Unknown option: $a" >&2; exit 1 ;;
  esac
done

amber() { printf '\033[38;5;215m%s\033[0m\n' "$*"; }
ok()    { printf '\033[38;5;114m  ✓\033[0m %s\n' "$*"; }
die()   { printf '\033[38;5;203m  ✗ %s\033[0m\n' "$*" >&2; exit 1; }

amber ""
amber "  ●  PracticeSpaceRecorder"
amber "     press space. play loud. press space."
echo

command -v curl >/dev/null || die "curl is needed"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "  Looking up the latest release…"
API="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest")" || die "Could not reach GitHub"
asset() { printf '%s' "$API" | grep -o '"browser_download_url": *"[^"]*"' | cut -d'"' -f4 | grep -E "$1" | head -n1; }
VERSION="$(printf '%s' "$API" | grep -o '"tag_name": *"[^"]*"' | cut -d'"' -f4)"
ok "Found $VERSION"

OS="$(uname -s)"
ARCH="$(uname -m)"

if [ "$OS" = "Darwin" ]; then
  if [ "$ARCH" = "arm64" ]; then URL="$(asset '_aarch64\.dmg$')"; else URL="$(asset '_x64\.dmg$')"; fi
  [ -n "$URL" ] || die "No macOS download in $VERSION"
  echo "  Downloading…"
  curl -fL --progress-bar "$URL" -o "$TMP/app.dmg"
  MNT="$(hdiutil attach -nobrowse -readonly "$TMP/app.dmg" | grep -o '/Volumes/.*' | head -n1)"
  [ -d "$MNT/$APP.app" ] || die "Disk image looks wrong"
  if pgrep -xq "$APP"; then osascript -e "quit app \"$APP\"" || true; fi
  rm -rf "/Applications/$APP.app"
  cp -R "$MNT/$APP.app" /Applications/
  hdiutil detach -quiet "$MNT" || true
  # The app is not notarized by Apple (that costs money every year). Since you
  # chose to download it, tell macOS it's fine so it opens without a fight.
  xattr -dr com.apple.quarantine "/Applications/$APP.app" 2>/dev/null || true
  ok "Installed to /Applications/$APP.app"
  if [ "$AUTOSTART" = 1 ]; then
    osascript -e "tell application \"System Events\" to delete (every login item whose name is \"$APP\")" >/dev/null 2>&1 || true
    osascript -e "tell application \"System Events\" to make login item at end with properties {path:\"/Applications/$APP.app\", hidden:false}" >/dev/null
    ok "Opens automatically when you log in"
  fi
  open "/Applications/$APP.app"

elif [ "$OS" = "Linux" ]; then
  case "$ARCH" in
    x86_64) DEB_ARCH="amd64"; AI_ARCH="amd64"; RPM_ARCH="x86_64" ;;
    aarch64|arm64) DEB_ARCH="arm64"; AI_ARCH="aarch64"; RPM_ARCH="aarch64" ;;
    *) die "Unsupported CPU: $ARCH" ;;
  esac
  if command -v apt-get >/dev/null; then
    URL="$(asset "_${DEB_ARCH}\.deb$")"
    [ -n "$URL" ] || die "No .deb for $ARCH in $VERSION"
    echo "  Downloading…"
    curl -fL --progress-bar "$URL" -o "$TMP/app.deb"
    echo "  Installing (asks for your password)…"
    sudo apt-get install -y "$TMP/app.deb"
    BIN="$(command -v practice-space-recorder || echo /usr/bin/practice-space-recorder)"
  elif command -v dnf >/dev/null || command -v zypper >/dev/null; then
    URL="$(asset "\.${RPM_ARCH}\.rpm$")"
    [ -n "$URL" ] || die "No .rpm for $ARCH in $VERSION"
    echo "  Downloading…"
    curl -fL --progress-bar "$URL" -o "$TMP/app.rpm"
    echo "  Installing (asks for your password)…"
    if command -v dnf >/dev/null; then sudo dnf install -y "$TMP/app.rpm"; else sudo zypper --non-interactive install --allow-unsigned-rpm "$TMP/app.rpm"; fi
    BIN="$(command -v practice-space-recorder || echo /usr/bin/practice-space-recorder)"
  else
    URL="$(asset "_${AI_ARCH}\.AppImage$")"
    [ -n "$URL" ] || die "No AppImage for $ARCH in $VERSION"
    mkdir -p "$HOME/.local/bin"
    echo "  Downloading…"
    curl -fL --progress-bar "$URL" -o "$HOME/.local/bin/$APP.AppImage"
    chmod +x "$HOME/.local/bin/$APP.AppImage"
    BIN="$HOME/.local/bin/$APP.AppImage"
  fi
  ok "Installed ($BIN)"
  # cpal talks to ALSA; your user needs to be allowed to use audio devices.
  if ! id -nG | grep -qw audio && getent group audio >/dev/null; then
    sudo usermod -aG audio "$USER" && ok "Added you to the 'audio' group (log out and back in once)"
  fi
  if [ "$AUTOSTART" = 1 ]; then
    mkdir -p "$HOME/.config/autostart"
    cat > "$HOME/.config/autostart/$APP.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=$APP
Exec=$BIN
X-GNOME-Autostart-enabled=true
EOF
    ok "Opens automatically when you log in"
  fi
  (nohup "$BIN" >/dev/null 2>&1 &) || true
else
  die "This installer knows macOS and Linux. Grab a download from https://github.com/$REPO/releases"
fi

echo
amber "  All set. Plug in your interface, pick a preset, press SPACE."
amber "  Now go be loud."
echo
