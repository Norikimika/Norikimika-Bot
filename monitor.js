const cron = require("node-cron");
const { exec, spawn } = require("child_process");

module.exports = function ({
  bot,
  safeSend,
  appendLogLine,
  ALLOWED_TOPIC_ID_USAGE,
  ALLOWED_TOPIC_ID_STATUS,
  ALLOWED_CHAT_ID,
}) {
  async function sendUsageReport() {
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
          appendLogLine(
            `Daily usage vnstat failed: ${errOutput || `code ${code}`}`
          );

          await safeSend(
            bot,
            ALLOWED_CHAT_ID,
            "❌ Failed to get daily usage",
            {
              message_thread_id: ALLOWED_TOPIC_ID_USAGE,
            }
          );

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

          await safeSend(bot, ALLOWED_CHAT_ID, usageReport, {
            parse_mode: "HTML",
            message_thread_id: ALLOWED_TOPIC_ID_USAGE,
          });
        } catch (err) {
          appendLogLine(`Daily usage parse failed: ${err.message}`);
        }

        resolve();
      });
    });
  }

  let battery80Sent = false;
  let battery20Sent = false;

  async function checkBatteryLevel() {
    exec("termux-battery-status", async (error, stdout) => {
      if (error) {
        appendLogLine(`Battery monitor failed: ${error.message}`);
        return;
      }

      try {
        const data = JSON.parse(stdout);

        const level = data.percentage;
        const status = data.status;
        const temperature = Math.round(data.temperature);

        if (level < 80) battery80Sent = false;
        if (level > 20) battery20Sent = false;

        if (level >= 80 && !battery80Sent) {
          battery80Sent = true;

          await safeSend(
            bot,
            ALLOWED_CHAT_ID,
            `<b>🔋 Battery Fully Charged</b>\n<pre>Percentage : ${level}%\nStatus : ${status}\nTemperature : ${temperature}°C</pre>`,
            {
              parse_mode: "HTML",
              message_thread_id: ALLOWED_TOPIC_ID_STATUS,
            }
          );
        }

        if (level <= 20 && !battery20Sent) {
          battery20Sent = true;

          await safeSend(
            bot,
            ALLOWED_CHAT_ID,
            `<b>🪫 Low Battery Warning</b>\n<pre>Percentage : ${level}%\nStatus : ${status}\nTemperature : ${temperature}°C</pre>`,
            {
              parse_mode: "HTML",
              message_thread_id: ALLOWED_TOPIC_ID_STATUS,
            }
          );
        }
      } catch (err) {
        appendLogLine(`Battery monitor parse error: ${err.message}`);
      }
    });
  }

  cron.schedule("0 0 * * *", sendUsageReport);
  cron.schedule("* * * * *", checkBatteryLevel);
};