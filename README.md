# LogError404Bot

Telegram bot for Termux with device monitoring, Android notification monitoring, repository update checks, Discord voice features, and OpenRouter reports.

## Features

- Telegram commands for prices, device status, data usage, speed tests, playback, media downloads, approvals, and reboots.
- Battery monitoring, data usage reports, scheduled speed tests, new OpenRouter model alerts, and weekly OpenRouter rankings.
- GitHub and GitLab repository update checks.
- Android notification monitoring.
- Discord voice playback, server statistics, welcome or goodbye messages, and a channel-specific chatbot.
- Bing Rewards automation through Android UI automation.

## Requirements

- Android with Termux.
- Node.js 24 or newer. `monitor.js` requires native `fetch`, `FormData`, and `Blob`.
- Native tools required by the relevant services:
  - `termux-battery-status`
  - `vnstat`
  - `speedtest-go`
  - `termux-tts-speak`
  - `ffmpeg`
  - `yt-dlp`
  - `su`
  - Android `uiautomator` and the `input` command

This repository does not include a `package.json` file. Install the following runtime dependencies in your deployment environment:

```text
dotenv
node-fetch
telegraf
node-cron
escape-html
@napi-rs/canvas
discord.js
@discordjs/voice
```

## Configuration

1. Copy `.env.example` to a local `.env` file.
2. Fill in the required values. Never use example tokens, API keys, or credentials as production values.
3. Do not commit `.env`, log files, or state files.
4. Make sure the storage locations used by the services are writable. Some modules write logs and state to runtime paths configured in the code.

Main variables:

- `BOT_TOKEN`, `CHAT_ID`, `TOPIC_ID`: Telegram authentication and topic routing.
- `TOPIC_ID_*`: Topics for usage, status, speed tests, music, TikTok, receipts, and updates.
- `DISCORD_BOT_TOKEN`, channel IDs, titles, text, and background files: Discord configuration.
- `DISCORD_CHATBOT_CHANNEL_ID`: Discord channel that receives automatic chatbot replies.
- `NINE_ROUTER_CHATBOT_MODEL`: Provider model ID used only by the Discord chatbot.
- `SHEETS_ID`, `SHEETS_GID`: Price-list source.
- `NINE_ROUTER_URL`, `NINE_ROUTER_API_KEY`: Shared OpenAI-compatible provider URL and API key.
- `NINE_ROUTER_MODEL`: Model used for receipt analysis.
- `PRINT_CMD`: Receipt printing command.
- `GITHUB_TOKEN`, `GITLAB_TOKEN`: Repository monitor access tokens, when required.
- `STORE_NAME`, `STORE_ADDRESS`, `STRUK_FONT`, `STRUK_FONTSIZE`: Receipt configuration.

`notification.js` has deployment-specific environment and storage configuration. Review it before running the notification service.

> **Warning:** The full process starts the bot, schedules tasks, writes logs or state, calls native commands, and can manage Android notifications. `bing.js` uses `su` and UI automation. Run it only on devices and accounts you control.

## Running

```bash
node index.js
```

`index.js` is the entry point and loads the following services:

- `index.js`: Telegram bot and main commands.
- `monitor.js`: Usage, battery, speed tests, OpenRouter models, and weekly rankings.
- `repo.js`: GitHub and GitLab update checks.
- `notification.js`: Android notification monitoring.
- `discord.js`: Discord voice, statistics, and welcome or goodbye messages.
- `bing.js`: Bing Rewards automation.

## Telegram Commands

| Command | Description |
| --- | --- |
| `/p [name]` | Search the configured price list. |
| `/help` | Show the command list. |
| `/speed` | Run a speed test. |
| `/battery` | Show battery status. |
| `/usage` | Show the data usage report. |
| `/playback` | Control voice playback. |
| `/approve` | Approve a request. |
| `/tt <link>` | Download a TikTok or Douyin video. |
| `/music [title]` | Search or play music from server 1. |
| `/music2 [title]` | Search or play music from server 2. |
| `/reboot` | Reboot the device. |

Some commands are restricted by topic and administrator permissions. Validation and allowed topics come from the configuration.

## Discord Chatbot

Set `DISCORD_CHATBOT_CHANNEL_ID` to one Discord channel ID. The bot replies to every non-bot message in that channel. It uses the shared `NINE_ROUTER_URL` and `NINE_ROUTER_API_KEY`, with the separate `NINE_ROUTER_CHATBOT_MODEL`, and does not keep conversation history. The bot replies only to messages in the configured channel and ignores other channels.

## Service Schedule

- `00:00`: Daily usage report and speed test.
- `12:00`: Second speed test.
- Every minute: Battery check.
- Every 15 minutes: New OpenRouter model check.
- `01:00`: Weekly OpenRouter ranking.
- `01:00`: Bing Rewards automation.
- Every 5 minutes: Repository update check.

Times use the device or host scheduler timezone.

## Weekly OpenRouter Ranking

`monitor.js` fetches data from:

```text
https://openrouter.ai/api/v1/models?sort=top-weekly&limit=10
https://openrouter.ai/api/frontend/v1/rankings/models?view=week&metric=tokens
```

Total tokens are calculated from:

```text
total_prompt_tokens + total_completion_tokens
```

Standard models, `:free`, and `:batch` variants are mapped to their matching variant slugs. Rows are sorted by total tokens before the ten highest-ranked models are sent to Telegram.

The OpenRouter frontend rankings endpoint is not documented as a stable public API. Its schema, filters, and availability may change. The image caption states the source and formula used.

## Verification

Run syntax checks without starting the bot:

```bash
node --check index.js
node --check monitor.js
node --check repo.js
node --check notification.js
node --check discord.js
node --check bing.js
git diff --check
```

`node --check` only checks syntax. Do not run `node index.js` in an environment that forbids access to the required storage, device automation, privileged commands, or network actions.