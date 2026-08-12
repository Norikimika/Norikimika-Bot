require("dotenv").config();

const fs = require("fs");
const path = require("path");
const os = require("os");
const { spawn } = require("child_process");

const {
  Client,
  GatewayIntentBits,
  Partials,
  EmbedBuilder,
  PermissionFlagsBits,
  ActivityType,
} = require("discord.js");

const {
  joinVoiceChannel,
  createAudioPlayer,
  createAudioResource,
  AudioPlayerStatus,
  getVoiceConnection,
  StreamType,
} = require("@discordjs/voice");

const LOG_FILE = path.join(os.homedir(), "SERVER", "Backup", "discord.log");

const AUDIO_CONFIG = {
  sampleRate: 48000,
  channels: 2,
  highWaterMark: 1 << 21,
  ytDlpTimeout: 20000,
  ffmpegStartupTimeout: 12000,
  maxConsecutiveFailures: 3,
  failureDelay: 1500,
  autoplayTimeout: 25000,
  searchTimeout: 15000,
  playlistTimeout: 30000,
};

const queues = new Map();

function log(message) {
  try {
    const dir = path.dirname(LOG_FILE);
    if (!fs.existsSync(dir)) {
      fs.mkdirSync(dir, { recursive: true });
    }

    fs.appendFileSync(LOG_FILE, `[${new Date().toLocaleString()}] ${message}\n`);
  } catch (err) {
    console.error("Failed to write log:", err.message);
  }
}

process.on("uncaughtException", (err) => {
  if (err.code === "EPIPE") {
    log(`[Warning] Ignored EPIPE: ${err.message}`);
    return;
  }
  log(`[Uncaught Exception] ${err.stack || err.message}`);
});

process.on("unhandledRejection", (reason) => {
  log(`[Unhandled Rejection] ${reason?.stack || reason}`);
});

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function chunkText(text, maxLength = 1900) {
  if (!text) {
    return [];
  }
  return text.match(new RegExp(`.{1,${maxLength}}`, "gs")) || [text];
}

function extractVideoId(url) {
  if (!url) {
    return null;
  }

  const value = String(url).trim();
  const patterns = [
    /[?&]v=([a-zA-Z0-9_-]{11})/,
    /youtu\.be\/([a-zA-Z0-9_-]{11})/,
    /youtube\.com\/embed\/([a-zA-Z0-9_-]{11})/,
    /youtube\.com\/v\/([a-zA-Z0-9_-]{11})/,
    /^([a-zA-Z0-9_-]{11})$/,
  ];

  for (const pattern of patterns) {
    const match = value.match(pattern);
    if (match) {
      return match[1];
    }
  }

  return null;
}

function normalizeUrl(url) {
  if (!url) {
    return url;
  }

  const videoId = extractVideoId(url);
  if (videoId) {
    return `https://www.youtube.com/watch?v=${videoId}`;
  }

  return String(url)
    .replace("music.youtube.com", "www.youtube.com")
    .replace("m.youtube.com", "www.youtube.com");
}

function cleanArtist(value) {
  if (!value) {
    return "Unknown Artist";
  }
  return String(value).replace(/\s+/g, " ").trim();
}

function cleanTitle(title, artist) {
  let value = String(title || "").replace(/\s+/g, " ").trim();
  const cleanArtistName = cleanArtist(artist);

  if (cleanArtistName && cleanArtistName !== "Unknown Artist") {
    const escapedArtist = cleanArtistName.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    value = value.replace(new RegExp(`^${escapedArtist}\\s*[-–—:]\\s*`, "i"), "");
  }

  return value.trim();
}

function normalizeSongMetadata(song) {
  if (!song) {
    return null;
  }

  const artist = cleanArtist(song.artist || song.uploader || song.channel || song.creator);

  return {
    ...song,
    title: cleanTitle(song.title, artist),
    artist,
    url: normalizeUrl(song.url),
    id: song.id || extractVideoId(song.url),
  };
}

function getSongId(song) {
  return song?.id || extractVideoId(song?.url);
}

function getSongKey(song) {
  const id = getSongId(song);
  if (id) {
    return `yt:${id}`;
  }
  return `${song?.artist || ""}:${song?.title || ""}`
    .toLowerCase()
    .replace(/\s+/g, " ")
    .trim();
}

function getSongUrlKey(song) {
  const id = getSongId(song);
  if (id) {
    return `https://www.youtube.com/watch?v=${id}`;
  }
  return normalizeUrl(song?.url || "");
}

function isValidSong(song) {
  return Boolean(song && song.title && song.url && getSongId(song));
}

function canControlQueue(message, serverQueue) {
  if (!serverQueue) {
    return true;
  }

  const currentSong = serverQueue.songs[0] || serverQueue.lastSong;

  if (!currentSong?.requestedBy) return true;
  if (currentSong.requestedBy.id === message.author.id) return true;
  if (message.member?.permissions?.has(PermissionFlagsBits.Administrator)) return true;
  if (message.member?.permissions?.has(PermissionFlagsBits.ManageChannels)) return true;

  return false;
}

function spawnYtDlp(args, timeoutMs = AUDIO_CONFIG.ytDlpTimeout) {
  return new Promise((resolve, reject) => {
    const proc = spawn("yt-dlp", args, {
      stdio: ["ignore", "pipe", "pipe"],
    });

    let stdout = "";
    let stderr = "";
    let settled = false;

    const timeoutId = setTimeout(() => {
      if (settled) return;
      settled = true;
      try {
        proc.kill("SIGKILL");
      } catch {}
      reject(new Error(`yt-dlp timeout after ${timeoutMs}ms`));
    }, timeoutMs);

    const cleanup = () => {
      clearTimeout(timeoutId);
    };

    proc.stdout.on("data", (chunk) => {
      stdout += chunk.toString();
    });

    proc.stderr.on("data", (chunk) => {
      stderr += chunk.toString();
      if (stderr.length > 4000) {
        stderr = stderr.slice(-4000);
      }
    });

    proc.once("error", (err) => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(new Error(`yt-dlp process error: ${err.message}`));
    });

    proc.once("close", (code) => {
      if (settled) return;
      settled = true;
      cleanup();
      if (code !== 0) {
        reject(new Error(`yt-dlp exited with code ${code}${stderr ? `: ${stderr.trim()}` : ""}`));
        return;
      }
      resolve({ stdout, stderr });
    });
  });
}

