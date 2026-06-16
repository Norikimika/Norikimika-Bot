import os
import re
import time
import json
import subprocess
import requests
import logging
from datetime import datetime
from dotenv import load_dotenv
import tempfile

home_dir = os.path.expanduser("~")
env_path = os.path.join(home_dir, "SERVER/LogError404Bot/.env")
json_path = os.path.join(home_dir, "SERVER/Backup/notification.json")

if not os.path.exists(env_path):
    raise SystemExit(f".env not found: {env_path}")

try:
    os.chmod(env_path, 0o600)
except Exception:
    pass

load_dotenv(env_path)

CONFIG = {
    "telegram_bot_token": os.getenv("BOT_TOKEN"),
    "telegram_chat_id": os.getenv("ALLOWED_CHAT_ID"),
    "monitored_packages": {
        "com.macrovideo.v380pro": {
            "name": "V380 Pro",
            "topic_id": int(os.getenv("TOPIC_ID_V380", 15208)),
            "keywords": [],
            "tts_enabled": False
        },
        "com.tuya.smart": {
            "name": "Tuya",
            "topic_id": int(os.getenv("TOPIC_ID_TUYA", 15208)),
            "keywords": [],
            "tts_enabled": False
        },
        "com.gojek.gopaymerchant": {
            "name": "GoPay Merchant",
            "topic_id": int(os.getenv("TOPIC_ID_GOPAY", 15210)),
            "keywords": [
                "Norikimika"
            ],
            "tts_enabled": True
        },
        "com.andi.alquran.id": {
            "name": "Al-Qur'an Indonesia",
            "topic_id": int(os.getenv("TOPIC_ID_AL-QUR'AN", 34084)),
            "keywords": [],
            "tts_enabled": False
        }
    },
    "check_interval": int(os.getenv("CHECK_INTERVAL", 2)),
    "max_notifications": int(os.getenv("MAX_NOTIFICATIONS", 49)),
    "auto_clear_enabled": os.getenv("AUTO_CLEAR_ENABLED", "True").lower() in ("1", "true", "yes"),
    "db_cleanup_cycles": int(os.getenv("DB_CLEANUP_CYCLES", 180)),
    "db_retention_seconds": int(os.getenv("DB_RETENTION_SECONDS", 24 * 3600)), 
    "pkill_throttle_seconds": int(os.getenv("PKILL_THROTTLE_SECONDS", 300)),
    "telegram_retry": 3,
    "telegram_retry_backoff": 2
}

REQUIRED = ["telegram_bot_token", "telegram_chat_id"]
for r in REQUIRED:
    if not CONFIG[r]:
        raise SystemExit(f"Missing required config: {r}")

log_dir = os.path.join(home_dir, "SERVER/Backup")
os.makedirs(log_dir, exist_ok=True)
log_file = os.path.join(log_dir, "notification.log")
logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(message)s",
    handlers=[
        logging.FileHandler(log_file),
        logging.StreamHandler()
    ]
)
logger = logging.getLogger("notif-monitor")

