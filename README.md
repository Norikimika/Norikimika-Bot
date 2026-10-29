# Norikimika-Bot (Rust port)

Rust rewrite of `Norikimika-Bot` (was Node.js/Telegraf). One binary, one module per process —
same deployment model as the JS: run each service under its own supervisor entry.

## Modules

| arg | replaces | what it does |
| --- | --- | --- |
| `repo` | `repo.js` | GitHub/GitLab release + commit + tag watcher → Telegram |
| `monitor` | `monitor.js` | battery, daily usage, speedtest, OpenRouter new models, weekly rankings, Artificial Analysis leaderboard, connection watchdog |
| `notification` | `notification.js` | Android notification bridge + TTS |
| `bot` | `index.js` | Telegram command bot |
| `discord` | `discord.js` | Discord voice playback (chatbot dropped) |
| `bing` | `bing.js` | Bing Rewards UI automation |

## Build

```bash
cargo build --release   # from a shell where cargo is already on PATH
```

Needs Rust stable, a C toolchain, and libopus headers (`opus` + `pkg-config` on Termux,
`libopus-dev` + `pkg-config` on Debian/Ubuntu) — songbird links opus. Everything else
(fonts, welcome/goodbye banners) is already compiled into the binary.

### Termux (native Android build)

A Linux aarch64 binary will not run in Termux (`not found` — no glibc loader). Two ways:

```bash
# 1. prebuilt, no toolchain needed
curl -L -o Norikimika-Bot https://github.com/Norikimika/Norikimika-Bot/raw/build/Norikimika-Bot-termux-aarch64
chmod +x Norikimika-Bot

# 2. build on the device
cd Norikimika-Bot && sh scripts/build-termux.sh
```

The script installs `rust opus pkg-config binutils` (+ `yt-dlp`), then builds. Either way set
`STATE_DIR` in `.env` to a Termux path (e.g. `$PREFIX/tmp/Norikimika-Bot`) instead of the
Debian default.

### CI

`.github/workflows/build.yml` runs, per platform: `cargo test --release --locked`,
`cargo clippy --release --all-targets --locked -- -D warnings` and the build itself. A separate
`Penetration Test` job (Strix) scans the tree once all builds pass. On every push to `master` the
compiled binary is committed to the **`build` branch** (no artifacts):

```
https://github.com/Norikimika/Norikimika-Bot/raw/build/Norikimika-Bot-linux-x86_64
https://github.com/Norikimika/Norikimika-Bot/raw/build/Norikimika-Bot-linux-aarch64
https://github.com/Norikimika/Norikimika-Bot/raw/build/Norikimika-Bot-termux-aarch64
```

Download then `chmod +x Norikimika-Bot-*`. `linux-*` are glibc builds: ARM servers and proot
Debian/Ubuntu. `termux-aarch64` is the native Android/bionic build (NDK r30, static opus, needs
Android 7 / API 24+) — it runs directly in Termux without proot.

## Run

```bash
./Norikimika-Bot                 # everything in one process: repo (5 min), monitor, notification,
                                 # bot, discord (if DISCORD_BOT_TOKEN), bing (01:00)
./Norikimika-Bot --dry-run       # one repo pass + render both monitor PNGs, no send, no state write
./Norikimika-Bot repo            # single module, e.g. for cron: */5 * * * * Norikimika-Bot repo
./Norikimika-Bot monitor         # long-running scheduler only
./Norikimika-Bot monitor --dry-run # render both PNG layouts to STATE_DIR and exit
```

Modules whose env vars are missing just stop themselves with a log line; the rest keep running.

## Config

`.env` is read from the working directory, or from `ENV_FILE=/path/to/.env`. Real environment
variables win over the file. `.env.example` lists names only, with no values — fill in what your
modules need; a module whose env is incomplete stops itself with one log line while the rest keep
running.

### Core

| Variable | Used by | Required | Default | Notes |
|---|---|---|---|---|
| `BOT_TOKEN` | bot, monitor, notification | yes | – | Token from @BotFather. Empty stops the Telegram modules. |
| `CHAT_ID` | notification | yes | – | Target group/channel id (negative for groups). |
| `ADMIN_IDS` | bot | no | empty | Comma separated Telegram user ids allowed to run admin commands. Empty means group admins only. The bot never calls `restrictChatMember`: non-admins are refused, not muted. |
| `STATE_DIR` | all | no | `$HOME/Norikimika-Bot` | State JSON (anti-spam, Telegram offset, `bing.json`), `<module>.log`, rendered PNGs. Written atomically (tmp + rename). Default is `$HOME/Norikimika-Bot` — visible on purpose, these are logs you open; `TMPDIR/Norikimika-Bot` if `HOME` is unset. |
| `ENV_FILE` | all | no | `.env` | Where to read the env file when it is not in the working directory. |

### Telegram topics

All optional; empty means the main chat with no thread. The value is a `message_thread_id`.

| Variable | Notes |
|---|---|
| `TOPIC_ID` | The bot's main topic; used to tell chatter in that topic apart. |
| `TOPIC_ID_USAGE` | `/usage` and the daily vnstat report. |
| `TOPIC_ID_STATUS` | Device status, uptime/downtime, dashboard screenshots. |
| `TOPIC_ID_SPEED` | Speedtest results. |
| `TOPIC_ID_CONNECTION` | Connection lost/restored notifications. |
| `TOPIC_ID_UPDATE` | Commit/release notifications (repo module). |
| `TOPIC_ID_TIKTOK` | `/tt` downloads. |
| `TOPIC_ID_INTAKE` | Incoming receipts; falls back to `416182`. |
| `TOPIC_ID_PREVIEW` | Receipt preview before printing. |
| `TOPIC_ID_MUSIC`, `TOPIC_ID_MUSIC2` | Targets for `/music` and `/music2`. |
| `TOPIC_ID_V380`, `TOPIC_ID_TUYA`, `TOPIC_ID_AL_QURAN`, `TOPIC_ID_GOPAY` | Per-app notifications (notification module). |

