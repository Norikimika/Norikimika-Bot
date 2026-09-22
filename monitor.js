// ====================== CONFIG ======================
require('dotenv').config();
const fetch = (...args) => import('node-fetch').then(({ default: fetch }) => fetch(...args));
const fs = require('fs');
const path = require('path');
const os = require('os');
const cron = require("node-cron");
const { exec, spawn } = require("child_process");
const escapeHTML = require("escape-html");

// ====== Tokens & IDs ======
const TELEGRAM_BOT_TOKEN = process.env.BOT_TOKEN;
const TELEGRAM_CHAT_ID = process.env.CHAT_ID;
const TOPIC_ID_USAGE = process.env.TOPIC_ID_USAGE;
const TOPIC_ID_STATUS = process.env.TOPIC_ID_STATUS;
const TOPIC_ID_SPEED = process.env.TOPIC_ID_SPEED;

// ====== Logger ======
const LOG_FILE = path.join(os.homedir(), 'SERVER', 'Backup', 'monitor.log');

function log(msg) {
  try {
    fs.appendFileSync(
      LOG_FILE,
      `[${new Date().toLocaleString()}] ${msg}\n`
    );
  } catch (err) {
    console.error("Failed to write log:", err.message);
  }
}

// ====== Telegram Sender ======
async function sendToTelegram(message, topicId) {
  const telegramUrl = `https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/sendMessage`;
  const payload = {
    chat_id: TELEGRAM_CHAT_ID,
    text: message,
    parse_mode: "HTML",
    message_thread_id: topicId,
    disable_web_page_preview: true
  };

  try {
    const res = await fetch(telegramUrl, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload)
    });

    if (!res.ok) {
      const errorText = await res.text();
      log(`❌ Telegram failed: ${res.status} - ${errorText}`);
    }
  } catch (err) {
    log(`❌ Telegram request failed: ${err.message}`);
  }
}

// ================== STATE VARIABLES ==================
const STATE_FILE = path.join(os.homedir(), 'SERVER', 'Backup', 'monitor.json');

let monitorState = {
  battery80Sent: false,
  battery20Sent: false,
  lastStatus: "UNKNOWN",
  chargeStartTime: Date.now(),
  useStartTime: Date.now(),
};

try {
  if (fs.existsSync(STATE_FILE)) {
    const raw = fs.readFileSync(STATE_FILE, 'utf8').trim();
    if (raw) {
      monitorState = { ...monitorState, ...JSON.parse(raw) };
    }
  }
} catch (err) {
  log(`⚠️ monitor.json corrupted, resetting. ${err.message}`);
}

function saveMonitorState() {
  try {
    const tmp = STATE_FILE + ".tmp";
    fs.writeFileSync(tmp, JSON.stringify(monitorState, null, 2));
    fs.renameSync(tmp, STATE_FILE);
  } catch (err) {
    log(`Failed to save monitor state: ${err.message}`);
  }
}

// ================== UTILITIES ==================
function formatDuration(startTime) {
  if (!startTime) return "0h 0m 0s";
  const diffMs = Date.now() - startTime;
  const totalSeconds = Math.floor(diffMs / 1000);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  return `${hours}h ${minutes}m ${seconds}s`;
}

// ================== USAGE REPORT ==================
async function sendUsageReport() {
  log("Running scheduled daily usage...");

  return new Promise((resolve) => {
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
        log(`Daily usage vnstat failed: ${errOutput || `code ${code}`}`);
        await sendToTelegram("❌ Failed to get daily usage", TOPIC_ID_USAGE);
        return resolve();
      }

      try {
        const data = JSON.parse(output);

        const iface = "wlan0";
        const ifaceData =
          data.interfaces.find((i) => i.name === iface) ||
          data.interfaces[0];

        const traffic = ifaceData?.traffic;

        const formatBytes = (kB) => {
          if (kB < 1024 * 1024) return `${kB.toFixed(2)} KB`;
          if (kB < 1024 * 1024 * 1024)
            return `${(kB / 1024 / 1024).toFixed(2)} MB`;
          if (kB < 1024 * 1024 * 1024 * 1024)
            return `${(kB / 1024 / 1024 / 1024).toFixed(2)} GB`;

          return `${(kB / 1024 / 1024 / 1024 / 1024).toFixed(2)} TB`;
        };

        const extractTraffic = (obj) => {
          if (!obj) return null;

          const up = obj.tx || 0;
          const down = obj.rx || 0;

          return {
            up: formatBytes(up),
            down: formatBytes(down),
            total: formatBytes(up + down),
          };
        };

        const todayData = extractTraffic(
          traffic?.day?.[traffic.day.length - 1]
        );

        const usageReport =
          `<b>📊 Daily Usage Report (${iface})</b>\n<blockquote>` +
          (todayData
            ? `📅 <b>Today</b>\nUpload : ${todayData.up}\nDownload : ${todayData.down}\nTotal : ${todayData.total}`
            : "No data") +
          `</blockquote>`;

        await sendToTelegram(usageReport, TOPIC_ID_USAGE);
      } catch (err) {
        log(`Daily usage parse failed: ${err.message}`);
      }

      resolve();
    });
  });
}

