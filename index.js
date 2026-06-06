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
        // const ifaceData = data.interfaces
          // .map((i) => ({ name: i.name, total: (i.traffic?.total?.rx || 0) + (i.traffic?.total?.tx || 0) }))
          // .sort((a, b) => b.total - a.total)[0];

        // const iface = ifaceData?.name || "unknown";
        // const traffic = data.interfaces.find((i) => i.name === iface)?.traffic;

        const iface = "wlan0";
        const ifaceData = data.interfaces.find((i) => i.name === iface) || data.interfaces[0];
        const traffic = ifaceData?.traffic;

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
function formatDuration(seconds) {
  if (!seconds || isNaN(seconds)) return "??:??";
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
  return `${m}:${s.toString().padStart(2, "0")}`;
}

bot.command("music", topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  if (threadId !== ALLOWED_TOPIC_ID) return;

  const text = ctx.message.text?.trim();
  const query = text.replace(/^\/music(@\w+)?\s*/i, "").trim();

  if (!query) {
    return ctx.reply("❌ Please provide a song title or YouTube link.", {
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  }

  const command = query.startsWith("http")
    ? `yt-dlp -f 251 -j "${query}"`
    : `yt-dlp -f 251 -j "ytsearch1:${query}"`;

  exec(command, async (err, stdout) => {
    if (err || !stdout) {
      appendLogLine(`yt-dlp error: ${err?.message}`);

      return ctx.reply("❌ Failed to fetch music.", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }

    let info;

    try {
      info = JSON.parse(stdout);
    } catch (e) {
      appendLogLine(`JSON parse error: ${e.message}`);

      return ctx.reply("❌ Failed to parse music information.", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }

    const title = info.track || info.title || "Unknown Title";
    const performer = info.artist || info.uploader || "Unknown Artist";
    const duration = formatDuration(info.duration);
    const videoUrl = `https://youtu.be/${info.id}`;
    const tempFile = path.join(__dirname, `${info.id}.webm`);

    const infoMessage = await ctx.reply(
      `<b>🎵 Music Info</b>\n` +
      `<b>📝 Title:</b>\n-> <code>${title}</code>\n` +
      `<b>👤 Artist:</b>\n-> <code>${performer}</code>\n` +
      `<b>⏳ Duration:</b>\n-> <code>${duration}</code>`,
      {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      }
    );

    exec(`yt-dlp -f 251 -o "${tempFile}" "${videoUrl}"`, async (err2) => {
      if (err2) {
        appendLogLine(`Download error: ${err2.message}`);

        return ctx.telegram.editMessageText(
          ctx.chat.id,
          infoMessage.message_id,
          undefined,
          "❌ Failed to download audio."
        );
      }

      try {
        await ctx.replyWithAudio(
          {
            source: fs.createReadStream(tempFile),
          },
          {
            title,
            performer,
            duration: info.duration,
            message_thread_id: ALLOWED_TOPIC_ID_MUSIC,
          }
        );
      } catch (e) {
        appendLogLine(`Send audio error: ${e.message}`);

        await ctx.telegram.editMessageText(
          ctx.chat.id,
          infoMessage.message_id,
          undefined,
          "❌ Failed to send audio."
        );
      } finally {
        fs.unlink(tempFile, () => {});
      }
    });
  });
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
      "<b>✹ Battery Status (admin)</b>\n" +
      "-> /battery\n" +
      "<b>✹ Download Music (admin)</b>\n" +
      "-> <code>/music tittle</code>\n" +
      "<b>✹ Data Usage (admin)</b>\n" +
      "-> /usage\n" +
      "<b>✹ Reboot Device (admin)</b>\n" +
      "-> /reboot\n" +
      "<b>✹ Speed Test (admin)</b>\n" +
      "-> /speed",
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
// cron.schedule('55 23 * * *', () => {
  // console.log('⏰ Running Python script at 23:55');
  // exec('python ./scripts/mi.py', (error, stdout, stderr) => {
    // if (error) {
      // console.error(`❌ Error: ${error.message}`);
      // return;
    // }
    // if (stderr) console.error(`⚠️ ${stderr}`);
    // console.log(`✅ Python Output:\n${stdout}`);
  // });
// });