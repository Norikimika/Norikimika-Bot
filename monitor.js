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
let battery80Sent = false;
let battery20Sent = false;
let lastStatus = "UNKNOWN";
let chargeStartTime = Date.now(); 
let useStartTime = Date.now(); 

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
        if (lastStatus === "DISCHARGING" || lastStatus === "NOT_CHARGING") {
          chargeStartTime = Date.now();
        }
      } else if (status === "DISCHARGING" || status === "NOT_CHARGING") {
        if (lastStatus === "CHARGING" || lastStatus === "FULL") {
          useStartTime = Date.now();
        }
      }

      if (status !== "UNKNOWN") {
        lastStatus = status;
      }

      if (level < 80) battery80Sent = false;
      if (level > 20) battery20Sent = false;

      if (level >= 80 && !battery80Sent) {
        battery80Sent = true;
        const chargeDuration = formatDuration(chargeStartTime);

        await sendToTelegram(
          `<b>🔋 Battery Fully Charged</b>\n<blockquote>Percentage : ${level}%\nStatus : ${status}\nTemperature : ${temperature}°C\nDuration : ${chargeDuration}</blockquote>`,
          TOPIC_ID_STATUS
        );
      }

      if (level <= 20 && !battery20Sent) {
        battery20Sent = true;
        const usageDuration = formatDuration(useStartTime);

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

// ================== MAIN SCHEDULER ==================
function startMonitor() {
  log("Starting monitor service...");
  
  cron.schedule("0 0 * * *", sendUsageReport, { scheduled: true });
  cron.schedule("0 0,12 * * *", runAutoSpeedtest, { scheduled: true });
  cron.schedule("* * * * *", checkBatteryLevel);
}

module.exports = startMonitor;
