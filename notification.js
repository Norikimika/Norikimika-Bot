// ====================== CONFIG ======================
require('dotenv').config();
const fetch = (...args) => import('node-fetch').then(({ default: fetch }) => fetch(...args));
// Logs, state and TTS files carry user data; keep them owner-only.
process.umask(0o077);
const fs = require('fs');
const path = require('path');
const os = require('os');
const { execSync, execFile, execFileSync } = require('child_process');
// ====== Speech synthesis via 9router (/v1/audio/speech) ======
// Announcements are rendered by the same local 9router instance that already
// serves the chatbot and receipt analysis. The previous implementation opened
// the Edge Read Aloud WebSocket directly, which Microsoft rate-limits with an
// HTTP 429 (no Retry-After) as soon as a few notifications land together, and
// which pins a handshake the vendor keeps moving. Going through 9router is one
// plain HTTP call, reuses the existing NINE_ROUTER_* credentials, and drops the
// direct `ws` dependency.

const TTS_MAX_ATTEMPTS = 3;
const TTS_RETRY_BASE_MS = 800;
const TTS_TIMEOUT_MS = 25000;
const TTS_DEFAULT_MODEL = "edge-tts/id-ID-GadisNeural";

// Accepts the base URL in any of the shapes NINE_ROUTER_URL is stored in
// (`http://host:port`, `.../v1`, or a full endpoint) and returns the speech
// endpoint. Mirrors the chat/receipt URL handling in index.js and discord.js.
function buildSpeechEndpoint(rawUrl) {
    const baseUrl = String(rawUrl || "").trim().replace(/\/+$/, "");
    if (!baseUrl) return "";
    if (baseUrl.endsWith("/audio/speech")) return baseUrl;
    return baseUrl.endsWith("/v1")
        ? `${baseUrl}/audio/speech`
        : `${baseUrl}/v1/audio/speech`;
}

// Carries the HTTP status so the retry layer can tell a transient 429/5xx from
// a permanent 400/401.
class TtsHttpError extends Error {
    constructor(statusCode, detail) {
        super(`Speech request failed (HTTP ${statusCode})${detail ? `: ${detail}` : ""}`);
        this.name = "TtsHttpError";
        this.statusCode = statusCode;
    }
}

function isRetryableTtsError(e) {
    if (e instanceof TtsHttpError) {
        return e.statusCode === 429 || e.statusCode >= 500;
    }
    // A dropped connection or 9router not yet up is worth another try; a
    // missing credential or a bad model is not.
    return /ECONNRESET|ECONNREFUSED|EPIPE|socket hang up|EAI_AGAIN|ENETUNREACH|ETIMEDOUT|fetch failed|network/i.test(
        String((e && e.message) || "")
    );
}

// Renders `text` to an mp3 at `audioPath` through the local 9router. Rejects on
// any HTTP or network failure so the caller can fall back to termux-tts.
async function ttsToFile(text, audioPath, { timeoutMs = TTS_TIMEOUT_MS } = {}) {
    const endpoint = buildSpeechEndpoint(process.env.NINE_ROUTER_URL);
    if (!endpoint) throw new Error("NINE_ROUTER_URL is missing; cannot synthesize speech");
    const apiKey = process.env.NINE_ROUTER_API_KEY;
    if (!apiKey) throw new Error("NINE_ROUTER_API_KEY is missing; cannot synthesize speech");
    const model = process.env.NINE_ROUTER_TTS_MODEL || TTS_DEFAULT_MODEL;

    const res = await fetch(endpoint, {
        method: "POST",
        headers: {
            "Content-Type": "application/json",
            Authorization: `Bearer ${apiKey}`,
        },
        body: JSON.stringify({ model, input: text }),
        signal: AbortSignal.timeout(timeoutMs),
    });

    if (!res.ok) {
        // Read the error body for the log, but never let a huge error page
        // flow into the message.
        const detail = await res.text().catch(() => "");
        throw new TtsHttpError(res.status, String(detail).slice(0, 200).trim());
    }

    const audio = Buffer.from(await res.arrayBuffer());
    if (audio.length === 0) throw new Error("Speech endpoint returned an empty body");
    fs.writeFileSync(audioPath, audio);
}