// ================== BATTERY LEVEL ==================
async function checkBatteryLevel() {
  exec("termux-battery-status", async (error, stdout) => {
    if (error) {
      log(`Battery monitor failed: ${error.message}`);
      return;
    }

    try {
      const data = JSON.parse(stdout);

      const level = data.percentage;
      const status = data.status.toUpperCase();
      const temperature = Math.round(data.temperature);

      if (status === "CHARGING" || status === "FULL") {
        if (monitorState.lastStatus === "DISCHARGING" || monitorState.lastStatus === "NOT_CHARGING") {
          monitorState.chargeStartTime = Date.now();
          saveMonitorState();
        }
      } else if (status === "DISCHARGING" || status === "NOT_CHARGING") {
        if (monitorState.lastStatus === "CHARGING" || monitorState.lastStatus === "FULL") {
          monitorState.useStartTime = Date.now();
          saveMonitorState();
        }
      }

      if (status !== "UNKNOWN") {
        monitorState.lastStatus = status;
        saveMonitorState();
      }

      if (level < 100 && monitorState.battery80Sent) {
        monitorState.battery80Sent = false;
        saveMonitorState();
      }
      if (level > 20 && monitorState.battery20Sent) {
        monitorState.battery20Sent = false;
        saveMonitorState();
      }

      if (level >= 100 && !monitorState.battery80Sent) {
        monitorState.battery80Sent = true;
        saveMonitorState();
        const chargeDuration = formatDuration(monitorState.chargeStartTime);

        await sendToTelegram(
          `<b>🔋 Battery Fully Charged</b>\n<blockquote>Percentage : ${level}%\nStatus : ${status}\nTemperature : ${temperature}°C\nDuration : ${chargeDuration}</blockquote>`,
          TOPIC_ID_STATUS
        );
      }

      if (level <= 20 && !monitorState.battery20Sent) {
        monitorState.battery20Sent = true;
        saveMonitorState();
        const usageDuration = formatDuration(monitorState.useStartTime);

        await sendToTelegram(
          `<b>🪫 Low Battery Warning</b>\n<blockquote>Percentage : ${level}%\nStatus : ${status}\nTemperature : ${temperature}°C\nDuration : ${usageDuration}</blockquote>`,
          TOPIC_ID_STATUS
        );
      }
    } catch (err) {
      log(`Battery monitor parse error: ${err.message}`);
    }
  });
}

// ================== AUTO SPEEDTEST ==================
async function runAutoSpeedtest() {
  log("Running scheduled speedtest...");

  exec("speedtest-go --json", async (error, stdout) => {
    if (error) {
      log(`Scheduled speedtest failed: ${error.message}`);
      return;
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

      await sendToTelegram(summary.trim(), TOPIC_ID_SPEED);

    } catch (e) {
      log(`Scheduled speedtest JSON parse failed: ${e.message}`);
    }
  });
}

// ================== OPENROUTER MODELS ==================
const MODELS_FILE = path.join(os.homedir(), 'SERVER', 'Backup', 'openrouter_models.json');
const MODELS_API = 'https://openrouter.ai/api/v1/models';

