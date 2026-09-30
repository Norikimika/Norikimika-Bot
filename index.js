// ================== BOT JS ==================
require("dotenv").config();
const fs = require("fs");
const { Telegraf } = require("telegraf");
const cron = require("node-cron");
const { spawn, exec, execFile } = require("child_process");
const escapeHTML = require("escape-html");
const https = require("https");
const dns = require("dns");
const path = require("path");
const os = require('os');
const util = require("util");
const execFilePromise = util.promisify(execFile);

const fetch = (...args) => import("node-fetch").then(({ default: fetch }) => fetch(...args));

const LOG_FILE = path.join(os.homedir(), 'SERVER', 'Backup', 'telegram.log');
const logStream = fs.createWriteStream(LOG_FILE, { flags: "a" });

function appendLogLine(line) {
  try {
    const ts = new Date().toLocaleString();
    fs.appendFileSync(LOG_FILE, `[${ts}] ${line}\n`);
  } catch (e) {
    console.error("Failed to write to log file:", e.message);
  }
}

process.on("uncaughtException", (err) => {
  if (err && /Unexpected server response: 5\d\d/.test(err.message || "")) {
    console.warn("WebSocket gateway 5xx ignored:", err.message);
    appendLogLine(`[Warning] WebSocket gateway 5xx: ${err.message}`);
    return;
  }
  console.error("Uncaught Exception:", err);
  appendLogLine(`Uncaught Exception: ${err.stack || err.message}`);
});

process.on("unhandledRejection", (reason) => {
  console.error("Unhandled Rejection:", reason);
  appendLogLine(`Unhandled Rejection: ${reason && reason.stack ? reason.stack : reason}`);
});

const bot = new Telegraf(process.env.BOT_TOKEN);

bot.telegram.setMyCommands([
  { command: "p", description: "Check product price" },
  { command: "help", description: "Show command list" },
  { command: "speed", description: "Run internet speed test" },
  { command: "battery", description: "Show battery status" },
  { command: "usage", description: "Show data usage" },
  { command: "playback", description: "Control media playback" },
  { command: "approve", description: "Approve request" },
  { command: "tt", description: "Download TikTok/Douyin video" },
  { command: "music", description: "Download music (server 1)" },
  { command: "music2", description: "Download music (server 2)" },
  { command: "reboot", description: "Reboot device" },
]).catch((e) => appendLogLine(`setMyCommands failed: ${e.message}`));

const CHAT_ID = parseInt(process.env.CHAT_ID);
const TOPIC_ID = parseInt(process.env.TOPIC_ID);
const TOPIC_ID_MUSIC = parseInt(process.env.TOPIC_ID_MUSIC);
const TOPIC_ID_MUSIC2 = parseInt(process.env.TOPIC_ID_MUSIC2);
const TOPIC_ID_TIKTOK = parseInt(process.env.TOPIC_ID_TIKTOK);

const DEFAULT_RETRIES = 2;
const DEFAULT_RETRY_DELAY = 2000;

// ================== TYPING INDICATOR HELPERS ==================
function startTyping(ctx, threadId, action = "typing") {
  const sendAction = () => {
    const opts = threadId ? { message_thread_id: threadId } : {};
    ctx.sendChatAction(action, opts).catch(() => {});
  };
  sendAction();
  return setInterval(sendAction, 4000);
}

function stopTyping(intervalId) {
  if (intervalId) clearInterval(intervalId);
}

async function safeSend(botInstance, chatId, text, options = {}, retries = DEFAULT_RETRIES) {
  for (let i = 0; i <= retries; i++) {
    try {
      await botInstance.telegram.sendMessage(chatId, text, options);
      return true;
    } catch (err) {
      appendLogLine(`safeSend error (chat ${chatId}): ${err && (err.stack || err.message)}`);
      if (i < retries) await new Promise((r) => setTimeout(r, DEFAULT_RETRY_DELAY));
    }
  }
  return false;
}

async function safeReply(ctx, text, options = {}, retries = DEFAULT_RETRIES) {
  for (let i = 0; i <= retries; i++) {
    try {
      await ctx.reply(text, options);
      return true;
    } catch (err) {
      appendLogLine(`safeReply error (chat ${ctx.chat?.id || "unknown"}): ${err && (err.stack || err.message)}`);
      if (i < retries) await new Promise((r) => setTimeout(r, DEFAULT_RETRY_DELAY));
    }
  }
  return false;
}

async function safeEditMessageText(ctx, text, options = {}, retries = DEFAULT_RETRIES) {
  for (let i = 0; i <= retries; i++) {
    try {
      if (typeof ctx.editMessageText === "function") {
        await ctx.editMessageText(text, options);
      } else {
        await ctx.telegram.editMessageText(ctx.chat.id, ctx.update?.callback_query?.message?.message_id, undefined, text, options);
      }
      return true;
    } catch (err) {
      appendLogLine(`safeEdit error (chat ${ctx.chat?.id || "unknown"}): ${err && (err.stack || err.message)}`);
      if (i < retries) await new Promise((r) => setTimeout(r, DEFAULT_RETRY_DELAY));
    }
  }
  return false;
}

async function safeAnswerCbQuery(
  ctx,
  text = "",
  options = {},
  retries = DEFAULT_RETRIES
) {
  if (!ctx?.callbackQuery?.id) return false;
  for (let i = 0; i <= retries; i++) {
    try {
      await ctx.answerCbQuery(text, options);
      return true;
    } catch (err) {
      const desc = err?.description || err?.message || "";
      if (
        desc.includes("query is too old") ||
        desc.includes("query ID is invalid")
      ) {
        return false;
      }
      appendLogLine(
        `safeAnswerCbQuery error (chat ${
          ctx.chat?.id || "unknown"
        }): ${err.stack || err.message}`
      );
      if (i < retries) {
        await new Promise((r) =>
          setTimeout(r, DEFAULT_RETRY_DELAY)
        );
      }
    }
  }
  return false;
}

async function safeReplyWithAudio(ctx, getAudio, options = {}, retries = DEFAULT_RETRIES) {
  for (let i = 0; i <= retries; i++) {
    try {
      const audio = typeof getAudio === "function" ? getAudio() : getAudio;
      return await ctx.replyWithAudio(audio, options);
    } catch (err) {
      appendLogLine(`safeReplyWithAudio error (chat ${ctx.chat?.id || "unknown"}): ${err && (err.stack || err.message)}`);
      if (i < retries) await new Promise((r) => setTimeout(r, DEFAULT_RETRY_DELAY));
    }
  }
  return false;
}

async function safeReplyWithVideo(ctx, getVideo, options = {}, retries = DEFAULT_RETRIES) {
  for (let i = 0; i <= retries; i++) {
    try {
      const video = typeof getVideo === "function" ? getVideo() : getVideo;
      return await ctx.replyWithVideo(video, options);
    } catch (err) {
      appendLogLine(`safeReplyWithVideo error (chat ${ctx.chat?.id || "unknown"}): ${err && (err.stack || err.message)}`);
      if (i < retries) await new Promise((r) => setTimeout(r, DEFAULT_RETRY_DELAY));
    }
  }
  return false;
}

async function safeReplyWithPhoto(ctx, getPhoto, options = {}, retries = DEFAULT_RETRIES) {
  for (let i = 0; i <= retries; i++) {
    try {
      const photo = typeof getPhoto === "function" ? getPhoto() : getPhoto;
      return await ctx.replyWithPhoto(photo, options);
    } catch (err) {
      appendLogLine(`safeReplyWithPhoto error (chat ${ctx.chat?.id || "unknown"}): ${err && (err.stack || err.message)}`);
      if (i < retries) await new Promise((r) => setTimeout(r, DEFAULT_RETRY_DELAY));
    }
  }
  return false;
}

async function safeReplyWithMediaGroup(ctx, getMediaGroup, options = {}, retries = DEFAULT_RETRIES) {
  for (let i = 0; i <= retries; i++) {
    try {
      const mediaGroup = typeof getMediaGroup === "function" ? getMediaGroup() : getMediaGroup;
      return await ctx.replyWithMediaGroup(mediaGroup, options);
    } catch (err) {
      appendLogLine(`safeReplyWithMediaGroup error (chat ${ctx.chat?.id || "unknown"}): ${err && (err.stack || err.message)}`);
      if (i < retries) await new Promise((r) => setTimeout(r, DEFAULT_RETRY_DELAY));
    }
  }
  return false;
}