function buildTrackFromYtDlpItem(item) {
  const id = item?.id || extractVideoId(item?.webpage_url) || extractVideoId(item?.url);

  if (!id || !item?.title) return null;
  if (item.title === "[Private video]" || item.title === "[Deleted video]") return null;

  return normalizeSongMetadata({
    title: item.title,
    artist: item.artist || item.uploader || item.channel || item.creator || "Unknown Artist",
    url: `https://www.youtube.com/watch?v=${id}`,
    id,
    durationInSec: Number(item.duration) || 0,
  });
}

async function searchYtMusic(query, limit = 10) {
  if (!query?.trim()) return [];

  const args = [
    "--extractor-args", "youtube:player_client=tv,mweb,ios,android",
    "--flat-playlist",
    "--no-warnings",
    "--no-cache-dir",
    "--skip-download",
    "--playlist-end", String(limit),
    "-J",
    `ytsearch${limit}:${query}`,
  ];

  try {
    const { stdout } = await spawnYtDlp(args, AUDIO_CONFIG.searchTimeout);
    const data = JSON.parse(stdout);
    return (Array.isArray(data?.entries) ? data.entries : [])
      .map(buildTrackFromYtDlpItem)
      .filter(Boolean)
      .filter(isValidSong);
  } catch (err) {
    log(`[yt-dlp Search Error] ${query}: ${err.message}`);
    return [];
  }
}

async function getPlaylistTracks(playlistUrl) {
  if (!playlistUrl) return [];

  const args = [
    "--extractor-args", "youtube:player_client=tv,mweb,ios,android",
    "--flat-playlist",
    "--no-warnings",
    "--no-cache-dir",
    "--skip-download",
    "-J",
    playlistUrl,
  ];

  try {
    const { stdout } = await spawnYtDlp(args, AUDIO_CONFIG.playlistTimeout);
    const data = JSON.parse(stdout);
    return (Array.isArray(data?.entries) ? data.entries : [])
      .map(buildTrackFromYtDlpItem)
      .filter(Boolean)
      .filter(isValidSong);
  } catch (err) {
    log(`[yt-dlp Playlist Error] ${playlistUrl}: ${err.message}`);
    return [];
  }
}

async function getYtMusicRadioTracks(videoId) {
  if (!videoId) return [];

  const radioUrl = `https://music.youtube.com/watch?v=${videoId}&list=RD${videoId}`;
  const args = [
    "--extractor-args", "youtube:player_client=tv,mweb,ios,android",
    "--flat-playlist",
    "--no-warnings",
    "--no-cache-dir",
    "--skip-download",
    "--playlist-end", "15",
    "-J",
    radioUrl,
  ];

  try {
    const { stdout } = await spawnYtDlp(args, AUDIO_CONFIG.autoplayTimeout);
    const data = JSON.parse(stdout);
    return (Array.isArray(data?.entries) ? data.entries : [])
      .map(buildTrackFromYtDlpItem)
      .filter(Boolean)
      .filter(isValidSong);
  } catch (err) {
    log(`[Autoplay Radio Error] ${err.message}`);
    return [];
  }
}

async function fetchVideoInfo(videoId) {
  if (!videoId) return null;

  const url = `https://www.youtube.com/watch?v=${videoId}`;
  const args = [
    "--extractor-args", "youtube:player_client=tv,mweb,ios,android",
    "--no-warnings",
    "--no-cache-dir",
    "--skip-download",
    "-j",
    url,
  ];

  try {
    const { stdout } = await spawnYtDlp(args, AUDIO_CONFIG.ytDlpTimeout);
    const json = JSON.parse(stdout);

    return normalizeSongMetadata({
      title: json.title,
      artist: json.artist || json.uploader || json.channel || json.creator || "Unknown Artist",
      url,
      id: videoId,
      durationInSec: Number(json.duration) || 0,
    });
  } catch (err) {
    log(`[Video Info Error] ${videoId}: ${err.message}`);
    return null;
  }
}

async function processGeminiRequest(prompt) {
  try {
    const apiKey = process.env.GEMINI_API_KEY;
    if (!apiKey) {
      throw new Error("GEMINI_API_KEY is missing in .env file!");
    }

    const model = process.env.DISCORD_AI_API_MODEL || "gemini-3.5-flash";
    const endpoint = `https://generativelanguage.googleapis.com/v1beta/models/${model}:generateContent?key=${apiKey}`;

    const res = await fetch(endpoint, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        systemInstruction: {
          parts: [{ text: "You are Zero Two, a helpful, friendly, and smart AI assistant." }],
        },
        contents: [{ role: "user", parts: [{ text: prompt }] }],
        generationConfig: { temperature: 0.7 },
      }),
    });

    if (!res.ok) {
      const errText = await res.text();
      throw new Error(`Gemini API HTTP ${res.status}: ${errText}`);
    }

    const data = await res.json();
    return data?.candidates?.[0]?.content?.parts?.[0]?.text || null;
  } catch (err) {
    log(`[Gemini Error] ${err.message}`);
    throw err;
  }
}

