# LogError404Bot

Telegram + Discord bot for an Android phone running Termux: device monitoring, data
usage, speed tests, receipts, media downloads, Android notification relay, repository
update checks, Discord chatbot and queue commands.

Single static-ish Rust binary — no runtime scripts to babysit.

## Features

**Telegram**
- `/p <name>` — price list from Google Sheets
- `/speed`, `/battery`, `/usage`, `/reboot`, `/approve` — device control (admin only)
- `/music`, `/music2` — yt-dlp audio download + upload
- `/playback` — volume stream picker, playback/media-session buttons
- `/tt <url>` — TikTok/Douyin downloader (video, photo slides, audio)
- Receipt photos — vision-model extraction → printed struk PNG + printer command

**Background services**
- Battery monitoring, 80 %/20 % reports, charge/use timers, input-suspend control
- Daily usage report, twice-daily speed tests with PNG chart
- Connection watchdog (10 s ping, downtime/uptime report)
- OpenRouter new-model alerts, weekly rankings, Artificial Analysis leaderboard
- GitHub/GitLab update polling
- Android notification relay (v380, Tuya, Al-Quran, GoPay) with TTS
- Bing Rewards automation via `uiautomator`
- Discord: chatbot with per-user history, welcome/goodbye cards, stats, music queue

## Requirements

Android + [Termux](https://termux.dev), with:

```
termux-battery-status  termux-volume  termux-media-player  termux-tts-speak
vnstat  speedtest-go  yt-dlp  ffmpeg  su  uiautomator  input  tesseract
```

`su` is used for reboot, GMS cleanup, approve taps and Bing automation.

## Configuration

```bash
cp .env.example .env
```

Fill in `BOT_TOKEN`, `CHAT_ID`, the `TOPIC_ID_*` values, Nine Router
(`NINE_ROUTER_URL`, `NINE_ROUTER_API_KEY`, models), Sheets (`SHEETS_ID`,
`SHEETS_GID`), receipt (`STORE_NAME`, `STORE_ADDRESS`, `STRUK_FONT`,
`STRUK_FONTSIZE`, `PRINT_CMD`) and Discord (`DISCORD_*`) keys.

Logs and state live in `~/SERVER/Backup/` (`telegram.log`, `monitor.json`,
`repo.json`, `notification.json`, `memory.json`).

## Build

On device:

```bash
pkg install rust
cargo build --release
```

Cross-compile from a Debian/Ubuntu host for Termux (Android NDK or the Termux clang
with `--target=aarch64-linux-android24`):

```bash
rustup target add aarch64-linux-android
cargo build --release --target aarch64-linux-android
```

CI (`.github/workflows/build.yml`) builds the same target and force-pushes the
stripped binary plus `LogError404Bot.sha256` to the `build` branch, then runs the
security scan:

```bash
curl -fL -o LogError404Bot \
  https://raw.githubusercontent.com/Norikimika/LogError404Bot/build/LogError404Bot
chmod +x LogError404Bot
```

## Run

The `build` branch holds only the compiled binary, its checksum and `.env.example`, so
it is the cheapest thing to clone on the phone:

```bash
git clone --depth 1 --branch build \
  https://github.com/Norikimika/LogError404Bot.git ~/SERVER/LogError404Bot
cd ~/SERVER/LogError404Bot
sha256sum -c LogError404Bot.sha256
cp .env.example .env          # fill it in once
chmod +x LogError404Bot
nohup ./LogError404Bot >> ~/SERVER/Backup/rust-bot.out 2>&1 &
termux-wake-lock
```

Re-deploy later:

```bash
cd ~/SERVER/LogError404Bot
git fetch --depth 1 origin build && git reset --hard FETCH_HEAD
sha256sum -c LogError404Bot.sha256
pkill -f LogError404Bot && nohup ./LogError404Bot >> ~/SERVER/Backup/rust-bot.out 2>&1 &
```

Autostart with Termux:Boot — `~/.termux/boot/start-bot.sh`:

```bash
#!/data/data/com.termux/files/usr/bin/sh
termux-wake-lock
cd ~/SERVER/LogError404Bot && nohup ./LogError404Bot >> ~/SERVER/Backup/rust-bot.out 2>&1 &
```

## Tests

```bash
cargo test
```

Covers duration/byte formatting, CSV price parsing, receipt text (byte-identical to
the printed struk layout), SVG→PNG rendering, notification dedupe and dumpsys
parsing, Bing URL/bounds parsing, Discord URL/token helpers.

## Notes

- Images (receipt struk, charts, welcome/goodbye cards) are rendered as SVG → PNG
  with `resvg`; the receipt keeps the printed Indonesian labels.
- Discord voice playback is not implemented (no `songbird`/opus/DAVE). Queue and
  metadata commands work, playback replies that audio is unavailable.
- Schedules use wall-clock loops; speed tests time out after 180 s.