// ================== ADMIN CHECK ==================
bot.use(async (ctx, next) => {
  const user = ctx.from?.username || ctx.from?.id || "unknown";
  let msg;
  if (ctx.callbackQuery) {
    msg = `[button] ${ctx.callbackQuery.data}`;
  } else if (ctx.message?.text) {
    msg = ctx.message.text;
  } else if (ctx.message?.caption) {
    msg = `[${ctx.message.photo ? "photo" : ctx.message.video ? "video" : "media"}+caption] ${ctx.message.caption}`;
  } else {
    const types = ["photo", "video", "sticker", "voice", "audio", "document", "animation", "video_note", "contact", "location"];
    msg = `[${types.find((t) => ctx.message?.[t]) || "non-text"}]`;
  }
  const line = `From ${user}: ${msg}`;
  logStream.write(`[${new Date().toLocaleString('id-ID')}] ${line}\n`);
  await next();
});

// ================== TOPIC ONLY MIDDLEWARE ==================
const topicOnly = (ctx, next) => {
  const threadId = ctx.message?.message_thread_id || ctx.callbackQuery?.message?.message_thread_id;
  if (threadId !== TOPIC_ID) return;
  return next();
};

const adminCache = new Map();
const CACHE_TTL = 5 * 60 * 1000;

async function isUserAdmin(ctx, chatId, userId) {
  const cacheKey = `${chatId}_${userId}`;
  const now = Date.now();

  if (adminCache.has(cacheKey) && (now - adminCache.get(cacheKey).timestamp < CACHE_TTL)) {
    return adminCache.get(cacheKey).isAdmin;
  }

  try {
    const admins = await ctx.telegram.getChatAdministrators(chatId);
    const isAdmin = admins.some((admin) => admin.user.id === userId);
    adminCache.set(cacheKey, { isAdmin, timestamp: now });
    return isAdmin;
  } catch (err) {
    return false;
  }
}

const adminOnly = async (ctx, next) => {
  try {
    const userId = ctx.from?.id;
    const chatId = ctx.chat?.id || ctx.message?.chat?.id || ctx.callbackQuery?.message?.chat?.id;
    const replyToMsgId = ctx.message?.message_id || ctx.callbackQuery?.message?.message_id;

    if (!userId || !chatId) {
      return safeReply(ctx, "❌ Permission error", { reply_to_message_id: replyToMsgId });
    }

    const isAdmin = await isUserAdmin(ctx, chatId, userId);

    if (!isAdmin) {
      const untilDate = Math.floor(Date.now() / 1000) + 5 * 60;
      try {
        await ctx.telegram.restrictChatMember(chatId, userId, {
          permissions: { can_send_messages: false },
          until_date: untilDate,
        });
      } catch (muteErr) {
        appendLogLine(`Mute failed: ${muteErr.message}`);
      }

      if (ctx.callbackQuery) {
        return safeAnswerCbQuery(ctx, "⛔ Not authorized. You are muted for 5 minutes.", { show_alert: true });
      }

      return safeReply(ctx, "⛔ Not authorized. You are muted for 5 minutes.", {
        reply_to_message_id: replyToMsgId,
      });
    }

    return next();
  } catch (err) {
    appendLogLine(`adminOnly error: ${err.message}`);
    return safeReply(ctx, "⚠️ Permission check failed.");
  }
};

// ================== SPEED ==================
bot.hears(/^\/speed/i, topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  const typing = startTyping(ctx, threadId);

  exec("speedtest-go --json", async (error, stdout) => {
    stopTyping(typing);
    if (error) {
      appendLogLine(`Speedtest failed: ${error.message}`);
      return safeReply(ctx, "❌ Failed to run speedtest", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }

    try {
      const result = JSON.parse(stdout);

      const isp = result.user_info?.Isp || "Unknown";
      const server = result.servers?.[0]?.name || "Unknown";
      const country = result.servers?.[0]?.country || "-";
      const sponsor = result.servers?.[0]?.sponsor || "Unknown";
      const latency = (result.servers?.[0]?.latency || 0) / 1e6;
      const download = (result.servers?.[0]?.dl_speed || 0) / 125000;
      const upload = (result.servers?.[0]?.ul_speed || 0) / 125000;

      const summary = `<b>⚡ Speed Test Result</b>\n<blockquote>ISP : ${escapeHTML(isp)}\nServer : ${escapeHTML(sponsor)}\nCountry : ${escapeHTML(server)} ${escapeHTML(country)}\nPing : ${latency.toFixed(2)} ms\nDownload : ${download.toFixed(2)} Mbps\nUpload : ${upload.toFixed(2)} Mbps</blockquote>`;

      await safeReply(ctx, summary.trim(), {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });

    } catch (e) {
      appendLogLine(`Speedtest JSON parse failed: ${e.message}`);
      await safeReply(ctx, "⚠️ Failed to parse speedtest result.", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }
  });
});

// ================== REBOOT ==================
bot.hears(/^\/reboot/i, topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  const typing = startTyping(ctx, threadId);

  await safeReply(ctx, "♻️ Rebooting system...", {
    reply_to_message_id: ctx.message?.message_id,
    message_thread_id: threadId,
  });

  exec(
    "su -c \"find /data/data -type f -name '*gms*' -print -exec rm {} \\;\"",
    async (error, stdout, stderr) => {
      if (stdout) appendLogLine(stdout);
      if (stderr) appendLogLine(stderr);

      if (error) {
        stopTyping(typing);
        appendLogLine(`GMS cleanup failed: ${error.message}`);

        return safeReply(ctx, "❌ Failed to reboot", {
          reply_to_message_id: ctx.message?.message_id,
          message_thread_id: threadId,
        });
      }

      exec("su -c reboot", (rebootError) => {
        stopTyping(typing);
        if (rebootError) {
          appendLogLine(`Reboot failed: ${rebootError.message}`);

          return safeReply(ctx, "❌ Failed to reboot", {
            reply_to_message_id: ctx.message?.message_id,
            message_thread_id: threadId,
          });
        }
      });
    }
  );
});

