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

const LOG_FILE = path.join(os.homedir(), 'SERVER', 'Backup', 'bot.log');
const logStream = fs.createWriteStream(LOG_FILE, { flags: "a" });

function appendLogLine(line) {
  try {
    const ts = new Date().toISOString();
    fs.appendFileSync(LOG_FILE, `[${ts}] ${line}\n`);
  } catch (e) {
    console.error("Failed to write to log file:", e.message);
  }
}

process.on("uncaughtException", (err) => {
  console.error("Uncaught Exception:", err);
  appendLogLine(`Uncaught Exception: ${err.stack || err.message}`);
});

process.on("unhandledRejection", (reason) => {
  console.error("Unhandled Rejection:", reason);
  appendLogLine(`Unhandled Rejection: ${reason && reason.stack ? reason.stack : reason}`);
});

const bot = new Telegraf(process.env.BOT_TOKEN);

const ALLOWED_CHAT_ID = parseInt(process.env.ALLOWED_CHAT_ID);
const ALLOWED_TOPIC_ID = parseInt(process.env.ALLOWED_TOPIC_ID);
const ALLOWED_TOPIC_ID_USAGE = parseInt(process.env.ALLOWED_TOPIC_ID_USAGE);
const ALLOWED_TOPIC_ID_MUSIC = parseInt(process.env.ALLOWED_TOPIC_ID_MUSIC);
const ALLOWED_TOPIC_ID_STATUS = parseInt(process.env.ALLOWED_TOPIC_ID_STATUS);

const DEFAULT_RETRIES = 2;
const DEFAULT_RETRY_DELAY = 2000;

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

