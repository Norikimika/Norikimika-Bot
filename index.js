// bot.js
require("dotenv").config();
const fs = require("fs");
const { Telegraf } = require("telegraf");
const cron = require("node-cron");
const { spawn, exec } = require("child_process");
const escapeHTML = require("escape-html");
const https = require("https");

// dynamic import for node-fetch
const fetch = (...args) => import("node-fetch").then(({ default: fetch }) => fetch(...args));

// Logging
const LOG_FILE = "bot.log";
const logStream = fs.createWriteStream(LOG_FILE, { flags: "a" });
const RESTART_TIMESTAMP_FILE = "last_restart.txt";

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

// Markdown to HTML converter
function convertMarkdownToHTML(text) {
  if (!text) return "";
  let safeText = escapeHTML(text);

  safeText = safeText.replace(/^#### (.*)$/gm, "<u>$1</u>");
  safeText = safeText.replace(/^### (.*)$/gm, "<b><u>$1</u></b>");
  safeText = safeText.replace(/^## (.*)$/gm, "<b>$1</b>");
  safeText = safeText.replace(/^# (.*)$/gm, "<b><i>$1</i></b>");
  safeText = safeText.replace(/^\s*[\*\-]\s+/gm, "• ");
  safeText = safeText.replace(/```(\w+)?\n([\s\S]*?)```/g, (_, lang, code) => {
    const escapedCode = escapeHTML(code.trim());
    return `<pre><code>${escapedCode}</code></pre>`;
  });
  safeText = safeText.replace(/`([^`\n]+?)`/g, (_, code) => `<code>${escapeHTML(code)}</code>`);
  safeText = safeText.replace(/\*\*(?!\s)([^\*\n]+?)\*\*/g, "<b>$1</b>");
  safeText = safeText.replace(/\*(?!\s)([^\*\n]+?)\*/g, "<i>$1</i>");

  return safeText;
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
        await safeSend(bot, process.env.GROUP_ID, "❌ Failed to get daily usage", {
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

        const usageReport =
          `<b>📊 Daily Usage Report (${iface})</b>\n\n` +
          (todayData ? `📅 <b>Today</b>\n⏫ Upload: ${todayData.up}\n⏬ Download: ${todayData.down}\n📦 Total: ${todayData.total}` : "⚠️ No data");

        await safeSend(bot, process.env.GROUP_ID, usageReport.trim(), {
          parse_mode: "HTML",
          message_thread_id: ALLOWED_TOPIC_ID_USAGE,
        });
      } catch (parseError) {
        appendLogLine(`Daily usage parse failed: ${parseError.message}`);
        await safeSend(bot, process.env.GROUP_ID, "❌ Failed to parse daily usage", {
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
    const chatId = ctx.chat?.id || ctx.message?.chat?.id || ctx.callbackQuery?.message?.chat?.id;

    if (!userId || !chatId) {
      return safeReply(ctx, "❌ Permission error", { reply_to_message_id: ctx.message?.message_id });
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

      return safeReply(ctx, "⛔ Not authorized. You are muted for 5 minutes.", {
        reply_to_message_id: ctx.message?.message_id,
      });
    }

    return next();
  } catch (err) {
    appendLogLine(`adminOnly error: ${err.message}`);
    return safeReply(ctx, "⚠️ Permission check failed.", { reply_to_message_id: ctx.message?.message_id });
  }
};

// ==========================
// Commands
// ==========================

// /speed
bot.hears(/^\/speed/i, topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  exec("speedtest-cli --simple", async (error, stdout) => {
    if (error) {
      appendLogLine(`Speedtest failed: ${error.message}`);
      return safeReply(ctx, "❌ Failed to run speedtest", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }
    await safeReply(ctx, `<b>⚡ Speed Test Result</b>\n<pre>${escapeHTML(stdout)}</pre>`, {
      parse_mode: "HTML",
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  });
});

// /reboot
bot.hears(/^\/reboot/i, topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  await safeReply(ctx, "♻️ Rebooting system...", {
    reply_to_message_id: ctx.message?.message_id,
    message_thread_id: threadId,
  });
  exec("su -c reboot", (error) => {
    if (error) {
      appendLogLine(`Reboot failed: ${error.message}`);
      return safeReply(ctx, "❌ Failed to reboot", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }
  });
});

// /restart
bot.hears(/^\/restart/i, topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  const now = Date.now();
  const cooldown = 90 * 1000;

  let lastRestartTime = 0;
  if (fs.existsSync(RESTART_TIMESTAMP_FILE)) {
    try {
      const raw = fs.readFileSync(RESTART_TIMESTAMP_FILE, "utf-8");
      lastRestartTime = parseInt(raw.trim(), 10) || 0;
    } catch (_) {}
  }

  if (now - lastRestartTime < cooldown) {
    const remaining = Math.ceil((cooldown - (now - lastRestartTime)) / 1000);
    return safeReply(ctx, `⏳ Please wait ${remaining}s before restarting again.`, {
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  }

  try {
    fs.writeFileSync(RESTART_TIMESTAMP_FILE, now.toString());
  } catch (e) {
    appendLogLine(`Write restart timestamp failed: ${e.message}`);
  }

  await safeReply(ctx, "♻️ Restarting server...", {
    reply_to_message_id: ctx.message?.message_id,
    message_thread_id: threadId,
  });

  exec("su -c scripts/restart.sh", (error) => {
    if (error) {
      appendLogLine(`Restart failed: ${error.message}`);
      return safeReply(ctx, "❌ Failed to restart", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }
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
        `<b>🔋 Battery Status</b>\n<pre>Percentage: ${data.percentage}%\nStatus: ${data.status}\nTemperature: ${Math.round(data.temperature)}°C</pre>`,
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
  let prompt = text.replace(/^\/ask(@\w+)?\s*/i, "").trim();

  if ((!prompt || prompt === "") && ctx.message.reply_to_message) {
    prompt = ctx.message.reply_to_message.text?.trim();
  }

  if (!prompt) {
    return safeReply(ctx, "❌ Please provide a prompt.", {
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
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
    const rawText = data?.candidates?.[0]?.content?.parts?.[0]?.text || "❌ No response.";
    const MAX_LENGTH = 3500;
    const parts = rawText.match(new RegExp(`.{1,${MAX_LENGTH}}`, "gs")) || [rawText];

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

      const usageReport =
        `<b>📊 Data Usage (${iface})</b>\n\n` +
        (todayData ? `📅 <b>Today</b>\n⏫ Upload: ${todayData.up}\n⏬ Download: ${todayData.down}\n📦 Total: ${todayData.total}\n` : "⚠️ No data\n") +
        (weekData ? `\n📅 <b>This Week</b>\n⏫ Upload: ${weekData.up}\n⏬ Download: ${weekData.down}\n📦 Total: ${weekData.total}\n` : "⚠️ No data\n") +
        (monthData ? `\n📅 <b>This Month</b>\n⏫ Upload: ${monthData.up}\n⏬ Download: ${monthData.down}\n📦 Total: ${monthData.total}` : "");

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

// /updatec
bot.hears(/^\/updatec/i, topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  await safeReply(ctx, "🦠 Choose core to update:", {
    reply_to_message_id: ctx.message?.message_id,
    message_thread_id: threadId,
    reply_markup: {
      inline_keyboard: [
        [{ text: "☘️ Update Clash", callback_data: "update_mihomo" }, { text: "🍀 Update Smart", callback_data: "update_smart" }],
        [{ text: "🌱 Update Sing-box", callback_data: "update_sing" }],
      ],
    },
  });
});

// callback_query
bot.on("callback_query", topicOnly, adminOnly, async (ctx) => {
  const data = ctx.callbackQuery?.data;
  const cbMsg = ctx.callbackQuery?.message;
  const threadId = cbMsg?.message_thread_id;

  const scriptMap = {
    update_mihomo: "scripts/update-mihomo.sh",
    update_smart: "scripts/update-smart.sh",
    update_sing: "scripts/update-sing.sh",
  };

  const scriptPath = scriptMap[data];
  if (!scriptPath) return safeAnswerCbQuery(ctx, "❌ Invalid selection!");

  await safeEditMessageText(ctx, `🚀 Updating core...`);

  exec(`su -c ${scriptPath}`, async (error) => {
    if (error) {
      appendLogLine(`Update core failed: ${error.message}`);
      return safeSend(bot, cbMsg.chat.id, "❌ Failed to update core", {
        reply_to_message_id: cbMsg?.message_id,
        message_thread_id: threadId,
      });
    }
    await safeSend(bot, cbMsg.chat.id, "✅ Core updated successfully!", {
      reply_to_message_id: cbMsg?.message_id,
      message_thread_id: threadId,
    });
  });

  await safeAnswerCbQuery(ctx, "⏳ Updating core...");
});

// /updated
bot.hears(/^\/updated/i, topicOnly, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  await safeReply(ctx, "🚀 Updating dashboard...", {
    reply_to_message_id: ctx.message?.message_id,
    message_thread_id: threadId,
  });

  exec("su -c scripts/update-dashboard.sh", async (error) => {
    if (error) {
      appendLogLine(`Update dashboard failed: ${error.message}`);
      return safeReply(ctx, "❌ Failed to update dashboard", {
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }
    await safeReply(ctx, "✅ Dashboard updated successfully!", {
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  });
});

// unknown commands
bot.hears(/^\/(\w+)/, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  const cmd = ctx.match[1].toLowerCase();
  if (threadId !== ALLOWED_TOPIC_ID) return;

  const known = ["ask", "battery", "reboot", "restart", "speed", "usage", "updatec", "updated"];
  if (known.includes(cmd)) return;

  return safeReply(
    ctx,
    "<b>🍁 Unknown Command 🍁</b>\n" +
      "<b>✹ Ask AI</b>\n" +
      "-> <code>/ask prompt</code>\n" +
      "<b>✹ Battery Status (admin)</b>\n" +
      "-> /battery\n" +
      "<b>✹ Data Usage (admin)</b>\n" +
      "-> /usage\n" +
      "<b>✹ Reboot Device (admin)</b>\n" +
      "-> /reboot\n" +
      "<b>✹ Restart Server (admin)</b>\n" +
      "-> /restart\n" +
      "<b>✹ Speed Test (admin)</b>\n" +
      "-> /speed\n" +
      "<b>✹ Update Core (admin)</b>\n" +
      "-> /updatec\n" +
      "<b>✹ Update Dashboard (admin)</b>\n" +
      "-> /updated",
    { parse_mode: "HTML", reply_to_message_id: ctx.message?.message_id, message_thread_id: threadId }
  );
});

// ==========================
// Mute non-command messages (except admins)
// ==========================
bot.on("text", async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  if (threadId !== ALLOWED_TOPIC_ID) return;

  const text = ctx.message?.text?.trim() || "";
  if (!text.startsWith("/")) {
    const userId = ctx.from?.id;
    const chatId = ctx.chat?.id;

    try {
      const admins = await ctx.telegram.getChatAdministrators(chatId);
      const isAdmin = admins.some((admin) => admin.user.id === userId);

      if (!isAdmin) {
        const untilDate = Math.floor(Date.now() / 1000) + 5 * 60; // 5 menit
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

// ==========================
// Graceful shutdown
// ==========================
process.once("SIGINT", () => {
  appendLogLine("SIGINT received, stopping bot...");
  bot.stop("SIGINT");
});
process.once("SIGTERM", () => {
  appendLogLine("SIGTERM received, stopping bot...");
  bot.stop("SIGTERM");
});
