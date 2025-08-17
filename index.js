// bot.js
require("dotenv").config();
const fs = require("fs");
const { Telegraf } = require("telegraf");
const cron = require("node-cron");
const { spawn, exec } = require("child_process");
const escapeHTML = require("escape-html");
const https = require("https");

// dynamic import for node-fetch (keeps original style)
const fetch = (...args) => import("node-fetch").then(({ default: fetch }) => fetch(...args));

// Log stream for general logs
const LOG_FILE = "bot.log";
const logStream = fs.createWriteStream(LOG_FILE, { flags: "a" });
const RESTART_TIMESTAMP_FILE = "last_restart.txt";

// Safe global logger
function appendLogLine(line) {
  try {
    const ts = new Date().toISOString();
    fs.appendFileSync(LOG_FILE, `[${ts}] ${line}\n`);
  } catch (e) {
    console.error("Failed to write to log file:", e.message);
  }
}

// Global process handlers (will not exit the process)
process.on("uncaughtException", (err) => {
  console.error("Uncaught Exception:", err);
  appendLogLine(`Uncaught Exception: ${err.stack || err.message}`);
});

process.on("unhandledRejection", (reason) => {
  console.error("Unhandled Rejection:", reason);
  appendLogLine(`Unhandled Rejection: ${reason && reason.stack ? reason.stack : reason}`);
});

// Create bot
const bot = new Telegraf(process.env.BOT_TOKEN);

// Basic config from env (with defaults)
const ALLOWED_TOPIC_ID = parseInt(process.env.ALLOWED_TOPIC_ID || "0");
const ALLOWED_USER_ID = parseInt(process.env.ALLOWED_USER_ID || "0");

