// ====================== CONFIG ======================
require('dotenv').config();
const nativeFetch = globalThis.fetch;
if (typeof nativeFetch !== 'function' || typeof globalThis.FormData !== 'function' || typeof globalThis.Blob !== 'function') {
  throw new Error('Node 24+ with native fetch, FormData, and Blob required');
}
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
    const res = await nativeFetch(telegramUrl, {
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

function renderNewModelsImage(list, totalCount = list.length, extra = '') {
  const { createCanvas, GlobalFonts } = require('@napi-rs/canvas');
  const F = '/data/data/com.termux/files/usr/share/fonts/TTF/';
  GlobalFonts.registerFromPath(F + 'DejaVuSans.ttf', 'Body');
  GlobalFonts.registerFromPath(F + 'DejaVuSans-Bold.ttf', 'Bold');
  GlobalFonts.registerFromPath(F + 'DejaVuSansMono.ttf', 'Mono');
  GlobalFonts.registerFromPath(F + 'DejaVuSansMono-Bold.ttf', 'MonoBold');

  const W = 1200, pad = 48, headerH = 174, rowH = 152, gap = 8;
  const contentW = W - pad * 2;
  const H = headerH + list.length * (rowH + gap) - gap + 58;
  const c = createCanvas(W, H), ctx = c.getContext('2d');
  const colors = {
    bg: '#050706', panel: '#0B0E0C', row: '#090C0A', line: '#202621',
    white: '#F7F9F7', muted: '#A4ADA6', green: '#76B900', bright: '#B7F34A',
    leader: '#10180C'
  };
  const canvas0 = createCanvas(W, 10), ctx0 = canvas0.getContext('2d');
  const measure = (text, font, maxW) => {
    ctx0.font = font;
    const lines = [];
    let line = '';
    const pushWord = (word) => {
      if (ctx0.measureText(word).width <= maxW) {
        const next = line ? `${line} ${word}` : word;
        if (ctx0.measureText(next).width <= maxW) line = next;
        else { if (line) lines.push(line); line = word; }
        return;
      }
      for (const char of word) {
        const next = line + char;
        if (ctx0.measureText(next).width <= maxW) line = next;
        else { if (line) lines.push(line); line = char; }
      }
    };
    for (const word of String(text).split(' ')) pushWord(word);
    if (line) lines.push(line);
    return lines;
  };
  const fitText = (text, font, maxW) => {
    ctx0.font = font;
    let value = String(text ?? '');
    if (ctx0.measureText(value).width <= maxW) return value;
    while (value.length && ctx0.measureText(`${value}…`).width > maxW) value = value.slice(0, -1);
    return `${value}…`;
  };
  const formatContext = (value) => {
    const n = Number(value);
    if (!Number.isFinite(n) || n <= 0) return 'Unknown';
    if (n >= 1e6) return `${(n / 1e6).toFixed(n % 1e6 ? 1 : 0)}M`;
    if (n >= 1e3) return `${Math.round(n / 1e3)}K`;
    return String(Math.round(n));
  };
  const formatPrice = (value) => {
    if (value === null || value === undefined) return 'N/A';
    if (typeof value === 'string' && value.trim() === '') return 'N/A';
    const n = Number(value);
    if (!Number.isFinite(n) || n < 0) return 'N/A';
    if (n === 0) return 'Free';
    const perMillion = n * 1e6;
    return perMillion < 0.01 ? `$${perMillion.toFixed(4)}` : `$${perMillion.toFixed(2)}`;
  };
  const descriptionLines = (text) => {
    const lines = measure(text, '20px Body', 1000);
    if (lines.length <= 2) return lines;
    return [lines[0], `${lines[1].replace(/[.,;:\s]+$/, '')}…`];
  };

  ctx.fillStyle = colors.bg;
  ctx.fillRect(0, 0, W, H);
  ctx.fillStyle = colors.green;
  ctx.fillRect(0, 0, W, 8);

  const now = new Date();
  const dateParts = Object.fromEntries(new Intl.DateTimeFormat('en-GB', {
    timeZone: 'Asia/Makassar', day: '2-digit', month: 'long', year: 'numeric'
  }).formatToParts(now).filter(p => p.type !== 'literal').map(p => [p.type, p.value]));

  ctx.fillStyle = colors.white;
  ctx.font = '600 48px Body';
  ctx.fillText('New OpenRouter Models', pad, 73);
  ctx.font = '22px Mono';
  ctx.fillStyle = colors.bright;
  ctx.fillText('NEWEST RELEASES FIRST', pad, 111);
  ctx.textAlign = 'right';
  ctx.fillStyle = colors.muted;
  ctx.font = '18px Mono';
  ctx.fillText(`${totalCount} new models`, W - pad, 72);
  ctx.font = '16px Body';
  ctx.fillText(`${dateParts.day} ${dateParts.month.toUpperCase()} ${dateParts.year}`, W - pad, 104);
  ctx.textAlign = 'left';

  ctx.fillStyle = colors.panel;
  ctx.beginPath(); ctx.roundRect(pad, 130, contentW, 34, 2); ctx.fill();
  ctx.fillStyle = colors.muted;
  ctx.font = '600 14px Mono';
  ctx.fillText('#', pad + 18, 153);
  ctx.fillText('MODEL / PROVIDER / RELEASED', 146, 153);
  ctx.textAlign = 'right';
  ctx.fillText('CONTEXT', 700, 153);
  ctx.fillText('INPUT / M', 835, 153);
  ctx.fillText('OUTPUT / M', 1030, 153);
  ctx.textAlign = 'left';

  list.forEach((m, i) => {
    const y = headerH + i * (rowH + gap);
    ctx.fillStyle = i === 0 ? colors.leader : i % 2 ? colors.row : colors.bg;
    ctx.fillRect(pad, y, contentW, rowH);
    if (i === 0) {
      ctx.fillStyle = colors.green;
      ctx.fillRect(pad, y, 5, rowH);
    }
    ctx.fillStyle = i === 0 ? colors.bright : colors.muted;
    ctx.font = '700 24px MonoBold';
    ctx.fillText(String(i + 1).padStart(2, '0'), pad + 18, y + 48);
    if (i === 0) {
      ctx.fillStyle = colors.green;
      ctx.font = '600 11px Mono';
      ctx.fillText('LATEST', pad + 18, y + 72);
    }

    ctx.fillStyle = colors.white;
    ctx.font = '600 30px Body';
    ctx.fillText(fitText(m.n, '600 30px Body', 470), 146, y + 39);
    ctx.fillStyle = colors.muted;
    ctx.font = '16px Body';
    ctx.fillText(fitText(`${m.i} · ${m.d}`, '16px Body', 520), 146, y + 65);

    ctx.textAlign = 'right';
    ctx.fillStyle = colors.white;
    ctx.font = '600 20px Mono';
    ctx.fillText(formatContext(m.c), 700, y + 43);
    ctx.fillText(formatPrice(m.p), 835, y + 43);
    ctx.fillText(formatPrice(m.q), 1030, y + 43);
    ctx.textAlign = 'left';

    ctx.fillStyle = i === 0 ? '#DDE8DD' : '#C7D0C9';
    ctx.font = '20px Body';
    const desc = descriptionLines(m.t);
    ctx.fillText(desc[0] || 'No description', 146, y + 96);
    if (desc[1]) ctx.fillText(desc[1], 146, y + 122);

    ctx.strokeStyle = colors.line;
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(pad, y + rowH);
    ctx.lineTo(W - pad, y + rowH);
    ctx.stroke();
  });

  if (extra) {
    ctx.fillStyle = colors.muted;
    ctx.font = '15px Body';
    ctx.fillText(extra, pad, H - 25);
  }
  return c.toBuffer('image/png');
}

// ================== OPENROUTER MODELS ==================
function stripLeadingModelName(name, description) {
  const original = String(description ?? '').trim();
  if (!original) return 'No description';

  const candidates = [
    String(name ?? '').trim(),
    String(name ?? '').replace(/^[^:]+:[ \t]*/, '').trim()
  ];
  for (const candidate of [...new Set(candidates)]) {
    if (!candidate || !original.toLowerCase().startsWith(candidate.toLowerCase())) continue;
    let rest = original.slice(candidate.length).trimStart();
    rest = rest.replace(/^[:,-][ \t]*/, '');
    rest = rest.replace(/^(?:is|are)[ \t]+/i, '').trimStart();
    if (rest) return rest;
  }
  return original;
}

async function checkOpenRouterModels() {
  log("Checking OpenRouter models...");
  try {
    const res = await nativeFetch('https://openrouter.ai/api/v1/models?sort=newest&output_modalities=all&limit=1000');
    if (!res.ok) {
      log(`OpenRouter models fetch failed: ${res.status}`);
      return;
    }
    const data = await res.json();
    const models = data.data || [];
    const cutoff = Date.now() / 1000 - 48 * 3600;
    const lastNotified = monitorState.lastOpenRouterNotified || 0;
    const notifiedIds = new Set(monitorState.notifiedOpenRouterModelIds || []);
    const newModels = models
      .filter(m => (m.created || 0) >= cutoff && ((m.created || 0) > lastNotified || !notifiedIds.has(m.id)))
      .sort((a, b) => (b.created || 0) - (a.created || 0));

    if (!newModels.length) {
      log("OpenRouter: no new models since last check");
      return;
    }

    const visibleModels = newModels.slice(0, 20);
    const list = visibleModels.map(m => {
      const date = m.created
        ? new Date(m.created * 1000).toLocaleDateString('en-GB', { day: '2-digit', month: 'short', year: 'numeric' })
        : 'Unknown';
      const desc = m.description
        ? stripLeadingModelName(m.name || m.id, m.description)
        : 'No description';
      const shortDesc = desc.length > 220
        ? `${desc.slice(0, 220).trim().replace(/[.,;:\s]+$/, '')}…`
        : desc;
      return { n: m.name || m.id, i: m.id, d: date, t: shortDesc, c: m.context_length || m.top_provider?.context_length || 0, p: m.pricing?.prompt || '0', q: m.pricing?.completion || '0' };
    });
    const extra = newModels.length > 20 ? `${newModels.length - 20} more models in this update` : 'Source: OpenRouter';

    try {
      const png = renderNewModelsImage(list, newModels.length, extra);
      const form = new globalThis.FormData();
      form.append('chat_id', TELEGRAM_CHAT_ID);
      form.append('message_thread_id', TOPIC_ID_STATUS);
      form.append('caption', `<b>New OpenRouter Models</b> (${newModels.length})`);
      form.append('parse_mode', 'HTML');
      form.append('photo', new Blob([png], { type: 'image/png' }), 'openrouter_models.png');
      const res = await nativeFetch(`https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/sendPhoto`, { method: 'POST', body: form });
      const newestCreated = Math.max(...newModels.map(m => m.created || 0));
      if (res.ok) {
        const sentIds = new Set(monitorState.notifiedOpenRouterModelIds || []);
        for (const model of visibleModels) sentIds.add(model.id);
        monitorState.notifiedOpenRouterModelIds = [...sentIds].slice(-500);
        monitorState.lastOpenRouterNotified = Math.max(
          monitorState.lastOpenRouterNotified || 0,
          newestCreated
        );
        saveMonitorState();
      } else {
        log(`document send failed: ${res.status}`);
      }
    } catch (err) {
      log(`image render failed (${err.message})`);
    }
    log(`OpenRouter: ${newModels.length} new model(s)`);
  } catch (err) {
    log(`OpenRouter models check failed: ${err.message}`);
  }
}

function renderLeaderboardImage(cfg) {
  const { createCanvas, GlobalFonts } = require('@napi-rs/canvas');
  const F = '/data/data/com.termux/files/usr/share/fonts/TTF/';
  GlobalFonts.registerFromPath(F + 'DejaVuSans.ttf', 'Body');
  GlobalFonts.registerFromPath(F + 'DejaVuSans-Bold.ttf', 'Bold');
  GlobalFonts.registerFromPath(F + 'DejaVuSansMono.ttf', 'Mono');
  GlobalFonts.registerFromPath(F + 'DejaVuSansMono-Bold.ttf', 'MonoBold');

  const W = 1200, pad = 48, headerH = 190, rowH = 90, gap = 8;
  const H = headerH + cfg.rows.length * (rowH + gap) - gap + 48;
  const c = createCanvas(W, H), ctx = c.getContext('2d');
  const colors = {
    bg: '#050706', panel: '#0B0E0C', line: '#202621', white: '#F7F9F7',
    muted: '#A4ADA6', green: '#76B900', greenBright: '#B7F34A'
  };

  ctx.fillStyle = colors.bg;
  ctx.fillRect(0, 0, W, H);
  ctx.fillStyle = colors.green;
  ctx.fillRect(0, 0, W, 8);

  ctx.fillStyle = colors.white;
  ctx.font = '600 48px Body';
  ctx.fillText(cfg.title, pad, 73);
  ctx.font = '22px Mono';
  ctx.fillStyle = colors.greenBright;
  ctx.fillText(cfg.subtitle, pad, 111);
  ctx.textAlign = 'right';
  ctx.fillStyle = colors.muted;
  ctx.font = '18px Mono';
  ctx.fillText(cfg.headerRight[0], W - pad, 72);
  ctx.font = '16px Body';
  ctx.fillText(cfg.headerRight[1], W - pad, 104);
  ctx.textAlign = 'left';

  ctx.fillStyle = colors.panel;
  ctx.beginPath(); ctx.roundRect(pad, 132, W - pad * 2, 34, 2); ctx.fill();
  ctx.fillStyle = colors.muted;
  ctx.font = '600 14px Mono';
  ctx.fillText('RANK', pad + 18, 154);
  ctx.fillText(cfg.modelColumn, 146, 154);
  ctx.textAlign = 'right';
  ctx.fillText(cfg.metricColumn, W - pad - 18, 154);
  ctx.textAlign = 'left';

  const fitText = (text, font, maxW) => {
    ctx.font = font;
    let value = String(text ?? '');
    if (ctx.measureText(value).width <= maxW) return value;
    while (value.length && ctx.measureText(`${value}\u2026`).width > maxW) value = value.slice(0, -1);
    return `${value}\u2026`;
  };
  const maxValue = Math.max(...cfg.rows.map(r => Number(r.value) || 0), 1);
  const modelX = 146, modelMaxW = 590, barX = 780, barW = 190, numberX = W - pad - 18;
  let y = headerH;
  cfg.rows.forEach((r, i) => {
    ctx.fillStyle = i === 0 ? '#10180C' : i % 2 ? '#090C0A' : colors.bg;
    ctx.fillRect(pad, y, W - pad * 2, rowH);
    if (i === 0) {
      ctx.fillStyle = colors.green;
      ctx.fillRect(pad, y, 5, rowH);
    }
    ctx.fillStyle = i === 0 ? colors.greenBright : colors.muted;
    ctx.font = '700 24px MonoBold';
    ctx.fillText(String(i + 1).padStart(2, '0'), pad + 18, y + 51);
    ctx.fillStyle = colors.white;
    ctx.font = '600 27px Body';
    ctx.fillText(fitText(r.name, '600 27px Body', modelMaxW), modelX, y + 39);
    ctx.fillStyle = colors.muted;
    ctx.font = '17px Body';
    ctx.fillText(fitText(r.sub, '17px Body', modelMaxW), modelX, y + 66);
    const barWidth = Math.max(2, barW * (Number(r.value) || 0) / maxValue);
    ctx.fillStyle = colors.line;
    ctx.fillRect(barX, y + 52, barW, 5);
    ctx.fillStyle = i === 0 ? colors.greenBright : colors.green;
    ctx.fillRect(barX, y + 52, barWidth, 5);
    ctx.textAlign = 'right';
    ctx.fillStyle = colors.white;
    ctx.font = '700 28px MonoBold';
    ctx.fillText(r.text, numberX, y + 51);
    ctx.fillStyle = colors.muted;
    ctx.font = '13px Mono';
    ctx.fillText(cfg.valueLabel, numberX, y + 73);
    ctx.textAlign = 'left';
    ctx.strokeStyle = colors.line;
    ctx.lineWidth = 1;
    ctx.beginPath(); ctx.moveTo(pad, y + rowH); ctx.lineTo(W - pad, y + rowH); ctx.stroke();
    y += rowH + gap;
  });

  ctx.fillStyle = colors.muted;
  ctx.font = '15px Body';
  ctx.fillText(cfg.footer, pad, H - 24);
  ctx.textAlign = 'right';
  ctx.font = '600 15px Mono';
  ctx.fillText(cfg.footerRight, W - pad, H - 24);
  return c.toBuffer('image/png');
}

function formatTokens(value) {
  const n = Number(value);
  if (!Number.isFinite(n) || n < 0) return 'N/A';
  if (n >= 1e12) return `${(n / 1e12).toFixed(n % 1e12 ? 1 : 0)}T`;
  if (n >= 1e9) return `${(n / 1e9).toFixed(n % 1e9 ? 1 : 0)}B`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(n % 1e6 ? 1 : 0)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(n % 1e3 ? 1 : 0)}K`;
  return String(Math.round(n));
}

// One renderer for both digests. Two near-identical renderers drift apart on the
// first tweak -- that is what made the Artificial Analysis card stop matching
// OpenRouter, so the layout lives here exactly once and each digest only supplies
// its own titles, column labels and row data.
function renderRankingsImage(rows) {
  const now = new Date();
  const dateParts = Object.fromEntries(new Intl.DateTimeFormat('en-GB', {
    timeZone: 'Asia/Makassar', day: '2-digit', month: 'long', year: 'numeric'
  }).formatToParts(now).filter(p => p.type !== 'literal').map(p => [p.type, p.value]));
  const start = new Date(Date.UTC(Number(dateParts.year), 0, 1));
  const current = new Date(Date.UTC(Number(dateParts.year), new Date(`${dateParts.month} 1, ${dateParts.year}`).getUTCMonth(), Number(dateParts.day)));
  const week = Math.ceil((((current - start) / 86400000) + start.getUTCDay() + 1) / 7);

  return renderLeaderboardImage({
    title: 'OpenRouter weekly',
    subtitle: 'TOP 10 BY TOTAL TOKENS',
    headerRight: [`WEEK ${String(week).padStart(2, '0')} / ${dateParts.year}`, `${dateParts.day} ${dateParts.month.toUpperCase()}`],
    modelColumn: 'MODEL / PROVIDER',
    metricColumn: 'TOKENS PROCESSED',
    valueLabel: 'TOKENS',
    footer: 'Prompt + completion tokens \u00b7 Source: OpenRouter',
    footerRight: 'WEEKLY RANKING',
    rows: rows.map(r => ({ name: r.name, sub: r.author, value: r.tokens, text: formatTokens(r.tokens) })),
  });
}

// Artificial Analysis serves the LLM leaderboard as HTML, and the v2 JSON API
// behind it needs a key we do not have. Parse the table rows directly. The header
// is a two-row group -- a features ribbon above the real column names -- and both
// would otherwise parse as a model, so they are dropped by name. Data columns are
// then read at fixed positions, since every row keeps all nine cells and marks an
// unmeasured one with "--". Scores carry footnote asterisks ("33 *").
function parseAaTable(html) {
  const rows = [];
  const trs = String(html || '').match(/<tr[\s\S]*?<\/tr>/gi) || [];
  const clean = (c) => c
    .replace(/<[^>]+>/g, ' ')
    .replace(/&amp;/g, '&').replace(/&nbsp;/g, ' ').replace(/&lt;/g, '<').replace(/&gt;/g, '>')
    .replace(/&#(\d+);/g, (_, d) => String.fromCharCode(+d))
    .replace(/\s+/g, ' ').trim();

  const num = (v) => {
    const t = String(v).replace(/[$,*]/g, '').trim();
    if (!t || t === '--' || !/^-?\d+(\.\d+)?$/.test(t)) return null;
    const n = Number(t);
    return Number.isFinite(n) ? n : null;
  };

  for (const tr of trs) {
    const cells = (tr.match(/<t[hd][^>]*>[\s\S]*?<\/t[hd]>/gi) || []).map(clean);
    if (cells.length < 3) continue;
    const name = cells[0];
    // the features ribbon and the real header row both land here
    if (!name || /^(features|intelligence|price|speed|latency|model|end-to-end response time)$/i.test(name)) continue;

    // fixed column order: name, context, creator, index, cost, tokens/s, latency, total, link
    const context = /^\d+(\.\d+)?\s?[KMB]$/i.test(cells[1] || '') ? cells[1] : '';
    const creator = context ? (cells[2] || '') : (cells[1] || '');
    const offset = context ? 3 : 2;
    const index = num(cells[offset]);
    if (index === null) continue; // not a model row

    rows.push({
      name,
      creator,
      context,
      intelligence: index,
      costPerTask: num(cells[offset + 1]),
      tokensPerSec: num(cells[offset + 2]),
      latency: num(cells[offset + 3]),
      totalResponse: num(cells[offset + 4]),
    });
  }
  return rows.filter((r) => r.intelligence !== null);
}

function renderAaImage(rows) {
  const now = new Date();
  const dateParts = Object.fromEntries(new Intl.DateTimeFormat('en-GB', {
    timeZone: 'Asia/Makassar', day: '2-digit', month: 'long', year: 'numeric'
  }).formatToParts(now).filter(p => p.type !== 'literal').map(p => [p.type, p.value]));

  return renderLeaderboardImage({
    title: 'Artificial Analysis',
    subtitle: 'TOP 10 BY INTELLIGENCE INDEX',
    headerRight: [`${dateParts.year}`, `${dateParts.day} ${dateParts.month.toUpperCase()}`],
    modelColumn: 'MODEL / CREATOR',
    metricColumn: 'INTELLIGENCE INDEX',
    valueLabel: 'INDEX',
    footer: 'Artificial Analysis Intelligence Index \u00b7 Source: artificialanalysis.ai',
    footerRight: 'LEADERBOARD',
    rows: rows.map(r => ({
      name: r.name,
      sub: [r.creator, r.context ? `${r.context} ctx` : '', r.costPerTask !== null ? `$${r.costPerTask}/task` : ''].filter(Boolean).join(' \u00b7 '),
      value: r.intelligence,
      text: String(r.intelligence),
    })),
  });
}

async function checkArtificialAnalysis() {
  log("Checking Artificial Analysis leaderboard...");
  try {
    const res = await nativeFetch('https://artificialanalysis.ai/leaderboards/models', {
      headers: { Accept: 'text/html' },
    });
    if (!res.ok) { log(`AA fetch failed: ${res.status}`); return; }
    const html = await res.text();

    const table = (html.match(/<table[\s\S]*?<\/table>/i) || [])[0];
    if (!table) { log('AA: no table in page'); return; }

    const rows = parseAaTable(table);
    if (!rows.length) { log('AA: no rows parsed'); return; }
    rows.sort((a, b) => b.intelligence - a.intelligence);

    log(`AA: ${rows.length} models, #1 ${rows[0].name} (${rows[0].intelligence})`);
    const png = renderAaImage(rows.slice(0, 10));
    const form = new globalThis.FormData();
    form.append('chat_id', TELEGRAM_CHAT_ID);
    form.append('message_thread_id', TOPIC_ID_STATUS);
    form.append('caption', `<b>Artificial Analysis Top Models</b>\nBy Artificial Analysis Intelligence Index`);
    form.append('parse_mode', 'HTML');
    form.append('photo', new Blob([png], { type: 'image/png' }), 'aa_top.png');
    const send = await nativeFetch(`https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/sendPhoto`, { method: 'POST', body: form });
    if (!send.ok) log(`AA photo send failed: ${send.status}`);
    else log('AA: leaderboard image sent');
  } catch (err) {
    log(`AA check failed: ${err.message}`);
  }
}