// Bounded retry for transient failures (429 rate limits, 5xx, dropped sockets).
// 9router fronts the upstream voices, so a burst of GoPay notifications can
// still hit a throttle; without this one 429 would silently degrade the voice
// to termux-tts.
async function ttsToFileWithRetry(text, audioPath, opts = {}) {
    let lastErr;
    for (let attempt = 1; attempt <= TTS_MAX_ATTEMPTS; attempt++) {
        try {
            await ttsToFile(text, audioPath, opts);
            return;
        } catch (e) {
            lastErr = e;
            if (attempt >= TTS_MAX_ATTEMPTS || !isRetryableTtsError(e)) break;
            // Exponential backoff with jitter: throttles arrive in bursts, so a
            // fixed delay would have every announcement retry in lockstep.
            const backoff = TTS_RETRY_BASE_MS * 2 ** (attempt - 1);
            const delay = Math.round(backoff * (0.5 + Math.random()));
            logger.warning(
                `TTS attempt ${attempt}/${TTS_MAX_ATTEMPTS} failed (${e.message}); retrying in ${delay}ms`
            );
            await sleep(delay);
        }
    }
    throw lastErr;
}

// ====== Paths ======
const homeDir = os.homedir();
const envPath = path.join(homeDir, "SERVER/LogError404Bot/.env");
const jsonPath = path.join(homeDir, "SERVER/Backup/notification.json");
const logDir = path.join(homeDir, "SERVER/Backup");
const logFile = path.join(logDir, "notification.log");
try { fs.chmodSync(logFile, 0o600); } catch {}


// ====== Env Setup ======
if (!fs.existsSync(envPath)) {
    console.error(`.env not found: ${envPath}`);
    process.exit(1);
}

try {
    fs.chmodSync(envPath, 0o600);
} catch (e) {}

require('dotenv').config({ path: envPath });

// ====== Configuration ======
const CONFIG = {
    telegram_bot_token: process.env.BOT_TOKEN,
    telegram_chat_id: process.env.CHAT_ID,
    monitored_packages: {
        "com.macrovideo.v380pro": {
            name: "V380 Pro",
            topic_id: parseInt(process.env.TOPIC_ID_V380 || "15208", 10),
            keywords: [],
            tts_enabled: false
        },
        "com.tuya.smart": {
            name: "Tuya",
            topic_id: parseInt(process.env.TOPIC_ID_TUYA || "15208", 10),
            keywords: [],
            tts_enabled: false
        },
        "com.andi.alquran.id": {
            name: "Al-Qur'an Indonesia",
            topic_id: parseInt(process.env.TOPIC_ID_AL_QURAN || "34084", 10),
            keywords: [],
            tts_enabled: false
        },
        "com.gojek.gopay": {
            name: "GoPay",
            topic_id: parseInt(process.env.TOPIC_ID_GOPAY || "15210", 10),
            keywords: ["Norikimika"],
            tts_enabled: true
        }
    },
    check_interval: parseInt(process.env.CHECK_INTERVAL || "2", 10),
    max_notifications: parseInt(process.env.MAX_NOTIFICATIONS || "49", 10),
    auto_clear_enabled: ['1', 'true', 'yes'].includes(String(process.env.AUTO_CLEAR_ENABLED || 'true').toLowerCase()),
    db_cleanup_cycles: parseInt(process.env.DB_CLEANUP_CYCLES || "180", 10),
    db_retention_seconds: parseInt(process.env.DB_RETENTION_SECONDS || String(24 * 3600), 10),
    pkill_throttle_seconds: parseInt(process.env.PKILL_THROTTLE_SECONDS || "300", 10),
    telegram_retry: 3,
    telegram_retry_backoff: 2
};

const REQUIRED = ["telegram_bot_token", "telegram_chat_id"];
for (const r of REQUIRED) {
    if (!CONFIG[r]) {
        console.error(`Missing required config: ${r}`);
        process.exit(1);
    }
}

