#!/bin/sh
# Installs Bubbles' launcher entry and icon for your user when running a
# cargo build rather than the Flatpak, so the desktop shows its name and icon
# (on Wayland, the window's icon comes from here, matched by the app ID).
# Usage: ./install-desktop.sh [path/to/bubbles]   (default: target/release/bubbles)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
bin=$(realpath "${1:-$here/target/release/bubbles}")
if [ ! -x "$bin" ]; then
    echo "No bubbles binary at $bin. Run 'cargo build --release' first." >&2
    exit 1
fi
app=io.github.chrispouliot.Bubbles
data=${XDG_DATA_HOME:-$HOME/.local/share}

for dir in "$here"/assets/icons/hicolor/*/apps; do
    size=$(basename "$(dirname "$dir")")
    for icon in "$dir/$app".*; do
        install -Dm644 "$icon" "$data/icons/hicolor/$size/apps/$(basename "$icon")"
    done
done
mkdir -p "$data/applications"
sed "s|^Exec=.*|Exec=\"$bin\"|" "$here/$app.desktop" > "$data/applications/$app.desktop"
chmod 644 "$data/applications/$app.desktop"

# Refresh the desktop's caches where those tools exist.
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q "$data/applications" || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -f -t "$data/icons/hicolor" || true
command -v kbuildsycoca6 >/dev/null 2>&1 && kbuildsycoca6 >/dev/null 2>&1 || true

echo "Installed $data/applications/$app.desktop (runs $bin) and the Bubbles icon."