### Media and downloads

| Variable | Used by | Default | Notes |
|---|---|---|---|
| `DOWNLOAD_DIR` | bot | `STATE_DIR/media` | Scratch space for `/tt`, audio and thumbnails. |
| `BING_WORK_DIR` | bing | `STATE_DIR` | Screenshots and `uiautomator` dumps. **Must be absolute** — a relative value is resolved against the bot's cwd, which is how the dump used to be written somewhere else and then fail to read. |
| `YTDLP_PROXY` | discord, bot | – | Passed as `--proxy` to every yt-dlp call. |

### Repositories and APIs

| Variable | Used by | Notes |
|---|---|---|
| `GITHUB_TOKEN` | repo | GitHub API token (rate limit 60 → 5000/h, required for private repos). |
| `GITLAB_TOKEN` | repo | GitLab API token for repos hosted there. |

### Receipts, prices and AI

| Variable | Used by | Required | Default | Notes |
|---|---|---|---|---|
| `NINE_ROUTER_URL` | bot, notification | for receipts and voice notes | – | Base URL of the endpoint (OpenAI-compatible). |
| `NINE_ROUTER_API_KEY` | bot, notification | same | – | Key for that endpoint. |
| `NINE_ROUTER_VISION_MODEL` | bot | for receipts | – | Vision model that reads the receipt photo. |
| `NINE_ROUTER_TTS_MODEL` | notification | no | built-in default | TTS model for voice notes. |
| `STORE_NAME`, `STORE_ADDRESS` | bot | no | empty | Header drawn on the receipt image. |
| `PRINT_CMD` | bot | no | empty | Print command, invoked as `<PRINT_CMD> "<path.png>"`. Empty sends the image only. |
| `SHEETS_ID` | bot | for `/price` | – | Google Sheet id read by `/price` (CSV export). |
| `SHEETS_GID` | bot | no | `0` | Sheet gid. |

### Discord

| Variable | Required | Notes |
|---|---|---|
| `DISCORD_BOT_TOKEN` | no | Empty skips the discord module; everything else keeps running. |
| `DISCORD_WELCOME_CHANNEL_ID`, `DISCORD_GOODBYE_CHANNEL_ID` | for banners | Target channel. Missing → the event is only logged. |
| `DISCORD_WELCOME_TITLE`, `DISCORD_WELCOME_TEXT`, `DISCORD_GOODBYE_TITLE`, `DISCORD_GOODBYE_TEXT` | for banners | Banner text; `{user}` is replaced with the member name. |

### Notification module

| Variable | Default | Notes |
|---|---|---|
| `AUTO_CLEAR_ENABLED` | `true` | Clear a notification from the shade after forwarding (`1/true/yes`). |
| `CHECK_INTERVAL` | `2` | Seconds between notification polls (minimum 1). |
| `MAX_NOTIFICATIONS` | `49` | Maximum notifications handled per cycle. |
| `DB_CLEANUP_CYCLES` | `180` | Prune the internal database every N cycles. |
| `DB_RETENTION_SECONDS` | `86400` | Age before an entry is pruned. |
| `DEDUPE_WINDOW_SECONDS` | `300` | Window in which two notifications count as the same. |
| `PKILL_THROTTLE_SECONDS` | `300` | Minimum gap between `pkill` calls. |

### Android environment notes

- `/usage` and the daily report read `vnstat --json`. `/proc/net/dev` is denied to app UIDs, so
  `vnstatd` must run as root and its database must exist; the bot says which of the two is missing.
- The bing module needs `su` (grant root once in Magisk), `uiautomator` and `am`. Claiming points
  needs `tesseract` (`pkg install tesseract`); without it the log reads `claim skipped: tesseract is
  not installed` instead of "no ready points".
- Discord music needs `yt-dlp` and `node` on the Termux PATH.

## Differences from the JS

- State and logs never go to `~/SERVER/Backup` (that path is off-limits here); `STATE_DIR`
  replaces it. Old state files can be copied in as `repo.json` / `monitor.json` — the JSON
  keys are unchanged, so the first run after a copy does not re-announce old commits.
- `@napi-rs/canvas` is replaced by a small software canvas (`src/render.rs`, `image` + `ab_glyph`)
  that draws the same layouts. The four DejaVu faces are compiled into the binary from
  `assets/fonts/` (`include_bytes!`), so no system font or font path is needed at runtime.
- `node-cron` is replaced by a 30s tick loop that fires on local-time minute boundaries.
- Receipt printing no longer needs ImageMagick: the struk text is rendered to PNG by the same
  canvas, then `PRINT_CMD` is invoked with that file.
- `discord`: music playback ported from `discord.js` — queue, `!p/!q/!s/!l/!d/!ap`, Spotify/Apple
  Music/SoundCloud/playlist resolvers, autoplay radio recommendations, track-end auto-advance via
  songbird events, requester/admin gating on skip/stop/disconnect/autoplay, 5-minute alone timer,
  welcome and goodbye banner images (800x400, circular avatar over a cover-scaled background), and the
  server-stats voice-channel auto-rename. `yt-dlp` calls retry with a different YouTube client when
  YouTube answers with a bot check. The channel chatbot was dropped on request — this module has no
  LLM/chat path, so no `NINE_ROUTER_*` keys are read here.

## Tests

```bash
cargo test
```
