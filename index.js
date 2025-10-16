// bot.js
require("dotenv").config();
const fs = require("fs");
const { Telegraf } = require("telegraf");
const cron = require("node-cron");
const { spawn, exec } = require("child_process");
const escapeHTML = require("escape-html");
const https = require("https");
const dns = require("dns");
const path = require("path");

// dynamic import for node-fetch
const fetch = (...args) => import("node-fetch").then(({ default: fetch }) => fetch(...args));

// Logging
const LOG_FILE = "bot.log";
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

const ALLOWED_TOPIC_ID = parseInt(process.env.ALLOWED_TOPIC_ID || "0");
const ALLOWED_TOPIC_ID_USAGE = parseInt(process.env.ALLOWED_TOPIC_ID_USAGE || "0");
const ALLOWED_TOPIC_ID_MUSIC = parseInt(process.env.ALLOWED_TOPIC_ID_MUSIC || "0");

// Markdown to HTML converter
function convertMarkdownToHTML(mdText) {
  if (!mdText) return '';

  const fenceRe = /```([a-zA-Z0-9+-]*)\n([\s\S]*?)```/g;

  let lastIndex = 0;
  let parts = [];
  let match;

  while ((match = fenceRe.exec(mdText)) !== null) {
    const idx = match.index;
    const lang = match[1] || '';
    const code = match[2];

    if (idx > lastIndex) {
      let preceding = mdText.slice(lastIndex, idx);

      preceding = preceding.replace(/^(\s*\d+)\.\s{2,}/gm, '$1. ');

      preceding = escapeHTML(preceding)
        .replace(/\*\*(.*?)\*\*/g, '<b>$1</b>')
        .replace(/\*([^*]+)\*/g, '<i>$1</i>')
        .replace(/`([^`]+)`/g, '<code>$1</code>');
      parts.push(preceding);
    }

    const escapedCode = escapeHTML(code);
    const classAttr = lang ? ` class="language-${escapeHTML(lang)}"` : '';
    parts.push(`<pre><code${classAttr}>${escapedCode}</code></pre>`);

    lastIndex = fenceRe.lastIndex;
  }

  if (lastIndex < mdText.length) {
    let trailing = mdText.slice(lastIndex);

    trailing = trailing.replace(/^(\s*\d+)\.\s{2,}/gm, '$1. ');

    trailing = escapeHTML(trailing)
      .replace(/\*\*(.*?)\*\*/g, '<b>$1</b>')   
      .replace(/\*([^*]+)\*/g, '<i>$1</i>')     
      .replace(/`([^`]+)`/g, '<code>$1</code>'); 
    parts.push(trailing);
  }

  return parts.join('');
}

// Convert country code -> flag emoji
function countryFlag(countryCode) {
  if (!countryCode) return "";
  return countryCode
    .toUpperCase()
    .replace(/./g, (char) =>
      String.fromCodePoint(127397 + char.charCodeAt())
    );
}

// Simple domain validator
function isValidDomain(domain) {
  return /^[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}$/.test(domain);
}

// Extract domain from text (ignore /ip etc.)
function extractDomain(text) {
  if (!text) return null;
  const match = text.match(/([a-zA-Z0-9.-]+\.[a-zA-Z]{2,})/);
  return match ? match[1] : null;
}

// Get geo info from ipinfo.io
function getGeo(ip) {
  return new Promise((resolve) => {
    https.get(`https://ipinfo.io/${ip}/json`, (res) => {
      let data = "";
      res.on("data", (chunk) => (data += chunk));
      res.on("end", () => {
        try {
          const json = JSON.parse(data);
          if (json && json.ip) {
            const flag = countryFlag(json.country);
            const loc = [json.country, json.region, json.city].filter(Boolean).join(", ");
            resolve(`<i>${loc || "Unknown location"} ${flag}</i>`);
          } else {
            resolve("<i>Unknown location</i>");
          }
        } catch {
          resolve("<i>Unknown location</i>");
        }
      });
    }).on("error", () => resolve("<i>Unknown location</i>"));
  });
}

// Validate IPv4 or IPv6
function isValidIP(ip) {
  return /^(25[0-5]|2[0-4]\d|[01]?\d\d?)(\.(25[0-5]|2[0-4]\d|[01]?\d\d?)){3}$/.test(ip) ||  // IPv4
         /^([0-9a-fA-F]{0,4}:){2,7}[0-9a-fA-F]{0,4}$/.test(ip); // IPv6
}

// Extract IP from text (ignore /domain etc.)
function extractIP(text) {
  if (!text) return null;
  const match = text.match(/((?:\d{1,3}\.){3}\d{1,3}|(?:[0-9a-fA-F]{0,4}:){2,7}[0-9a-fA-F]{0,4})/);
  return match ? match[1] : null;
}

// ==========================
// Safe wrappers for Telegram API calls
// ==========================
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

async function sendUsageReport() {
  return new Promise((resolve) => {
    const vnstat = spawn("vnstat", ["--json"]);
    let output = "";
    let errOutput = "";

    vnstat.stdout.on("data", (data) => (output += data.toString()));
    vnstat.stderr.on("data", (data) => (errOutput += data.toString()));

    vnstat.on("close", async (code) => {
      if (code !== 0 || errOutput) {
        appendLogLine(`Daily usage vnstat failed: ${errOutput || `code ${code}`}`);
        await safeSend(bot, process.env.ALLOWED_CHAT_ID, "❌ Failed to get daily usage", {
          message_thread_id: ALLOWED_TOPIC_ID_USAGE,
        });
        return resolve();
      }

      try {
        const data = JSON.parse(output);
        const ifaceData = data.interfaces
          .map((i) => ({ name: i.name, total: (i.traffic?.total?.rx || 0) + (i.traffic?.total?.tx || 0) }))
          .sort((a, b) => b.total - a.total)[0];

        const iface = ifaceData?.name || "unknown";
        const traffic = data.interfaces.find((i) => i.name === iface)?.traffic;

        const formatBytes = (kB) => {
          if (kB < 1024 * 1024) return `${kB.toFixed(2)} KB`;
          if (kB < 1024 * 1024 * 1024) return `${(kB / 1024 / 1024).toFixed(2)} MB`;
          if (kB < 1024 * 1024 * 1024 * 1024) return `${(kB / 1024 / 1024 / 1024).toFixed(2)} GB`;
          return `${(kB / 1024 / 1024 / 1024 / 1024).toFixed(2)} TB`;
        };

        const extractTraffic = (obj) => {
          if (!obj) return null;
          const up = obj.tx || 0, down = obj.rx || 0;
          return { up: formatBytes(up), down: formatBytes(down), total: formatBytes(up + down) };
        };

        const todayData = extractTraffic(traffic?.day?.at(-1));

        const usageReport = `<b>📊 Daily Usage Report (${iface})</b>\n<blockquote>` +
          (todayData ? `📅 <b>Today</b>\nUpload : ${todayData.up}\nDownload : ${todayData.down}\nTotal : ${todayData.total}` : "No data") +
          `</blockquote>`;

        await safeSend(bot, process.env.ALLOWED_CHAT_ID, usageReport.trim(), {
          parse_mode: "HTML",
          message_thread_id: ALLOWED_TOPIC_ID_USAGE,
        });
      } catch (parseError) {
        appendLogLine(`Daily usage parse failed: ${parseError.message}`);
        await safeSend(bot, process.env.ALLOWED_CHAT_ID, "❌ Failed to parse daily usage", {
          message_thread_id: ALLOWED_TOPIC_ID_USAGE,
        });
      }
      resolve();
    });
  });
}

// ==========================
// Middleware & Admin check
// ==========================
bot.use(async (ctx, next) => {
  const user = ctx.from?.username || ctx.from?.id || "unknown";
  const msg = ctx.message?.text || "<non-text>";
  const line = `From ${user}: ${msg}`;
  logStream.write(`[${new Date().toISOString()}] ${line}\n`);
  await next();
});

// topicOnly middleware
const topicOnly = (ctx, next) => {
  const threadId = ctx.message?.message_thread_id || ctx.callbackQuery?.message?.message_thread_id;
  if (threadId !== ALLOWED_TOPIC_ID) return;
  return next();
};

const adminOnly = async (ctx, next) => {
  try {
    const userId = ctx.from?.id;
    const firstName = ctx.from?.first_name || "User";
    const chatId =
      ctx.chat?.id ||
      ctx.message?.chat?.id ||
      ctx.callbackQuery?.message?.chat?.id;

    const replyToMsgId =
      ctx.message?.message_id || ctx.callbackQuery?.message?.message_id;

    if (!userId || !chatId) {
      return safeReply(ctx, "❌ Permission error", {
        reply_to_message_id: replyToMsgId,
      });
    }

    const admins = await ctx.telegram.getChatAdministrators(chatId);
    const isAdmin = admins.some((admin) => admin.user.id === userId);

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
        await ctx.answerCbQuery(
          "⛔ Not authorized. You are muted for 5 minutes.",
          { show_alert: true }
        );
        return;
      }

      return safeReply(ctx, "⛔ Not authorized. You are muted for 5 minutes.", {
        reply_to_message_id: replyToMsgId,
      });
    }

    return next();
  } catch (err) {
    appendLogLine(`adminOnly error: ${err.message}`);
    return safeReply(ctx, "⚠️ Permission check failed.", {
      reply_to_message_id:
        ctx.message?.message_id || ctx.callbackQuery?.message?.message_id,
    });
  }
};