class NotificationMonitor:
    def __init__(self, config, json_path):
        self.config = config
        self.json_path = json_path
        self.last_clear_kill = 0
        self.last_clear_time = 0
        self.su_available = None
        self.validate_config()
        self.setup_storage()

    def validate_config(self):
        logger.info("Validating configuration")
        if len(self.config["telegram_bot_token"]) < 10:
            raise SystemExit("Telegram bot token looks invalid")
        if not self.config["telegram_chat_id"]:
            raise SystemExit("Telegram chat id missing")
        logger.info("Configuration OK")

    def setup_storage(self):
        os.makedirs(os.path.dirname(self.json_path), exist_ok=True)
        if not os.path.exists(self.json_path):
            self._write_json([])
            logger.info("JSON storage initialized (new file)")
        else:
            try:
                data = self._read_json()
                if not isinstance(data, list):
                    raise ValueError("Expected a list")
                logger.info(f"JSON storage loaded ({len(data)} entries)")
            except Exception as e:
                logger.warning(f"JSON file invalid ({e}), resetting.")
                self._write_json([])

    def _read_json(self):
        with open(self.json_path, "r", encoding="utf-8") as f:
            return json.load(f)

    def _write_json(self, data):
        dir_name = os.path.dirname(self.json_path)
        with tempfile.NamedTemporaryFile(
            "w", dir=dir_name, delete=False,
            suffix=".tmp", encoding="utf-8"
        ) as f:
            json.dump(data, f, ensure_ascii=False, indent=2)
            tmp_path = f.name
        os.replace(tmp_path, self.json_path)

    def is_notification_sent(self, package, title, text, notif_time):
        try:
            data = self._read_json()
            return any(
                n.get("package_name") == package
                and n.get("title") == title
                and n.get("message_text") == text
                and n.get("notification_time") == notif_time
                for n in data
            )
        except Exception:
            logger.exception("is_notification_sent error")
            return False

    def mark_notification_sent(self, package, title, text, notif_time):
        try:
            data = self._read_json()
            already_exists = any(
                n.get("package_name") == package
                and n.get("title") == title
                and n.get("message_text") == text
                and n.get("notification_time") == notif_time
                for n in data
            )
            if not already_exists:
                data.append({
                    "package_name": package,
                    "title": title,
                    "message_text": text,
                    "notification_time": notif_time,
                    "sent_time": int(time.time())
                })
                self._write_json(data)
        except Exception:
            logger.exception("mark_notification_sent error")

    def cleanup_old_notifications(self):
        now = int(time.time())
        cutoff = now - self.config["db_retention_seconds"]
        try:
            data = self._read_json()
            before = len(data)
            data = [n for n in data if n.get("sent_time", 0) >= cutoff]
            deleted = before - len(data)
            if deleted > 0:
                self._write_json(data)
                logger.info(f"Cleaned up {deleted} old notifications")
        except Exception:
            logger.exception("cleanup_old_notifications error")

    def check_su(self):
        if self.su_available is not None:
            return self.su_available
        try:
            res = subprocess.run(['su', '-c', 'id'], capture_output=True, text=True, timeout=5)
            ok = res.returncode == 0 and 'uid' in res.stdout.lower()
        except Exception:
            ok = False
        self.su_available = ok
        if not ok:
            logger.warning("Root (su) not available or not permitted. Some operations may fail.")
        return ok

    def execute_root_command(self, command, timeout=10):
        try:
            if self.check_su():
                proc = subprocess.run(['su', '-c', command], capture_output=True, text=True, timeout=timeout)
            else:
                proc = subprocess.run(command, shell=True, capture_output=True, text=True, timeout=timeout)
            out = proc.stdout or ""
            if proc.stderr:
                logger.debug(f"Cmd stderr: {proc.stderr.strip()}")
            return out
        except subprocess.TimeoutExpired:
            logger.error(f"Command timed out: {command}")
            return ""
        except Exception as e:
            logger.exception(f"Command execution error: {e}")
            return ""

    def get_notification_count(self):
        out = self.execute_root_command("dumpsys notification 2>/dev/null | grep -c 'NotificationRecord'")
        if out and out.strip().isdigit():
            return int(out.strip())
        out2 = self.execute_root_command("dumpsys notification 2>/dev/null")
        if out2:
            return out2.count("NotificationRecord")
        return 0

    def clear_all_notifications(self):
        logger.info("Attempting to clear notifications (safe mode)")
        before = self.get_notification_count()

        self.execute_root_command("cmd notification remove_all")
        time.sleep(0.8)
        after = self.get_notification_count()
        if after < before:
            logger.info("Cleared notifications via 'cmd notification remove_all'")
            return True

        self.execute_root_command("service call notification 1")
        time.sleep(0.8)
        after2 = self.get_notification_count()
        if after2 < before:
            logger.info("Cleared notifications via 'service call notification 1'")
            return True

        now = time.time()
        if now - self.last_clear_kill < self.config["pkill_throttle_seconds"]:
            logger.warning("Skipping pkill (throttled). Will not kill SystemUI frequently.")
            return False

        logger.warning("As a last resort: killing SystemUI (pkill). This may cause UI restart.")
        self.last_clear_kill = now
        self.execute_root_command("pkill -f com.android.systemui")
        time.sleep(2)
        after3 = self.get_notification_count()
        if after3 < before:
            logger.info("Cleared notifications via pkill SystemUI")
            return True

        logger.error("Failed to clear notifications by available methods")
        return False

    def check_and_clear_notifications(self):
        if not self.config["auto_clear_enabled"]:
            return self.get_notification_count()
        try:
            current_count = self.get_notification_count()
            logger.info(f"Current notification count: {current_count}")
            if current_count >= self.config["max_notifications"]:
                logger.warning(
                    f"Threshold reached ({current_count} >= {self.config['max_notifications']}). Auto-clearing..."
                )
                self.clear_all_notifications()
                return self.get_notification_count()
            return current_count
        except Exception:
            logger.exception("check_and_clear_notifications error")
            return 0

    def get_new_notifications(self):
        out = self.execute_root_command("dumpsys notification --noredact 2>/dev/null")
        if not out:
            return []
        blocks = []
        current = []
        for line in out.splitlines():
            if "NotificationRecord(" in line:
                if current:
                    blocks.append("\n".join(current))
                current = [line]
            else:
                if current is not None:
                    current.append(line)
        if current:
            blocks.append("\n".join(current))

        results = []
        now_ms = int(time.time() * 1000)
        for blk in blocks:
            pkg_m = re.search(r'pkg=([^\s\]]+)', blk)
            if not pkg_m:
                continue
            pkg = pkg_m.group(1)
            if pkg not in self.config["monitored_packages"]:
                continue

            title_m = re.search(r'title=([^\n\r]+)', blk)
            if not title_m:
                continue
            title = self.clean_text(title_m.group(1).strip())

            text_m = re.search(r'text=([^\n\r]+)', blk)
            text = self.clean_text(text_m.group(1).strip()) if text_m else ""

            keywords = self.config["monitored_packages"][pkg].get("keywords", [])
            if keywords:
                combined_text = f"{title} {text}".lower()
                if not any(keyword.lower() in combined_text for keyword in keywords):
                    continue

            when_m = re.search(r'when=(\d+)', blk)
            timestamp = int(when_m.group(1)) if when_m else now_ms

            if self.is_notification_sent(pkg, title, text, timestamp):
                continue

            results.append({
                "package": pkg,
                "title": title,
                "text": text,
                "timestamp": timestamp,
                "time_str": datetime.fromtimestamp(timestamp / 1000).strftime("%Y-%m-%d %H:%M:%S")
            })
        return results

    def clean_text(self, text):
        text = re.sub(r'&[^;]+;', '', text)
        text = text.replace('canonical=1', '')
        text = re.sub(r'String\s*\(([^)]+)\)', r'\1', text)
        return text.strip()

    def send_telegram_message(self, message, topic_id=None):
        url = f"https://api.telegram.org/bot{self.config['telegram_bot_token']}/sendMessage"
        data = {
            "chat_id": self.config["telegram_chat_id"],
            "text": message,
            "parse_mode": "HTML"
        }
        if topic_id:
            data["message_thread_id"] = int(topic_id)

        attempt = 0
        backoff = self.config["telegram_retry_backoff"]
        while attempt < self.config["telegram_retry"]:
            attempt += 1
            try:
                r = requests.post(url, data=data, timeout=10)
                if r.status_code == 200:
                    return True
                if r.status_code == 429:
                    retry_after = int(r.headers.get("Retry-After", "5"))
                    logger.warning(f"Telegram rate limited. Retry after {retry_after}s")
                    time.sleep(retry_after)
                else:
                    logger.warning(f"Telegram send failed (status {r.status_code}): {r.text}")
                    time.sleep(backoff ** attempt)
            except requests.RequestException as e:
                logger.warning(f"Telegram request error: {e}. Backing off.")
                time.sleep(backoff ** attempt)
        logger.error("Telegram send failed after retries")
        return False

    def format_message(self, notif_data, app_name):
        return (
            f"<b>🔔 New {app_name} Notification</b>\n"
            f"<blockquote>⏰ Time: {notif_data['time_str']}\n"
            f"📝 Title: {notif_data['title']}\n"
            f"💬 Text: {notif_data['text']}</blockquote>"
        )

    def speak_notification(self, title, text):
        try:
            speech = f"{title}. {text}"
            logger.info(f"TTS: {speech}")
            subprocess.run([
                "termux-tts-speak",
                "-l", "id",
                speech
            ])
        except Exception as e:
            logger.error(f"TTS error: {e}")

    def speak_notification_edge(self, title, text):
        try:
            speech = f"{title}. {text}"
            with tempfile.NamedTemporaryFile(suffix=".mp3", delete=False) as f:
                mp3_file = f.name

            subprocess.run([
                "edge-tts",
                "--voice", "id-ID-GadisNeural",
                "--text", speech,
                "--write-media", mp3_file
            ], check=True)

            subprocess.run(["termux-media-player", "play", mp3_file])

            while True:
                result = subprocess.run(
                    ["termux-media-player", "info"],
                    capture_output=True,
                    text=True
                )
                if "Playing" not in result.stdout:
                    break
                time.sleep(0.5)

            os.remove(mp3_file)
        except Exception as e:
            logger.error(f"Edge TTS error: {e}")

    def stop_media_playback(self):
        try:
            subprocess.run(
                ["cmd", "media_session", "dispatch", "pause"],
                capture_output=True
            )
            logger.info("Media playback paused")
        except Exception as e:
            logger.error(f"Failed to stop media playback: {e}")

    def monitor(self):
        logger.info("Notification Monitor Started")
        logger.info(f"Auto-clear: {self.config['auto_clear_enabled']} (threshold {self.config['max_notifications']})")
        for pkg, c in self.config["monitored_packages"].items():
            logger.info(f"  • {c['name']} ({pkg}) - topic {c['topic_id']}")

        self.check_su()

        cycle = 0
        last_db_cleanup = 0

        try:
            while True:
                cycle += 1

                if cycle % max(1, self.config["db_cleanup_cycles"]) == 0:
                    self.cleanup_old_notifications()
                    last_db_cleanup = time.time()

                current_time = time.time()
                if cycle % 10 == 0 or (self.config["auto_clear_enabled"] and current_time - self.last_clear_time > 30):
                    notification_count = self.check_and_clear_notifications()
                    self.last_clear_time = current_time
                    logger.info(f"System notification count: {notification_count}")

                logger.debug(f"Cycle {cycle}: scanning for new notifications")
                new_notifs = self.get_new_notifications()
                if new_notifs:
                    logger.info(f"Found {len(new_notifs)} new notification(s)")
                    for n in new_notifs:
                        pkg = n["package"]
                        app_name = self.config["monitored_packages"][pkg]["name"]
                        topic_id = self.config["monitored_packages"][pkg]["topic_id"]

                        logger.info(f"Sending: {app_name} - {n['title']}")
                        message = self.format_message(n, app_name)
                        if self.send_telegram_message(message, topic_id):
                            self.mark_notification_sent(
                                pkg,
                                n["title"],
                                n["text"],
                                n["timestamp"]
                            )
                            if self.config["monitored_packages"][pkg].get("tts_enabled", False):
                                self.stop_media_playback()
                                self.speak_notification(n["title"], n["text"])
                            logger.info("Sent & marked")
                        else:
                            logger.warning("Failed to send to Telegram")
                else:
                    logger.debug("No new notifications")

                time.sleep(self.config["check_interval"])

        except KeyboardInterrupt:
            logger.info("Monitor stopped by user.")

if __name__ == "__main__":
    monitor = NotificationMonitor(CONFIG, json_path)
    monitor.monitor()