async function checkTopWeekly() {
  log("Checking OpenRouter top weekly...");
  try {
    const [modelsRes, rankingsRes] = await Promise.all([
      nativeFetch('https://openrouter.ai/api/v1/models?sort=top-weekly&limit=10'),
      nativeFetch('https://openrouter.ai/api/frontend/v1/rankings/discovery?view=week&metric=tokens'),
    ]);
    if (!modelsRes.ok) {
      log(`Rankings model fetch failed: ${modelsRes.status}`);
      return;
    }
    if (!rankingsRes.ok) {
      log(`Rankings token fetch failed: ${rankingsRes.status}`);
      return;
    }

    const [modelsData, rankingsData] = await Promise.all([modelsRes.json(), rankingsRes.json()]);
    const tokenTotals = new Map();
    for (const [key, entry] of Object.entries(rankingsData.data?.totals || {})) {
      const model = key.replace(/:[a-z0-9-]+$/, '');
      const total = Number(entry?.t) || 0;
      if (!Number.isFinite(total)) continue;
      tokenTotals.set(model, (tokenTotals.get(model) || 0) + total);
    }

    const rows = (modelsData.data || []).map(model => {
      const canonicalSlug = model.canonical_slug || model.id.replace(/:[^/]+$/, '');
      const tokens = tokenTotals.get(canonicalSlug);
      return {
        name: (model.name || model.id).replace(/^[^:]+:\s*/, ''),
        author: (model.id || '').split('/')[0],
        tokens: Number.isFinite(tokens) ? tokens : null,
      };
    }).filter(row => row.tokens !== null).slice(0, 10);
    if (!rows.length) {
      log('Rankings: no rows parsed');
      return;
    }
    log(`Rankings: ${rows.length} models, #1 ${rows[0].name}`);
    const png = renderRankingsImage(rows);
    const form = new globalThis.FormData();
    form.append('chat_id', TELEGRAM_CHAT_ID);
    form.append('message_thread_id', TOPIC_ID_STATUS);
    form.append('caption', `<b>OpenRouter Top Weekly Models</b>`);
    form.append('parse_mode', 'HTML');
    form.append('photo', new Blob([png], { type: 'image/png' }), 'top_weekly.png');
    const send = await nativeFetch(`https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/sendPhoto`, { method: 'POST', body: form });
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
  cron.schedule("*/15 * * * *", checkOpenRouterModels, { scheduled: true, recoverMissedExecutions: false });
  cron.schedule("0 1 * * *", checkTopWeekly, { scheduled: true, recoverMissedExecutions: false });
  cron.schedule("30 1 * * *", checkArtificialAnalysis, { scheduled: true, recoverMissedExecutions: false });
}

module.exports = startMonitor;