// ==========================
// Commands
// ==========================

// /speed
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

// /reboot
bot.hears(/^\/reboot/i, topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  await safeReply(ctx, "♻️ Rebooting system...", {
    reply_to_message_id: ctx.message?.message_id,
    message_thread_id: threadId,
  });

  exec("am force-stop com.termux", (error) => {
    if (error) {
      appendLogLine(`Force stop Termux failed: ${error.message}`);
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
  });
});

// /battery
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

// /ask
bot.command("ask", async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  if (threadId !== ALLOWED_TOPIC_ID) return;

  const text = ctx.message.text?.trim();
  let argPrompt = text.replace(/^\/ask(@\w+)?\s*/i, "").trim();
  let replyText = ctx.message.reply_to_message?.text?.trim();

  let prompt = "";

  if (argPrompt && replyText) {
    prompt = `${argPrompt}: ${replyText}`;
  } else if (argPrompt) {
    prompt = argPrompt;
  } else if (replyText) {
    prompt = replyText;
  }

  if (!prompt) {
    return safeReply(
      ctx,
      "❌ Please provide a prompt.",
      {
        reply_to_message_id: ctx.message?.message_id,
        parse_mode: "HTML",
        message_thread_id: threadId,
      }
    );
  }

  try {
    const res = await fetch(
      `https://generativelanguage.googleapis.com/v1beta/models/${process.env.GEMINI_API_MODEL}:generateContent?key=${process.env.GEMINI_API_KEY}`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ contents: [{ parts: [{ text: prompt }] }] }),
      }
    );

    const data = await res.json();
    const rawText =
      data?.candidates?.[0]?.content?.parts?.[0]?.text || "❌ No response.";
    const MAX_LENGTH = 3500;
    const parts =
      rawText.match(new RegExp(`.{1,${MAX_LENGTH}}`, "gs")) || [rawText];

    for (let i = 0; i < parts.length; i++) {
      const html = convertMarkdownToHTML(parts[i]);
      await safeReply(ctx, html, {
        reply_to_message_id: ctx.message?.message_id,
        parse_mode: "HTML",
        message_thread_id: threadId,
      });
    }
  } catch (err) {
    appendLogLine(`Gemini Error: ${err.message}`);
    await safeReply(ctx, "❌ Failed to process request", {
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  }
});

// /usage
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
      const ifaceData = data.interfaces
        .map((i) => ({ name: i.name, total: (i.traffic?.total?.rx || 0) + (i.traffic?.total?.tx || 0) }))
        .sort((a, b) => b.total - a.total)[0];

      const iface = ifaceData?.name || "unknown";
      const traffic = data.interfaces.find((i) => i.name === iface)?.traffic;

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

// /update
bot.hears(/^\/update/i, topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  await safeReply(ctx, "🦠 Choose a component to update:", {
    reply_to_message_id: ctx.message?.message_id,
    message_thread_id: threadId,
    reply_markup: {
      inline_keyboard: [
        [
          { text: "☘️ Clash", callback_data: "update_mihomo" },
          { text: "🍀 Smart", callback_data: "update_smart" },
        ],
        [
          { text: "🌱 Sing-box", callback_data: "update_sing-box" },
          { text: "🍃 Geo", callback_data: "update_geo" }
        ],
        [{ text: "🌿 Dashboard", callback_data: "update_dashboard" }],
      ],
    },
  });
});