function getDirectAudioUrl(targetUrl, extraArgs = []) {
  return new Promise((resolve, reject) => {
    const args = [
      "-g", "-f", "bestaudio/ba",
      "--no-playlist", "--no-warnings",
      "--no-check-certificates", "--no-cache-dir",
      ...extraArgs, targetUrl,
    ];

    const proc = spawn("yt-dlp", args, { stdio: ["ignore", "pipe", "pipe"] });

    let stdout = "";
    let stderr = "";
    let settled = false;

    const timeoutId = setTimeout(() => {
      if (settled) return;
      settled = true;
      try { proc.kill("SIGKILL"); } catch {}
      reject(new Error("yt-dlp timed out while resolving audio URL"));
    }, AUDIO_CONFIG.ytDlpTimeout);

    const cleanup = () => clearTimeout(timeoutId);

    proc.stdout.on("data", (chunk) => { stdout += chunk.toString(); });
    proc.stderr.on("data", (chunk) => {
      stderr += chunk.toString();
      if (stderr.length > 3000) stderr = stderr.slice(-3000);
    });

    proc.once("error", (err) => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(new Error(`yt-dlp process error: ${err.message}`));
    });

    proc.once("close", (code) => {
      if (settled) return;
      settled = true;
      cleanup();

      if (code !== 0) {
        reject(new Error(`yt-dlp exited with code ${code}${stderr ? `: ${stderr.trim()}` : ""}`));
        return;
      }

      const directUrl = stdout.trim().split(/\r?\n/).find((line) => /^https?:\/\//i.test(line.trim()));
      if (!directUrl) {
        reject(new Error("yt-dlp returned no direct audio URL"));
        return;
      }

      resolve(directUrl.trim());
    });
  });
}

function streamFromDirectUrl(directUrl, song = null) {
  return new Promise((resolve, reject) => {
    const ffmpegProcess = spawn("ffmpeg", [
      "-hide_banner", "-loglevel", "warning",
      "-reconnect", "1", "-reconnect_streamed", "1",
      "-reconnect_at_eof", "1", "-reconnect_delay_max", "5",
      "-i", directUrl,
      "-vn", "-ac", String(AUDIO_CONFIG.channels),
      "-ar", String(AUDIO_CONFIG.sampleRate),
      "-c:a", "pcm_s16le", "-f", "s16le", "pipe:1"
    ], { stdio: ["ignore", "pipe", "pipe"] });

    let settled = false;
    let started = false;
    let stderr = "";

    const startTimeout = setTimeout(() => {
      if (settled) return;
      fail(`FFmpeg startup timeout${stderr ? `: ${stderr.slice(-800)}` : ""}`);
    }, AUDIO_CONFIG.ffmpegStartupTimeout);

    function cleanup() {
      clearTimeout(startTimeout);
      if (!ffmpegProcess.killed) {
        try { ffmpegProcess.kill("SIGKILL"); } catch {}
      }
    }

    function fail(message) {
      if (settled) return;
      settled = true;
      cleanup();
      reject(new Error(message));
    }

    ffmpegProcess.stdout.once("data", (chunk) => {
      if (settled) return;
      started = true;
      settled = true;
      clearTimeout(startTimeout);

      ffmpegProcess.stdout.pause();
      ffmpegProcess.stdout.unshift(chunk);

      const resource = createAudioResource(ffmpegProcess.stdout, {
        inputType: StreamType.Raw,
        inlineVolume: false,
        silencePaddingFrames: 5,
        highWaterMark: AUDIO_CONFIG.highWaterMark,
        metadata: {
          title: song?.title || null,
          artist: song?.artist || null,
          id: song?.id || null,
          url: song?.url || null,
        },
      });

      resource.ffmpegProcess = ffmpegProcess;
      resolve(resource);
    });

    ffmpegProcess.stderr.on("data", (chunk) => {
      const message = chunk.toString().trim();
      if (message) {
        stderr = `${stderr}\n${message}`.slice(-3000);
      }
    });

    ffmpegProcess.once("error", (err) => {
      fail(`FFmpeg process error: ${err.message}`);
    });

    ffmpegProcess.once("close", (code) => {
      if (!started && !settled) {
        fail(`FFmpeg exited before producing audio (code ${code})${stderr ? `: ${stderr.slice(-800)}` : ""}`);
      }
    });
  });
}

function streamWithYtdlp(targetUrl, extraArgs = [], song = null) {
  return new Promise((resolve, reject) => {
    const ytdlpProcess = spawn("yt-dlp", [
      "-o", "-", "-f", "bestaudio/ba",
      "--no-playlist", "--no-warnings",
      "--no-check-certificates", "--no-cache-dir",
      ...extraArgs, targetUrl
    ], { stdio: ["ignore", "pipe", "pipe"] });

    const ffmpegProcess = spawn("ffmpeg", [
      "-hide_banner", "-loglevel", "warning",
      "-probesize", "32768", "-analyzeduration", "0",
      "-i", "pipe:0",
      "-vn", "-ac", String(AUDIO_CONFIG.channels),
      "-ar", String(AUDIO_CONFIG.sampleRate),
      "-c:a", "pcm_s16le", "-f", "s16le", "pipe:1"
    ], { stdio: ["pipe", "pipe", "pipe"] });

    let settled = false;
    let started = false;
    let stderr = "";

    const startTimeout = setTimeout(() => {
      if (settled) return;
      fail(`yt-dlp/FFmpeg startup timeout${stderr ? `: ${stderr.slice(-800)}` : ""}`);
    }, AUDIO_CONFIG.ffmpegStartupTimeout);

    function cleanup() {
      clearTimeout(startTimeout);
      try { ytdlpProcess.stdout.unpipe(ffmpegProcess.stdin); } catch {}
      try { if (!ytdlpProcess.killed) ytdlpProcess.kill("SIGKILL"); } catch {}
      try { if (!ffmpegProcess.killed) ffmpegProcess.kill("SIGKILL"); } catch {}
    }

    function fail(message) {
      if (settled) return;
      settled = true;
      cleanup();
      reject(new Error(message));
    }

    ytdlpProcess.stdout.on("error", (err) => {
      if (err.code === "EPIPE") return;
      fail(`yt-dlp stdout error: ${err.message}`);
    });

    ffmpegProcess.stdin.on("error", (err) => {
      if (err.code === "EPIPE") return;
      fail(`FFmpeg stdin error: ${err.message}`);
    });

    ytdlpProcess.stdout.pipe(ffmpegProcess.stdin);

    ffmpegProcess.stdout.once("data", (chunk) => {
      if (settled) return;
      started = true;
      settled = true;
      clearTimeout(startTimeout);

      ffmpegProcess.stdout.pause();
      ffmpegProcess.stdout.unshift(chunk);

      const resource = createAudioResource(ffmpegProcess.stdout, {
        inputType: StreamType.Raw,
        inlineVolume: false,
        silencePaddingFrames: 5,
        highWaterMark: AUDIO_CONFIG.highWaterMark,
        metadata: {
          title: song?.title || null,
          artist: song?.artist || null,
          id: song?.id || null,
          url: song?.url || null,
        },
      });

      resource.ytdlpProcess = ytdlpProcess;
      resource.ffmpegProcess = ffmpegProcess;
      resolve(resource);
    });

    ytdlpProcess.stderr.on("data", (chunk) => {
      const message = chunk.toString().trim();
      if (message && !message.includes("WARNING")) {
        stderr = `${stderr}\n[yt-dlp] ${message}`.slice(-3000);
      }
    });

    ffmpegProcess.stderr.on("data", (chunk) => {
      const message = chunk.toString().trim();
      if (message) {
        stderr = `${stderr}\n[ffmpeg] ${message}`.slice(-3000);
      }
    });

    ytdlpProcess.once("error", (err) => fail(`yt-dlp process error: ${err.message}`));
    ffmpegProcess.once("error", (err) => fail(`FFmpeg process error: ${err.message}`));

    ffmpegProcess.once("close", (code) => {
      if (!started && !settled) {
        fail(`FFmpeg exited before producing audio (code ${code})${stderr ? `: ${stderr.slice(-800)}` : ""}`);
      }
    });
  });
}