// ====== Logger ======
fs.mkdirSync(logDir, { recursive: true });

// Telegram request URLs carry the bot token; node-fetch echoes them in error
// messages, which then landed in plaintext log files.
const redactSecrets = (text) =>
    String(text).replace(/bot\d+:[A-Za-z0-9_-]{20,}/g, "bot<redacted>");

const escapeHtml = (value) =>
    String(value ?? "")
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;")
        .replace(/"/g, "&quot;");

function log(level, message) {
    const ts = new Date().toLocaleString();
    const formatted = `[${ts}] [${level}] ${redactSecrets(message)}`;
    try {
        fs.appendFileSync(logFile, formatted + '\n', 'utf8');
    } catch (err) {
        console.error("Failed to write log:", err.message);
    }
}

const logger = {
    info: (msg) => log("INFO", msg),
    warning: (msg) => log("WARNING", msg),
    error: (msg) => log("ERROR", msg),
    debug: (msg) => log("DEBUG", msg),
    exception: (msg, err) => log("ERROR", `${msg} ${err ? err.stack || err.message : ""}`)
};

const sleep = (ms) => new Promise(resolve => setTimeout(resolve, ms));

// ====================== MONITOR CLASS ======================
class NotificationMonitor {
    constructor(config, jsonPath) {
        this.config = config;
        this.json_path = jsonPath;
        this.last_clear_kill = 0;
        this.last_clear_time = 0;
        this.su_available = null;
        this.validate_config();
        this.setup_storage();
    }

    validate_config() {
        logger.info("Validating configuration");
        if (this.config.telegram_bot_token.length < 10) {
            console.error("Telegram bot token looks invalid");
            process.exit(1);
        }
        if (!this.config.telegram_chat_id) {
            console.error("Telegram chat id missing");
            process.exit(1);
        }
        logger.info("Configuration OK");
    }

    setup_storage() {
        fs.mkdirSync(path.dirname(this.json_path), { recursive: true });
        if (!fs.existsSync(this.json_path)) {
            this._write_json([]);
            logger.info("JSON storage initialized (new file)");
        } else {
            try {
                const data = this._read_json();
                if (!Array.isArray(data)) throw new Error("Expected a list");
                logger.info(`JSON storage loaded (${data.length} entries)`);
            } catch (e) {
                logger.warning(`JSON file invalid (${e.message}), resetting.`);
                this._write_json([]);
            }
        }
    }

    _read_json() {
        const raw = fs.readFileSync(this.json_path, 'utf8');
        return JSON.parse(raw);
    }

    _write_json(data) {
        const dirName = path.dirname(this.json_path);
        const tmpPath = path.join(dirName, `notif_${Date.now()}_${Math.random().toString(36).substring(2, 7)}.tmp`);
        fs.writeFileSync(tmpPath, JSON.stringify(data, null, 2), 'utf8');
        fs.renameSync(tmpPath, this.json_path);
    }

    is_notification_sent(package_name, title, text, notif_time) {
        try {
            const data = this._read_json();
            return data.some(n =>
                n.package_name === package_name &&
                n.title === title &&
                n.message_text === text &&
                n.notification_time === notif_time
            );
        } catch (e) {
            logger.exception("is_notification_sent error", e);
            return false;
        }
    }

    mark_notification_sent(package_name, title, text, notif_time) {
        try {
            const data = this._read_json();
            const already_exists = data.some(n =>
                n.package_name === package_name &&
                n.title === title &&
                n.message_text === text &&
                n.notification_time === notif_time
            );
            if (!already_exists) {
                data.push({
                    package_name,
                    title,
                    message_text: text,
                    notification_time: notif_time,
                    sent_time: Math.floor(Date.now() / 1000)
                });
                this._write_json(data);
            }
        } catch (e) {
            logger.exception("mark_notification_sent error", e);
        }
    }

    cleanup_old_notifications() {
        const now = Math.floor(Date.now() / 1000);
        const cutoff = now - this.config.db_retention_seconds;
        try {
            const data = this._read_json();
            const before = data.length;
            const filtered = data.filter(n => (n.sent_time || 0) >= cutoff);
            const deleted = before - filtered.length;
            if (deleted > 0) {
                this._write_json(filtered);
                logger.info(`Cleaned up ${deleted} old notifications`);
            }
        } catch (e) {
            logger.exception("cleanup_old_notifications error", e);
        }
    }

    check_su() {
        if (this.su_available !== null) return this.su_available;
        try {
            const res = execSync('su -c id', { encoding: 'utf8', timeout: 5000 });
            this.su_available = res.toLowerCase().includes('uid');
        } catch (e) {
            this.su_available = false;
        }
        if (!this.su_available) {
            logger.warning("Root (su) not available or not permitted. Some operations may fail.");
        }
        return this.su_available;
    }

    execute_root_command(command, timeout = 10000) {
        try {
            let out;
            if (this.check_su()) {
                const escapedCmd = command.replace(/"/g, '\\"');
                out = execSync(`su -c "${escapedCmd}"`, { encoding: 'utf8', timeout });
            } else {
                out = execSync(command, { encoding: 'utf8', shell: true, timeout });
            }
            return out || "";
        } catch (e) {
            if (e.stderr) {
                logger.debug(`Cmd stderr: ${e.stderr.trim()}`);
            }
            if (e.code === 'ETIMEDOUT') {
                logger.error(`Command timed out: ${command}`);
            }
            return e.stdout || "";
        }
    }

    get_notification_count() {
        let out = this.execute_root_command("dumpsys notification 2>/dev/null | grep -c 'NotificationRecord'");
        if (out && /^\d+$/.test(out.trim())) {
            return parseInt(out.trim(), 10);
        }
        let out2 = this.execute_root_command("dumpsys notification 2>/dev/null");
        if (out2) {
            return (out2.match(/NotificationRecord/g) || []).length;
        }
        return 0;
    }

    async clear_all_notifications() {
        logger.info("Attempting to clear notifications (safe mode)");
        const before = this.get_notification_count();

        this.execute_root_command("cmd notification remove_all");
        await sleep(800);
        let after = this.get_notification_count();
        if (after < before) {
            logger.info("Cleared notifications via 'cmd notification remove_all'");
            return true;
        }

        this.execute_root_command("service call notification 1");
        await sleep(800);
        let after2 = this.get_notification_count();
        if (after2 < before) {
            logger.info("Cleared notifications via 'service call notification 1'");
            return true;
        }

        const now = Date.now() / 1000;
        if (now - this.last_clear_kill < this.config.pkill_throttle_seconds) {
            logger.warning("Skipping pkill (throttled). Will not kill SystemUI frequently.");
            return false;
        }

        logger.warning("As a last resort: killing SystemUI (pkill). This may cause UI restart.");
        this.last_clear_kill = now;
        this.execute_root_command("pkill -f com.android.systemui");
        await sleep(2000);
        let after3 = this.get_notification_count();
        if (after3 < before) {
            logger.info("Cleared notifications via pkill SystemUI");
            return true;
        }

        logger.error("Failed to clear notifications by available methods");
        return false;
    }

    async check_and_clear_notifications() {
        if (!this.config.auto_clear_enabled) {
            return this.get_notification_count();
        }
        try {
            const current_count = this.get_notification_count();
            // logger.info(`Current notification count: ${current_count}`);
            if (current_count >= this.config.max_notifications) {
                logger.warning(`Threshold reached (${current_count} >= ${this.config.max_notifications}). Auto-clearing...`);
                await this.clear_all_notifications();
                return this.get_notification_count();
            }
            return current_count;
        } catch (e) {
            logger.exception("check_and_clear_notifications error", e);
            return 0;
        }
    }

    get_new_notifications() {
        const out = this.execute_root_command("dumpsys notification --noredact 2>/dev/null");
        if (!out) return [];

        const blocks = [];
        let current = [];
        const lines = out.split(/\r?\n/);

        for (const line of lines) {
            if (line.includes("NotificationRecord(")) {
                if (current.length > 0) {
                    blocks.push(current.join("\n"));
                }
                current = [line];
            } else {
                if (current.length > 0) {
                    current.push(line);
                }
            }
        }
        if (current.length > 0) {
            blocks.push(current.join("\n"));
        }

        const results = [];
        const now_ms = Date.now();

        for (const blk of blocks) {
            const pkg_m = blk.match(/pkg=([^\s\]]+)/);
            if (!pkg_m) continue;
            const pkg = pkg_m[1];

            if (!this.config.monitored_packages[pkg]) continue;

            const title_m = blk.match(/title=([^\n\r]+)/);
            if (!title_m) continue;
            const title = this.clean_text(title_m[1].trim());

            const text_m = blk.match(/text=([^\n\r]+)/);
            const text = text_m ? this.clean_text(text_m[1].trim()) : "";

            const keywords = this.config.monitored_packages[pkg].keywords || [];
            if (keywords.length > 0) {
                const combined_text = `${title} ${text}`.toLowerCase();
                if (!keywords.some(kw => combined_text.includes(kw.toLowerCase()))) {
                    continue;
                }
            }

            const when_m = blk.match(/when=(\d+)/);
            const timestamp = when_m ? parseInt(when_m[1], 10) : now_ms;

            if (this.is_notification_sent(pkg, title, text, timestamp)) {
                continue;
            }

            const d = new Date(timestamp);
            const pad = (n) => String(n).padStart(2, '0');
            const time_str = `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;

            results.push({ pkg, title, text, timestamp, time_str });
        }
        return results;
    }

    clean_text(text) {
        text = text.replace(/&[^;]+;/g, '');
        text = text.replace(/canonical=1/g, '');
        text = text.replace(/String\s*\(([^)]+)\)/g, '$1');
        return text.trim();
    }

    async send_telegram_message(message, topic_id = null) {
        const url = `https://api.telegram.org/bot${this.config.telegram_bot_token}/sendMessage`;
        const payload = {
            chat_id: this.config.telegram_chat_id,
            text: message,
            parse_mode: "HTML"
        };
        if (topic_id) {
            payload.message_thread_id = parseInt(topic_id, 10);
        }

        let attempt = 0;
        const backoff = this.config.telegram_retry_backoff;

        while (attempt < this.config.telegram_retry) {
            attempt++;
            try {
                const response = await fetch(url, {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(payload),
                    signal: AbortSignal.timeout(10000)
                });

                if (response.status === 200) {
                    return true;
                }

                if (response.status === 429) {
                    const retryAfter = parseInt(response.headers.get("Retry-After") || "5", 10);
                    logger.warning(`Telegram rate limited. Retry after ${retryAfter}s`);
                    await sleep(retryAfter * 1000);
                } else {
                    const errText = await response.text();
                    logger.warning(`Telegram send failed (status ${response.status}): ${errText}`);
                    await sleep(Math.pow(backoff, attempt) * 1000);
                }
            } catch (e) {
                logger.warning(`Telegram request error: ${e.message}. Backing off.`);
                await sleep(Math.pow(backoff, attempt) * 1000);
            }
        }
        logger.error("Telegram send failed after retries");
        return false;
    }

    format_message(notif_data, app_name) {
        // Notification text comes from arbitrary apps; unescaped it breaks the
        // HTML parse mode or injects markup into the Telegram message.
        return (
            `<b>🔔 New ${escapeHtml(app_name)} Notification</b>\n` +
            `<blockquote>⏰ Time: ${escapeHtml(notif_data.time_str)}\n` +
            `📝 Title: ${escapeHtml(notif_data.title)}\n` +
            `💬 Text: ${escapeHtml(notif_data.text)}</blockquote>`
        );
    }

    speak_notification(title, text) {
        if (!this._tts_queue) this._tts_queue = Promise.resolve();
        const speech = `${title}. ${text}`;
        logger.info(`TTS: ${speech}`);

        // Generate the announcement audio in parallel with the silence check —
        // network runs while the player is being muted, not after. Local
        // fallback when generation fails.
        const prefetchAudio = () => {
            const audioFile = path.join(
                os.tmpdir(),
                `tts_${Date.now()}.mp3`
            );
            return ttsToFileWithRetry(speech, audioFile)
                .then(() => (fs.existsSync(audioFile) ? audioFile : null))
                .catch((e) => {
                    logger.warning(
                        `TTS prefetch failed: ${e.message}; fallback termux-tts`
                    );
                    try {
                        if (fs.existsSync(audioFile)) fs.unlinkSync(audioFile);
                    } catch (x) {}
                    return null;
                });
        };

        const playFile = (audioFile) => new Promise((resolve) => {
            execFile(
                "termux-media-player",
                ["play", audioFile],
                { timeout: 20000 },
                (playError) => {
                    try {
                        fs.unlinkSync(audioFile);
                    } catch (e) {}
                    if (!playError) return resolve(true);
                    logger.warning(`TTS play failed: ${playError.message}`);
                    resolve(false);
                }
            );
        });

        const fallbackTts = () => new Promise((resolve) => {
            // -s music is not a guess: dumpsys audio shows this device's TTS
            // opening an AudioTrack with usage=USAGE_MEDIA, which is
            // STREAM_MUSIC(3).
            execFile(
                "termux-tts-speak",
                ["-l", "id", "-s", "music", speech],
                { timeout: 20000 },
                (error) => {
                    if (error)
                        logger.warning(`TTS fallback failed: ${error.message}`);
                    resolve(!error);
                }
            );
        });

        const task = async () => {
            const report = this.stop_media_playback();
            const gen = prefetchAudio();
            // Wait for the music player to actually go idle. The pause command
            // returns immediately but the player stays started for about a
            // second, and the announcement competes with it for the same stream
            // when it does not.
            const silence = await this.wait_for_silence();
            if (!silence.checked) {
                logger.warning("Audio state unavailable; speaking without silence check");
            } else if (silence.players.length > 0) {
                logger.warning(`Music still playing after pause (${silence.players.length}); announcement may be masked`);
                silence.players.forEach((p) => logger.info(`  still playing: ${p}`));
            } else {
                logger.info("Music stream idle before speaking");
            }

            const t0 = Date.now();
            const audioFile = await gen;
            let spoke = false;
            if (audioFile) {
                spoke = await playFile(audioFile);
            }
            if (!spoke) {
                spoke = await fallbackTts();
            }
            logger.info(`TTS: ${spoke ? "ok" : "failed"} after ${Date.now() - t0}ms (volume ${report.volume ?? "unset"})`);
            // execFile play and termux-tts-speak both resolve when audio ends,
            // so the queue is already serial with no extra tail sleep.
            await sleep(500);
        };

        this._tts_queue = this._tts_queue.then(task).catch((e) => logger.error(`TTS error: ${e.message}`));
        return this._tts_queue;
    }

    // Returns a report of what was actually still playing, so the caller can log
    // evidence instead of assuming the pause took effect. `media_session
    // dispatch pause` only sends a command: the player stays active for
    // roughly a second afterwards, and the announcement starts immediately,
    // so the two overlap on the same stream.
    stop_media_playback() {
        const report = { paused: false, volume: null, error: null, stillPlaying: [] };

        // Volume first. Setting it after the pause left the announcement playing
        // for a second at whatever level the stream was already at, which is one
        // way this ends up silent while the log says the volume was raised.
        try {
            const volumes = JSON.parse(execSync("termux-volume", { stdio: ['ignore','pipe','ignore'], encoding: 'utf8', timeout: 5000 }));
            const music = volumes.find((v) => String(v.stream).toLowerCase() === "music");
            if (music && music.max_volume > 0) {
                execFileSync("termux-volume", ["music", String(music.max_volume)], { stdio: 'ignore', timeout: 5000 });
                report.volume = music.max_volume;
                logger.info(`Music volume set to max (${music.max_volume})`);
            }
        } catch (e) {
            report.error = `volume: ${e.message}`;
            logger.error(`Failed to set music volume: ${e.message}`);
        }

        try {
            execSync("cmd media_session dispatch pause", { stdio: 'ignore' });
            report.paused = true;
            logger.info("Media playback paused");
        } catch (e) {
            report.error = report.error || `pause: ${e.message}`;
            logger.error(`Failed to stop media playback: ${e.message}`);
        }

        return report;
    }

    // Polls dumpsys audio until the music stream has no started player, or the
    // deadline passes. Returns the players that were still active at the end so
    // the log records the overlap rather than hiding it.
    async wait_for_silence(timeout_ms = 4000) {
        const deadline = Date.now() + timeout_ms;
        let last = [];
        while (Date.now() < deadline) {
            let out = "";
            try {
                out = execSync("su -c 'dumpsys audio'", { stdio: ['ignore','pipe','ignore'], encoding: 'utf8', timeout: 5000 });
            } catch (e) {
                logger.warning(`Audio state unavailable: ${e.message}`);
                return { checked: false, players: [] };
            }
            last = out.split("\n").filter((l) => /state:started/.test(l) && /STREAM_MUSIC/.test(l));
            if (last.length === 0) return { checked: true, players: [] };
            await sleep(400);
        }
        return { checked: true, players: last.map((l) => l.trim().slice(0, 120)) };
    }

    async monitor() {
        logger.info("Notification Monitor Started");
        logger.info(`Auto-clear: ${this.config.auto_clear_enabled} (threshold ${this.config.max_notifications})`);

        for (const [pkg, c] of Object.entries(this.config.monitored_packages)) {
            logger.info(`${c.name} (${pkg}) - topic ${c.topic_id}`);
        }

        this.check_su();

        let cycle = 0;
        let running = true;

        process.on('SIGINT', () => {
            logger.info("Monitor stopped by user.");
            running = false;
            process.exit(0);
        });

        while (running) {
            cycle++;

            if (cycle % Math.max(1, this.config.db_cleanup_cycles) === 0) {
                this.cleanup_old_notifications();
            }

            // logger.debug(`Cycle ${cycle}: scanning for new notifications`);
            const new_notifs = this.get_new_notifications();

            if (new_notifs.length > 0) {
                logger.info(`Found ${new_notifs.length} new notification(s)`);
                for (const n of new_notifs) {
                    const pkg = n.pkg;
                    const app_name = this.config.monitored_packages[pkg].name;
                    const topic_id = this.config.monitored_packages[pkg].topic_id;

                    logger.info(`Sending: ${app_name} - ${n.title}`);
                    const message = this.format_message(n, app_name);

                    let tts_promise = null;
                    if (this.config.monitored_packages[pkg].tts_enabled) {
                        tts_promise = this.speak_notification(n.title, n.text)
                            .catch((e) => logger.error(`TTS pipeline error: ${e.message}`));
                    }

                    const successfully_sent = await this.send_telegram_message(message, topic_id);
                    if (successfully_sent) {
                        this.mark_notification_sent(pkg, n.title, n.text, n.timestamp);
                        logger.info("Sent & marked");
                    } else {
                        logger.warning("Failed to send to Telegram");
                    }

                    if (tts_promise) {
                        await tts_promise;
                    }
                }
            } else {
                // logger.debug("No new notifications");
            }

            const current_time = Date.now() / 1000;
            if (cycle % 10 === 0 || (this.config.auto_clear_enabled && (current_time - this.last_clear_time > 30))) {
                const notification_count = await this.check_and_clear_notifications();
                this.last_clear_time = current_time;
                // logger.info(`System notification count: ${notification_count}`);
            }

            await sleep(this.config.check_interval * 1000);
        }
    }
}

// ====================== MAIN EXPORT ======================
function startNotificationMonitor() {
    logger.info("Initializing Notification Monitor Service...");
    const monitorInstance = new NotificationMonitor(CONFIG, jsonPath);

    monitorInstance.monitor().catch(err => {
        logger.error(`Monitor service encountered a fatal error: ${err.message}`);
    });
}

module.exports = startNotificationMonitor;