// Unified handler for all update actions
bot.action(/^update_/, topicOnly, adminOnly, async (ctx) => {
  const data = ctx.callbackQuery?.data;
  const cbMsg = ctx.callbackQuery?.message;
  const threadId = cbMsg?.message_thread_id;
  const replyTargetId = cbMsg?.reply_to_message?.message_id;

  const scriptMap = {
    update_mihomo: "scripts/update-mihomo.sh",
    update_smart: "scripts/update-smart.sh",
    "update_sing-box": "scripts/update-sing-box.sh",
    update_dashboard: "scripts/update-dashboard.sh",
    update_geo: "/data/adb/box/scripts/box.tool geox"
  };

  const scriptPath = scriptMap[data];
  if (!scriptPath) return safeAnswerCbQuery(ctx, "❌ Invalid selection!");

  const rawName = data.replace("update_", "");
  const targetName = rawName.charAt(0).toUpperCase() + rawName.slice(1);
  await safeEditMessageText(ctx, `🚀 Updating ${targetName}...`);

  exec(`su -c ${scriptPath}`, async (error) => {
    if (error) {
      appendLogLine(`Update ${targetName} failed: ${error.message}`);
      return safeSend(bot, cbMsg.chat.id, `❌ Failed to update ${targetName}`, {
        reply_to_message_id: replyTargetId,
        message_thread_id: threadId,
      });
    }
    await safeSend(bot, cbMsg.chat.id, `✅ ${targetName} updated successfully!`, {
      reply_to_message_id: replyTargetId,
      message_thread_id: threadId,
    });
  });

  await safeAnswerCbQuery(ctx, "⏳ Updating...");
});