async function createAudioResourceFromSong(song) {
  if (!isValidSong(song)) {
    throw new Error("Invalid song object");
  }

  const targetUrl = normalizeUrl(song.url);

  try {
    const directUrl = await getDirectAudioUrl(targetUrl, ["--extractor-args", "youtube:player_client=tv,mweb,ios,android"]);
    return await streamFromDirectUrl(directUrl, song);
  } catch (err) {
    log(`[Audio Engine #1 Failed] ${song.title}: ${err.message}`);
  }

  try {
    const directUrl = await getDirectAudioUrl(targetUrl);
    return await streamFromDirectUrl(directUrl, song);
  } catch (err) {
    log(`[Audio Engine #2 Failed] ${song.title}: ${err.message}`);
  }

  try {
    return await streamWithYtdlp(targetUrl, ["--extractor-args", "youtube:player_client=tv,mweb,ios,android"], song);
  } catch (err) {
    log(`[Audio Engine #3 Failed] ${song.title}: ${err.message}`);
  }

  throw new Error(`All audio streaming engines failed for "${song.title}"`);
}

function destroyAudioResource(resource) {
  if (!resource) return;

  try {
    if (resource.ffmpegProcess && !resource.ffmpegProcess.killed) {
      resource.ffmpegProcess.kill("SIGKILL");
    }
  } catch {}

  try {
    if (resource.ytdlpProcess && !resource.ytdlpProcess.killed) {
      resource.ytdlpProcess.kill("SIGKILL");
    }
  } catch {}
}

async function getAutoplayCandidate(serverQueue, baseSong) {
  if (!serverQueue || !baseSong) return null;

  const videoId = getSongId(baseSong);
  if (!videoId) return null;

  const radioTracks = await getYtMusicRadioTracks(videoId);
  if (!radioTracks.length) return null;

  for (const candidate of radioTracks) {
    if (!candidate?.id) continue;

    const candidateSong = normalizeSongMetadata({
      ...candidate,
      requestedBy: baseSong.requestedBy || null,
      isManual: false,
    });

    if (!isValidSong(candidateSong)) continue;

    const candidateId = getSongId(candidateSong);
    const candidateKey = getSongKey(candidateSong);
    const candidateUrl = getSongUrlKey(candidateSong);

    if (candidateId === getSongId(baseSong)) continue;
    if (serverQueue.playedIds.has(candidateId)) continue;
    if (serverQueue.playedKeys.has(candidateKey)) continue;
    if (serverQueue.playedUrls.has(candidateUrl)) continue;
    if (serverQueue.songs.some((song) => getSongId(song) === candidateId || getSongKey(song) === candidateKey)) continue;

    return candidateSong;
  }

  return null;
}

function warmAutoplayCandidate(serverQueue, baseSong) {
  if (!serverQueue?.autoplay || !baseSong) return;

  const songKey = getSongKey(baseSong);
  if (serverQueue.autoplayPrefetch && serverQueue.autoplayPrefetch.songKey === songKey) return;

  const generation = ++serverQueue.autoplayGeneration;
  const promise = getAutoplayCandidate(serverQueue, baseSong).catch((err) => {
    log(`[Autoplay Prefetch Error] ${err.message}`);
    return null;
  });

  serverQueue.autoplayPrefetch = { songKey, generation, promise };
}

function destroyServerQueue(guildId, deleteQueue = true) {
  const serverQueue = queues.get(guildId);
  if (!serverQueue) return;

  serverQueue.transitioning = true;
  serverQueue.playing = false;
  serverQueue.autoplayPrefetch = null;

  destroyAudioResource(serverQueue.currentResource);
  serverQueue.currentResource = null;
  serverQueue.currentSong = null;

  try { serverQueue.player.stop(true); } catch {}
  try { serverQueue.connection.destroy(); } catch {}

  if (deleteQueue) {
    queues.delete(guildId);
  }
}