// ================== BATTERY ==================
bot.hears(/^\/battery/i, topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  const typing = startTyping(ctx, threadId);

  exec("termux-battery-status", (error, stdout) => {
    stopTyping(typing);
    if (error) {
      appendLogLine(`Battery failed: ${error.message}`);
      return safeReply(ctx, "❌ Failed to get battery info", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }
    try {
      const data = JSON.parse(stdout);
      return safeReply(
        ctx,
        `<b>🔋 Battery Status</b>\n<pre>Percentage : ${data.percentage}%\nStatus : ${data.status}\nTemperature : ${Math.round(data.temperature)}°C</pre>`,
        { parse_mode: "HTML", reply_to_message_id: ctx.message?.message_id, message_thread_id: threadId }
      );
    } catch (parseError) {
      appendLogLine(`Battery parse error: ${parseError.message}`);
      return safeReply(ctx, "❌ Failed to parse battery info", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }
  });
});

// ================== USAGE ==================
bot.hears(/^\/usage/i, topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  const typing = startTyping(ctx, threadId);

  const vnstat = spawn("vnstat", ["--json"]);
  let output = "";
  let errOutput = "";

  vnstat.stdout.on("data", (data) => (output += data.toString()));
  vnstat.stderr.on("data", (data) => (errOutput += data.toString()));

  vnstat.on("close", (code) => {
    stopTyping(typing);
    if (code !== 0 || errOutput) {
      appendLogLine(`vnstat failed: ${errOutput || `code ${code}`}`);
      return safeReply(ctx, "❌ Failed to get usage", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }

    try {
      const data = JSON.parse(output);
      // const ifaceData = data.interfaces
        // .map((i) => ({ name: i.name, total: (i.traffic?.total?.rx || 0) + (i.traffic?.total?.tx || 0) }))
        // .sort((a, b) => b.total - a.total)[0];

      // const iface = ifaceData?.name || "unknown";
      // const traffic = data.interfaces.find((i) => i.name === iface)?.traffic;

      const iface = "wlan0";
      const ifaceData = data.interfaces.find((i) => i.name === iface) || data.interfaces[0];
      const traffic = ifaceData?.traffic;

      function formatBytes(kB) {
        if (kB < 1024 * 1024) return `${kB.toFixed(2)} KB`;
        if (kB < 1024 * 1024 * 1024) return `${(kB / 1024 / 1024).toFixed(2)} MB`;
        if (kB < 1024 * 1024 * 1024 * 1024) return `${(kB / 1024 / 1024 / 1024).toFixed(2)} GB`;
        return `${(kB / 1024 / 1024 / 1024 / 1024).toFixed(2)} TB`;
      }

      const extractTraffic = (obj) => {
        if (!obj) return null;
        const up = obj.tx || 0, down = obj.rx || 0;
        return { up: formatBytes(up), down: formatBytes(down), total: formatBytes(up + down) };
      };

      const getWeekData = () => {
        if (traffic?.week?.length) {
          return extractTraffic(traffic.week.at(-1));
        }
        if (traffic?.day?.length) {
          const last7 = traffic.day.slice(-7);
          const sum = last7.reduce(
            (acc, d) => {
              acc.tx += d.tx || 0;
              acc.rx += d.rx || 0;
              return acc;
            },
            { tx: 0, rx: 0 }
          );
          return extractTraffic(sum);
        }
        return null;
      };

      const todayData = extractTraffic(traffic?.day?.at(-1));
      const weekData = getWeekData();
      const monthData = extractTraffic(traffic?.month?.at(-1));

      const usageReport = `<b>📊 Data Usage (${iface})</b>\n<blockquote>` +
        (todayData ? `📅 <b>Today</b>\nUpload : ${todayData.up}\nDownload : ${todayData.down}\nTotal : ${todayData.total}\n` : "No data\n") +
        (weekData ? `\n📆 <b>This Week</b>\nUpload : ${weekData.up}\nDownload : ${weekData.down}\nTotal : ${weekData.total}\n` : "No data\n") +
        (monthData ? `\n🗓️ <b>This Month</b>\nUpload : ${monthData.up}\nDownload : ${monthData.down}\nTotal : ${monthData.total}` : "") +
        `</blockquote>`;

      return safeReply(ctx, usageReport.trim(), {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    } catch (parseError) {
      appendLogLine(`vnstat parse failed: ${parseError.message}`);
      return safeReply(ctx, "❌ Failed to parse usage", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }
  });
});

// ================== MUSIC ==================
function formatDuration(seconds) {
  if (!seconds || isNaN(seconds)) return "??:??";

  const m = Math.floor(seconds / 60);
  const s = Math.floor(seconds % 60);

  return `${m}:${s.toString().padStart(2, "0")}`;
}

async function updateMusicStatus(ctx, messageId, { title, performer, duration, status }) {
  for (let i = 0; i <= DEFAULT_RETRIES; i++) {
    try {
      await ctx.telegram.editMessageText(
        ctx.chat.id,
        messageId,
        undefined,
        `<b>🎵 Music Info</b>\n` +
          `<blockquote>` +
          `<b>📝 Title:</b>\n-> <code>${escapeHTML(title)}</code>\n` +
          `<b>👤 Artist:</b>\n-> <code>${escapeHTML(performer)}</code>\n` +
          `<b>⏳ Duration:</b>\n-> <code>${duration}</code>\n` +
          `<b>📦 Status:</b>\n-> <code>${escapeHTML(status)}</code>` +
          `</blockquote>`,
        { parse_mode: "HTML" }
      );
      return;
    } catch (err) {
      if (err.description && err.description.includes("message is not modified")) {
        return;
      }
      appendLogLine(`updateMusicStatus error: ${err.message}`);
      if (i < DEFAULT_RETRIES) await new Promise((r) => setTimeout(r, DEFAULT_RETRY_DELAY));
    }
  }
}

// player_client web_embedded: only client that survives YouTube's bot check here.
const YT_PRIMARY_CLIENT_ARGS = ["--extractor-args", "youtube:player_client=web_embedded"];

async function handleMusicCommand(ctx, targetTopicId) {
  const threadId = ctx.message?.message_thread_id;
  const text = ctx.message.text?.trim() || "";
  const query = text.replace(/^\/music2?(@\w+)?\s*/i, "").trim();

  const replyOpts = {
    reply_to_message_id: ctx.message?.message_id,
    message_thread_id: threadId,
  };

  if (!query) {
    return safeReply(ctx, "❌ Please provide a song title or link.", replyOpts);
  }

  let typing = startTyping(ctx, threadId);

  try {
    let searchArg = query;
    if (!query.startsWith("http")) {
      if (query.toLowerCase().startsWith("sc:")) {
        const cleanQuery = query.replace(/^sc:/i, "").trim();
        searchArg = `scsearch1:${cleanQuery}`;
      } else {
        searchArg = `ytsearch1:${query}`;
      }
    }

    // player_client web_embedded: only client that survives YouTube's bot check here.
    // SABR-only sessions can still omit URLs, so accept m4a 140 before generic bestaudio.
    const ytArgs = [
      "-f", "140/bestaudio[ext=m4a]/bestaudio",
      "--no-playlist",
      "--extractor-args", YT_PRIMARY_CLIENT_ARGS,
      "-j", searchArg,
    ];
    const { stdout } = await execFilePromise("yt-dlp", ytArgs);

    const info = JSON.parse(stdout);

    const title = info.track || info.title || "Unknown Title";
    const performer = info.artist || info.uploader || info.uploader_id || "Unknown Artist";
    const duration = formatDuration(info.duration);
    const mediaUrl = info.webpage_url || (query.startsWith("http") ? query : info.url);

    const fileId = `${Date.now()}_${info.id}`;
    const tempFile = path.join(__dirname, `${fileId}.webm`);
    const thumbBase = path.join(__dirname, fileId);
    const thumbFile = `${thumbBase}.jpg`;

    let infoMessage;
    try {
      infoMessage = await ctx.reply(
        `<b>🎵 Music Info</b>\n<blockquote><b>📝 Title:</b>\n-> <code>${escapeHTML(title)}</code>\n<b>👤 Artist:</b>\n-> <code>${escapeHTML(performer)}</code>\n<b>⏳ Duration:</b>\n-> <code>${duration}</code>\n<b>📦 Status:</b>\n-> <code>Downloading</code></blockquote>`,
        { parse_mode: "HTML", ...replyOpts }
      );
    } catch (err) { 
      if (typing) stopTyping(typing);
      return; 
    }

    try {
      await execFilePromise("yt-dlp", [
        "-f", "140/bestaudio[ext=m4a]/bestaudio",
        "--no-playlist",
        "--extractor-args", YT_PRIMARY_CLIENT_ARGS,
        "-o", tempFile,
        "--write-thumbnail",
        "--convert-thumbnails", "jpg",
        "-o", `thumbnail:${thumbBase}`,
        mediaUrl
      ]);

      if (fs.existsSync(tempFile)) {
        const stats = fs.statSync(tempFile);
        const maxSizeBytes = 49 * 1024 * 1024;
        if (stats.size > maxSizeBytes) {
          await updateMusicStatus(ctx, infoMessage.message_id, { 
            title, 
            performer, 
            duration, 
            status: "Too Large" 
          });
          return;
        }
      } else {
        throw new Error("Downloaded audio file not found.");
      }

      await updateMusicStatus(ctx, infoMessage.message_id, { title, performer, duration, status: "Sending" });

      if (typing) {
        stopTyping(typing);
        typing = null;
      }
      try {
        await ctx.telegram.sendChatAction(ctx.chat.id, "upload_audio", {
          message_thread_id: targetTopicId || threadId,
        });
      } catch {}

      const audioOptions = {
        title,
        performer,
        duration: info.duration,
        message_thread_id: targetTopicId || threadId,
      };

      if (fs.existsSync(thumbFile)) {
        audioOptions.thumb = { source: fs.createReadStream(thumbFile) };
      }

      await safeReplyWithAudio(ctx, () => ({ source: fs.createReadStream(tempFile) }), audioOptions);
      await updateMusicStatus(ctx, infoMessage.message_id, { title, performer, duration, status: "Success" });

    } catch (e) {
      appendLogLine(`[SEND AUDIO ERROR] ${e.stack || e.message}`);
      await updateMusicStatus(ctx, infoMessage.message_id, { title, performer, duration, status: "Failed" });
    } finally {
      try { if (fs.existsSync(tempFile)) fs.unlinkSync(tempFile); } catch {}
      try { if (fs.existsSync(thumbFile)) fs.unlinkSync(thumbFile); } catch {}
      try { if (fs.existsSync(thumbBase + ".webp")) fs.unlinkSync(thumbBase + ".webp"); } catch {}
      try { if (fs.existsSync(thumbBase + ".png")) fs.unlinkSync(thumbBase + ".png"); } catch {}
    }
  } catch (e) {
    appendLogLine(`[FETCH ERROR] ${e.stack || e.message}`);
    return safeReply(ctx, "❌ Failed to fetch music.", replyOpts);
  } finally {
    if (typing) stopTyping(typing);
  }
}

bot.command("music", topicOnly, adminOnly, (ctx) => handleMusicCommand(ctx, TOPIC_ID_MUSIC));
bot.command("music2", topicOnly, adminOnly, (ctx) => handleMusicCommand(ctx, TOPIC_ID_MUSIC2));

// ================== HELPERS ==================
function buildVolumeText(target) {
  return `<b>🔊 Playback Control</b>\n<blockquote><b>Stream</b>\n-> ${target.stream}\n<b>Current</b>\n-> ${target.volume}\n<b>Maximum</b>\n-> ${target.max_volume}</blockquote>`;
}

function buildVolumeKeyboard(stream, target) {
  return [
    [
      { text: "➖5", callback_data: `volume_set_${stream}_${Math.max(0, target.volume - 5)}` },
      { text: "➖1", callback_data: `volume_set_${stream}_${Math.max(0, target.volume - 1)}` },
      { text: "➕1", callback_data: `volume_set_${stream}_${Math.min(target.max_volume, target.volume + 1)}` },
      { text: "➕5", callback_data: `volume_set_${stream}_${Math.min(target.max_volume, target.volume + 5)}` },
    ],
    [
      { text: "🔇 Mute", callback_data: `volume_set_${stream}_0` },
      { text: "🔊 Max",  callback_data: `volume_set_${stream}_${target.max_volume}` },
    ],
    [
      { text: "⏮️", callback_data: "media_previous" },
      { text: "▶️",  callback_data: "media_play" },
      { text: "⏹️", callback_data: "media_stop" },
      { text: "⏭️", callback_data: "media_next" },
    ],
    [{ text: "⬅️ Back", callback_data: "volume_home" }],
  ];
}

// ================== PLAYBACK ==================
bot.hears(/^\/playback(?:@\w+)?$/i, topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  const typing = startTyping(ctx, threadId);

  exec("termux-volume", async (error, stdout) => {
    stopTyping(typing);
    if (error) {
      return safeReply(ctx, "❌ Failed to get volume information", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }

    try {
      const volumes = JSON.parse(stdout);

      const text =
        "<b>🔊 Playback Control</b>\n<blockquote>" +
        volumes
          .map((v) => `<b>Stream</b>\n-> ${v.stream}\n<b>Current</b>\n-> ${v.volume}\n<b>Maximum</b>\n-> ${v.max_volume}\n`)
          .join("\n") +
        "</blockquote>";

      const keyboard = volumes.map((v) => [
        { text: v.stream, callback_data: `volume_select_${v.stream}` },
      ]);

      return safeReply(ctx, text, {
        parse_mode: "HTML",
        reply_markup: { inline_keyboard: keyboard },
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    } catch (err) {
      appendLogLine(`Volume parse failed: ${err.message}`);
      return safeReply(ctx, "❌ Failed to parse volume information", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }
  });
});

// ================== VOLUME SELECT ==================
bot.action(/^volume_select_(.+)$/i, topicOnly, adminOnly, async (ctx) => {
  const stream = ctx.match[1];

  exec("termux-volume", async (error, stdout) => {
    if (error) return safeAnswerCbQuery(ctx, "Failed to get volume information", { show_alert: true });

    try {
      const volumes = JSON.parse(stdout);
      const target = volumes.find((v) => v.stream.toLowerCase() === stream.toLowerCase());

      if (!target) return safeAnswerCbQuery(ctx, "Invalid stream", { show_alert: true });

      await safeEditMessageText(ctx, buildVolumeText(target), {
        parse_mode: "HTML",
        reply_markup: { inline_keyboard: buildVolumeKeyboard(stream, target) },
      });

      await safeAnswerCbQuery(ctx);
    } catch {
      await safeAnswerCbQuery(ctx, "Failed", { show_alert: true });
    }
  });
});

// ================== VOLUME SET ==================
bot.action(/^volume_set_([a-zA-Z0-9]+)_(\d+)$/i, topicOnly, adminOnly, async (ctx) => {
  const stream = ctx.match[1];
  const value  = parseInt(ctx.match[2], 10);

  execFile("termux-volume", [stream, value.toString()], async (error) => {
    if (error) {
      appendLogLine(`Volume set failed: ${error.message}`);
      return safeAnswerCbQuery(ctx, "Failed to set volume", { show_alert: true });
    }

    exec("termux-volume", async (error, stdout) => {
      if (error) return safeAnswerCbQuery(ctx, "Failed to refresh volume", { show_alert: true });

      try {
        const volumes = JSON.parse(stdout);
        const target = volumes.find((v) => v.stream.toLowerCase() === stream.toLowerCase());

        if (!target) return safeAnswerCbQuery(ctx, "Invalid stream", { show_alert: true });

        await safeEditMessageText(ctx, buildVolumeText(target), {
          parse_mode: "HTML",
          reply_markup: { inline_keyboard: buildVolumeKeyboard(stream, target) },
        });

        await safeAnswerCbQuery(ctx, `${target.stream}: ${target.volume}/${target.max_volume}`);
      } catch {
        await safeAnswerCbQuery(ctx, "Failed", { show_alert: true });
      }
    });
  });
});

// ================== VOLUME HOME ==================
bot.action("volume_home", topicOnly, adminOnly, async (ctx) => {
  exec("termux-volume", async (error, stdout) => {
    if (error) return safeAnswerCbQuery(ctx, "Failed", { show_alert: true });

    try {
      const volumes = JSON.parse(stdout);

      const text =
        "<b>🔊 Playback Control</b>\n<blockquote>" +
        volumes
          .map((v) => `<b>Stream</b>\n-> ${v.stream}\n<b>Current</b>\n-> ${v.volume}\n<b>Maximum</b>\n-> ${v.max_volume}\n`)
          .join("\n") +
        "</blockquote>";

      const keyboard = volumes.map((v) => [
        { text: v.stream, callback_data: `volume_select_${v.stream}` },
      ]);

      await safeEditMessageText(ctx, text, {
        parse_mode: "HTML",
        reply_markup: { inline_keyboard: keyboard },
      });

      await safeAnswerCbQuery(ctx);
    } catch {
      await safeAnswerCbQuery(ctx, "Failed", { show_alert: true });
    }
  });
});

// ================== MEDIA ACTIONS ==================
bot.action(/^media_(play|next|previous|stop)$/i, topicOnly, adminOnly, async (ctx) => {
  const action = ctx.match[1];

  execFile("cmd", ["media_session", "dispatch", action], async (error) => {
    if (error) return safeAnswerCbQuery(ctx, `Failed: ${action}`, { show_alert: true });
    await safeAnswerCbQuery(ctx, `${action} executed`);
  });
});

// ================== RECEIPT ==================
const STORE_NAME = process.env.STORE_NAME || "";
const STORE_ADDRESS = process.env.STORE_ADDRESS || "";
const STRUK_FONT = process.env.STRUK_FONT || "";
const STRUK_FONTSIZE = process.env.STRUK_FONTSIZE || "";
const TOPIC_ID_INTAKE = process.env.TOPIC_ID_INTAKE  ? parseInt(process.env.TOPIC_ID_INTAKE,  10) : 416182;
const TOPIC_ID_PREVIEW = process.env.TOPIC_ID_PREVIEW ? parseInt(process.env.TOPIC_ID_PREVIEW, 10) : 468585;

function rupiah(num) {
  return num.toString().replace(/\B(?=(\d{3})+(?!\d))/g, ".");
}

function truncateMiddle(text, maxLen) {
  const str = String(text || "-");
  if (str.length <= maxLen) return str;
  return str.slice(0, maxLen);
}

function padC(text, W) {
  const str = truncateMiddle(text, W);
  if (str.length >= W) return str;
  return " ".repeat(Math.floor((W - str.length) / 2)) + str;
}

function padR(label, value, W) {
  const spaces = W - label.length - value.length;
  if (spaces <= 0) return label + value;
  return label + " ".repeat(spaces) + value;
}

function calcAdmin(nominalStr) {
  const num = parseInt((nominalStr || "0").replace(/\./g, ""), 10) || 0;

  if (num <= 500000) {
    return { num: 5000, str: "5.000" };
  }

  const steps = Math.ceil((num - 500000) / 1000000);
  const finalAdmin = 5000 + (steps * 5000);

  return {
    num: finalAdmin,
    str: finalAdmin.toLocaleString("id-ID")
  };
}

function roundTokenNominal(nominalStr) {
  const num = parseInt((nominalStr || "0").replace(/\./g, ""), 10) || 0;
  if (num <= 0) return "0";

  const remainder = num % 10000;
  if (remainder > 0 && remainder <= 5000) {
    return rupiah(num - remainder);
  }

  return rupiah(num);
}

function adjustToWita(timeStr) {
  if (!timeStr || !timeStr.includes(":")) return "-";
  try {
    const parts = timeStr.trim().split(":");
    let hours = parseInt(parts[0], 10);
    const minutes = parts[1] || "00";
    const seconds = parts[2] || "00";

    if (isNaN(hours)) return timeStr;

    hours = (hours + 1) % 24;

    const formattedHours = hours.toString().padStart(2, "0");
    return `${formattedHours}:${minutes}:${seconds}`;
  } catch (err) {
    return timeStr;
  }
}

function buildStrukPLN(data) {
  const W = 32, EQ = "=".repeat(W), DA = "-".repeat(W);

  const roundedNominal = roundTokenNominal(data.nominal);
  const admin = calcAdmin(roundedNominal);
  const nom = parseInt((roundedNominal || "0").replace(/\./g, ""), 10) || 0;
  const tot = rupiah(nom + admin.num);

  const witaTime = adjustToWita(data.waktu);

  const tk = (data.token || "").replace(/[\s\-]/g, "");
  const gr = tk.match(/.{1,4}/g) || [];
  const tl1 = gr.slice(0, 3).join(" ");
  const tl2 = gr.slice(3).join(" ");

  return [
    padC(STORE_NAME, W),
    padC(STORE_ADDRESS, W),
    EQ,
    padC("STRUK PEMBELIAN TOKEN LISTRIK", W),
    EQ,
    padR("TANGGAL", data.tanggal || "-", W),
    padR("WAKTU", witaTime, W),
    DA,
    `ID PEL     : ${truncateMiddle(data.idPel, 18)}`,
    `NO METER   : ${truncateMiddle(data.noMeter, 18)}`,
    `NAMA       : ${truncateMiddle(data.nama, 18)}`,
    `TARIF/DAYA : ${truncateMiddle(data.tarif, 18)}`,
    `JML KWH    : ${truncateMiddle(data.jmlKwh, 18)}`,
    DA,
    padC("TOKEN", W),
    padC(tl1, W),
    tl2 ? padC(tl2, W) : null,
    DA,
    padR("NOMINAL", `Rp ${roundedNominal}`, W),
    padR("ADMIN", `Rp ${admin.str}`, W),
    EQ,
    padR("TOTAL", `Rp ${tot}`, W),
    EQ,
    "",
    padC("** TRANSAKSI BERHASIL **", W),
    "",
    padC("TERIMA KASIH", W),
  ].filter((r) => r !== null).join("\n");
}

function buildStrukTransfer(data) {
  const W = 32, EQ = "=".repeat(W), DA = "-".repeat(W);
  const admin = calcAdmin(data.nominal);
  const nom = parseInt((data.nominal || "0").replace(/\./g, ""), 10) || 0;
  const tot = rupiah(nom + admin.num);

  const witaTime = adjustToWita(data.waktu);

  return [
    padC(STORE_NAME, W),
    padC(STORE_ADDRESS, W),
    EQ,
    padR("TANGGAL", data.tanggal || "-", W),
    padR("WAKTU", witaTime, W),
    DA,
    padC("DATA PENERIMA", W),
    `TUJUAN   : ${truncateMiddle(data.bankTujuan, 10)}`,
    `NOMOR    : ${truncateMiddle(data.noRekening, 15)}`,
    `PENERIMA : ${truncateMiddle(data.penerima, 21)}`,
    DA,
    padR("NOMINAL", `Rp ${data.nominal || "0"}`, W),
    padR("ADMIN", `Rp ${admin.str}`, W),
    EQ,
    padR("TOTAL", `Rp ${tot}`, W),
    EQ,
    "",
    padC("** TRANSAKSI BERHASIL **", W),
    "",
    padC("TERIMA KASIH", W),
  ].filter((r) => r !== null).join("\n");
}

function buildStrukPulsa(data) {
  const W = 32, EQ = "=".repeat(W), DA = "-".repeat(W);
  const roundedNominal = roundTokenNominal(data.nominal);
  const admin = { num: 3000, str: "3.000" };
  const nom = parseInt((roundedNominal || "0").replace(/\./g, ""), 10) || 0;
  const tot = rupiah(nom + admin.num);

  const witaTime = adjustToWita(data.waktu);

  return [
    padC(STORE_NAME, W),
    padC(STORE_ADDRESS, W),
    EQ,
    padR("TANGGAL", data.tanggal || "-", W),
    padR("WAKTU", witaTime, W),
    DA,
    padC("DATA PENERIMA", W),
    `TUJUAN   : ${truncateMiddle(data.providerTujuan, 18)}`,
    `NOMOR    : ${truncateMiddle(data.noProvider, 18)}`,
    DA,
    padR("NOMINAL", `Rp ${roundedNominal}`, W),
    padR("ADMIN", `Rp ${admin.str}`, W),
    EQ,
    padR("TOTAL", `Rp ${tot}`, W),
    EQ,
    "",
    padC("** TRANSAKSI BERHASIL **", W),
    "",
    padC("TERIMA KASIH", W),
  ].filter((r) => r !== null).join("\n");
}

function buildStruk(data) {
  if (data.type === "transfer") return buildStrukTransfer(data);
  if (data.type === "pulsa") return buildStrukPulsa(data);
  return buildStrukPLN(data);
}

function execPromise(command, timeout = 20000) {
  return new Promise((resolve, reject) => {
    exec(command, { timeout }, (err, stdout) => {
      if (err) return reject(err);
      resolve(stdout);
    });
  });
}

async function buildStrukImage(struk) {
  const id = Date.now();
  const files = {
    nameTxt: `struk_name_${id}.txt`,
    addrTxt: `struk_addr_${id}.txt`,
    bodyTxt: `struk_body_${id}.txt`,
    namePng: `struk_name_${id}.png`,
    addrPng: `struk_addr_${id}.png`,
    bodyPng: `struk_body_${id}.png`,
    nameExtPng: `struk_namex_${id}.png`,
    addrExtPng: `struk_addrx_${id}.png`,
    outPng: `struk_${id}.png`,
  };

  const lines = struk.split('\n');
  const nameLine = (lines[0] || '').trim();
  const addrLine = (lines[1] || '').trim();
  const bodyContent = lines.slice(2).join('\n');

  fs.writeFileSync(files.nameTxt, nameLine);
  fs.writeFileSync(files.addrTxt, addrLine);
  fs.writeFileSync(files.bodyTxt, bodyContent);

  const normalSize = parseInt(STRUK_FONTSIZE, 10);
  const smallSize = Math.max(9, normalSize - 4);
  const largeSize = normalSize + 8;
  const base = `-background white -fill black -font "${STRUK_FONT}"`;

  const getDim = async (file, fmt) => {
    try {
      const out = await execPromise(`magick "${file}" -format "${fmt}" info:`);
      return parseInt(out.trim(), 10) || 0;
    } catch { return 0; }
  };

  try {
    await execPromise(`magick ${base} -pointsize ${largeSize} label:@${files.nameTxt} "${files.namePng}"`);
    await execPromise(`magick ${base} -pointsize ${smallSize}  label:@${files.addrTxt} "${files.addrPng}"`);
    await execPromise(`magick ${base} -pointsize ${normalSize} label:@${files.bodyTxt} "${files.bodyPng}"`);

    const bodyW = (await getDim(files.bodyPng, '%w')) || 350;
    const nameH = (await getDim(files.namePng, '%h')) || 40;
    const addrH = (await getDim(files.addrPng, '%h')) || 30;

    const nameW = await getDim(files.namePng, '%w');
    const canvasW = Math.max(bodyW, nameW + 20); 

    await execPromise(
      `magick -size ${canvasW}x${nameH} xc:white` +
      ` "${files.namePng}" -gravity Center -composite "${files.nameExtPng}"`
    );
    await execPromise(
      `magick -size ${canvasW}x${addrH} xc:white` +
      ` "${files.addrPng}" -gravity Center -composite "${files.addrExtPng}"`
    );
    await execPromise(
      `magick -size ${canvasW}x${await getDim(files.bodyPng, '%h')} xc:white` +
      ` "${files.bodyPng}" -gravity Center -composite "body_centered_${id}.png"`
    );
    await execPromise(
      `magick "${files.nameExtPng}" "${files.addrExtPng}" "body_centered_${id}.png"` +
      ` -append -bordercolor white -border 25x20 "${files.outPng}"`
    );
  } finally {
    ['nameTxt','addrTxt','bodyTxt','namePng','addrPng','bodyPng','nameExtPng','addrExtPng', `body_centered_${id}.png`]
      .forEach(k => { try { fs.unlinkSync(files[k] || k); } catch {} });
  }

  return files.outPng;
}

const RECEIPT_PROMPT = `Identify the type of this receipt and extract its data. Return ONLY JSON (no markdown, no explanations).

If it's a Money TRANSFER receipt (DANA, OVO, GoPay, BCA, BRI, etc):
{
  "type": "transfer",
  "tanggal": "YYYY-MM-DD",
  "waktu": "HH:MM:SS",
  "bankTujuan": "destination bank/wallet name e.g. DANA",
  "noRekening": "destination account/phone number",
  "penerima": "recipient name",
  "nominal": "amount transferred excluding admin fee"
}

If it's a PLN ELECTRICITY TOKEN receipt:
{
  "type": "pln",
  "tanggal": "YYYY-MM-DD",
  "waktu": "HH:MM:SS",
  "idPel": "customer ID number",
  "noMeter": "meter number",
  "nama": "customer name",
  "tarif": "tariff/power e.g. R2/3500VA",
  "jmlKwh": "kWh total amount",
  "token": "20 digits token numbers only without space or hyphen",
  "nominal": "total payment paid by customer with thousands separator"
}

If it's a Pulsa receipt (Telkomsel, Tri, XL, simPATI, Indosat, etc):
{
  "type": "pulsa",
  "tanggal": "YYYY-MM-DD",
  "waktu": "HH:MM:SS",
  "providerTujuan": "destination provider/sim name e.g. SIMPATI",
  "noProvider": "destination account/phone number",
  "nominal": "amount transferred excluding admin fee"
}`;

async function analyzeReceipt(base64Image) {
  const rawUrl = process.env.NINE_ROUTER_URL;
  if (!rawUrl) {
    throw new Error("NINE_ROUTER_URL is missing in .env file!");
  }

  const apiKey = process.env.NINE_ROUTER_API_KEY;
  if (!apiKey) {
    throw new Error("NINE_ROUTER_API_KEY is missing in .env file!");
  }

  const model = process.env.NINE_ROUTER_MODEL;
  if (!model) {
    throw new Error("NINE_ROUTER_MODEL is missing in .env file!");
  }

  const baseUrl = rawUrl.replace(/\/+$/, "");
  const endpoint = baseUrl.endsWith("/chat/completions")
    ? baseUrl
    : baseUrl.endsWith("/v1")
    ? `${baseUrl}/chat/completions`
    : `${baseUrl}/v1/chat/completions`;

  try {
    const res = await fetch(endpoint, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${apiKey}`,
      },
      signal: AbortSignal.timeout(60000),
      body: JSON.stringify({
        model,
        messages: [
          {
            role: "user",
            content: [
              {
                type: "image_url",
                image_url: {
                  url: `data:image/jpeg;base64,${base64Image}`,
                },
              },
              {
                type: "text",
                text: RECEIPT_PROMPT,
              },
            ],
          },
        ],
        stream: false,
        response_format: { type: "json_object" },
      }),
    });

    if (!res.ok) {
      const errText = await res.text();
      throw new Error(`9Router API HTTP ${res.status}: ${errText}`);
    }

    const rawText = await res.text();
    let rawContent = "";

    try {
      const data = JSON.parse(rawText);
      rawContent = data?.choices?.[0]?.message?.content ?? "";
    } catch {
      const lines = rawText.split("\n");
      for (const line of lines) {
        const trimmed = line.trim();
        if (trimmed.startsWith("data: ") && trimmed !== "data: [DONE]") {
          try {
            const parsed = JSON.parse(trimmed.slice(6));
            rawContent +=
              parsed.choices?.[0]?.delta?.content ||
              parsed.choices?.[0]?.message?.content ||
              "";
          } catch {}
        }
      }
    }

    if (!rawContent) {
      throw new Error("9Router returned empty response for vision analysis");
    }

    appendLogLine("Vision: 9Router OK");

    const cleanedJson = rawContent.replace(/```(?:json)?\n?|```/g, "").trim();

    try {
      return JSON.parse(cleanedJson);
    } catch (parseErr) {
      appendLogLine(`[JSON Parse Failed] Content received:\n${cleanedJson}`);
      throw new Error(`Invalid JSON format from model: ${parseErr.message}`);
    }
  } catch (err) {
    appendLogLine(`Vision Error: ${err.message}`);
    throw err;
  }
}

async function processReceipt(ctx, targetThreadId) {
  const fileId = ctx.message.photo[ctx.message.photo.length - 1].file_id;
  const typing = startTyping(ctx, targetThreadId, "upload_photo");

  try {
    const file = await ctx.telegram.getFile(fileId);
    const fileUrl = `https://api.telegram.org/file/bot${process.env.BOT_TOKEN}/${file.file_path}`;
    const imgRes = await fetch(fileUrl, { signal: AbortSignal.timeout(15000) });
    const base64Image = Buffer.from(await imgRes.arrayBuffer()).toString("base64");

    let parsed;
    try {
      parsed = await analyzeReceipt(base64Image);
    } catch (err) {
      appendLogLine(`Receipt analysis error: ${err.message}`);
      return;
    }

    const isValid =
      (parsed.type === "pln" && parsed.token && parsed.token.replace(/\D/g, "").length >= 10) ||
      (parsed.type === "transfer" && (parsed.noRekening || parsed.bankTujuan)) ||
      (parsed.type === "pulsa" && (parsed.providerTujuan || parsed.noProvider));

    if (!isValid) {
      appendLogLine("Struk validation failed: Image is not recognized as a valid receipt.");
      return;
    }

    if (parsed.penerima) {
      parsed.penerima = parsed.penerima
        .replace(/\bDana\b/g, '')
        .replace(/\bDnid\b/g, '')
        .replace(/\s+/g, ' ')
        .trim();
    }

    if (parsed.nominal) {
      parsed.nominal = Number(
        String(parsed.nominal).replace(/[^\d]/g, '')
      ).toLocaleString('id-ID');
    }

    const strukLines = buildStruk(parsed);
    let imgPath = null;

    try {
      imgPath = await buildStrukImage(strukLines);
    } catch (imgErr) {
      appendLogLine(`ImageMagick error: ${imgErr.message}`);
    }

    if (imgPath) {
      const imgBuffer = fs.readFileSync(imgPath);
      const sendOptions = targetThreadId ? { message_thread_id: targetThreadId } : {};

      try {
        await ctx.telegram.sendPhoto(
          ctx.chat.id,
          { source: imgBuffer, filename: "receipt.png" },
          sendOptions
        );
      } catch (photoErr) {
        appendLogLine(`Failed to send receipt to Telegram: ${photoErr.message}`);
      }

      const printCmd = process.env.PRINT_CMD;
      if (!printCmd) {
        appendLogLine("Print error: PRINT_CMD is not configured in .env.");
        try { fs.unlinkSync(imgPath); } catch {}
      } else {
        exec(`${printCmd} "${imgPath}"`, (err) => {
          if (err) appendLogLine(`Print execution error: ${err.message}`);
          setTimeout(() => {
            try { if (fs.existsSync(imgPath)) fs.unlinkSync(imgPath); } catch {}
          }, 3000);
        });
      }
    }
  } catch (err) {
    appendLogLine(`Receipt process error: ${err.message}`);
  } finally {
    stopTyping(typing);
  }
}

bot.on("photo", adminOnly, async (ctx) => {
  const currentThreadId = ctx.message?.message_thread_id ?? null;
  if (TOPIC_ID_INTAKE !== null && currentThreadId !== TOPIC_ID_INTAKE) return;

  const targetThreadId = (TOPIC_ID_PREVIEW !== null && !isNaN(TOPIC_ID_PREVIEW))
    ? TOPIC_ID_PREVIEW
    : currentThreadId;

  processReceipt(ctx, targetThreadId).catch((err) =>
    appendLogLine(`Receipt background error: ${err.message}`)
  );
});

// ================== PRICE ==================
async function fetchPriceList() {
  const url = `https://docs.google.com/spreadsheets/d/${process.env.SHEETS_ID}/export?format=csv&gid=${process.env.SHEETS_GID || "0"}`;
  const res = await fetch(url);
  if (!res.ok) throw new Error(`Sheets HTTP ${res.status}`);
  const csv = await res.text();

  return csv
    .split("\n")
    .map((row) => {
      const fields = [];
      let field = "";
      let inQuotes = false;
      for (const ch of row) {
        if (ch === '"') {
          inQuotes = !inQuotes;
        } else if (ch === "," && !inQuotes) {
          fields.push(field.trim());
          field = "";
        } else {
          field += ch;
        }
      }
      fields.push(field.trim());
      return fields;
    })
    .filter((row) => row.some((cell) => cell.replace(/\r/, "")));
}

bot.hears(/^\/p(?:@\w+)?(?:\s+(.+))?$/i, topicOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  const keyword = (ctx.match[1] || "").trim().toLowerCase();

  if (!keyword) {
    return safeReply(ctx, "❌ Please provide an item name.", {
      parse_mode: "HTML",
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  }

  const typing = startTyping(ctx, threadId);

  try {
    const rows = await fetchPriceList();
    const dataRows = rows.slice(1).filter((row) => row[0]?.replace(/\r/, ""));

    if (!dataRows.length) {
      stopTyping(typing);
      return safeReply(ctx, "❌ Price list is empty.", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }

    const filtered = dataRows.filter((row) =>
      (row[0] || "").toLowerCase().includes(keyword) ||
      (row[1] || "").toLowerCase().includes(keyword)
    );

    if (!filtered.length) {
      stopTyping(typing);
      return safeReply(ctx, `❌ No items found matching "<b>${escapeHTML(keyword)}</b>".`, {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }

    const chunks = [];
    let current = `<b>💰 Norikimika Price</b>\n`;

    for (const row of filtered) {
      const name = escapeHTML((row[0] || "-").replace(/\r/, ""));
      const contents = escapeHTML((row[1] || "-").replace(/\r/, ""));
      const price = escapeHTML((row[2] || "-").replace(/\r/, ""));
      const rawCapital = (row[5] || "-").replace(/\r/g, "").trim();
      const capital = escapeHTML(rawCapital.toUpperCase() === "#VALUE!" ? "-" : rawCapital);
      const rawProfit = (row[6] || "-").replace(/\r/g, "").trim();
      const profit = escapeHTML(rawProfit.toUpperCase() === "#VALUE!" ? "-" : rawProfit);

      const item = `<blockquote>Name : ${name}\nContents : ${contents}\nPrice : ${price}\nCapital : <tg-spoiler>${capital}</tg-spoiler>\nProfit : <tg-spoiler>${profit}</tg-spoiler></blockquote>\n`;

      if (current.length + item.length > 3800) {
        chunks.push(current.trim());
        current = item;
      } else {
        current += item;
      }
    }
    if (current.trim()) chunks.push(current.trim());

    stopTyping(typing);

    for (const chunk of chunks) {
      await safeReply(ctx, chunk, {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }
  } catch (err) {
    stopTyping(typing);
    appendLogLine(`Price fetch error: ${err.message}`);
    return safeReply(ctx, "❌ Failed to fetch price list.", {
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  } finally {
    stopTyping(typing);
  }
});

// ================== APPROVE ==================
bot.hears(/^\/approve/i, topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  const typing = startTyping(ctx, threadId);

  exec(
    "su -c \"input tap 150 1260 && sleep 2 && input tap 880 1435\"",
    async (error, stdout, stderr) => {
      stopTyping(typing);
      if (stdout) appendLogLine(stdout);
      if (stderr) appendLogLine(stderr);

      if (error) {
        appendLogLine(`Approve failed: ${error.message}`);
        return safeReply(ctx, "❌ Failed to run approve", {
          reply_to_message_id: ctx.message?.message_id,
          message_thread_id: threadId,
        });
      }

      return safeReply(ctx, "✅ Approve executed", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }
  );
});

// ================== TIKTOK DOWNLOADER ==================
async function updateTTStatus(ctx, messageId, { title, uploader, status, platform = "TikTok" }) {
  for (let i = 0; i <= DEFAULT_RETRIES; i++) {
    try {
      await ctx.telegram.editMessageText(
        ctx.chat.id,
        messageId,
        undefined,
        `<b>🎬 ${platform} Info</b>\n` +
          `<blockquote>` +
          `<b>👤 Author:</b>\n-> <code>${escapeHTML(uploader || "Unknown")}</code>\n` +
          `<b>📝 Title:</b>\n-> <code>${escapeHTML(title || "TikTok Post")}</code>\n` +
          `<b>📦 Status:</b>\n-> <code>${status}</code>` +
          `</blockquote>`,
        { parse_mode: "HTML" }
      );
      return;
    } catch (err) {
      if (err.description && err.description.includes("message is not modified")) return;
      appendLogLine(`updateTTStatus error: ${err.message}`);
      if (i < DEFAULT_RETRIES) await new Promise((r) => setTimeout(r, DEFAULT_RETRY_DELAY));
    }
  }
}

bot.command("tt", topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  const targetTopicId = TOPIC_ID_TIKTOK || threadId;
  const text = ctx.message?.text?.trim() || "";
  let argUrl = text.replace(/^\/tt(@\w+)?\s*/i, "").trim();
  let replyText = ctx.message?.reply_to_message?.text?.trim() || "";

  const combinedText = `${argUrl} ${replyText}`.trim();
  const urlMatch = combinedText.match(/https?:\/\/(?:www\.|vm\.|vt\.|v\.)?(?:tiktok|douyin|iesdouyin)\.com\/[^\s]+/i) || combinedText.match(/https?:\/\/[^\s]+/i);
  const targetUrl = urlMatch ? urlMatch[0] : null;

  if (!targetUrl) {
    return safeReply(ctx, "❌ Please provide a TikTok/Douyin URL or reply to a message containing one.", {
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  }

  const isDouyin = /(?:douyin|iesdouyin)\.com/i.test(targetUrl);
  const platformLabel = isDouyin ? "Douyin" : "TikTok";

  const typing = startTyping(ctx, targetTopicId, "upload_document");
  const tempDir = path.join(__dirname, `tt_${Date.now()}_${Math.random().toString(36).substring(2, 7)}`);
  let infoMessage = null;

  try {
    fs.mkdirSync(tempDir, { recursive: true });

    infoMessage = await ctx.reply(
      `<b>🎬 ${platformLabel} Info</b>\n<blockquote><b>👤 Author:</b>\n-> <code>Fetching...</code>\n<b>📝 Title:</b>\n-> <code>Fetching...</code>\n<b>📦 Status:</b>\n-> <code>Downloading</code></blockquote>`,
      { parse_mode: "HTML", reply_to_message_id: ctx.message?.message_id, message_thread_id: threadId }
    );

    let title, uploader, videoUrl = null, images = [], liveVideos = [], liveRaw = [], musicUrl = null;

    if (isDouyin) {
      const apiUrl = `https://douyin.wtf/api/hybrid/video_data?url=${encodeURIComponent(targetUrl)}&minimal=true`;
      const apiRes = await fetch(apiUrl);
      const apiJson = await apiRes.json();

      if (!apiJson || apiJson.code !== 200 || !apiJson.data) {
        throw new Error(apiJson?.message || "Failed to fetch media from Douyin API");
      }

      const data = apiJson.data;
      title = data.desc || "Douyin Post";
      uploader = data.author?.nickname || data.author?.unique_id || "Unknown";
      const vd = data.video_data || {};
      videoUrl = vd.nwm_video_url || vd.nwm_video_url_HQ || vd.wm_video_url || vd.wm_video_url_HQ || null;
      const imgData = data.image_data?.no_watermark_image_list || data.image_data?.watermark_image_list;
      if (Array.isArray(imgData) && imgData.length > 0) {
        images = imgData;
      } else if (Array.isArray(data.images)) {
        images = data.images.map((img) => (img?.url_list || []).pop() || (img?.url_list || [])[0]).filter(Boolean);
      } else {
        images = [];
      }
      musicUrl = data.music?.play_url?.url_list?.[0] || null;
    } else {
      const apiUrl = `https://www.tikwm.com/api/?url=${encodeURIComponent(targetUrl)}&hd=1`;
      const apiRes = await fetch(apiUrl);
      const apiJson = await apiRes.json();

      if (!apiJson || apiJson.code !== 0 || !apiJson.data) {
        throw new Error(apiJson?.msg || "Failed to fetch media from TikTok API");
      }

      const data = apiJson.data;
      title = data.title || "TikTok Post";
      uploader = data.author?.nickname || data.author?.unique_id || "Unknown";
      videoUrl = data.hdplay || data.play || null;
      images = Array.isArray(data.images) ? data.images : [];
      liveRaw = Array.isArray(data.live_images) ? data.live_images : [];
      liveVideos = liveRaw.filter(Boolean);
      musicUrl = data.music_info?.play || data.music || null;
    }

    if (title.length > 80) title = title.substring(0, 80) + "...";

    await updateTTStatus(ctx, infoMessage.message_id, { title, uploader, status: "Downloading", platform: platformLabel });

    const isImages = images.length > 0;
    const isLive = liveVideos.length > 0;
    const downloadedFiles = [];
    const slides = [];
    let audioPath = null;

    if (isLive) {
      const n = Math.max(images.length, liveRaw.length);
      for (let i = 0; i < n; i++) {
        const tag = String(i + 1).padStart(2, "0");
        if (liveRaw[i]) {
          const vidRes = await fetch(liveRaw[i]);
          const buffer = Buffer.from(await vidRes.arrayBuffer());
          const filePath = path.join(tempDir, `slide_${tag}.mp4`);
          fs.writeFileSync(filePath, buffer);
          slides.push({ kind: "video", path: filePath });
        } else if (images[i]) {
          const imgRes = await fetch(images[i]);
          const buffer = Buffer.from(await imgRes.arrayBuffer());
          const filePath = path.join(tempDir, `slide_${tag}.jpg`);
          fs.writeFileSync(filePath, buffer);
          slides.push({ kind: "photo", path: filePath });
        }
      }
    } else if (isImages) {
      for (let i = 0; i < images.length; i++) {
        const imgUrl = images[i];
        const imgRes = await fetch(imgUrl);
        const buffer = Buffer.from(await imgRes.arrayBuffer());
        const filePath = path.join(tempDir, `photo_${String(i + 1).padStart(2, "0")}.jpg`);
        fs.writeFileSync(filePath, buffer);
        downloadedFiles.push(filePath);
      }
    } else if (!isLive) {
      if (!videoUrl) throw new Error("No video URL found in API response");

      const videoRes = await fetch(videoUrl);
      const videoBuffer = Buffer.from(await videoRes.arrayBuffer());
      const filePath = path.join(tempDir, "video.mp4");
      fs.writeFileSync(filePath, videoBuffer);
      downloadedFiles.push(filePath);
    }

    if (musicUrl) {
      try {
        const audioRes = await fetch(musicUrl);
        const audioBuffer = Buffer.from(await audioRes.arrayBuffer());
        audioPath = path.join(tempDir, "audio.mp3");
        fs.writeFileSync(audioPath, audioBuffer);
      } catch {}
    }

    if (downloadedFiles.length === 0 && slides.length === 0) {
      throw new Error(`No media downloaded from ${platformLabel} response`);
    }

    await updateTTStatus(ctx, infoMessage.message_id, { title, uploader, status: "Sending", platform: platformLabel });

    if (isLive) {
      if (slides.length > 0) {
        const chunkSize = 10;
        for (let i = 0; i < slides.length; i += chunkSize) {
          const chunk = slides.slice(i, i + chunkSize);
          const mediaGroup = chunk.map((s) => ({
            type: s.kind,
            media: { source: fs.createReadStream(s.path) },
          }));
          await safeReplyWithMediaGroup(
            ctx,
            () => mediaGroup,
            { message_thread_id: targetTopicId }
          );
        }
      }
    } else if (!isImages) {
      await safeReplyWithVideo(
        ctx,
        () => ({ source: fs.createReadStream(downloadedFiles[0]) }),
        {
          message_thread_id: targetTopicId,
        }
      );
    } else {
      if (downloadedFiles.length === 1) {
        await safeReplyWithPhoto(
          ctx,
          () => ({ source: fs.createReadStream(downloadedFiles[0]) }),
          {
            message_thread_id: targetTopicId,
          }
        );
      } else if (downloadedFiles.length > 1) {
        const chunkSize = 10;
        for (let i = 0; i < downloadedFiles.length; i += chunkSize) {
          const chunk = downloadedFiles.slice(i, i + chunkSize);
          const mediaGroup = chunk.map((photoPath, idx) => {
            return {
              type: "photo",
              media: { source: fs.createReadStream(photoPath) },
            };
          });

          await safeReplyWithMediaGroup(
            ctx,
            () => mediaGroup,
            { message_thread_id: targetTopicId }
          );
        }
      }
    }

    if (audioPath && fs.existsSync(audioPath)) {
      await safeReplyWithAudio(
        ctx,
        () => ({ source: fs.createReadStream(audioPath) }),
        {
          title: title || `${platformLabel} Audio`,
          performer: uploader || platformLabel,
          message_thread_id: targetTopicId,
        }
      );
    }

    await updateTTStatus(ctx, infoMessage.message_id, { title, uploader, status: "Success", platform: platformLabel });

  } catch (err) {
    appendLogLine(`[TT ERROR] ${err.stack || err.message}`);
    if (infoMessage) {
      await updateTTStatus(ctx, infoMessage.message_id, { title: "Error", uploader: "Unknown", status: "Failed", platform: platformLabel });
    }
  } finally {
    stopTyping(typing);
    try {
      if (fs.existsSync(tempDir)) {
        fs.rmSync(tempDir, { recursive: true, force: true });
      }
    } catch {}
  }
});

// ================== HELP COMMAND ==================
bot.hears(/^\/(\w+)/, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  if (threadId !== TOPIC_ID) return;

  const typing = startTyping(ctx, threadId);
  await safeReply(
    ctx,
      "<b>🤖 Public Commands</b>\n" +
      "<blockquote><b>✹ Price List</b>\n" +
      "-> <code>/p [name]</code></blockquote>\n" +
      "<b>⚠️ Admin Commands</b>\n" +
      "<blockquote><b>✹ Approve</b>\n" +
      "-> /approve</blockquote>\n" +
      "<blockquote><b>✹ Battery Status</b>\n" +
      "-> /battery</blockquote>\n" +
      "<blockquote><b>✹ Download Music</b>\n" +
      "-> <code>/music [title]</code>\n" +
      "-> <code>/music2 [title]</code></blockquote>\n" +
      "<blockquote><b>✹ Download TikTok/Douyin</b>\n" +
      "-> <code>/tt [link]</code></blockquote>\n" +
      "<blockquote><b>✹ Data Usage</b>\n" +
      "-> /usage</blockquote>\n" +
      "<blockquote><b>✹ Playback Controller</b>\n" +
      "-> /playback</blockquote>\n" +
      "<blockquote><b>✹ Reboot Device</b>\n" +
      "-> /reboot</blockquote>\n" +
      "<blockquote><b>✹ Speed Test</b>\n" +
      "-> /speed</blockquote>",
    { parse_mode: "HTML", reply_to_message_id: ctx.message?.message_id, message_thread_id: threadId }
  );
  stopTyping(typing);
});

// ================== MUTE ==================
bot.on("text", async (ctx, next) => {
  const threadId = ctx.message?.message_thread_id;
  const text = ctx.message?.text?.trim() || "";
  const chatId = ctx.chat?.id;
  const userId = ctx.from?.id;

  if (threadId !== TOPIC_ID) return next();

  if (!text.startsWith("/")) {
    try {
      const admins = await ctx.telegram.getChatAdministrators(chatId);
      const isAdmin = admins.some((admin) => admin.user.id === userId);

      if (!isAdmin) {
        const untilDate = Math.floor(Date.now() / 1000) + 5 * 60;
        await ctx.telegram.restrictChatMember(chatId, userId, {
          permissions: { can_send_messages: false },
          until_date: untilDate,
        });

        return safeReply(ctx, "⛔ Commands only! You are muted for 5 minutes.", {
          reply_to_message_id: ctx.message?.message_id,
        });
      }
    } catch (err) {
      appendLogLine(`Mute non-command failed: ${err.message}`);
    }
  }
  return next();
});

// ================== GLOBAL ERROR HANDLER ==================
bot.catch((err, ctx) => {
  appendLogLine(`Bot error for update ${ctx.update?.update_id}: ${err && (err.stack || err.message)}`);
});

// ================== LAUNCH BOT ==================
bot.launch()
  .then(() => {
    console.log("Bot launched");
    appendLogLine("Bot launched");
  })
  .catch((err) => {
    console.error("Failed to launch bot:", err.message);
    appendLogLine(`Failed to launch bot: ${err.message}`);
  });

// ================== EXPORT ==================
const runRepo = require('./repo.js');
const runMonitor = require('./monitor.js');
const runNotification = require('./notification.js');
const runDiscord = require('./discord.js');
const { startBingRewards } = require('./bing.js');

async function loop() {
  runRepo().catch((err) => {
    appendLogLine(`Repo Error: ${err.message}`);
  });
  setTimeout(loop, 300000);
}

loop();
runMonitor();
runNotification();
runDiscord();
startBingRewards(cron);

execFile("termux-tts-speak", ["Bot launched"]);