async function safeAnswerCbQuery(ctx, text, options = {}, retries = DEFAULT_RETRIES) {
  for (let i = 0; i <= retries; i++) {
    try {
      await ctx.answerCbQuery(text, options);
      return true;
    } catch (err) {
      appendLogLine(`safeAnswerCbQuery error (chat ${ctx.chat?.id || "unknown"}): ${err && (err.stack || err.message)}`);
      if (i < retries) await new Promise((r) => setTimeout(r, DEFAULT_RETRY_DELAY));
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

// ================== ADMIN CHECK ==================
bot.use(async (ctx, next) => {
  const user = ctx.from?.username || ctx.from?.id || "unknown";
  const msg = ctx.message?.text || "<non-text>";
  const line = `From ${user}: ${msg}`;
  logStream.write(`[${new Date().toLocaleString('id-ID')}] ${line}\n`);
  await next();
});

// ================== TOPIC ONLY MIDDLEWARE ==================
const topicOnly = (ctx, next) => {
  const threadId = ctx.message?.message_thread_id || ctx.callbackQuery?.message?.message_thread_id;
  if (threadId !== ALLOWED_TOPIC_ID) return;
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

  exec("speedtest-go --json", async (error, stdout) => {
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
      const latency = (result.servers?.[0]?.latency || 0) / 1e6; // ns → ms
      const download = (result.servers?.[0]?.dl_speed || 0) / 125000; // bps → Mbps
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
        appendLogLine(`GMS cleanup failed: ${error.message}`);

        return safeReply(ctx, "❌ Failed to reboot", {
          reply_to_message_id: ctx.message?.message_id,
          message_thread_id: threadId,
        });
      }

      exec("su -c reboot", (rebootError) => {
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
  exec("termux-battery-status", (error, stdout) => {
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

// ================== AI ==================
bot.command("ask", async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  if (threadId !== ALLOWED_TOPIC_ID) return;

  const text = ctx.message.text?.trim();
  let argPrompt = text.replace(/^\/ask(@\w+)?\s*/i, "").trim();
  let replyText = ctx.message.reply_to_message?.text?.trim();

  const useSearch = /^-s\b/i.test(argPrompt);
  if (useSearch) argPrompt = argPrompt.replace(/^-s\s*/i, "").trim();

  let prompt = "";
  if (argPrompt && replyText) prompt = `${argPrompt}: ${replyText}`;
  else if (argPrompt) prompt = argPrompt;
  else if (replyText) prompt = replyText;

  if (!prompt) {
    return safeReply(ctx, "❌ Please provide a prompt.", {
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  }

  async function fetchSearchContext(query) {
    if (!process.env.TAVILY_API_KEY) return null;
    try {
      const res = await fetch("https://api.tavily.com/search", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          api_key: process.env.TAVILY_API_KEY,
          query,
          max_results: 5,
        }),
      });
      if (!res.ok) return null;
      const data = await res.json();
      if (!data?.results?.length) return null;
      return data.results
        .map((r, i) => `[${i + 1}] ${r.title}\n${r.content}\nURL: ${r.url}`)
        .join("\n\n");
    } catch {
      return null;
    }
  }

  function buildSearchPrompt(userPrompt, searchContext) {
    if (!searchContext) return userPrompt;
    return (
      `You are a helpful assistant with access to recent web search results.\n\n` +
      `Search Results:\n${searchContext}\n\n` +
      `User Question: ${userPrompt}\n\n` +
      `Answer based on the search results above. Cite sources where relevant.`
    );
  }

  try {
    let rawText;

    const searchContext = useSearch ? await fetchSearchContext(prompt) : null;
    const enhancedPrompt = buildSearchPrompt(prompt, searchContext);

    try {
      const geminiRes = await fetch(
        `https://generativelanguage.googleapis.com/v1beta/models/${process.env.GEMINI_API_MODEL}:generateContent?key=${process.env.GEMINI_API_KEY}`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            contents: [{ parts: [{ text: enhancedPrompt }] }],
            ...(useSearch && { tools: [{ google_search: {} }] }),
          }),
        }
      );

      if (!geminiRes.ok) throw new Error(`Gemini HTTP ${geminiRes.status}`);
      const geminiData = await geminiRes.json();
      rawText = geminiData?.candidates?.[0]?.content?.parts?.[0]?.text || null;
      if (!rawText) throw new Error("Gemini empty response");

    } catch (geminiError) {
      appendLogLine(`Gemini Failed: ${geminiError.message}`);

      try {
        const groqRes = await fetch(
          "https://api.groq.com/openai/v1/chat/completions",
          {
            method: "POST",
            headers: {
              Authorization: `Bearer ${process.env.GROQ_API_KEY}`,
              "Content-Type": "application/json",
            },
            body: JSON.stringify({
              model: process.env.GROQ_API_MODEL || "llama-3.3-70b-versatile",
              messages: [{ role: "user", content: enhancedPrompt }],
              temperature: 0.7,
            }),
          }
        );

        if (!groqRes.ok) throw new Error(`Groq HTTP ${groqRes.status}`);
        const groqData = await groqRes.json();
        rawText = groqData?.choices?.[0]?.message?.content || null;
        if (!rawText) throw new Error("Groq empty response");

      } catch (groqError) {
        appendLogLine(`Groq Failed: ${groqError.message}`);

        const nvidiaRes = await fetch(
          "https://integrate.api.nvidia.com/v1/chat/completions",
          {
            method: "POST",
            headers: {
              Authorization: `Bearer ${process.env.NVIDIA_API_KEY}`,
              "Content-Type": "application/json",
            },
            body: JSON.stringify({
              model: process.env.NVIDIA_API_MODEL || "openai/gpt-oss-120b",
              messages: [{ role: "user", content: enhancedPrompt }],
              temperature: 0.7,
              max_tokens: 1024,
            }),
          }
        );

        if (!nvidiaRes.ok) throw new Error(`NVIDIA HTTP ${nvidiaRes.status}`);
        const nvidiaData = await nvidiaRes.json();
        rawText = nvidiaData?.choices?.[0]?.message?.content || null;
      }
    }

    const MAX_LENGTH = 3500;
    const parts = rawText.match(new RegExp(`.{1,${MAX_LENGTH}}`, "gs")) || [rawText];

    for (const part of parts) {
      await safeReply(ctx, part, {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }

  } catch (err) {
    appendLogLine(`AI Error: ${err.message}`);
    await safeReply(ctx, "❌ Failed to process request", {
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  }
});

// ================== USAGE ==================
bot.hears(/^\/usage/i, topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;

  const vnstat = spawn("vnstat", ["--json"]);
  let output = "";
  let errOutput = "";

  vnstat.stdout.on("data", (data) => (output += data.toString()));
  vnstat.stderr.on("data", (data) => (errOutput += data.toString()));

  vnstat.on("close", (code) => {
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
  const s = seconds % 60;

  return `${m}:${s.toString().padStart(2, "0")}`;
}

function execPromise(command) {
  return new Promise((resolve, reject) => {
    exec(command, (err, stdout) => {
      if (err) return reject(err);
      resolve(stdout);
    });
  });
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
          `<b>📝 Title:</b>\n-> <code>${title}</code>\n` +
          `<b>👤 Artist:</b>\n-> <code>${performer}</code>\n` +
          `<b>⏳ Duration:</b>\n-> <code>${duration}</code>\n` +
          `<b>📦 Status:</b>\n-> <code>${status}</code>` +
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

bot.command("music", topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  const text = ctx.message.text?.trim() || "";
  const query = text.replace(/^\/music(@\w+)?\s*/i, "").trim();

  if (!query) {
    return safeReply(ctx, "❌ Please provide a song title or YouTube link.", {
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  }

  try {
    const ytArgs = ["-f", "251", "-j", query.startsWith("http") ? query : `ytsearch1:${query}`];
    const { stdout } = await execFilePromise("yt-dlp", ytArgs);
    
    const info = JSON.parse(stdout);

    const title = info.track || info.title || "Unknown Title";
    const performer = info.artist || info.uploader || "Unknown Artist";
    const duration = formatDuration(info.duration);
    const videoUrl = `https://youtu.be/${info.id}`;
    const tempFile = path.join(__dirname, `${info.id}.webm`);
    const thumbBase = path.join(__dirname, info.id);
    const thumbFile = `${thumbBase}.jpg`;

    let infoMessage;
    try {
      infoMessage = await ctx.reply(
        `<b>🎵 Music Info</b>\n<blockquote><b>📝 Title:</b>\n-> <code>${escapeHTML(title)}</code>\n<b>👤 Artist:</b>\n-> <code>${escapeHTML(performer)}</code>\n<b>⏳ Duration:</b>\n-> <code>${duration}</code>\n<b>📦 Status:</b>\n-> <code>Downloading</code></blockquote>`,
        { parse_mode: "HTML", reply_to_message_id: ctx.message?.message_id, message_thread_id: threadId }
      );
    } catch (err) { return; }

    try {
      await execFilePromise("yt-dlp", ["-f", "251", "-o", tempFile, videoUrl]);
      await execFilePromise("yt-dlp", ["--skip-download", "--write-thumbnail", "--convert-thumbnails", "jpg", "-o", thumbBase, videoUrl]);

      await updateMusicStatus(ctx, infoMessage.message_id, { title, performer, duration, status: "Sending" });

      const audioOptions = {
        title,
        performer,
        duration: info.duration,
        message_thread_id: ALLOWED_TOPIC_ID_MUSIC,
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
    }
  } catch (e) {
    return safeReply(ctx, "❌ Failed to fetch music.");
  }
});

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

  exec("termux-volume", async (error, stdout) => {
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
const GEMINI_VISION_MODEL = process.env.GEMINI_VISION_MODEL || "";
const TOPIC_ID_INTAKE = process.env.TOPIC_ID_INTAKE  ? parseInt(process.env.TOPIC_ID_INTAKE,  10) : 416182;
const TOPIC_ID_PREVIEW = process.env.TOPIC_ID_PREVIEW ? parseInt(process.env.TOPIC_ID_PREVIEW, 10) : 468585;

function rupiah(num) {
  return num.toString().replace(/\B(?=(\d{3})+(?!\d))/g, ".");
}

function padC(text, W) {
  if (text.length >= W) return text;
  return " ".repeat(Math.floor((W - text.length) / 2)) + text;
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
    `ID PEL     : ${data.idPel || "-"}`,
    `NO METER   : ${data.noMeter || "-"}`,
    `NAMA       : ${data.nama || "-"}`,
    `TARIF/DAYA : ${data.tarif || "-"}`,
    `JML KWH    : ${data.jmlKwh || "-"}`,
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
    `TUJUAN   : ${data.bankTujuan || "-"}`,
    `NOMOR    : ${data.noRekening || "-"}`,
    `PENERIMA : ${data.penerima || "-"}`,
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
    `TUJUAN   : ${data.providerTujuan || "-"}`,
    `NOMOR    : ${data.noProvider || "-"}`,
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

async function buildStrukImage(struk) {
  const id = Date.now();
  const files = {
    nameTxt:    `struk_name_${id}.txt`,
    addrTxt:    `struk_addr_${id}.txt`,
    bodyTxt:    `struk_body_${id}.txt`,
    namePng:    `struk_name_${id}.png`,
    addrPng:    `struk_addr_${id}.png`,
    bodyPng:    `struk_body_${id}.png`,
    nameExtPng: `struk_namex_${id}.png`,
    addrExtPng: `struk_addrx_${id}.png`,
    outPng:     `struk_${id}.png`,
  };

  const lines       = struk.split('\n');
  const nameLine    = (lines[0] || '').trim();
  const addrLine    = (lines[1] || '').trim();
  const bodyContent = lines.slice(2).join('\n');

  fs.writeFileSync(files.nameTxt, nameLine);
  fs.writeFileSync(files.addrTxt, addrLine);
  fs.writeFileSync(files.bodyTxt, bodyContent);

  const normalSize = parseInt(STRUK_FONTSIZE, 10);
  const smallSize  = Math.max(9, normalSize - 4);
  const base       = `-background white -fill black -font "${STRUK_FONT}"`;

  const getDim = async (file, fmt) => {
    try {
      const out = await execPromise(`magick "${file}" -format "${fmt}" info:`);
      return parseInt(out.trim(), 10) || 0;
    } catch { return 0; }
  };

  try {
    await execPromise(`magick ${base} -pointsize ${normalSize} label:@${files.nameTxt} "${files.namePng}"`);
    await execPromise(`magick ${base} -pointsize ${smallSize}  label:@${files.addrTxt} "${files.addrPng}"`);
    await execPromise(`magick ${base} -pointsize ${normalSize} label:@${files.bodyTxt} "${files.bodyPng}"`);

    const bodyW = (await getDim(files.bodyPng, '%w')) || 350;
    const nameH = (await getDim(files.namePng, '%h')) || 40;
    const addrH = (await getDim(files.addrPng, '%h')) || 30;

    await execPromise(
      `magick -size ${bodyW}x${nameH} xc:white` +
      ` "${files.namePng}" -gravity Center -composite "${files.nameExtPng}"`
    );
    await execPromise(
      `magick -size ${bodyW}x${addrH} xc:white` +
      ` "${files.addrPng}" -gravity Center -composite "${files.addrExtPng}"`
    );

    await execPromise(
      `magick "${files.nameExtPng}" "${files.addrExtPng}" "${files.bodyPng}"` +
      ` -append -bordercolor white -border 25x20 "${files.outPng}"`
    );
  } finally {
    ['nameTxt','addrTxt','bodyTxt','namePng','addrPng','bodyPng','nameExtPng','addrExtPng']
      .forEach(k => { try { fs.unlinkSync(files[k]); } catch {} });
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

async function analyzeWithGemini(base64Image) {
  const res = await fetch(
    `https://generativelanguage.googleapis.com/v1beta/models/${process.env.GEMINI_VISION_MODEL}:generateContent?key=${process.env.GEMINI_API_KEY}`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        contents: [{
          parts: [
            { inline_data: { mime_type: "image/jpeg", data: base64Image } },
            { text: RECEIPT_PROMPT },
          ],
        }],
      }),
    }
  );

  if (!res.ok) throw new Error(`Gemini HTTP ${res.status}`);

  const data = await res.json();
  const raw = data?.candidates?.[0]?.content?.parts?.[0]?.text ?? "{}";
  return JSON.parse(raw.replace(/```[\w]*\n?|```/g, "").trim());
}

async function analyzeWithGroq(base64Image) {
  const res = await fetch("https://api.groq.com/openai/v1/chat/completions", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Authorization: `Bearer ${process.env.GROQ_API_KEY}`,
    },
    body: JSON.stringify({
      model: process.env.GROQ_VISION_MODEL ?? "meta-llama/llama-4-scout-17b-16e-instruct",
      messages: [{
        role: "user",
        content: [
          { type: "image_url", image_url: { url: `data:image/jpeg;base64,${base64Image}` } },
          { type: "text", text: RECEIPT_PROMPT },
        ],
      }],
      max_tokens: 1000,
    }),
  });

  if (!res.ok) throw new Error(`Groq HTTP ${res.status}`);

  const data = await res.json();
  const raw = data?.choices?.[0]?.message?.content ?? "{}";
  return JSON.parse(raw.replace(/```[\w]*\n?|```/g, "").trim());
}

async function analyzeReceipt(base64Image) {
  try {
    const result = await analyzeWithGemini(base64Image);
    appendLogLine("Vision: Gemini OK");
    return result;
  } catch (err) {
    appendLogLine(`Vision: Gemini failed (${err.message}), trying Groq…`);
  }

  try {
    const result = await analyzeWithGroq(base64Image);
    appendLogLine("Vision: Groq OK");
    return result;
  } catch (err) {
    throw new Error(`Both providers failed — Groq: ${err.message}`);
  }
}

bot.on("photo", adminOnly, async (ctx) => {
  const currentThreadId = ctx.message?.message_thread_id ?? null;

  if (TOPIC_ID_INTAKE !== null && currentThreadId !== TOPIC_ID_INTAKE) return;

  const fileId = ctx.message.photo[ctx.message.photo.length - 1].file_id;

  try {
    const file = await ctx.telegram.getFile(fileId);
    const fileUrl = `https://api.telegram.org/file/bot${process.env.BOT_TOKEN}/${file.file_path}`;
    const imgRes = await fetch(fileUrl);
    const base64Image = Buffer.from(await imgRes.arrayBuffer()).toString("base64");

    let parsed;
    try {
      parsed = await analyzeReceipt(base64Image);
    } catch (err) {
      appendLogLine(`Receipt analysis error: ${err.message}`);
      return;
    }

    const isValid =
      (parsed.type === "pln"      && parsed.token && parsed.token.replace(/\D/g, "").length >= 10) ||
      (parsed.type === "transfer" && (parsed.noRekening || parsed.bankTujuan)) ||
      (parsed.type === "pulsa"    && (parsed.providerTujuan || parsed.noProvider));

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
        parsed.nominal.replace(/[^\d]/g, '')
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
      const targetThreadId = TOPIC_ID_PREVIEW !== null ? TOPIC_ID_PREVIEW : currentThreadId;
      const sendOptions = targetThreadId ? { message_thread_id: targetThreadId } : {};

      await ctx.telegram.sendPhoto(
        ctx.chat.id,
        { source: imgBuffer, filename: "receipt.png" },
        sendOptions
      );
      try { fs.unlinkSync(imgPath); } catch {}
    }

    const plainText = strukLines + "\n\n\n";
    const tmpFile = `struk_print_${ctx.message?.message_id}.txt`;

    try {
      fs.writeFileSync(tmpFile, plainText);
      const printCmd = process.env.PRINT_CMD;
      if (!printCmd) {
        appendLogLine("Print error: PRINT_CMD is not configured in .env.");
        return;
      }
      exec(`${printCmd} "${tmpFile}"`, (err) => {
        if (err) appendLogLine(`Print execution error: ${err.message}`);
      });
    } catch (err) {
      appendLogLine(`Print setup error: ${err.message}`);
    } finally {
      setTimeout(() => {
        try { if (fs.existsSync(tmpFile)) fs.unlinkSync(tmpFile); } catch {}
      }, 1000);
    }

  } catch (err) {
    appendLogLine(`Receipt process error: ${err.message}`);
  }
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

bot.hears(/^\/price(?:@\w+)?(?:\s+(.+))?$/i, topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  const keyword = (ctx.match[1] || "").trim().toLowerCase();

  if (!keyword) {
    return safeReply(ctx, "❌ Please provide an item name.", {
      parse_mode: "HTML",
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  }

  try {
    const rows = await fetchPriceList();
    const dataRows = rows.slice(1).filter((row) => row[0]?.replace(/\r/, ""));

    if (!dataRows.length) {
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

      const item = `<blockquote>Name : ${name}\nContents : ${contents}\nPrice : ${price}</blockquote>\n`;

      if (current.length + item.length > 3800) {
        chunks.push(current.trim());
        current = item;
      } else {
        current += item;
      }
    }
    if (current.trim()) chunks.push(current.trim());

    for (const chunk of chunks) {
      await safeReply(ctx, chunk, {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }
  } catch (err) {
    appendLogLine(`Price fetch error: ${err.message}`);
    return safeReply(ctx, "❌ Failed to fetch price list.", {
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  }
});

// ================== HELP COMMAND ==================
bot.hears(/^\/(\w+)/, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  if (threadId !== ALLOWED_TOPIC_ID) return;

  return safeReply(
    ctx,
      "<b>🤖 Public Commands</b>\n" +
      "<blockquote><b>✹ Ask AI</b>\n" +
      "-> <code>/ask [prompt]</code></blockquote>\n" +
      "<b>🤖 Admin Commands</b>\n" +
      "<blockquote><b>✹ Battery Status</b>\n" +
      "-> /battery</blockquote>\n" +
      "<blockquote><b>✹ Download Music</b>\n" +
      "-> <code>/music [title]</code></blockquote>\n" +
      "<blockquote><b>✹ Data Usage</b>\n" +
      "-> /usage</blockquote>\n" +
      "<blockquote><b>✹ Playback Controller</b>\n" +
      "-> /playback</blockquote>\n" +
      "<blockquote><b>✹ Price List</b>\n" +
      "-> <code>/price [name]</code></blockquote>\n" +
      "<blockquote><b>✹ Reboot Device</b>\n" +
      "-> /reboot</blockquote>\n" +
      "<blockquote><b>✹ Speed Test</b>\n" +
      "-> /speed</blockquote>",
    { parse_mode: "HTML", reply_to_message_id: ctx.message?.message_id, message_thread_id: threadId }
  );
});

// ================== MUTE ==================
bot.on("text", async (ctx, next) => {
  const threadId = ctx.message?.message_thread_id;
  const text = ctx.message?.text?.trim() || "";
  const chatId = ctx.chat?.id;
  const userId = ctx.from?.id;

  if (threadId !== ALLOWED_TOPIC_ID) return next();

  const session = buySessions[chatId];
  if (session && session.process) {
    const replied = ctx.message?.reply_to_message;
    if (replied && replied.from?.id === ctx.botInfo.id) {
      try {
        const admins = await ctx.telegram.getChatAdministrators(chatId);
        const isAdmin = admins.some((a) => a.user.id === userId);
        if (!isAdmin) return;

        if (text && !text.startsWith("/")) {
          session.process.stdin.write(text + "\n");
          resetTimeout(chatId, ctx);
          return;
        }
      } catch (err) {
        console.error("buy-forwarder error:", err);
      }
    }
  }

  if (!text.startsWith("/")) {
    try {
      const admins = await ctx.telegram.getChatAdministrators(chatId);
      const isAdmin = admins.some((admin) => admin.user.id === userId);

      if (!isAdmin) {
        const untilDate = Math.floor(Date.now() / 1000) + 5 * 60; // 5 min
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

// ================== REPOSITORY UPDATE ==================
const runRepo = require('./repo.js');

async function loop() {
  try {
    await runRepo();
  } catch (err) {
    console.error(`❌ Error: ${err.message}`);
    appendLogLine(`Repo Error: ${err.message}`);
  }
  setTimeout(loop, 10000);
}

loop();

// ================== MONITOR STAT ==================
const startMonitor = require("./monitor.js");

startMonitor({
  bot,
  safeSend,
  appendLogLine,
  ALLOWED_TOPIC_ID_USAGE,
  ALLOWED_TOPIC_ID_STATUS,
  ALLOWED_CHAT_ID,
});

const { NotificationMonitor, CONFIG, jsonPath } = require('./notification.js');

const Notmonitor = new NotificationMonitor(CONFIG, jsonPath);
Notmonitor.monitor();