async function advanceQueue(guildId, reason = "idle") {
  const serverQueue = queues.get(guildId);
  if (!serverQueue) return;

  if (serverQueue.transitioning) {
    log(`[Queue Transition Ignored] Guild ${guildId} | ${reason}`);
    return;
  }

  serverQueue.transitioning = true;

  try {
    if (serverQueue.songs.length > 0) {
      const finishedSong = serverQueue.songs.shift();
      if (finishedSong) {
        log(`[Queue Advance] Guild ${guildId} | Finished: ${finishedSong.title} | Reason: ${reason}`);
      }
    }

    if (reason === "error") {
      await delay(AUDIO_CONFIG.failureDelay);
    }

    await playNextSong(guildId);
  } catch (err) {
    log(`[Queue Advance Error] Guild ${guildId}: ${err.message}`);
  } finally {
    const currentQueue = queues.get(guildId);
    if (currentQueue) {
      currentQueue.transitioning = false;
    }
  }
}

async function playNextSong(guildId) {
  const serverQueue = queues.get(guildId);
  if (!serverQueue || serverQueue.playing) return;

  if (serverQueue.failCount >= AUDIO_CONFIG.maxConsecutiveFailures) {
    const embed = new EmbedBuilder()
      .setColor("#FF0000")
      .setDescription("Too many consecutive playback errors. Music stopped.");
    await serverQueue.textChannel?.send({ embeds: [embed] }).catch(() => {});
    destroyServerQueue(guildId);
    return;
  }

  if (serverQueue.songs.length === 0) {
    if (serverQueue.autoplay && serverQueue.lastSong) {
      try {
        let nextSong = null;
        const prefetch = serverQueue.autoplayPrefetch;
        const lastSongKey = getSongKey(serverQueue.lastSong);

        if (prefetch && prefetch.songKey === lastSongKey) {
          nextSong = await prefetch.promise;
        }

        serverQueue.autoplayPrefetch = null;

        if (!nextSong) {
          nextSong = await getAutoplayCandidate(serverQueue, serverQueue.lastSong);
        }

        if (!nextSong) throw new Error("No suitable unique autoplay track found");

        serverQueue.songs.push(nextSong);
        const songId = getSongId(nextSong);
        const songKey = getSongKey(nextSong);
        const songUrl = getSongUrlKey(nextSong);

        serverQueue.playedIds.add(songId);
        serverQueue.playedKeys.add(songKey);
        serverQueue.playedUrls.add(songUrl);

        log(`[Autoplay] Guild ${guildId} | ${nextSong.title} - ${nextSong.artist}`);
      } catch (err) {
        log(`[Autoplay Error] Guild ${guildId}: ${err.message}`);
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("Autoplay could not find another suitable track. Playback stopped.");
        await serverQueue.textChannel?.send({ embeds: [embed] }).catch(() => {});
        destroyServerQueue(guildId);
        return;
      }
    } else {
      const embed = new EmbedBuilder().setColor("#FF0000").setDescription("Queue finished.");
      await serverQueue.textChannel?.send({ embeds: [embed] }).catch(() => {});
      destroyServerQueue(guildId);
      return;
    }
  }

  const song = normalizeSongMetadata(serverQueue.songs[0]);
  serverQueue.songs[0] = song;

  if (!isValidSong(song)) {
    serverQueue.songs.shift();
    return playNextSong(guildId);
  }

  serverQueue.lastSong = song;
  const songId = getSongId(song);
  const songKey = getSongKey(song);
  const songUrl = getSongUrlKey(song);

  serverQueue.playedIds.add(songId);
  serverQueue.playedKeys.add(songKey);
  serverQueue.playedUrls.add(songUrl);

  if (serverQueue.autoplay && serverQueue.songs.length === 1) {
    warmAutoplayCandidate(serverQueue, song);
  }

  serverQueue.playing = true;

  try {
    log(`[Music Stream Start] Guild ${guildId} | ${song.title} - ${song.artist}`);
    const resource = await createAudioResourceFromSong(song);
    const currentQueue = queues.get(guildId);

    if (!currentQueue || currentQueue !== serverQueue) {
      destroyAudioResource(resource);
      return;
    }

    currentQueue.currentResource = resource;
    currentQueue.currentSong = song;
    currentQueue.player.play(resource);
    currentQueue.failCount = 0;

    const artistText = song.artist && song.artist !== "Unknown Artist" ? ` by ${song.artist}` : "";
    const reqText = song.isManual && song.requestedBy ? `\nRequested by: <@${song.requestedBy.id}>` : "\n*(Autoplay)*";

    const embed = new EmbedBuilder()
      .setColor("#00FF00")
      .setDescription(`Started playing [${song.title}](${song.url})${artistText}${reqText}`);

    if (songId) {
      embed.setThumbnail(`https://img.youtube.com/vi/${songId}/mqdefault.jpg`);
    }

    await currentQueue.textChannel?.send({ embeds: [embed] }).catch(() => {});
    log(`[Music Stream Ready] Guild ${guildId} | ${song.title}`);
  } catch (err) {
    log(`[Play Stream Error] Guild ${guildId} | ${song.title}: ${err.message}`);
    const currentQueue = queues.get(guildId);
    if (!currentQueue) return;

    currentQueue.playing = false;
    destroyAudioResource(currentQueue.currentResource);
    currentQueue.currentResource = null;
    currentQueue.currentSong = null;
    currentQueue.failCount++;

    const errEmbed = new EmbedBuilder()
      .setColor("#FF0000")
      .setDescription(`Failed to play: **${song.title}**. Skipping...`);

    await currentQueue.textChannel?.send({ embeds: [errEmbed] }).catch(() => {});

    if (currentQueue.songs[0] === song) {
      currentQueue.songs.shift();
    }

    await delay(AUDIO_CONFIG.failureDelay);
    await playNextSong(guildId);
  }
}

function getActiveUsersCount(client) {
  const activeUserIds = new Set();

  for (const [guildId, queue] of queues.entries()) {
    if (!queue) continue;
    const connection = getVoiceConnection(guildId);
    if (!connection) continue;

    const channelId = connection.joinConfig?.channelId;
    if (!channelId) continue;

    const channel = client.channels.cache.get(channelId);
    if (channel?.members) {
      channel.members.forEach((member) => {
        if (!member.user.bot) {
          activeUserIds.add(member.id);
        }
      });
    }
  }

  return activeUserIds.size;
}

