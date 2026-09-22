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
async function checkOpenRouterModels() {
  log("Checking OpenRouter models...");
  try {
    const res = await fetch('https://openrouter.ai/api/v1/models?sort=newest&limit=50');
    if (!res.ok) {
      log(`OpenRouter models fetch failed: ${res.status}`);
      return;
    }
    const data = await res.json();
    const cutoff = Date.now() / 1000 - 48 * 3600;
    const newModels = (data.data || [])
      .filter(m => (m.created || 0) >= cutoff)
      .sort((a, b) => (a.created || 0) - (b.created || 0));

    if (!newModels.length) {
      log("OpenRouter: no new models in last 48h");
      return;
    }

    const list = newModels.slice(0, 20).map(m => {
      const date = m.created
        ? new Date(m.created * 1000).toLocaleDateString('en-GB', { day: '2-digit', month: 'short', year: 'numeric' })
        : 'Unknown';
      const desc = m.description
        ? (m.description.length > 180 ? m.description.slice(0, 180).trim().replace(/[.,;:\s]+$/, '') + '…' : m.description)
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

      const W = 640, pad = 32, gap = 16;
      const tF = '24px Bold', tS = 24, mF = '18px Body', mS = 18, sF = '16px Italic', sS = 16, pF = '13px Bold';
      const cw = W - pad * 2;
      const inner = cw - 36; // card inner padding

      const canvas0 = createCanvas(W, 10), ctx0 = canvas0.getContext('2d');
      const measure = (text, font, maxW) => {
        ctx0.font = font;
        const out = [];
        let line = '';
        for (const w of text.split(' ')) {
          const t = (line + ' ' + w).trim();
          if (ctx0.measureText(t).width <= maxW) line = t;
          else { out.push(line); line = w; }
        }
        if (line) out.push(line);
        return out;
      };

      // pass 1: layout every card, collect lines + height
      const layouts = list.map(m => {
        const title = measure(m.n, tF, inner);
        const desc = measure(m.t, mF, inner);
        const h = 20 + 22 + 12 + (title.length * (tS + 6)) + 10 + (sS + 8) + 12 + (desc.length * (mS + 8)) + 20;
        return { m, title, desc, h };
      });
      const maxH = Math.max(...layouts.map(l => l.h));
      const H = pad + 36 + 8 + sS + 12 + layouts.length * (maxH + gap) - gap + pad;

      const c = createCanvas(W, H), ctx = c.getContext('2d');
      ctx.fillStyle = '#08090a';
      ctx.fillRect(0, 0, W, H);

      let y = pad;
      ctx.fillStyle = '#f7f8f8'; ctx.font = '30px Bold';
      ctx.fillText('New OpenRouter Models', pad, y + 30);
      y += 44;
      ctx.fillStyle = '#8a8f98'; ctx.font = sF;
      ctx.fillText(`${list.length} new models added`, pad, y + 16);
      y += 36;

      for (const { m, title, desc } of layouts) {
        ctx.fillStyle = '#191a1b';
        ctx.strokeStyle = 'rgba(255,255,255,0.08)';
        ctx.lineWidth = 1;
        ctx.beginPath();
        ctx.roundRect(pad, y, cw, maxH, 12);
        ctx.fill(); ctx.stroke();

        let cy = y + 20;
        // pill
        ctx.font = pF;
        const pw = ctx.measureText('NEW').width + 20;
        ctx.fillStyle = '#5e6ad2';
        ctx.beginPath();
        ctx.roundRect(pad + 18, cy, pw, 22, 999);
        ctx.fill();
        ctx.fillStyle = '#ffffff';
        ctx.fillText('NEW', pad + 28, cy + 15);
        cy += 22 + 12;

        // title
        ctx.fillStyle = '#f7f8f8'; ctx.font = tF;
        for (const line of title) {
          ctx.fillText(line, pad + 18, cy + tS);
          cy += tS + 6;
        }
        cy += 10;

        // meta
        ctx.fillStyle = '#8a8f98'; ctx.font = sF;
        ctx.fillText(`${m.i}  ·  ${m.d}`, pad + 18, cy + sS);
        cy += sS + 12;

        // desc
        ctx.fillStyle = '#d0d6e0'; ctx.font = mF;
        for (const line of desc) {
          ctx.fillText(line, pad + 18, cy + mS);
          cy += mS + 8;
        }
        y += maxH + gap;
      }

      if (extra) {
        ctx.fillStyle = '#8a8f98'; ctx.font = sF;
        ctx.fillText(extra, pad, y + 16);
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
      log(`image render failed (${err.message})`);
    }
    log(`OpenRouter: ${newModels.length} new model(s)`);
  } catch (err) {
    log(`OpenRouter models check failed: ${err.message}`);
  }
}

// ================== OPENROUTER TOP WEEKLY ==================
function renderRankingsImage(rows) {
  const { createCanvas, GlobalFonts } = require('@napi-rs/canvas');
  const F = '/data/data/com.termux/files/usr/share/fonts/TTF/';
  GlobalFonts.registerFromPath(F + 'DejaVuSans.ttf', 'Body');
  GlobalFonts.registerFromPath(F + 'DejaVuSans-Bold.ttf', 'Bold');
  GlobalFonts.registerFromPath(F + 'DejaVuSans-Oblique.ttf', 'Italic');

  const W = 640, pad = 32, gap = 12;
  const cardH = 62;
  const H = pad + 48 + 26 + 10 + rows.length * (cardH + gap) - gap + pad;
  const c = createCanvas(W, H), ctx = c.getContext('2d');

  ctx.fillStyle = '#08090a';
  ctx.fillRect(0, 0, W, H);
  let y = pad;
  ctx.fillStyle = '#f7f8f8'; ctx.font = '30px Bold';
  ctx.fillText('Top OpenRouter Models', pad, y + 30);
  y += 40;
  ctx.fillStyle = '#8a8f98'; ctx.font = '17px Italic';
  ctx.fillText('Weekly usage by tokens processed', pad, y + 17);
  y += 36;

  rows.forEach((r, i) => {
    ctx.fillStyle = '#191a1b';
    ctx.strokeStyle = 'rgba(255,255,255,0.08)';
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.roundRect(pad, y, W - pad * 2, cardH, 12);
    ctx.fill(); ctx.stroke();

    // rank pill
    const top = i < 3;
    ctx.font = '18px Bold';
    const rw = ctx.measureText(String(i + 1)).width + 30;
    ctx.fillStyle = top ? '#5e6ad2' : 'rgba(255,255,255,0.06)';
    ctx.beginPath();
    ctx.roundRect(pad + 16, y + cardH / 2 - 16, rw, 32, 999);
    ctx.fill();
    ctx.fillStyle = top ? '#ffffff' : '#8a8f98';
    ctx.fillText(String(i + 1), pad + 31, y + cardH / 2 + 6);

    // name + provider
    ctx.fillStyle = '#f7f8f8'; ctx.font = '21px Body';
    ctx.fillText(r.name, pad + 16 + rw + 18, y + 27);
    ctx.fillStyle = '#8a8f98'; ctx.font = '16px Italic';
    ctx.fillText(r.author, pad + 16 + rw + 18, y + 49);

    y += cardH + gap;
  });

  return c.toBuffer('image/png');
}

async function checkTopWeekly() {
  log("Checking OpenRouter top weekly...");
  try {
    const res = await fetch('https://openrouter.ai/api/v1/models?sort=top-weekly&limit=10');
    if (!res.ok) {
      log(`Rankings fetch failed: ${res.status}`);
      return;
    }
    const data = await res.json();
    const rows = (data.data || []).slice(0, 10).map(m => ({
      name: (m.name || m.id).replace(/^[^:]+:\s*/, ''),
      author: (m.id || '').split('/')[0],
    }));
    if (!rows.length) {
      log('Rankings: no rows parsed');
      return;
    }
    log(`Rankings: ${rows.length} models, #1 ${rows[0].name}`);
    const png = renderRankingsImage(rows);
    const FormData = (await import('form-data')).default;
    const form = new FormData();
    form.append('chat_id', TELEGRAM_CHAT_ID);
    form.append('message_thread_id', TOPIC_ID_STATUS);
    form.append('caption', `<b>🏆 OpenRouter Top Weekly Models</b>`);
    form.append('parse_mode', 'HTML');
    form.append('photo', png, { filename: 'top_weekly.png', contentType: 'image/png' });
    const send = await fetch(`https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/sendPhoto`, { method: 'POST', body: form });
    if (!send.ok) log(`top weekly photo send failed: ${send.status}`);
    else log('Rankings: weekly image sent');
  } catch (err) {
    log(`Rankings check failed: ${err.message}`);
  }
}


function startMonitor() {
  log("Starting monitor service...");
  
  cron.schedule("0 0 * * *", sendUsageReport, { scheduled: true, recoverMissedExecutions: false });
  cron.schedule("0 0,12 * * *", runAutoSpeedtest, { scheduled: true, recoverMissedExecutions: false });
  cron.schedule("* * * * *", checkBatteryLevel, { scheduled: true, recoverMissedExecutions: false });
  cron.schedule("0 4 * * *", checkOpenRouterModels, { scheduled: true, recoverMissedExecutions: false });
  cron.schedule("0 5 * * *", checkTopWeekly, { scheduled: true, recoverMissedExecutions: false });
}

module.exports = startMonitor;
