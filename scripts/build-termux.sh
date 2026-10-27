#!/data/data/com.termux/files/usr/bin/sh
# Native Termux (Android aarch64) build. Run inside Termux, NOT inside proot.
set -eu

pkg install -y rust opus pkg-config binutils

cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

# yt-dlp is a runtime dependency of the bot/discord modules, not a build one
command -v yt-dlp >/dev/null || pkg install -y yt-dlp

echo "state dir default is /root/.local/share/Norikimika-Bot (Debian);"
echo "set STATE_DIR in .env to e.g. \$PREFIX/tmp/Norikimika-Bot on Termux."

cargo build --release
ls -l target/release/Norikimika-Bot