function runDiscord() {
  const discordClient = new Client({
    intents: [
      GatewayIntentBits.Guilds,
      GatewayIntentBits.GuildMessages,
      GatewayIntentBits.MessageContent,
      GatewayIntentBits.DirectMessages,
      GatewayIntentBits.GuildVoiceStates,
    ],
    partials: [Partials.Channel],
  });

  discordClient.once("clientReady", () => {
    log(`Discord Bot online as: ${discordClient.user.tag}`);

    const updateActivity = () => {
      const activeUsers = getActiveUsersCount(discordClient);
      discordClient.user.setActivity(`!h | ${activeUsers}`, {
        type: ActivityType.Listening,
      });
    };

    updateActivity();
    setInterval(updateActivity, 30000);
  });

  discordClient.on("messageCreate", async (message) => {
    if (message.author.bot || !message.guild) return;

    const content = message.content.trim();
    if (!content.startsWith("!")) return;

    const args = content.slice(1).trim().split(/ +/);
    const command = args.shift()?.toLowerCase();
    const voiceChannel = message.member?.voice?.channel;

    log(`[Bot Command] Guild: ${message.guild.id} | From ${message.author.tag} in #${message.channel.name}: ${content}`);

    if (command === "h") {
      const embed = new EmbedBuilder()
        .setColor("#0099FF")
        .setTitle(`Available Commands - ${message.guild.name}`)
        .setDescription(
          "**AI Assistant**\n" +
          "`!a` - Ask AI assistant a question or prompt\n\n" +
          "**Music**\n" +
          "`!p` - Play a track by link or search term\n" +
          "`!q` - Display the current song queue\n" +
          "`!s` - Skip the current track\n" +
          "`!l` - Make the bot leave the voice channel\n" +
          "`!ap` - Toggle automatic song recommendations"
        )
        .setFooter({ text: "Skips and stops are restricted to the track requester or admins." });

      return message.channel.send({ embeds: [embed] });
    }

    if (command === "a") {
      const argPrompt = args.join(" ").trim();
      let replyText = "";

      if (message.reference?.messageId) {
        try {
          const referencedMsg = await message.channel.messages.fetch(message.reference.messageId);
          replyText = referencedMsg?.content?.trim() || "";
        } catch {}
      }

      let prompt = "";
      if (argPrompt && replyText) {
        prompt = `${argPrompt}: ${replyText}`;
      } else if (argPrompt) {
        prompt = argPrompt;
      } else if (replyText) {
        prompt = replyText;
      }

      if (!prompt) {
        const embed = new EmbedBuilder().setColor("#FF0000").setDescription("Please provide a question or prompt!");
        return message.channel.send({ embeds: [embed] });
      }

      log(`[Discord AI Chat] Guild: ${message.guild.id} | From ${message.author.tag}: ${prompt}`);

      let typingInterval;
      try {
        await message.channel.sendTyping();
        typingInterval = setInterval(() => {
          message.channel.sendTyping().catch(() => {});
        }, 5000);

        const responseText = await processGeminiRequest(prompt);
        const parts = chunkText(responseText, 1900);

        for (let i = 0; i < parts.length; i++) {
          await message.channel.send(parts[i]);
        }
      } catch (err) {
        log(`Discord AI Error: ${err.message}`);
        await message.channel.send("Failed to process request.").catch(() => {});
      } finally {
        if (typingInterval) clearInterval(typingInterval);
      }
      return;
    }

    if (command === "q") {
      const serverQueue = queues.get(message.guild.id);
      if (!serverQueue || serverQueue.songs.length === 0) {
        const embed = new EmbedBuilder().setColor("#FF0000").setDescription("The queue is currently empty.");
        return message.channel.send({ embeds: [embed] });
      }

      serverQueue.textChannel = message.channel;
      const nowPlaying = serverQueue.songs[0];
      const queuedSongs = serverQueue.songs.slice(1);
      const reqInfo = nowPlaying.isManual && nowPlaying.requestedBy ? ` | Requested by: <@${nowPlaying.requestedBy.id}>` : " | *(Autoplay)*";

      let description = `**Now Playing:**\n[${nowPlaying.title}](${nowPlaying.url}) ${nowPlaying.artist && nowPlaying.artist !== "Unknown Artist" ? `by ${nowPlaying.artist}` : ""}${reqInfo}\n\n**Up Next:**\n`;

      if (queuedSongs.length === 0) {
        description += "No upcoming songs manually queued.";
      } else {
        const displayList = queuedSongs.slice(0, 10);
        description += displayList
          .map((song, index) => `${index + 1}. [${song.title}](${song.url}) - ${song.requestedBy?.id ? `<@${song.requestedBy.id}>` : "Autoplay"}`)
          .join("\n");

        if (queuedSongs.length > 10) {
          description += `\n\n...and ${queuedSongs.length - 10} more song(s).`;
        }
      }

      const queueEmbed = new EmbedBuilder()
        .setColor("#0099FF")
        .setTitle(`Music Queue - ${message.guild.name}`)
        .setDescription(description)
        .setFooter({ text: `Autoplay: ${serverQueue.autoplay ? "Enabled" : "Disabled"} | Total Songs in Queue: ${serverQueue.songs.length}` });

      return message.channel.send({ embeds: [queueEmbed] });
    }

    if (command === "p") {
      if (!voiceChannel) {
        const embed = new EmbedBuilder().setColor("#FF0000").setDescription("You need to join a voice channel first!");
        return message.channel.send({ embeds: [embed] });
      }

      const botChannelId = getVoiceConnection(message.guild.id)?.joinConfig?.channelId || message.guild.members.me?.voice?.channelId;
      if (botChannelId && voiceChannel.id !== botChannelId) {
        const embed = new EmbedBuilder().setColor("#FF0000").setDescription("You must be in the same voice channel as the bot to play music!");
        return message.channel.send({ embeds: [embed] });
      }

      const query = args.join(" ").trim();
      if (!query) {
        const embed = new EmbedBuilder().setColor("#FF0000").setDescription("Please provide a song title, playlist URL, or song URL!");
        return message.channel.send({ embeds: [embed] });
      }

      try {
        const isPlaylist = /[?&]list=|\/playlist/i.test(query);
        let songsToAdd = [];
        const requesterData = { id: message.author.id, tag: message.author.tag };
        const directVideoId = extractVideoId(query);

        if (isPlaylist && !directVideoId) {
          const playlistTracks = await getPlaylistTracks(query);
          songsToAdd = playlistTracks.map((track) => normalizeSongMetadata({ ...track, requestedBy: requesterData, isManual: true }));
        } else if (directVideoId) {
          const normUrl = `https://www.youtube.com/watch?v=${directVideoId}`;
          const trackInfo = await fetchVideoInfo(directVideoId);
          if (trackInfo) {
            songsToAdd.push(normalizeSongMetadata({ ...trackInfo, url: normUrl, requestedBy: requesterData, isManual: true }));
          }
        } else {
          const searchResult = await searchYtMusic(query, 10);
          if (searchResult.length > 0) {
            songsToAdd.push(normalizeSongMetadata({ ...searchResult[0], requestedBy: requesterData, isManual: true }));
          }
        }

        songsToAdd = songsToAdd.map(normalizeSongMetadata).filter(isValidSong);

        if (songsToAdd.length === 0) {
          const embed = new EmbedBuilder().setColor("#FF0000").setDescription("No valid tracks were found.");
          return message.channel.send({ embeds: [embed] });
        }

        let serverQueue = queues.get(message.guild.id);
        if (!serverQueue) {
          const connection = joinVoiceChannel({
            channelId: voiceChannel.id,
            guildId: message.guild.id,
            adapterCreator: message.guild.voiceAdapterCreator,
            selfDeaf: true,
          });

          const player = createAudioPlayer();
          connection.subscribe(player);

          serverQueue = {
            textChannel: message.channel,
            connection,
            player,
            songs: [],
            lastSong: null,
            currentSong: null,
            currentResource: null,
            autoplay: true,
            failCount: 0,
            transitioning: false,
            playing: false,
            autoplayPrefetch: null,
            autoplayGeneration: 0,
            playedIds: new Set(),
            playedUrls: new Set(),
            playedKeys: new Set(),
          };

          queues.set(message.guild.id, serverQueue);

          for (const song of songsToAdd) {
            const id = getSongId(song);
            if (!serverQueue.playedIds.has(id)) {
              serverQueue.songs.push(song);
            }
          }

          const currentGuildId = message.guild.id;

          player.on(AudioPlayerStatus.Idle, async () => {
            const sq = queues.get(currentGuildId);
            if (!sq) return;

            log(`[Audio Idle] Guild ${currentGuildId} | Current: ${sq.currentSong?.title || "Unknown"}`);
            sq.playing = false;
            destroyAudioResource(sq.currentResource);
            sq.currentResource = null;
            sq.currentSong = null;
            await advanceQueue(currentGuildId, "idle");
          });

          player.on("error", async (err) => {
            log(`[Audio Player Error] Guild ${currentGuildId}: ${err.message}`);
            const sq = queues.get(currentGuildId);
            if (!sq) return;

            sq.playing = false;
            destroyAudioResource(sq.currentResource);
            sq.currentResource = null;
            sq.currentSong = null;
            sq.failCount++;
            await advanceQueue(currentGuildId, "error");
          });

          connection.on("stateChange", (oldState, newState) => {
            log(`[Voice State] Guild ${currentGuildId} | ${oldState.status} -> ${newState.status}`);
          });

          if (isPlaylist && !directVideoId) {
            const embed = new EmbedBuilder().setColor("#00FF00").setDescription(`Added **${songsToAdd.length}** tracks from playlist to queue!`);
            await message.channel.send({ embeds: [embed] });
          }

          playNextSong(message.guild.id).catch((err) => {
            log(`[Initial Playback Error] Guild ${message.guild.id}: ${err.message}`);
          });

          return;
        }

        serverQueue.textChannel = message.channel;
        const existingIds = new Set(serverQueue.songs.map(getSongId).filter(Boolean));
        const newSongs = songsToAdd.filter((song) => {
          const id = getSongId(song);
          return id && !existingIds.has(id);
        });

        serverQueue.songs.push(...newSongs);
        log(`[Queue Added] Guild ${message.guild.id} | Added ${newSongs.length} tracks`);

        if (isPlaylist && !directVideoId) {
          const embed = new EmbedBuilder().setColor("#00FF00").setDescription(`Added **${newSongs.length}** tracks from playlist to queue!`);
          return message.channel.send({ embeds: [embed] });
        }

        const firstSong = newSongs[0] || songsToAdd[0];
        const artistText = firstSong.artist && firstSong.artist !== "Unknown Artist" ? ` by ${firstSong.artist}` : "";
        const embed = new EmbedBuilder()
          .setColor("#00FF00")
          .setDescription(`Added to queue: [${firstSong.title}](${firstSong.url})${artistText}`);

        const videoId = getSongId(firstSong);
        if (videoId) {
          embed.setThumbnail(`https://img.youtube.com/vi/${videoId}/mqdefault.jpg`);
        }

        return message.channel.send({ embeds: [embed] });
      } catch (err) {
        log(`Music Play Error: ${err.message}`);
        const embed = new EmbedBuilder().setColor("#FF0000").setDescription("An error occurred while searching or loading the track.");
        return message.channel.send({ embeds: [embed] });
      }
    }

    if (command === "ap") {
      const serverQueue = queues.get(message.guild.id);
      if (!serverQueue) {
        const embed = new EmbedBuilder().setColor("#FF0000").setDescription("Play a song first to toggle autoplay!");
        return message.channel.send({ embeds: [embed] });
      }

      if (!voiceChannel) {
        const embed = new EmbedBuilder().setColor("#FF0000").setDescription("You need to join a voice channel first!");
        return message.channel.send({ embeds: [embed] });
      }

      const botChannelId = getVoiceConnection(message.guild.id)?.joinConfig?.channelId || message.guild.members.me?.voice?.channelId;
      if (botChannelId && voiceChannel.id !== botChannelId) {
        const embed = new EmbedBuilder().setColor("#FF0000").setDescription("You must be in the same voice channel as the bot to toggle autoplay!");
        return message.channel.send({ embeds: [embed] });
      }

      serverQueue.textChannel = message.channel;
      if (!canControlQueue(message, serverQueue)) {
        const embed = new EmbedBuilder().setColor("#FF0000").setDescription("Only the person who played this song can toggle autoplay.");
        return message.channel.send({ embeds: [embed] });
      }

      serverQueue.autoplay = !serverQueue.autoplay;
      log(`[Autoplay Toggle] Guild ${message.guild.id} | ${serverQueue.autoplay}`);

      if (serverQueue.autoplay && serverQueue.lastSong && serverQueue.songs.length === 1 && !serverQueue.autoplayPrefetch) {
        warmAutoplayCandidate(serverQueue, serverQueue.lastSong);
      }

      if (!serverQueue.autoplay) {
        serverQueue.autoplayPrefetch = null;
        serverQueue.autoplayGeneration++;
      }

      const embed = new EmbedBuilder()
        .setColor(serverQueue.autoplay ? "#00FF00" : "#FF0000")
        .setDescription(`Autoplay has been **${serverQueue.autoplay ? "enabled" : "disabled"}**`);

      return message.channel.send({ embeds: [embed] });
    }

    if (command === "s") {
      const serverQueue = queues.get(message.guild.id);
      if (!serverQueue || serverQueue.songs.length === 0) {
        const embed = new EmbedBuilder().setColor("#FF0000").setDescription("No song is currently playing.");
        return message.channel.send({ embeds: [embed] });
      }

      if (!voiceChannel) {
        const embed = new EmbedBuilder().setColor("#FF0000").setDescription("You need to join a voice channel first!");
        return message.channel.send({ embeds: [embed] });
      }

      const botChannelId = getVoiceConnection(message.guild.id)?.joinConfig?.channelId || message.guild.members.me?.voice?.channelId;
      if (botChannelId && voiceChannel.id !== botChannelId) {
        const embed = new EmbedBuilder().setColor("#FF0000").setDescription("You must be in the same voice channel as the bot to skip!");
        return message.channel.send({ embeds: [embed] });
      }

      serverQueue.textChannel = message.channel;
      if (!canControlQueue(message, serverQueue)) {
        const reqUser = serverQueue.songs[0]?.requestedBy;
        const reqTag = reqUser ? `<@${reqUser.id}>` : "the requester";
        const embed = new EmbedBuilder().setColor("#FF0000").setDescription(`Only ${reqTag} (or an Admin) can skip this track.`);
        return message.channel.send({ embeds: [embed] });
      }

      const currentSong = serverQueue.songs[0];
      log(`[Skip Command] Guild ${message.guild.id} | Skipped by ${message.author.tag}`);

      const artistText = currentSong.artist && currentSong.artist !== "Unknown Artist" ? ` by ${currentSong.artist}` : "";
      const embed = new EmbedBuilder()
        .setColor("#FFFF00")
        .setDescription(`**${currentSong.title}**${artistText} was skipped by ${message.author}`);

      try { serverQueue.player.stop(); } catch {}
      return message.channel.send({ embeds: [embed] });
    }

    if (command === "l") {
      const serverQueue = queues.get(message.guild.id);
      const botChannelId = getVoiceConnection(message.guild.id)?.joinConfig?.channelId || message.guild.members.me?.voice?.channelId;

      if (!voiceChannel) {
        const embed = new EmbedBuilder().setColor("#FF0000").setDescription("You need to join a voice channel first!");
        return message.channel.send({ embeds: [embed] });
      }

      if (botChannelId && voiceChannel.id !== botChannelId) {
        const embed = new EmbedBuilder().setColor("#FF0000").setDescription("You must be in the same voice channel as the bot to stop/leave!");
        return message.channel.send({ embeds: [embed] });
      }

      if (serverQueue) {
        if (!canControlQueue(message, serverQueue)) {
          const reqUser = serverQueue.songs[0]?.requestedBy;
          const reqTag = reqUser ? `<@${reqUser.id}>` : "the requester";
          const embed = new EmbedBuilder().setColor("#FF0000").setDescription(`Only ${reqTag} (or an Admin) can stop the player.`);
          return message.channel.send({ embeds: [embed] });
        }

        log(`[Stop Command] Guild ${message.guild.id} | Stopped by ${message.author.tag}`);
        destroyServerQueue(message.guild.id);

        const embed = new EmbedBuilder().setColor("#FF0000").setDescription("Music stopped and disconnected.");
        return message.channel.send({ embeds: [embed] });
      }

      const existingConnection = getVoiceConnection(message.guild.id);
      if (existingConnection) {
        log(`[Leave Command] Guild ${message.guild.id} | Left by ${message.author.tag}`);
        try { existingConnection.destroy(); } catch {}

        const embed = new EmbedBuilder().setColor("#FF0000").setDescription("Disconnected from voice channel.");
        return message.channel.send({ embeds: [embed] });
      }

      const embed = new EmbedBuilder().setColor("#FF0000").setDescription("Bot is not in a voice channel.");
      return message.channel.send({ embeds: [embed] });
    }
  });

  if (process.env.DISCORD_BOT_TOKEN) {
    discordClient.login(process.env.DISCORD_BOT_TOKEN).catch((err) => {
      log(`Discord Login Error: ${err.message}`);
    });
  } else {
    log("DISCORD_BOT_TOKEN is missing in .env file!");
  }
}

module.exports = runDiscord;
