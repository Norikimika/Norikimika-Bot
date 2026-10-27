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
export PATH=/root/.cargo/bin:$PATH
cargo build --release
```

Needs Rust stable, a C toolchain, and libopus headers (`opus` + `pkg-config` on Termux,
`libopus-dev` + `pkg-config` on Debian/Ubuntu) — songbird links opus. Everything else
(fonts, welcome/goodbye banners) is already compiled into the binary.

### Termux (native Android build)

Termux binaries must be built on the device — a Linux aarch64 binary will not run there.
From Termux (not proot):

```bash
cd Norikimika-Bot && sh scripts/build-termux.sh
```

The script installs `rust opus pkg-config binutils` (+ `yt-dlp`), then builds. Run it from
Termux, and set `STATE_DIR` in `.env` to a Termux path (e.g. `$PREFIX/tmp/Norikimika-Bot`)
instead of the Debian default.

### CI

`.github/workflows/build.yml` builds and tests on `x86_64` and `aarch64` runners. On every push
to `master` the compiled binary is committed to the **`build` branch** (no artifacts):

```
https://github.com/<owner>/<repo>/raw/build/Norikimika-Bot-linux-x86_64
https://github.com/<owner>/<repo>/raw/build/Norikimika-Bot-linux-aarch64
https://github.com/<owner>/<repo>/raw/build/Norikimika-Bot-termux-aarch64
```

Download then `chmod +x Norikimika-Bot-*`. `linux-*` are glibc builds: ARM servers and proot
Debian/Ubuntu. `termux-aarch64` is the native Android/bionic build (NDK r30, static opus, needs
Android 7 / API 24+) — it runs directly in Termux without proot.

## Run

```bash
./target/release/Norikimika-Bot repo          # one watcher pass
./target/release/Norikimika-Bot repo --dry-run   # fetch + print messages, no Telegram send, no state write
./target/release/Norikimika-Bot monitor       # long-running scheduler
./target/release/Norikimika-Bot monitor --dry-run # render both PNG layouts to STATE_DIR and exit
```

## Config

Reads `.env` from the working directory (or `ENV_FILE=/path/to/.env`); real environment
variables win over the file. Same variable names as the JS `.env.example`.

Extra variable this port adds:

- `STATE_DIR` — where state JSON, logs and rendered images live. Default
  `/root/.local/share/Norikimika-Bot`.

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