async function checkOpenRouterModels() {
  log("Checking OpenRouter models...");
  try {
    const res = await fetch(MODELS_API);
    if (!res.ok) {
      log(`OpenRouter models fetch failed: ${res.status}`);
      return;
    }
    const data = await res.json();
    const current = (data.data || [])
      .map(m => ({ id: m.id, name: m.name, created: m.created, description: m.description }))
      .sort((a, b) => (a.id > b.id ? 1 : -1));
    if (!current.length) return;

    let known = [];
    try {
      if (fs.existsSync(MODELS_FILE)) {
        known = JSON.parse(fs.readFileSync(MODELS_FILE, 'utf8'));
      }
    } catch (err) {
      log(`openrouter_models.json corrupted, starting fresh. ${err.message}`);
    }

    const knownSet = new Set(known.map(m => m.id));
    const newModels = current.filter(m => !knownSet.has(m.id));

    fs.writeFileSync(MODELS_FILE, JSON.stringify(current, null, 2));

    if (!known.length) {
      log(`OpenRouter models initialized with ${current.length} models`);
      return;
    }

    if (newModels.length) {
      newModels.sort((a, b) => (a.created || 0) - (b.created || 0));
      const list = newModels.slice(0, 20).map(m => {
        const date = m.created
          ? new Date(m.created * 1000).toLocaleDateString('en-GB', { day: '2-digit', month: 'short', year: 'numeric' })
          : 'Unknown';
        const desc = m.description
          ? (m.description.length > 180 ? m.description.slice(0, 180).trim() + '…' : m.description)
          : 'No description';
        return { n: m.name || m.id, i: m.id, d: date, t: desc };
      });
      const extra = newModels.length > 20 ? `...and ${newModels.length - 20} more` : '';

      // render image (Linear-style cards, JS-only via @napi-rs/canvas)
      const imgPath = path.join(os.tmpdir(), 'openrouter_models.png');
      try {
        const { createCanvas, GlobalFonts } = require('@napi-rs/canvas');
        const F = '/data/data/com.termux/files/usr/share/fonts/TTF/';
        GlobalFonts.registerFromPath(F + 'DejaVuSans.ttf', 'Body');
        GlobalFonts.registerFromPath(F + 'DejaVuSans-Bold.ttf', 'Bold');
        GlobalFonts.registerFromPath(F + 'DejaVuSans-Oblique.ttf', 'Italic');

        const W = 640, pad = 32;
        const tF = '38px Bold', mF = '19px Body', sF = '18px Italic', pF = '15px Bold';
        const cardH = 18 + 38 + 16 + 18 + 14 + 3 * (19 + 8) + pad;
        const H = pad + 70 + list.length * (cardH + 20) + (extra ? pad + 30 : 0) + pad;
        const c = createCanvas(W, H), ctx = c.getContext('2d');

        const wrap = (text, font, maxW) => {
          ctx.font = font;
          const out = [];
          let line = '';
          for (const w of text.split(' ')) {
            const t = (line + ' ' + w).trim();
            if (ctx.measureText(t).width <= maxW) line = t;
            else { out.push(line); line = w; }
          }
          if (line) out.push(line);
          return out;
        };

        ctx.fillStyle = '#08090a';
        ctx.fillRect(0, 0, W, H);
        let y = pad;
        ctx.fillStyle = '#f7f8f8'; ctx.font = tF;
        ctx.fillText('New OpenRouter Models', pad, y + 30);
        y += 48;
        ctx.fillStyle = '#8a8f98'; ctx.font = mF;
        ctx.fillText(`${list.length} new models added`, pad, y + 19);
        y += 43;

        for (const m of list) {
          ctx.fillStyle = '#191a1b';
          ctx.strokeStyle = 'rgba(255,255,255,0.08)';
          ctx.lineWidth = 1;
          ctx.beginPath();
          ctx.roundRect(pad, y, W - pad * 2, cardH, 12);
          ctx.fill(); ctx.stroke();

          let cy = y + 18;
          ctx.font = pF;
          const pw = ctx.measureText('NEW').width + 18;
          ctx.fillStyle = '#5e6ad2';
          ctx.beginPath();
          ctx.roundRect(pad + 18, cy, pw, 26, 999);
          ctx.fill();
          ctx.fillStyle = '#f7f8f8';
          ctx.fillText('NEW', pad + 27, cy + 18);

          ctx.fillStyle = '#f7f8f8'; ctx.font = tF;
          ctx.fillText(m.n, pad + 34 + pw, cy + 30);
          cy += 54;

          ctx.fillStyle = '#8a8f98'; ctx.font = sF;
          ctx.fillText(`${m.i}  ·  ${m.d}`, pad + 18, cy + 18);
          cy += 32;

          ctx.fillStyle = '#d0d6e0'; ctx.font = mF;
          for (const line of wrap(m.t, mF, W - pad * 2 - 36)) {
            ctx.fillText(line, pad + 18, cy + 19);
            cy += 27;
          }
          y += cardH + 20;
        }

        if (extra) {
          ctx.fillStyle = '#8a8f98'; ctx.font = sF;
          ctx.fillText(extra, pad, y + 18);
        }

        fs.writeFileSync(imgPath, c.toBuffer('image/png'));
        const FormData = (await import('form-data')).default;
        const form = new FormData();
        form.append('chat_id', TELEGRAM_CHAT_ID);
        form.append('message_thread_id', TOPIC_ID_STATUS);
        form.append('caption', `<b>🧠 New OpenRouter Models</b> (${newModels.length})`);
        form.append('parse_mode', 'HTML');
        form.append('photo', fs.createReadStream(imgPath));
        const res = await fetch(`https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/sendPhoto`, { method: 'POST', body: form });
        if (!res.ok) log(`photo send failed: ${res.status}`);
      } catch (err) {
        log(`image render failed (${err.message}), fallback to text`);
        await sendToTelegram(
          `<b>🧠 New OpenRouter Models</b> (${newModels.length})\n<blockquote>${list.map(m => `<b>${m.n}</b>\n${m.i} — ${m.d}\n${m.t}`).join('\n\n')}${extra ? '\n\n' + extra : ''}</blockquote>`,
          TOPIC_ID_STATUS
        );
      }
      log(`OpenRouter: ${newModels.length} new model(s)`);
    } else {
      log("OpenRouter: no new models");
    }
  } catch (err) {
    log(`OpenRouter models check failed: ${err.message}`);
  }
}

// ================== MAIN SCHEDULER ==================
function startMonitor() {
  log("Starting monitor service...");
  
  cron.schedule("0 0 * * *", sendUsageReport, { scheduled: true, recoverMissedExecutions: false });
  cron.schedule("0 0,12 * * *", runAutoSpeedtest, { scheduled: true, recoverMissedExecutions: false });
  cron.schedule("* * * * *", checkBatteryLevel, { scheduled: true, recoverMissedExecutions: false });
  cron.schedule("0 4 * * *", checkOpenRouterModels, { scheduled: true, recoverMissedExecutions: false });
}

module.exports = startMonitor;
