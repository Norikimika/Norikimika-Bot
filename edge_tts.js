// Edge TTS murni Node — tanpa Python, tanpa dep baru (pakai `ws` bawaan SERVER).
// Protokol: WS ke speech.platform.bing.com, kirim SSML, terima chunk audio biner.
// Token/gec ikut pola klien Edge publik (sama seperti lib node-edge-tts).
const crypto = require("node:crypto");
const fs = require("fs");
const { WebSocket } = require("ws");

const CHROMIUM_VERSION = "143.0.3650.75";
const TRUSTED_CLIENT_TOKEN = "6A5AA1D4EAFF4E9FB37E23D68491D6F4";
const WINDOWS_EPOCH = 11644473600n;

function secMsGec() {
    const ticks =
        BigInt(Math.floor(Date.now() / 1000 + Number(WINDOWS_EPOCH))) * 10000000n;
    const rounded = ticks - (ticks % 3000000000n);
    return crypto
        .createHash("sha256")
        .update(`${rounded}${TRUSTED_CLIENT_TOKEN}`, "ascii")
        .digest("hex")
        .toUpperCase();
}

function escapeXml(s) {
    return s.replace(/[<>&"']/g, (c) => {
        switch (c) {
            case "<": return "&lt;";
            case ">": return "&gt;";
            case "&": return "&amp;";
            case '"': return "&quot;";
            default: return "&apos;";
        }
    });
}

// text -> mp3 di audioPath. Resolve saat turn.end, reject saat timeout/error.
function edgeTtsToFile(
    text,
    audioPath,
    { voice = "id-ID-GadisNeural", lang = "id-ID", timeoutMs = 25000 } = {}
) {
    return new Promise((resolve, reject) => {
        const url =
            `wss://speech.platform.bing.com/consumer/speech/synthesize/readaloud/edge/v1` +
            `?TrustedClientToken=${TRUSTED_CLIENT_TOKEN}&Sec-MS-GEC=${secMsGec()}&Sec-MS-GEC-Version=1-${CHROMIUM_VERSION}`;
        const major = CHROMIUM_VERSION.split(".")[0];
        const ws = new WebSocket(url, {
            host: "speech.platform.bing.com",
            origin: "chrome-extension://jdiccldimpdaibmpdkjnbmckianbfold",
            headers: {
                Pragma: "no-cache",
                "Cache-Control": "no-cache",
                "User-Agent": `Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/${major}.0.0.0 Safari/537.36 Edg/${major}.0.0.0`,
                "Accept-Language": "en-US,en;q=0.9",
            },
        });
        const timer = setTimeout(() => {
            try { ws.close(); } catch (e) {}
            reject(new Error("edge TTS timeout"));
        }, timeoutMs);

        const stream = fs.createWriteStream(audioPath);
        ws.on("open", () => {
            ws.send(
                `Content-Type:application/json; charset=utf-8\r\nPath:speech.config\r\n\r\n` +
                `{"context":{"synthesis":{"audio":{"metadataoptions":{"sentenceBoundaryEnabled":"false","wordBoundaryEnabled":"true"},"outputFormat":"audio-24khz-48kbitrate-mono-mp3"}}}}`
            );
            const reqId = crypto.randomBytes(16).toString("hex");
            ws.send(
                `X-RequestId:${reqId}\r\nContent-Type:application/ssml+xml\r\nPath:ssml\r\n\r\n` +
                `<speak version="1.0" xmlns="http://www.w3.org/2001/10/synthesis" xml:lang="${lang}">` +
                `<voice name="${voice}"><prosody rate="default" pitch="default" volume="default">` +
                `${escapeXml(text)}</prosody></voice></speak>`
            );
        });
        ws.on("message", (data, isBinary) => {
            if (isBinary) {
                const sep = "Path:audio\r\n";
                const i = data.indexOf(sep) + sep.length;
                stream.write(data.subarray(i));
            } else if (String(data).includes("Path:turn.end")) {
                stream.end();
                stream.on("finish", () => {
                    clearTimeout(timer);
                    try { ws.close(); } catch (e) {}
                    resolve();
                });
            }
        });
        ws.on("error", (e) => {
            clearTimeout(timer);
            reject(e);
        });
    });
}

module.exports = { edgeTtsToFile };