// Utility: convert markdown-like text to safe HTML (preserve your original mappings)
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

  safeText = safeText.replace(/`([^`\n]+?)`/g, (_, code) => {
    return `<code>${escapeHTML(code)}</code>`;
  });

  safeText = safeText.replace(/\*\*(?!\s)([^\*\n]+?)\*\*/g, "<b>$1</b>");
  safeText = safeText.replace(/\*(?!\s)([^\*\n]+?)\*/g, "<i>$1</i>");

  return safeText;
}

// ==========================
// Safe wrappers for Telegram API calls
// ==========================
const DEFAULT_RETRIES = 2;
const DEFAULT_RETRY_DELAY = 2000; // ms

async function safeSend(botInstance, chatId, text, options = {}, retries = DEFAULT_RETRIES) {
  for (let i = 0; i <= retries; i++) {
    try {
      await botInstance.telegram.sendMessage(chatId, text, options);
      return true;
    } catch (err) {
      console.error(`safeSend attempt ${i + 1} failed:`, err && (err.code || err.message || err));
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
      console.error(`safeReply attempt ${i + 1} failed:`, err && (err.code || err.message || err));
      appendLogLine(`safeReply error (chat ${ctx.chat?.id || "unknown"}): ${err && (err.stack || err.message)}`);
      if (i < retries) await new Promise((r) => setTimeout(r, DEFAULT_RETRY_DELAY));
    }
  }
  return false;
}

async function safeEditMessageText(ctx, text, options = {}, retries = DEFAULT_RETRIES) {
  for (let i = 0; i <= retries; i++) {
    try {
      // editMessageText may be on ctx or telegram; try both
      if (typeof ctx.editMessageText === "function") {
        await ctx.editMessageText(text, options);
      } else {
        await ctx.telegram.editMessageText(ctx.chat.id, ctx.update?.callback_query?.message?.message_id, undefined, text, options);
      }
      return true;
    } catch (err) {
      console.error(`safeEditMessageText attempt ${i + 1} failed:`, err && (err.code || err.message || err));
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
      console.error(`safeAnswerCbQuery attempt ${i + 1} failed:`, err && (err.code || err.message || err));
      appendLogLine(`safeAnswerCbQuery error (chat ${ctx.chat?.id || "unknown"}): ${err && (err.stack || err.message)}`);
      if (i < retries) await new Promise((r) => setTimeout(r, DEFAULT_RETRY_DELAY));
    }
  }
  return false;
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

const adminOnly = async (ctx, next) => {
  try {
    const userId = ctx.from?.id;
    const chatId = ctx.chat?.id || ctx.message?.chat?.id || ctx.callbackQuery?.message?.chat?.id;

    if (!userId || !chatId) {
      console.warn("Missing userId or chatId in adminOnly.");
      if (ctx.updateType === "callback_query") {
        return safeAnswerCbQuery(ctx, "❌ Cannot verify permissions.", { show_alert: true });
      } else {
        return safeReply(ctx, "❌ Cannot verify permissions.", {
          reply_to_message_id: ctx.message?.message_id || undefined,
        });
      }
    }

    const admins = await ctx.telegram.getChatAdministrators(chatId);
    const isAdmin = admins.some((admin) => admin.user.id === userId);

    if (!isAdmin) {
      // Mute user selama 5 menit
      const untilDate = Math.floor(Date.now() / 1000) + 5 * 60;
      try {
        await ctx.telegram.restrictChatMember(chatId, userId, {
          permissions: {
            can_send_messages: false,
            can_send_media_messages: false,
            can_send_other_messages: false,
            can_add_web_page_previews: false
          },
          until_date: untilDate
        });

        console.log(`User ${userId} muted for 5 minutes for trying admin command.`);
      } catch (muteErr) {
        console.error("Failed to mute user:", muteErr);
      }

      if (ctx.updateType === "callback_query") {
        return safeAnswerCbQuery(ctx, "⛔ You are not authorized. You are muted for 5 minutes.", { show_alert: true });
      } else {
        return safeReply(ctx, "⛔ You are not authorized. You are muted for 5 minutes.", {
          reply_to_message_id: ctx.message?.message_id || undefined,
        });
      }
    }

    return next();
  } catch (err) {
    console.error("adminOnly error:", err && (err.message || err));
    appendLogLine(`adminOnly error: ${err && (err.stack || err.message)}`);
    if (ctx.updateType === "callback_query") {
      return safeAnswerCbQuery(ctx, "⚠️ Permission check failed.", { show_alert: true });
    } else {
      return safeReply(ctx, "⚠️ Error checking permissions.", {
        reply_to_message_id: ctx.message?.message_id || undefined,
      });
    }
  }
};

// ==========================
// Commands (safe versions)
// ==========================

// /speed
bot.hears(/^\/speed/i, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  exec("speedtest-cli --simple", async (error, stdout, stderr) => {
    if (error) {
      return safeReply(ctx, `❌ Speed test failed:\n<pre>${escapeHTML(error.message)}</pre>`, {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }
    if (stderr) {
      return safeReply(ctx, `⚠️ stderr:\n<pre>${escapeHTML(stderr)}</pre>`, {
        parse_mode: "HTML",
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
bot.hears(/^\/reboot/i, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  await safeReply(ctx, "♻️ Rebooting system...", {
    reply_to_message_id: ctx.message?.message_id,
    message_thread_id: threadId,
  });
  exec("su -c reboot", (error) => {
    if (error) {
      safeReply(ctx, `❌ Failed to reboot:\n<pre>${escapeHTML(error.message)}</pre>`, {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }
  });
});

// /restart
bot.hears(/^\/restart/i, adminOnly, async (ctx) => {
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
    return safeReply(ctx, `⏳ Please wait ${remaining} seconds before restarting again.`, {
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  }

  try {
    fs.writeFileSync(RESTART_TIMESTAMP_FILE, now.toString());
  } catch (e) {
    appendLogLine(`Failed to write restart timestamp: ${e.message}`);
  }

  await safeReply(ctx, "♻️ Restarting server...", {
    reply_to_message_id: ctx.message?.message_id,
    message_thread_id: threadId,
  });

  exec("su -c scripts/restart.sh", (error) => {
    if (error) {
      safeReply(ctx, `❌ Failed to restart:\n<pre>${escapeHTML(error.message)}</pre>`, {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }
  });
});

// /battery
bot.hears(/^\/battery/i, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;

  exec("termux-battery-status", (error, stdout, stderr) => {
    if (error || stderr) {
      return safeReply(ctx, `❌ Failed to get battery info:\n<pre>${escapeHTML(error?.message || stderr)}</pre>`, {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }

    try {
      const data = JSON.parse(stdout);
      return safeReply(
        ctx,
        `<b>🔋 Battery Status</b>\n<pre>Percentage: ${data.percentage}%\nStatus: ${data.status}\nTemperature: ${Math.round(data.temperature)}°C</pre>`,
        {
          parse_mode: "HTML",
          reply_to_message_id: ctx.message?.message_id,
          message_thread_id: threadId,
        }
      );
    } catch (parseError) {
      return safeReply(ctx, `❌ Failed to parse battery info:\n<pre>${escapeHTML(parseError.message)}</pre>`, {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }
  });
});

// /ask (AI handler) - only in allowed topic
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
    const rawText = data?.candidates?.[0]?.content?.parts?.[0]?.text || "❌ No response from Gemini API.";
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
    appendLogLine(`Gemini Error: ${err && (err.stack || err.message)}`);
    await safeReply(ctx, "❌ An error occurred while processing your request. Please try again later.", {
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  }
});

// /usage
bot.hears(/^\/usage/i, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;

  const vnstat = spawn("vnstat", ["--json"]);
  let output = "";
  let errOutput = "";

  vnstat.stdout.on("data", (data) => {
    output += data.toString();
  });

  vnstat.stderr.on("data", (data) => {
    errOutput += data.toString();
  });

  vnstat.on("close", (code) => {
    if (code !== 0 || errOutput) {
      return safeReply(ctx, `❌ Failed to get data usage:\n<pre>${escapeHTML(errOutput || `Exited with code ${code}`)}</pre>`, {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }

    try {
      const data = JSON.parse(output);

      const ifaceData = data.interfaces
        .map((i) => ({
          name: i.name,
          total: (i.traffic?.total?.rx || 0) + (i.traffic?.total?.tx || 0),
        }))
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
        if (!obj || typeof obj.rx !== "number" || typeof obj.tx !== "number") return null;
        const up = obj.tx;
        const down = obj.rx;
        const total = up + down;
        return {
          up: formatBytes(up),
          down: formatBytes(down),
          total: formatBytes(total),
        };
      };

      const getWeekData = () => {
        if (traffic?.week?.length) {
          return extractTraffic(traffic.week.at(-1));
        }
        // fallback: sum 7 hari terakhir
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

      const buildSection = (title, data) => {
        if (!data) {
          return `📅 <b>${title}</b>\n⚠️ No data available\n`;
        }
        return (
          `📅 <b>${title}</b>\n` +
          `⏫ Upload: ${data.up}\n` +
          `⏬ Download: ${data.down}\n` +
          `📦 Total: ${data.total}\n`
        );
      };

      const usageReport =
        `<b>📊 Data Usage (interface: ${iface})</b>\n\n` +
        buildSection("Today", todayData) + `\n` +
        buildSection("This Week", weekData) + `\n` +
        buildSection("This Month", monthData);

      return safeReply(ctx, usageReport.trim(), {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    } catch (parseError) {
      return safeReply(ctx, `❌ Failed to parse vnstat output:\n<pre>${escapeHTML(parseError.message)}</pre>`, {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }
  });
});

// /updatec
bot.hears(/^\/updatec/i, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;

  await safeReply(ctx, "🦠 Please choose the core to update:", {
    reply_to_message_id: ctx.message?.message_id,
    message_thread_id: threadId,
    reply_markup: {
      inline_keyboard: [
        [
          { text: "☘️ Update Clash", callback_data: "update_mihomo" },
          { text: "🍀 Update Smart", callback_data: "update_smart" },
        ],
        [
          { text: "🌱 Update Sing-box", callback_data: "update_sing" },
        ],
      ],
    },
  });
});

// callback_query handler
bot.on("callback_query", adminOnly, async (ctx) => {
  const data = ctx.callbackQuery?.data;
  const cbMsg = ctx.callbackQuery?.message;

  const scriptMap = {
    update_mihomo: "scripts/update-mihomo.sh",
    update_smart: "scripts/update-smart.sh",
    update_sing: "scripts/update-sing.sh",
  };

  const scriptPath = scriptMap[data];
  const threadId = cbMsg?.message_thread_id;
  const replyId = cbMsg?.reply_to_message?.message_id;

  if (!scriptPath) {
    return safeAnswerCbQuery(ctx, "❌ Invalid selection!");
  }

  await safeEditMessageText(ctx, `🚀 Updating core...`);

  exec(`su -c ${scriptPath}`, async (error, stdout, stderr) => {
    if (error) {
      const escapedError = escapeHTML(error.message || "Unknown error");
      return safeSend(bot, cbMsg.chat.id, `❌ Failed to update:\n<pre>${escapedError}</pre>`, {
        parse_mode: "HTML",
        reply_to_message_id: replyId || undefined,
        message_thread_id: threadId || undefined,
      });
    }

    try {
      await safeSend(bot, cbMsg.chat.id, "✅ Core updated successfully!", {
        reply_to_message_id: replyId || undefined,
        message_thread_id: threadId || undefined,
      });
    } catch (sendErr) {
      console.error("Failed to send success message:", sendErr && (sendErr.message || sendErr));
      appendLogLine(`Failed to send success after update: ${sendErr && (sendErr.stack || sendErr.message)}`);
    }
  });

  await safeAnswerCbQuery(ctx, "⏳ Updating core...");
});

// /updated
bot.hears(/^\/updated/i, adminOnly, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;

  await safeReply(ctx, "🚀 Updating dashboard...", {
    reply_to_message_id: ctx.message?.message_id,
    message_thread_id: threadId,
  });

  exec("su -c scripts/update-dashboard.sh", async (error, stdout, stderr) => {
    if (error) {
      const escapedError = escapeHTML(error.message || "Unknown error");
      const message = `❌ Failed to update dashboard:\n<pre>${escapedError}</pre>`;
      return safeReply(ctx, message.slice(0, 4000), {
        parse_mode: "HTML",
        reply_to_message_id: ctx.message?.message_id,
        message_thread_id: threadId,
      });
    }

    if (stderr) {
      // console.warn("⚠️ [Update Dashboard] stderr output:", stderr);
      // appendLogLine(`[update-dashboard] stderr: ${stderr}`);
    }

    await safeReply(ctx, "✅ Dashboard updated successfully!", {
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    });
  });
});

// unknown slash commands (help)
bot.hears(/^\/(\w+)/, async (ctx) => {
  const threadId = ctx.message?.message_thread_id;
  const cmd = ctx.match[1].toLowerCase();
  if (threadId !== ALLOWED_TOPIC_ID) return;

  const known = ["ask", "battery", "reboot", "restart", "speed", "usage", "updatec", "updated"];
  if (known.includes(cmd)) return;

  return safeReply(
    ctx,
    "<b>🍁 Unknown Command 🍁</b>\n" +
      "<b>✹ Ask Gemini AI</b>\n<i>-> <code>/ask prompt</code></i>\n" +
      "<b>✹ Battery Status</b>\n<i>-> /battery</i>\n" +
      "<b>✹ Data Usage</b>\n<i>-> /usage</i>\n" +
      "<b>✹ Reboot Device</b>\n<i>-> /reboot</i>\n" +
      "<b>✹ Restart Server</b>\n<i>-> /restart</i>\n" +
      "<b>✹ Speed Test</b>\n<i>-> /speed</i>\n" +
      "<b>✹ Update Core</b>\n<i>-> /updatec</i>\n" +
      "<b>✹ Update Dashboard</b>\n<i>-> /updated</i>",
    {
      parse_mode: "HTML",
      reply_to_message_id: ctx.message?.message_id,
      message_thread_id: threadId,
    }
  );
});

// ==========================
// Daily data usage report (cron)
// ==========================
cron.schedule("59 23 * * *", async () => {
  try {
    const chatId = parseInt(process.env.ALLOWED_CHAT_ID);
    const topicId = parseInt(process.env.ALLOWED_TOPIC_IDS || "0");

    const vnstat = spawn("vnstat", ["--json"]);
    let output = "";
    let errOutput = "";

    vnstat.stdout.on("data", (data) => {
      output += data.toString();
    });

    vnstat.stderr.on("data", (data) => {
      errOutput += data.toString();
    });

    vnstat.on("close", async (code) => {
      if (code !== 0 || errOutput) {
        return safeSend(bot, chatId, `❌ Daily Usage Error:\n<pre>${escapeHTML(errOutput || `Exited with code ${code}`)}</pre>`, {
          parse_mode: "HTML",
          message_thread_id: topicId,
        });
      }

      try {
        const data = JSON.parse(output);

        const ifaceData = data.interfaces
          .map((i) => ({
            name: i.name,
            total: (i.traffic?.total?.rx || 0) + (i.traffic?.total?.tx || 0),
          }))
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
          if (!obj || typeof obj.rx !== "number" || typeof obj.tx !== "number") return null;
          const up = obj.tx;
          const down = obj.rx;
          const total = up + down;
          return {
            up: formatBytes(up),
            down: formatBytes(down),
            total: formatBytes(total),
          };
        };

        const todayData = extractTraffic(traffic?.day?.at(-1));
        const usageReport = `<b>📊 Daily Usage Report (interface: ${iface})</b>\n\n` +
          `📅 <b>Today</b>\n` +
          `⏫ Upload: ${todayData?.up || "-"}` + `\n` +
          `⏬ Download: ${todayData?.down || "-"}` + `\n` +
          `📦 Total: ${todayData?.total || "-"}`;

        await safeSend(bot, chatId, usageReport, {
          parse_mode: "HTML",
          message_thread_id: topicId,
        });
      } catch (e) {
        await safeSend(bot, chatId, `❌ Failed to parse vnstat:\n<pre>${escapeHTML(e.message)}</pre>`, {
          parse_mode: "HTML",
          message_thread_id: topicId,
        });
      }
    });
  } catch (err) {
    console.error("Daily report error:", err && (err.message || err));
    appendLogLine(`Daily report error: ${err && (err.stack || err.message)}`);
  }
});

// ==========================
// Launch bot
// ==========================
bot.launch().then(() => {
  console.log("Bot launched");
  appendLogLine("Bot launched");
}).catch((err) => {
  console.error("Failed to launch bot:", err && (err.stack || err.message));
  appendLogLine(`Failed to launch bot: ${err && (err.stack || err.message)}`);
});

// Graceful stop (optional)
process.once("SIGINT", () => {
  appendLogLine("SIGINT received, stopping bot...");
  bot.stop("SIGINT");
});
process.once("SIGTERM", () => {
  appendLogLine("SIGTERM received, stopping bot...");
  bot.stop("SIGTERM");
});