// /ip
bot.hears(/^\/ip(?:@[\w_]+)?(?:\s+(.+))?/i, topicOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;

  let domain = ctx.match[1]?.trim();

  // If /ip@BotUsername → ignore
  if (domain && domain.startsWith("@")) domain = null;

  // If no domain given, check reply
  if (!domain && ctx.message?.reply_to_message?.text) {
    const candidate = extractDomain(ctx.message.reply_to_message.text);
    if (candidate && isValidDomain(candidate)) {
      domain = candidate;
    }
  }

  // Still empty → ask user
  if (!domain) {
    return safeReply(ctx,
      "❌ Please provide a domain.",
      {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      }
    );
  }

  // Validate domain
  if (!isValidDomain(domain)) {
    return safeReply(ctx, `❌ <code>${domain}</code> is not a valid domain.`, {
      parse_mode: "HTML",
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  }

  try {
    const resolve4 = () =>
      new Promise((resolve) => dns.resolve4(domain, (err, addresses) => resolve(err ? [] : addresses)));
    const resolve6 = () =>
      new Promise((resolve) => dns.resolve6(domain, (err, addresses) => resolve(err ? [] : addresses)));

    const [ipv4, ipv6] = await Promise.all([resolve4(), resolve6()]);

    if (ipv4.length === 0 && ipv6.length === 0) {
      return safeReply(ctx, `❌ No IP address found for <code>${domain}</code>`, {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }

    const replyParts = [`<b>🌐 Domain:</b>\n➤ <code>${domain}</code>\n`];

    // IPv4
    if (ipv4.length) {
      replyParts.push("🔹 <b>IPv4:</b>");
      for (const ip of ipv4) {
        const loc = await getGeo(ip);
        replyParts.push(`-> <code>${ip}</code>\n📍 ${loc}`);
      }
    }

    // IPv6
    if (ipv6.length) {
      replyParts.push("\n🔹 <b>IPv6:</b>");
      for (const ip of ipv6) {
        const loc = await getGeo(ip);
        replyParts.push(`-> <code>${ip}</code>\n📍 ${loc}`);
      }
    }

    return safeReply(ctx, replyParts.join("\n"), {
      parse_mode: "HTML",
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  } catch (err) {
    appendLogLine(`IP lookup failed for ${domain}: ${err.message}`);
    return safeReply(ctx, `❌ Failed to resolve domain: <code>${domain}</code>`, {
      parse_mode: "HTML",
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  }
});

//domain
bot.hears(/^\/domain(?:@[\w_]+)?(?:\s+(.+))?/i, topicOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;

  let ip = ctx.match[1]?.trim();

  // If no IP given, check reply
  if (!ip && ctx.message?.reply_to_message?.text) {
    const candidate = extractIP(ctx.message.reply_to_message.text);
    if (candidate && isValidIP(candidate)) ip = candidate;
  }

  // Still empty
  if (!ip) {
    return safeReply(ctx,
      "❌ Please provide an IP address.",
      {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      }
    );
  }

  // Validate IP
  if (!isValidIP(ip)) {
    return safeReply(ctx, `❌ <code>${ip}</code> is not a valid IP address.`, {
      parse_mode: "HTML",
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  }

  try {
    const hostnames = await new Promise((resolve) => {
      dns.reverse(ip, (err, domains) => resolve(err ? [] : domains));
    });

    if (!hostnames.length) {
      return safeReply(ctx, `❌ No domain found for <code>${ip}</code>`, {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }

    const replyParts = [`<b>🔎 IP:</b>\n➤ <code>${ip}</code>\n`];
    replyParts.push("🌐 Domain:");
    for (const host of hostnames) {
      replyParts.push(`-> <code>${host}</code>`);
    }

    return safeReply(ctx, replyParts.join("\n"), {
      parse_mode: "HTML",
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  } catch (err) {
    appendLogLine(`Reverse DNS lookup failed for ${ip}: ${err.message}`);
    return safeReply(ctx, `❌ Failed to resolve IP: <code>${ip}</code>`, {
      parse_mode: "HTML",
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  }
});

// ================== MUSIC STATE ==================
let musicQueue = [];
let isPlaying = false;
let currentTrack = null;
let currentProcess = null;
let stoppedManually = false;
let nowPlayingMessageId = null;

function musicControls(queueLength) {
  if (queueLength > 0) {
    return {
      inline_keyboard: [
        [
          { text: "⏭ Skip", callback_data: "music_skip" },
          { text: "⏹ Stop", callback_data: "music_stop" },
        ],
      ],
    };
  } else {
    return {
      inline_keyboard: [[{ text: "⏹ Stop", callback_data: "music_stop" }]],
    };
  }
}

async function updateNowPlaying(ctx, threadId, replyTo, forcedStop = false) {
  let text = "";

  if (isPlaying && currentTrack) {
    text += `<b>🎵 Now playing:</b>\n-> <code>${currentTrack.performer} - ${currentTrack.title}</code>\n<b>⏳ Duration:</b>\n-> ${currentTrack.duration}\n`;
  } else if (!isPlaying && musicQueue.length === 0) {
    text += forcedStop ? "⏹ Music stopped." : "❌ Please provide a song title or YouTube link.";

    if (nowPlayingMessageId) {
      try {
        await ctx.deleteMessage(nowPlayingMessageId);
      } catch (e) {
        appendLogLine(`Failed to delete now playing message: ${e.message}`);
      }
      nowPlayingMessageId = null;
    }

    return ctx.reply(text, {
      reply_to_message_id: replyTo,
      message_thread_id: threadId,
      parse_mode: "HTML",
    });
  }

  if (musicQueue.length > 0) {
    text += "<b>📜 Queue:</b>\n<blockquote>";
    musicQueue.slice(0, 5).forEach((song, i) => {
      text += `${i + 1}. ${song.title}\n`;
    });
    if (musicQueue.length > 5) {
      text += `...and ${musicQueue.length - 5} more`;
    }
    text += "</blockquote>";
  }

  const replyMarkup = isPlaying ? musicControls(musicQueue.length) : undefined;

  if (nowPlayingMessageId) {
    try {
      await ctx.deleteMessage(nowPlayingMessageId);
    } catch (e) {
      appendLogLine(`Failed to delete now playing message: ${e.message}`);
    }
  }

  const message = await ctx.reply(text.trim(), {
    reply_to_message_id: replyTo,
    message_thread_id: threadId,
    parse_mode: "HTML",
    reply_markup: replyMarkup,
  });

  nowPlayingMessageId = message.message_id;
}

function formatDuration(seconds) {
  if (!seconds || isNaN(seconds)) return "??:??";
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
  return `${m}:${s.toString().padStart(2, "0")}`;
}

// ================== PLAY NEXT ==================
function playNext(ctx, threadId, replyTo) {
  if (musicQueue.length === 0) {
    isPlaying = false;
    currentTrack = null;
    currentProcess = null;
    updateNowPlaying(ctx, threadId, replyTo, true);
    return;
  }

  const next = musicQueue.shift();
  const videoUrl = `https://youtu.be/${next.id}`;
  const tempFile = path.join(__dirname, `${next.id}.webm`);

  exec(`yt-dlp -f 251 -j "${videoUrl}"`, (err, stdout) => {
    if (err || !stdout) {
      appendLogLine(`Failed to get video info for ${videoUrl}: ${err?.message || 'No output'}`);
      return playNext(ctx, threadId, replyTo);
    }

    let info;
    try {
      info = JSON.parse(stdout);
    } catch (e) {
      appendLogLine(`Failed to parse video info JSON: ${e.message}`);
      return playNext(ctx, threadId, replyTo);
    }

    const title = info.track || info.title || "Unknown Title";
    const performer = info.artist || info.uploader || "Unknown Artist";
    const duration = formatDuration(info.duration);
    const audioUrl = info.url;

    currentTrack = { title, performer, duration };

    if (currentProcess) currentProcess.kill("SIGKILL");

    currentProcess = spawn("mpv", ["--no-video", audioUrl]);

    currentProcess.on("exit", () => {
      const wasStopped = stoppedManually;
      stoppedManually = false;
      currentProcess = null;
      if (!wasStopped) playNext(ctx, threadId, replyTo);
    });

    isPlaying = true;
    updateNowPlaying(ctx, threadId, replyTo);

    exec(`yt-dlp -f 251 -o "${tempFile}" "${videoUrl}"`, async (err2) => {
      if (err2) {
        appendLogLine(`Failed to download audio for ${videoUrl}: ${err2.message}`);
        return;
      }
      
      try {
        await ctx.replyWithAudio(
          { source: tempFile },
          {
            title,
            performer,
            duration: info.duration,
            message_thread_id: ALLOWED_TOPIC_ID_MUSIC,
          }
        );
      } catch (e) {
        appendLogLine(`Failed to send audio: ${e.message}`);
      } finally {
        fs.unlink(tempFile, (unlinkErr) => {
          if (unlinkErr) {
            appendLogLine(`Failed to delete temp file ${tempFile}: ${unlinkErr.message}`);
          }
        });
      }
    });
  });
}

// ================== COMMAND ==================
bot.command("music", topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  if (threadId !== ALLOWED_TOPIC_ID) return;

  const text = ctx.message.text?.trim();
  const query = text.replace(/^\/music(@\w+)?\s*/i, "").trim();

  if (!query) {
    await updateNowPlaying(ctx, threadId, ctx.message?.message_id);
    return;
  }

  let command;
  if (/^https?:\/\/(www\.)?youtube\.com\/playlist\?/.test(query)) {
    command = `yt-dlp --flat-playlist --get-id --get-title "${query}"`;
  } else if (query.startsWith("http")) {
    command = `yt-dlp --get-id --get-title "${query}"`;
  } else {
    command = `yt-dlp --get-id --get-title "ytsearch1:${query}"`;
  }

  exec(command, async (err, stdout) => {
    if (err || !stdout) {
      appendLogLine(`Failed to execute yt-dlp command: ${err?.message || 'No output'}`);
      return ctx.reply("❌ Failed to add music.", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }

    const lines = stdout.trim().split("\n");
    const ids = [];
    for (let i = 0; i < lines.length; i += 2) {
      if (lines[i + 1]) {
        ids.push({ title: lines[i], id: lines[i + 1] });
      }
    }

    if (ids.length === 0) {
      return ctx.reply("❌ No results found.", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }

    musicQueue.push(...ids);

    if (!isPlaying) {
      playNext(ctx, threadId, ctx.message?.message_id);
    } else {
      updateNowPlaying(ctx, threadId, ctx.message?.message_id);
    }
  });
});

// ================== INLINE CONTROLS ==================
bot.action(/^music_/, topicOnly, adminOnly, async (ctx) => {
  const data = ctx.callbackQuery?.data;
  const threadId = ctx.callbackQuery?.message?.message_thread_id;
  const chatId = ctx.callbackQuery?.message?.chat.id;
  const messageId = ctx.callbackQuery?.message?.message_id;

  if (data === "music_skip") {
    if (currentProcess && isPlaying) {
      await ctx.answerCbQuery("⏭ Skipping...");
      currentProcess.kill("SIGKILL");
      return;
    }
    return ctx.answerCbQuery("⚠️ Nothing to skip.");
  }

  if (data === "music_stop") {
    if (isPlaying || musicQueue.length > 0) {
      await ctx.answerCbQuery("⏹ Stopping...");

      stoppedManually = true;
      if (currentProcess) {
        currentProcess.kill("SIGKILL");
        currentProcess = null;
      }

      musicQueue = [];
      isPlaying = false;
      currentTrack = null;

      try {
        await ctx.telegram.editMessageText(
          chatId,
          messageId,
          null,
          "⏹ Music stopped.",
          { parse_mode: "HTML" }
        );
        nowPlayingMessageId = null;
      } catch (e) {
        appendLogLine(`Failed to edit message: ${e.message}`);
        await ctx.reply("⏹ Music stopped.", {
          message_thread_id: threadId,
        });
        nowPlayingMessageId = null;
      }
      return;
    }
    return ctx.answerCbQuery("⚠️ No music is playing.");
  }
});

// ================== /STATUS ==================
bot.hears(/^\/status/i, topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;

  exec("su -c /data/adb/box/scripts/box.service status", async (error, stdout, stderr) => {
    if (error) {
      appendLogLine(`Status check failed: ${error.message}`);
      return safeReply(ctx, "❌ Failed to check status.", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }

    const output = stdout || stderr || "⚠️ No output.";
    await safeReply(ctx, `<b>📡 Box Status</b>\n<blockquote>${escapeHTML(output)}</blockquote>`, {
      parse_mode: "HTML",
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  });
});

// ==================== /WORKER ====================
const axios = require('axios');
const crypto = require('crypto');

const DOMAIN = process.env.WORKER_DOMAIN || 'error.zerotwo.my.id';
const PATH_PREFIX = process.env.WORKER_PATH_PREFIX || 'zerotwo';
const UUID = process.env.WORKER_UUID || crypto.randomUUID();
const MAX_SERVERS = 8; // Changed from 10 to 8

function countryFlag(code) {
  if (typeof code !== 'string' || code.length !== 2) return '🌐';
  const base = 0x1F1E6;
  const c = code.toUpperCase();
  return String.fromCodePoint(base + c.charCodeAt(0) - 65) +
         String.fromCodePoint(base + c.charCodeAt(1) - 65);
}

async function pingServer(host, port) {
  const url = `https://api.jb8fd7grgd.workers.dev/${host}:${port}`;

  try {
    const res = await axios.get(url, { timeout: 10000 });
    const data = res.data;
    
    if (Array.isArray(data) && data.length > 0 && data[0].latency) {
      const latency = data[0].latency;
      const latencyMs = parseInt(latency.replace('ms', ''));
      return {
        success: true,
        latency: latencyMs,
        rawLatency: latency
      };
    }
    return { success: false, latency: null, rawLatency: 'Timeout' };
  } catch (err) {
    return { success: false, latency: null, rawLatency: 'Error' };
  }
}

async function getProxyServers() {
  const url =
    'https://raw.githubusercontent.com/Rashgn/Proxy/master/ProxyList.txt';

  try {
    const res = await axios.get(url, { timeout: 10000 });
    const rawData = res.data;

    const lines = rawData.split('\n').filter(line => line.trim() !== '');

    return lines.map(line => {
      const parts = line.split(',');

      if (parts.length < 4) return null;

      const host = parts[0].trim();
      const port = parseInt(parts[1].trim(), 10);
      const code = parts[2].trim().toUpperCase();
      const name = parts[3].trim();

      return {
        host,
        port,
        code,
        flag: countryFlag(code),
        name,
        latency: null,
        rawLatency: ''
      };
    }).filter(Boolean);

  } catch (err) {
    appendLogLine(`worker: failed to fetch or parse proxy list: ${err.message}`);
    return [];
  }
}

const BUG_OPTIONS = [
  'ava.game.naver.com',
  'grabacademyportal.grab.com',
  'quiz.int.vidio.com',
  'support.zoom.us'
];

const workerSessions = {};

bot.command('worker', topicOnly, async ctx => {
  const threadId = ctx.message?.message_thread_id;
  if (threadId !== ALLOWED_TOPIC_ID) return;

  const chatId = ctx.chat.id;
  workerSessions[chatId] = { 
    protocol: null, 
    allServers: [], 
    filteredServers: [], 
    selectedBug: null,
    selectedServers: [], // Array to store multiple selected servers
    selectionMode: false // Flag to indicate if we're in multi-selection mode
  };

  await ctx.reply(
    '<b>🔧 Choose protocol:</b>',
    {
      parse_mode: 'HTML',
      message_thread_id: threadId,
      reply_to_message_id: ctx.message?.message_id,
      reply_markup: {
        inline_keyboard: [
          [
            { text: 'VLESS', callback_data: 'worker_proto_vless' },
            { text: 'VMess', callback_data: 'worker_proto_vmess' },
            { text: 'Trojan', callback_data: 'worker_proto_trojan' },
          ],
        ],
      },
    }
  );
});

bot.action(/^worker_proto_/, topicOnly, async ctx => {
  const chatId = ctx.chat.id;
  const protocol = ctx.callbackQuery.data.replace('worker_proto_', '');

  if (!workerSessions[chatId]) workerSessions[chatId] = {};
  workerSessions[chatId].protocol = protocol;

  const bugKeyboard = BUG_OPTIONS.map((bug, index) => [
    { text: bug, callback_data: `worker_bug_${index}` }
  ]);

  bugKeyboard.push([{ text: '⬅️ Back', callback_data: 'worker_back_proto' }]);

  await ctx.editMessageText(
    `<b>🔧 Protocol:</b> ${protocol.toUpperCase()}\n<b>🐛 Choose custom bug:</b>`,
    {
      parse_mode: 'HTML',
      reply_markup: { inline_keyboard: bugKeyboard },
    }
  );
});

bot.action(/^worker_bug_/, topicOnly, async ctx => {
  const chatId = ctx.chat.id;
  const bugIndex = parseInt(ctx.callbackQuery.data.replace('worker_bug_', ''), 10);
  const session = workerSessions[chatId];

  if (!session || bugIndex >= BUG_OPTIONS.length) {
    return ctx.answerCbQuery(
      '⚠️ Invalid bug selection. Please start over.',
      { show_alert: true }
    );
  }

  session.selectedBug = BUG_OPTIONS[bugIndex];

  let servers;
  try {
    await ctx.answerCbQuery('Fetching server list…');
    servers = await getProxyServers();
    session.allServers = servers;
  } catch (e) {
    appendLogLine(`worker: failed to fetch servers: ${e.message}`);
    return ctx.editMessageText(
      '❌ Failed to fetch server list. Please try again later.',
      { message_thread_id: ctx.callbackQuery.message.message_thread_id }
    );
  }

  const countryCodes = [...new Set(servers.map(s => s.code))].sort();
  if (countryCodes.length === 0) {
    return ctx.editMessageText(
      '❌ No servers found in the list.',
      { message_thread_id: ctx.callbackQuery.message.message_thread_id }
    );
  }

  const keyboard = countryCodes.reduce((acc, code, i) => {
    const flag = countryFlag(code);
    const button = { text: `${flag} ${code}`, callback_data: `worker_country_${code}` };
    const row = Math.floor(i / 5);
    if (!acc[row]) acc[row] = [];
    acc[row].push(button);
    return acc;
  }, []);

  keyboard.push([{ text: '⬅️ Back', callback_data: 'worker_back_bug' }]);

  await ctx.editMessageText(
    `<b>🔧 Protocol:</b> ${session.protocol.toUpperCase()}\n` +
    `<b>🐛 Bug:</b> ${session.selectedBug}\n` +
    `<b>🌏 Choose country:</b>`,
    {
      parse_mode: 'HTML',
      reply_markup: { inline_keyboard: keyboard },
    }
  );
});

bot.action(/^worker_country_/, topicOnly, async ctx => {
  const chatId = ctx.chat.id;
  const session = workerSessions[chatId];
  const countryCode = ctx.callbackQuery.data.replace('worker_country_', '');

  if (!session || !session.allServers) {
    return ctx.answerCbQuery(
      '⚠️ Session expired. Please start over with /worker.',
      { show_alert: true }
    );
  }

  const filteredServers = session.allServers.filter(s => s.code === countryCode);

  await ctx.answerCbQuery('Checking server latency…');

  const pingPromises = filteredServers.map(async (server) => {
    const pingResult = await pingServer(server.host, server.port);
    server.latency = pingResult.latency;
    server.rawLatency = pingResult.rawLatency;
    return server;
  });

  const serversWithPing = await Promise.all(pingPromises);

  serversWithPing.sort((a, b) => {
    if (a.latency === null) return 1;
    if (b.latency === null) return -1;
    return a.latency - b.latency;
  });

  workerSessions[chatId].filteredServers = serversWithPing;
  workerSessions[chatId].selectedServers = []; // Reset selected servers

  // Create server selection keyboard
  const keyboard = serversWithPing.slice(0, 12).map((s, i) => {
    let latencyText = '';
    if (s.latency !== null) {
      latencyText = ` (${s.rawLatency})`;
    } else {
      latencyText = ' (N/A)';
    }
    
    // Check if this server is already selected
    const isSelected = session.selectedServers.some(selected => 
      selected.host === s.host && selected.port === s.port
    );
    
    const emoji = isSelected ? '✅ ' : '';
    // Add number before flag to distinguish servers with similar names
    return [
      { 
        text: `${emoji}${i+1}. ${s.flag} ${s.name}${latencyText}`, 
        callback_data: `worker_srv_${i}` 
      },
    ];
  });

  // Add action buttons
  keyboard.push([
    { text: '🔄 Select/Deselect All', callback_data: 'worker_toggle_all' },
    { text: '🚀 Generate Config', callback_data: 'worker_generate_config' }
  ]);
  
  keyboard.push([{ text: '⬅️ Back', callback_data: 'worker_back_country' }]);

  const flag = countryFlag(countryCode);

  let message = `<b>🔧 Protocol:</b> ${session.protocol.toUpperCase()}\n` +
                `<b>🐛 Bug:</b> ${session.selectedBug}\n` +
                `<b>🌏 Country:</b> ${flag} ${countryCode}\n` +
                `<b>🌐 Select servers (max ${MAX_SERVERS}):</b>\n` + // Updated to use MAX_SERVERS
                `<i>Click on servers to select/deselect them</i>`;

  await ctx.editMessageText(
    message,
    {
      parse_mode: 'HTML',
      reply_markup: { inline_keyboard: keyboard },
    }
  );
});

bot.action('worker_toggle_all', topicOnly, async ctx => {
  const chatId = ctx.chat.id;
  const session = workerSessions[chatId];
  
  if (!session || !session.filteredServers) {
    return ctx.answerCbQuery(
      '⚠️ Session expired. Please start over with /worker.',
      { show_alert: true }
    );
  }
  
  // Toggle all servers selection
  if (session.selectedServers.length === session.filteredServers.length) {
    // Deselect all
    session.selectedServers = [];
  } else {
    // Select all (up to MAX_SERVERS)
    session.selectedServers = session.filteredServers.slice(0, MAX_SERVERS);
  }
  
  // Update the message with new selection state
  await updateServerSelectionMessage(ctx, session);
});

bot.action('worker_generate_config', topicOnly, async ctx => {
  const chatId = ctx.chat.id;
  const session = workerSessions[chatId];
  
  if (!session || !session.selectedServers || session.selectedServers.length === 0) {
    return ctx.answerCbQuery(
      '⚠️ Please select at least one server.',
      { show_alert: true }
    );
  }
  
  await generateConfigurations(ctx, session);
});

bot.action(/^worker_srv_/, topicOnly, async ctx => {
  const chatId = ctx.chat.id;
  const idx = parseInt(ctx.callbackQuery.data.replace('worker_srv_', ''), 10);
  const session = workerSessions[chatId];

  if (!session || !session.filteredServers || !session.filteredServers[idx]) {
    return ctx.answerCbQuery(
      '⚠️ Session expired or invalid server. Please start over.',
      { show_alert: true }
    );
  }

  const server = session.filteredServers[idx];
  
  // Check if server is already selected
  const serverIndex = session.selectedServers.findIndex(s => 
    s.host === server.host && s.port === server.port
  );
  
  if (serverIndex !== -1) {
    // Remove from selection
    session.selectedServers.splice(serverIndex, 1);
  } else {
    // Add to selection if we haven't reached the limit
    if (session.selectedServers.length >= MAX_SERVERS) { // Updated to use MAX_SERVERS
      return ctx.answerCbQuery(
        `⚠️ Maximum ${MAX_SERVERS} servers can be selected.`, // Updated to use MAX_SERVERS
        { show_alert: true }
      );
    }
    session.selectedServers.push(server);
  }
  
  // Update the message with new selection state
  await updateServerSelectionMessage(ctx, session);
});

async function updateServerSelectionMessage(ctx, session) {
  const countryCode = session.filteredServers[0]?.code || '';
  const flag = countryFlag(countryCode);
  
  // Create updated keyboard
  const keyboard = session.filteredServers.slice(0, 12).map((s, i) => {
    let latencyText = '';
    if (s.latency !== null) {
      latencyText = ` (${s.rawLatency})`;
    } else {
      latencyText = ' (N/A)';
    }
    
    // Check if this server is selected
    const isSelected = session.selectedServers.some(selected => 
      selected.host === s.host && selected.port === s.port
    );
    
    const emoji = isSelected ? '✅ ' : '';
    // Add number before flag to distinguish servers with similar names
    return [
      { 
        text: `${emoji}${i+1}. ${s.flag} ${s.name}${latencyText}`, 
        callback_data: `worker_srv_${i}` 
      },
    ];
  });

  // Add action buttons
  keyboard.push([
    { text: '🔄 Select/Deselect All', callback_data: 'worker_toggle_all' },
    { text: '🚀 Generate Config', callback_data: 'worker_generate_config' }
  ]);
  
  keyboard.push([{ text: '⬅️ Back', callback_data: 'worker_back_country' }]);

  let message = `<b>🔧 Protocol:</b> ${session.protocol.toUpperCase()}\n` +
                `<b>🐛 Bug:</b> ${session.selectedBug}\n` +
                `<b>🌏 Country:</b> ${flag} ${countryCode}\n` +
                `<b>🌐 Select servers (max ${MAX_SERVERS}):</b>\n` + // Updated to use MAX_SERVERS
                `<i>Selected: ${session.selectedServers.length} server(s)</i>`;

  await ctx.editMessageText(
    message,
    {
      parse_mode: 'HTML',
      reply_markup: { inline_keyboard: keyboard },
    }
  );
}

async function generateConfigurations(ctx, session) {
  const proto = session.protocol || 'vless';
  const selectedBug = session.selectedBug;
  const selectedServers = session.selectedServers;
  
  let yamlConfigs = '';
  let v2rayConfigs = '';
  
  const domain = `${selectedBug}.${DOMAIN}`;
  
  for (const [index, server] of selectedServers.entries()) {
    const path = `/${PATH_PREFIX}/${server.host}-${server.port}`;
    const port = 443;
    
    let yamlConfig;
    let v2rayConfig;

    // Add number directly to the server name
    const numberedName = `${index+1}. ${server.flag} ${server.name}`;

    switch (proto.toLowerCase()) {
      case 'vless':
        yamlConfig = `- name: ${numberedName}
  type: vless
  server: ${selectedBug}
  port: ${port}
  uuid: ${UUID}
  cipher: auto
  skip-cert-verify: true
  tls: true
  servername: ${domain}
  network: ws
  ws-opts:
      path: ${path}
      headers:
          Host: ${domain}
  udp: true
  xudp: true\n`;
        v2rayConfig = `vless://${UUID}@${selectedBug}:${port}?encryption=none&security=tls&type=ws&host=${domain}&path=${encodeURIComponent(path)}&sni=${domain}#${encodeURIComponent(numberedName)}\n`;
        break;

      case 'vmess':
        yamlConfig = `- name: ${numberedName}
  type: vmess
  server: ${selectedBug}
  port: ${port}
  uuid: ${UUID}
  alterId: 0
  cipher: auto
  skip-cert-verify: true
  tls: true
  servername: ${domain}
  network: ws
  ws-opts:
      path: ${path}
      headers:
          Host: ${domain}
  udp: true
  xudp: true\n`;
        const vmessConfig = {
          v: "2",
          ps: numberedName,
          add: selectedBug,
          port: port.toString(),
          id: UUID,
          aid: "0",
          scy: "auto",
          net: "ws",
          type: "none",
          host: domain,
          path: path,
          tls: "tls",
          sni: domain
        };
        v2rayConfig = "vmess://" + (typeof btoa !== "undefined" ? btoa(unescape(encodeURIComponent(JSON.stringify(vmessConfig)))) : Buffer.from(JSON.stringify(vmessConfig), "utf8").toString("base64")) + "\n";
        break;

      case 'trojan':
        yamlConfig = `- name: ${numberedName}
  type: trojan
  server: ${selectedBug}
  port: ${port}
  password: ${UUID}
  cipher: auto
  skip-cert-verify: true
  tls: true
  sni: ${domain}
  network: ws
  ws-opts:
      path: ${path}
      headers:
          Host: ${domain}
  udp: true
  xudp: true\n`;
        v2rayConfig = `trojan://${UUID}@${selectedBug}:${port}?security=tls&type=ws&host=${domain}&path=${encodeURIComponent(path)}&sni=${domain}#${encodeURIComponent(numberedName)}\n`
        break;
    }
    
    yamlConfigs += yamlConfig;
    v2rayConfigs += v2rayConfig;
  }
  
  let latencyInfo = '';
  if (selectedServers.length === 1 && selectedServers[0].rawLatency) {
    latencyInfo = `<b>⏱ Latency :</b> ${selectedServers[0].rawLatency}\n`;
  }

  let message = `<b>✅ Worker Accounts Created (${selectedServers.length})</b>\n` +
                `<blockquote><b>🔧 Protocol :</b> ${proto.toUpperCase()}\n` +
                `<b>🔐 Security :</b> TLS\n` +
                `<b>🐛 Bug :</b> ${selectedBug}\n` +
                `${latencyInfo}</blockquote>` +
                `<pre>${yamlConfigs}</pre>`;
                // `<b>YAML Configuration:</b>\n<pre>${yamlConfigs}</pre>` +
                // `<b>V2Ray Configuration:</b>\n<pre>${v2rayConfigs}</pre>`;

  // If the message is too long, split it into multiple messages
  if (message.length > 4096) {
    const yamlPart = `<b>YAML Configuration:</b>\n<pre>${yamlConfigs}</pre>`;
    // const v2rayPart = `<b>V2Ray Configuration:</b>\n<pre>${v2rayConfigs}</pre>`;
    
    const header = `<b>✅ Worker Accounts Created (${selectedServers.length})</b>\n` +
                   `<blockquote><b>🔧 Protocol :</b> ${proto.toUpperCase()}\n` +
                   `<b>🔐 Security :</b> TLS\n` +
                   `<b>🐛 Bug :</b> ${selectedBug}\n` +
                   `${latencyInfo}</blockquote>`;
    
    await ctx.editMessageText(header + yamlPart, { parse_mode: 'HTML' });
    // await ctx.reply(v2rayPart, { parse_mode: 'HTML', message_thread_id: ctx.callbackQuery.message.message_thread_id });
  } else {
    await ctx.editMessageText(message, { parse_mode: 'HTML' });
  }

  delete workerSessions[ctx.chat.id];
}

bot.action(/^worker_back_/, topicOnly, async ctx => {
  const chatId = ctx.chat.id;
  const backAction = ctx.callbackQuery.data.replace('worker_back_', '');
  const session = workerSessions[chatId];

  if (!session) {
    return ctx.answerCbQuery(
      '⚠️ Session expired. Please start over with /worker.',
      { show_alert: true }
    );
  }

  if (backAction === 'proto') {
    await ctx.editMessageText(
      '<b>🔧 Choose protocol:</b>',
      {
      parse_mode: 'HTML',
      reply_markup: {
        inline_keyboard: [
          [
            { text: 'VLESS', callback_data: 'worker_proto_vless' },
            { text: 'VMess', callback_data: 'worker_proto_vmess' },
            { text: 'Trojan', callback_data: 'worker_proto_trojan' },
          ],
        ],
      },
    }
    );
  } else if (backAction === 'bug') {
    const bugKeyboard = BUG_OPTIONS.map((bug, index) => [
      { text: bug, callback_data: `worker_bug_${index}` }
    ]);

    bugKeyboard.push([{ text: '⬅️ Back', callback_data: 'worker_back_proto' }]);

    await ctx.editMessageText(
      `<b>🔧 Protocol:</b> ${session.protocol.toUpperCase()}\n<b>🐛 Choose custom bug:</b>`,
      {
        parse_mode: 'HTML',
        reply_markup: { inline_keyboard: bugKeyboard },
      }
    );
  } else if (backAction === 'country') {
    const countryCodes = [...new Set(session.allServers.map(s => s.code))].sort();
    const keyboard = countryCodes.reduce((acc, code, i) => {
      const flag = countryFlag(code);
      const button = { text: `${flag} ${code}`, callback_data: `worker_country_${code}` };
      const row = Math.floor(i / 5);
      if (!acc[row]) acc[row] = [];
      acc[row].push(button);
      return acc;
    }, []);

    keyboard.push([{ text: '⬅️ Back', callback_data: 'worker_back_bug' }]);

    await ctx.editMessageText(
      `<b>🔧 Protocol:</b> ${session.protocol.toUpperCase()}\n` +
      `<b>🐛 Bug:</b> ${session.selectedBug}\n` +
      `<b>🌏 Choose country:</b>`,
      {
        parse_mode: 'HTML',
        reply_markup: { inline_keyboard: keyboard },
      }
    );
  }
});

// unknown commands
bot.hears(/^\/(\w+)/, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  const cmd = ctx.match[1].toLowerCase();
  if (threadId !== ALLOWED_TOPIC_ID) return;

  return safeReply(
    ctx,
      "<b>✹ Ask AI</b>\n" +
      "-> <code>/ask prompt</code>\n" +
      "<b>✹ Domain Lookup</b>\n" +
      "-> <code>/domain ip</code>\n" +
      "<b>✹ IP Lookup</b>\n" +
      "-> <code>/ip domain</code>\n" +
      "<b>✹ Create Worker Acc</b>\n" +
      "-> /worker\n" +
      "<b>✹ Battery Status (admin)</b>\n" +
      "-> /battery\n" +
      "<b>✹ Play Music (admin)</b>\n" +
      "-> <code>/music tittle</code>\n" +
      "<b>✹ Data Usage (admin)</b>\n" +
      "-> /usage\n" +
      "<b>✹ Reboot Device (admin)</b>\n" +
      "-> /reboot\n" +
      "<b>✹ Speed Test (admin)</b>\n" +
      "-> /speed\n" +
      "<b>✹ Box Status (admin)</b>\n" +
      "-> /status\n" +
      "<b>✹ Update Box (admin)</b>\n" +
      "-> /update",
    { parse_mode: "HTML", reply_to_message_id: ctx.message?.message_id, message_thread_id: threadId }
  );
});

// ==========================
// Mute non-command messages (except admins)
// ==========================
bot.on("text", async (ctx, next) => {
  const threadId = ctx.message?.message_thread_id;
  const text = ctx.message?.text?.trim() || "";
  const chatId = ctx.chat?.id;
  const userId = ctx.from?.id;

  // 1. Skip if not in allowed topic
  if (threadId !== ALLOWED_TOPIC_ID) return next();

  // 2. Handle /buy reply session if active
  const session = buySessions[chatId];
  if (session && session.process) {
    const replied = ctx.message?.reply_to_message;
    if (replied && replied.from?.id === ctx.botInfo.id) {
      // Admin check
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

  // 3. Mute check for non-command messages
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

  // 4. Let other handlers run
  return next();
});

// ==========================
// Cron daily usage report
// ==========================
cron.schedule("59 23 * * *", async () => {
  await sendUsageReport();
});

// ==========================
// Launch bot
// ==========================
bot.launch()
  .then(() => {
    console.log("Bot launched");
    appendLogLine("Bot launched");
  })
  .catch((err) => {
    console.error("Failed to launch bot:", err.message);
    appendLogLine(`Failed to launch bot: ${err.message}`);
  });

const runRepo = require('./repo.js');

async function loop() {
  try {
    await runRepo();
  } catch (err) {
    console.error(`❌ Error: ${err.message}`);
  }
  loop();
}

loop();

// ==================================
//  Run Python script every day at 23:55
// ==================================
cron.schedule('55 23 * * *', () => {
  console.log('⏰ Running Python script at 23:55');
  exec('python ./scripts/mi.py', (error, stdout, stderr) => {
    if (error) {
      console.error(`❌ Error: ${error.message}`);
      return;
    }
    if (stderr) console.error(`⚠️ ${stderr}`);
    console.log(`✅ Python Output:\n${stdout}`);
  });
});