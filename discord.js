require("dotenv").config();
const fs = require("fs");
const path = require("path");
const os = require("os");
const { spawn, exec } = require("child_process");
const util = require("util");
const execPromise = util.promisify(exec);
const { Client, GatewayIntentBits, Partials, EmbedBuilder, PermissionFlagsBits, ActivityType } = require("discord.js");
const {
  joinVoiceChannel,
  createAudioPlayer,
  createAudioResource,
  AudioPlayerStatus,
  getVoiceConnection,
  StreamType,
} = require("@discordjs/voice");
const play = require("play-dl");

const LOG_FILE = path.join(os.homedir(), "SERVER", "Backup", "discord.log");

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

process.on("uncaughtException", (err) => {
  if (err.code === "EPIPE") {
    log(`[Warning] Ignored EPIPE stream error: ${err.message}`);
    return;
  }
  log(`[Uncaught Exception]: ${err.stack || err.message}`);
});

process.on("unhandledRejection", (reason) => {
  log(`[Unhandled Rejection]: ${reason?.stack || reason}`);
});

function chunkText(text, maxLength = 1900) {
  if (!text) return [];
  return text.match(new RegExp(`.{1,${maxLength}}`, "gs")) || [text];
}

function extractVideoId(url) {
  if (!url) return null;
  const match = url.match(/(?:v=|\/embed\/|\/v\/|youtu\.be\/|watch\?v=|^)([a-zA-Z0-9_-]{11})/);
  return match ? match[1] : null;
}

function normalizeUrl(url) {
  if (!url) return url;
  const videoId = extractVideoId(url);
  if (videoId) {
    return `https://www.youtube.com/watch?v=${videoId}`;
  }
  return url.replace("music.youtube.com", "www.youtube.com");
}

function getTitleKey(title) {
  if (!title) return "";
  return title
    .toLowerCase()
    .replace(/[\(\[\{].*?[\)\]\}]/g, "")
    .replace(/\b(official|video|audio|lyrics?|lirik|lagu|terjemahan|subtitle|sub|indonesia|mv|hd|4k|ft|feat|featuring|remix|cover|prod|topic)\b/gi, "")
    .replace(/[^a-z0-9]/g, " ")
    .trim()
    .split(/\s+/)
    .filter((w) => w.length > 1)
    .sort()
    .join("");
}

function cleanAiText(text) {
  if (!text) return "";
  return text
    .replace(/User Safety:\s*\w+/gi, "")
    .replace(/\[\s*Safety Status:\s*\w+\s*\]/gi, "")
    .trim();
}

function isSingleSong(video) {
  if (!video) return false;

  const duration = video.durationInSec || video.duration || 0;
  if (duration > 600) return false;

  const title = (video.title || "").toLowerCase();
  const blacklist = [
    "lyric",
    "lyrics",
    "lirik",
    "orchestral",
    "soundtrack version",
    "fanmade",
    "slowed",
    "reverb",
    "full album",
    "greatest hits",
    "best of",
    "compilation",
    "1 hour",
    "2 hour",
    "3 hour",
    "10 hour",
    "nonstop",
    "collection",
    "top 10",
    "top 20",
    "top 50",
    "megamix",
    "discography",
    "safety",
    "induction",
    "tutorial",
  ];

  return !blacklist.some((term) => title.includes(term));
}

function canControlQueue(message, serverQueue) {
  if (!serverQueue) return true;
  const currentSong = serverQueue.songs[0] || serverQueue.lastSong;
  if (!currentSong || !currentSong.requestedBy) return true;

  if (currentSong.requestedBy.id === message.author.id) return true;

  if (
    message.member?.permissions?.has(PermissionFlagsBits.Administrator) ||
    message.member?.permissions?.has(PermissionFlagsBits.ManageChannels)
  ) {
    return true;
  }

  return false;
}

async function fetchOembedInfo(videoId, requesterData) {
  try {
    const normUrl = `https://www.youtube.com/watch?v=${videoId}`;
    const res = await fetch(`https://www.youtube.com/oembed?url=${encodeURIComponent(normUrl)}&format=json`);
    if (res.ok) {
      const json = await res.json();
      return {
        title: json.title,
        artist: json.author_name ? json.author_name.replace(/ - Topic$/i, "") : "Unknown Artist",
        url: normUrl,
        id: videoId,
        durationInSec: 0,
        requestedBy: requesterData,
        isManual: true,
      };
    }
  } catch (err) {
    log(`oEmbed Fetch Error: ${err.message}`);
  }
  return null;
}

async function searchYtMusic(query, limit = 10) {
  try {
    const results = await play.search(query, { limit, source: { youtube: "video" } }).catch((e) => {
      log(`play-dl search catch: ${e.message}`);
      return null;
    });

    if (results && results.length > 0) {
      const formatted = results.map((r) => {
        let artistName = "Unknown Artist";
        if (r.channel && r.channel.name) {
          artistName = r.channel.name.replace(/ - Topic$/i, "");
        }
        return {
          title: r.title,
          artist: artistName,
          url: r.url,
          id: r.id || extractVideoId(r.url),
          durationInSec: r.durationInSec || 0,
        };
      });

      const validTracks = formatted.filter((t) => isSingleSong(t));
      if (validTracks.length > 0) return validTracks;
      return formatted;
    }
  } catch (err) {
    log(`play-dl Search Error: ${err.message}`);
  }

  return new Promise((resolve) => {
    const proc = spawn("yt-dlp", [
      "--extractor-args", "youtube:player_client=ios,android",
      "--flat-playlist",
      "--no-warnings",
      "-j",
      `ytsearch${limit}:${query}`,
    ]);

    let data = "";
    proc.stdout.on("data", (chunk) => (data += chunk));
    proc.on("close", () => {
      try {
        const lines = data.trim().split("\n").filter(Boolean);
        const tracks = lines.map((line) => {
          const json = JSON.parse(line);
          return {
            title: json.title,
            artist: json.artist || json.uploader || json.channel || "Unknown Artist",
            url: json.url || `https://www.youtube.com/watch?v=${json.id}`,
            id: json.id,
            durationInSec: json.duration || 0,
          };
        });
        const valid = tracks.filter((t) => isSingleSong(t));
        resolve(valid.length > 0 ? valid : tracks);
      } catch {
        resolve([]);
      }
    });
    proc.on("error", () => resolve([]));
  });
}

function getPlaylistTracks(playlistUrl) {
  return new Promise((resolve) => {
    if (!playlistUrl) return resolve([]);

    const proc = spawn("yt-dlp", [
      "--extractor-args", "youtube:player_client=ios,android",
      "--flat-playlist",
      "--no-warnings",
      "-j",
      playlistUrl,
    ]);

    let data = "";
    proc.stdout.on("data", (chunk) => (data += chunk));
    proc.on("close", () => {
      try {
        const lines = data.trim().split("\n").filter(Boolean);
        const tracks = lines
          .map((line) => {
            const json = JSON.parse(line);
            return {
              title: json.title,
              artist: json.artist || json.uploader || json.channel || "Unknown Artist",
              url: json.url || `https://www.youtube.com/watch?v=${json.id}`,
              id: json.id,
              durationInSec: json.duration || 0,
            };
          })
          .filter((t) => t.id && t.title && t.title !== "[Private video]" && t.title !== "[Deleted video]");

        resolve(tracks);
      } catch {
        resolve([]);
      }
    });
    proc.on("error", () => resolve([]));
  });
}

function getYtMusicRadioTracks(videoId) {
  return new Promise((resolve) => {
    if (!videoId) return resolve([]);
    const radioUrl = `https://www.youtube.com/watch?v=${videoId}&list=RD${videoId}`;
    
    const proc = spawn("yt-dlp", [
      "--extractor-args", "youtube:player_client=ios,android",
      "--flat-playlist",
      "--no-warnings",
      "-j",
      radioUrl
    ]);

    let data = "";
    proc.stdout.on("data", (chunk) => (data += chunk));
    proc.on("close", () => {
      try {
        const lines = data.trim().split("\n").filter(Boolean);
        const tracks = lines.map((line) => {
          const json = JSON.parse(line);
          return {
            title: json.title,
            artist: json.artist || json.uploader || json.channel || "Unknown Artist",
            url: `https://www.youtube.com/watch?v=${json.id}`,
            id: json.id,
            durationInSec: json.duration || 0,
          };
        });
        resolve(tracks);
      } catch {
        resolve([]);
      }
    });
    proc.on("error", () => resolve([]));
  });
}

const queues = new Map();
const delay = (ms) => new Promise((res) => setTimeout(res, ms));

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
      headers: {
        "Content-Type": "application/json",
      },
      body: JSON.stringify({
        systemInstruction: {
          parts: [
            { text: "You are Zero Two, a helpful, friendly, and smart AI assistant." }
          ],
        },
        contents: [
          {
            role: "user",
            parts: [{ text: prompt }],
          },
        ],
        generationConfig: {
          temperature: 0.7,
        },
      }),
    });

    if (!res.ok) {
      const errText = await res.text();
      throw new Error(`Gemini API HTTP ${res.status}: ${errText}`);
    }

    const data = await res.json();
    const rawContent = data?.candidates?.[0]?.content?.parts?.[0]?.text || null;
    return cleanAiText(rawContent);
  } catch (err) {
    log(`Gemini Error: ${err.message}`);
    throw err;
  }
}

async function getDirectAudioUrl(targetUrl, extraArgs = []) {
  const args = [
    "-g",
    "-f", "251/bestaudio/b",
    "--no-playlist",
    "--no-warnings",
    "--no-check-certificates",
    ...extraArgs,
    `"${targetUrl}"`
  ].join(" ");

  const { stdout } = await execPromise(`yt-dlp ${args}`, { timeout: 8000 });
  const directUrl = stdout.trim().split("\n")[0];
  if (!directUrl || !directUrl.startsWith("http")) {
    throw new Error("Invalid direct audio URL retrieved");
  }
  return directUrl;
}

function streamFromDirectUrl(directUrl) {
  return new Promise((resolve, reject) => {
    const ffmpegProcess = spawn(
      "ffmpeg",
      [
        "-reconnect", "1",
        "-reconnect_streamed", "1",
        "-reconnect_delay_max", "5",
        "-probesize", "32768",
        "-analyzeduration", "0",
        "-loglevel", "error",
        "-hide_banner",
        "-i", directUrl,
        "-vn",
        "-af", "volume=0.85",
        "-ac", "2",
        "-ar", "48000",
        "-c:a", "pcm_s16le",
        "-f", "s16le",
        "pipe:1",
      ],
      { stdio: ["ignore", "pipe", "pipe"] }
    );

    let settled = false;
    let stderrBuf = "";

    const fail = (reason) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeoutId);
      try { ffmpegProcess.kill("SIGKILL"); } catch {}
      reject(new Error(reason));
    };

    const timeoutId = setTimeout(() => fail("Timed out waiting for FFmpeg audio data"), 10000);

    ffmpegProcess.stdout.once("data", (chunk) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeoutId);
      ffmpegProcess.stdout.pause();
      ffmpegProcess.stdout.unshift(chunk);
      resolve(
        createAudioResource(ffmpegProcess.stdout, {
          inputType: StreamType.Raw,
          inlineVolume: false,
          highWaterMark: 1 << 23,
        })
      );
    });

    ffmpegProcess.stderr.on("data", (data) => {
      const msg = data.toString().trim();
      if (msg && !msg.includes("Broken pipe") && !msg.includes("Error writing trailer")) {
        stderrBuf = `${stderrBuf} [ffmpeg] ${msg}`.slice(-500);
      }
    });

    ffmpegProcess.once("error", (err) => fail(`ffmpeg process error: ${err.message}`));

    ffmpegProcess.once("close", (code) => {
      if (!settled) {
        fail(stderrBuf.trim() || `FFmpeg process exited (code ${code}) before producing audio`);
      }
    });
  });
}

function streamWithYtdlp(targetUrl, extraArgs = []) {
  return new Promise((resolve, reject) => {
    const ytdlpArgs = [
      "-o", "-",
      "-f", "251/bestaudio/b",
      "--no-playlist",
      "--no-warnings",
      "--no-check-certificates",
      ...extraArgs,
      targetUrl,
    ];

    const ytdlpProcess = spawn("yt-dlp", ytdlpArgs, {
      stdio: ["ignore", "pipe", "pipe"],
    });

    const ffmpegProcess = spawn(
      "ffmpeg",
      [
        "-probesize", "32768",
        "-analyzeduration", "0",
        "-loglevel", "error",
        "-hide_banner",
        "-i", "pipe:0",
        "-vn",
        "-af", "volume=0.85",
        "-ac", "2",
        "-ar", "48000",
        "-c:a", "pcm_s16le",
        "-f", "s16le",
        "pipe:1",
      ],
      { stdio: ["pipe", "pipe", "pipe"] }
    );

    ffmpegProcess.stdin.on("error", (err) => {
      if (err.code === "EPIPE") return;
    });

    ytdlpProcess.stdout.on("error", (err) => {
      if (err.code === "EPIPE") return;
    });

    ytdlpProcess.stdout.pipe(ffmpegProcess.stdin);

    let settled = false;
    let stderrBuf = "";

    const fail = (reason) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeoutId);
      try { ytdlpProcess.stdout.unpipe(ffmpegProcess.stdin); } catch {}
      try { ytdlpProcess.kill("SIGKILL"); } catch {}
      try { ffmpegProcess.kill("SIGKILL"); } catch {}
      reject(new Error(reason));
    };

    const timeoutId = setTimeout(() => fail("Timed out waiting for audio data"), 10000);

    ffmpegProcess.stdout.once("data", (chunk) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeoutId);
      ffmpegProcess.stdout.pause();
      ffmpegProcess.stdout.unshift(chunk);
      resolve(
        createAudioResource(ffmpegProcess.stdout, {
          inputType: StreamType.Raw,
          inlineVolume: false,
          highWaterMark: 1 << 23,
        })
      );
    });

    ytdlpProcess.stderr.on("data", (data) => {
      const msg = data.toString().trim();
      if (msg && !msg.includes("WARNING")) {
        stderrBuf = `${stderrBuf} [yt-dlp] ${msg}`.slice(-500);
      }
    });

    ffmpegProcess.stderr.on("data", (data) => {
      const msg = data.toString().trim();
      if (msg && !msg.includes("Broken pipe") && !msg.includes("Error writing trailer")) {
        stderrBuf = `${stderrBuf} [ffmpeg] ${msg}`.slice(-500);
      }
    });

    ytdlpProcess.once("error", (err) => fail(`yt-dlp process error: ${err.message}`));
    ffmpegProcess.once("error", (err) => fail(`ffmpeg process error: ${err.message}`));

    ffmpegProcess.once("close", (code) => {
      if (!settled) {
        fail(stderrBuf.trim() || `Process exited (code ${code}) before producing audio`);
      }
    });
  });
}

async function createAudioResourceFromSong(song) {
  const targetUrl = normalizeUrl(song.url);

  try {
    const directUrl = await getDirectAudioUrl(targetUrl, [
      "--extractor-args", "youtube:player_client=ios,android",
    ]);
    return await streamFromDirectUrl(directUrl);
  } catch (err1) {
    log(`Direct URL (iOS/Android) Stream Failed: ${err1.message}`);
  }

  try {
    const directUrl = await getDirectAudioUrl(targetUrl, []);
    return await streamFromDirectUrl(directUrl);
  } catch (err2) {
    log(`Direct URL (Default) Stream Failed: ${err2.message}`);
  }

  try {
    return await streamWithYtdlp(targetUrl, [
      "--extractor-args", "youtube:player_client=ios,android",
    ]);
  } catch (err3) {
    log(`yt-dlp Pipe Stream Failed: ${err3.message}`);
  }

  try {
    const pdlStream = await play.stream(targetUrl, {
      discordPlayerCompatibility: true,
    }).catch((e) => {
      log(`play-dl stream Catch: ${e.message}`);
      return null;
    });
    if (pdlStream) {
      return createAudioResource(pdlStream.stream, {
        inputType: pdlStream.type,
        highWaterMark: 1 << 23,
      });
    }
  } catch (err4) {
    log(`play-dl Engine Failed: ${err4.message}`);
  }

  throw new Error("All audio streaming engines failed.");
}

async function getAutoplayCandidate(serverQueue, baseSong) {
  const videoId = baseSong.id || extractVideoId(baseSong.url);
  let nextSong = null;

  const baseArtist = (baseSong.artist || "").toLowerCase().replace(/ - topic$/i, "").trim();

  const radioTracks = await getYtMusicRadioTracks(videoId);
  if (radioTracks && radioTracks.length > 0) {
    nextSong = radioTracks.find((s) => {
      if (!isSingleSong(s)) return false;
      const titleKey = getTitleKey(s.title);
      if (serverQueue.playedKeys.has(titleKey)) return false;
      if (serverQueue.playedUrls.has(s.url)) return false;

      const candidateArtist = (s.artist || "").toLowerCase().replace(/ - topic$/i, "").trim();
      if (baseArtist && candidateArtist && (candidateArtist.includes(baseArtist) || baseArtist.includes(candidateArtist))) {
        return false;
      }
      return true;
    });

    if (!nextSong) {
      nextSong = radioTracks.find((s) => {
        if (!isSingleSong(s)) return false;
        const titleKey = getTitleKey(s.title);
        if (serverQueue.playedKeys.has(titleKey)) return false;
        if (serverQueue.playedUrls.has(s.url)) return false;
        return true;
      });
    }
  }

  if (!nextSong) {
    const cleanTitle = baseSong.title
      .replace(/[\(\[\{].*?[\)\]\}]/g, "")
      .replace(/ft\..*$/i, "")
      .replace(/feat\..*$/i, "")
      .trim();

    const searchResult = await searchYtMusic(`songs like ${cleanTitle}`, 15);
    if (searchResult && searchResult.length > 0) {
      nextSong = searchResult.find((s) => {
        if (!isSingleSong(s)) return false;
        const titleKey = getTitleKey(s.title);
        if (serverQueue.playedKeys.has(titleKey)) return false;
        if (serverQueue.playedUrls.has(s.url)) return false;

        const candidateArtist = (s.artist || "").toLowerCase().replace(/ - topic$/i, "").trim();
        if (baseArtist && candidateArtist && (candidateArtist.includes(baseArtist) || baseArtist.includes(candidateArtist))) {
          return false;
        }
        return true;
      });

      if (!nextSong) {
        nextSong = searchResult.find((s) => {
          if (!isSingleSong(s)) return false;
          const titleKey = getTitleKey(s.title);
          if (serverQueue.playedKeys.has(titleKey)) return false;
          if (serverQueue.playedUrls.has(s.url)) return false;
          return true;
        });
      }
    }
  }

  if (!nextSong) return null;

  return {
    title: nextSong.title,
    artist: nextSong.artist || "Unknown Artist",
    url: nextSong.url,
    id: nextSong.id || extractVideoId(nextSong.url),
    requestedBy: baseSong.requestedBy || null,
    isManual: false,
  };
}

function warmAutoplayCandidate(serverQueue, baseSong) {
  if (!serverQueue.autoplay) return;
  serverQueue.autoplayPrefetch = {
    songUrl: baseSong.url,
    promise: getAutoplayCandidate(serverQueue, baseSong)
      .catch((err) => {
        log(`Autoplay Prefetch Error: ${err.message}`);
        return null;
      }),
  };
}

async function playNextSong(guildId) {
  const serverQueue = queues.get(guildId);
  if (!serverQueue) return;

  if (serverQueue.failCount >= 3) {
    log(`[Music] Guild ${guildId}: 3 consecutive errors. Stopping playback.`);
    const errEmbed = new EmbedBuilder()
      .setColor("#FF0000")
      .setDescription("Too many consecutive playback errors.");
    serverQueue.textChannel.send({ embeds: [errEmbed] });
    serverQueue.connection.destroy();
    queues.delete(guildId);
    return;
  }

  if (serverQueue.songs.length === 0) {
    if (serverQueue.autoplay && serverQueue.lastSong) {
      try {
        let nextSong = null;

        const prefetch = serverQueue.autoplayPrefetch;
        if (prefetch && prefetch.songUrl === serverQueue.lastSong.url) {
          nextSong = await prefetch.promise;
        }
        serverQueue.autoplayPrefetch = null;

        if (!nextSong) {
          nextSong = await getAutoplayCandidate(serverQueue, serverQueue.lastSong);
        }

        if (nextSong) {
          serverQueue.songs.push(nextSong);
          serverQueue.playedUrls.add(nextSong.url);
          serverQueue.playedKeys.add(getTitleKey(nextSong.title));

          log(`[Autoplay YTM Picked]: ${nextSong.title} - ${nextSong.artist} (${nextSong.url})`);
        } else {
          throw new Error("No suitable unique YouTube Music radio track found.");
        }
      } catch (err) {
        log(`Autoplay Error: ${err.message}`);
        serverQueue.connection.destroy();
        queues.delete(guildId);
        return;
      }
    } else {
      const finishEmbed = new EmbedBuilder()
        .setColor("#FF0000")
        .setDescription("Queue finished.");
      serverQueue.textChannel.send({ embeds: [finishEmbed] });
      serverQueue.connection.destroy();
      queues.delete(guildId);
      return;
    }
  }

  const song = serverQueue.songs[0];

  if (!song.isManual && !isSingleSong(song)) {
    log(`[Skipping Blacklisted Autoplay Song]: ${song.title}`);
    serverQueue.songs.shift();
    return playNextSong(guildId);
  }

  serverQueue.lastSong = song;
  serverQueue.playedUrls.add(song.url);
  serverQueue.playedKeys.add(getTitleKey(song.title));

  if (serverQueue.songs.length === 1) {
    warmAutoplayCandidate(serverQueue, song);
  }

  try {
    log(`[Music Stream Start] Guild: ${guildId} | Playing: ${song.title} - ${song.artist}`);
    const resource = await createAudioResourceFromSong(song);

    serverQueue.player.play(resource);
    serverQueue.failCount = 0;

    const artistText = song.artist && song.artist !== "Unknown Artist" ? ` by ${song.artist}` : "";
    const reqText = song.isManual && song.requestedBy ? `\nRequested by: <@${song.requestedBy.id}>` : "";
    
    const embed = new EmbedBuilder()
      .setColor("#00FF00")
      .setDescription(`Started playing [${song.title}](${song.url})${artistText}${reqText}`)
      .setThumbnail(`https://img.youtube.com/vi/${song.id}/mqdefault.jpg`);

    serverQueue.textChannel.send({ embeds: [embed] });
  } catch (err) {
    log(`Play Stream Error: ${err.message}`);
    serverQueue.failCount++;
    const errPlayEmbed = new EmbedBuilder()
      .setColor("#FF0000")
      .setDescription(`Failed to play: **${song.title}**. Skipping...`);
    serverQueue.textChannel.send({ embeds: [errPlayEmbed] });
    serverQueue.songs.shift();
    await delay(2000);
    playNextSong(guildId);
  }
}

function getActiveUsersCount(client) {
  const activeUserIds = new Set();

  for (const [guildId, queue] of queues.entries()) {
    if (!queue) continue;
    
    const connection = getVoiceConnection(guildId);
    if (connection) {
      const channelId = connection.joinConfig?.channelId;
      if (channelId) {
        const channel = client.channels.cache.get(channelId);
        if (channel && channel.members) {
          channel.members.forEach((member) => {
            if (!member.user.bot) {
              activeUserIds.add(member.id);
            }
          });
        }
      }
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
      discordClient.user.setActivity(`!h | ${activeUsers}`, { type: ActivityType.Listening });
    };

    updateActivity();
    setInterval(updateActivity, 30 * 1000);
  });

  discordClient.on("messageCreate", async (message) => {
    if (message.author.bot) return;
    if (!message.guild) return;

    const content = message.content.trim();
    if (!content.startsWith("!")) return;

    const prefixLen = 1;
    const args = content.slice(prefixLen).trim().split(/ +/);
    const command = args.shift()?.toLowerCase();
    const voiceChannel = message.member?.voice?.channel;

    log(`[Bot Command] Guild: ${message.guild.id} | From ${message.author.tag} in #${message.channel.name}: ${content}`);

    if (command === "h") {
      const helpEmbed = new EmbedBuilder()
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
        .setFooter({
          text: "Note: Skips and stops are restricted to the track requester or admins.",
        });

      return message.channel.send({ embeds: [helpEmbed] });
    }

    if (command === "a") {
      let argPrompt = args.join(" ").trim();
      let replyText = "";

      if (message.reference?.messageId) {
        try {
          const referencedMsg = await message.channel.messages.fetch(
            message.reference.messageId
          );
          replyText = referencedMsg?.content?.trim() || "";
        } catch {}
      }

      let prompt = "";
      if (argPrompt && replyText) prompt = `${argPrompt}: ${replyText}`;
      else if (argPrompt) prompt = argPrompt;
      else if (replyText) prompt = replyText;

      if (!prompt) {
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("Please provide a question or prompt!");
        return message.channel.send({ embeds: [embed] });
      }

      log(`[Discord AI Chat] Guild: ${message.guild.id} | From ${message.author.tag}: ${prompt}`);

      let typingInterval;
      try {
        await message.channel.sendTyping();
        typingInterval = setInterval(() => {
          message.channel.sendTyping().catch(() => {});
        }, 5000);

        let responseText = await processGeminiRequest(prompt);

        const parts = chunkText(responseText, 1900);
        for (let i = 0; i < parts.length; i++) {
          await message.channel.send(parts[i]);
        }
      } catch (err) {
        log(`Discord AI Error: ${err.message}`);
        await message.channel.send("Failed to process request.");
      } finally {
        if (typingInterval) clearInterval(typingInterval);
      }
      return;
    }

    if (command === "q") {
      const serverQueue = queues.get(message.guild.id);
      if (!serverQueue || serverQueue.songs.length === 0) {
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("The queue is currently empty.");
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
          .map((song, index) => `${index + 1}. [${song.title}](${song.url}) - <@${song.requestedBy?.id}>`)
          .join("\n");

        if (queuedSongs.length > 10) {
          description += `\n\n...and ${queuedSongs.length - 10} more song(s).`;
        }
      }

      const queueEmbed = new EmbedBuilder()
        .setColor("#0099FF")
        .setTitle(`Music Queue - ${message.guild.name}`)
        .setDescription(description)
        .setFooter({
          text: `Autoplay: ${serverQueue.autoplay ? "Enabled" : "Disabled"} | Total Songs in Queue: ${serverQueue.songs.length}`,
        });

      return message.channel.send({ embeds: [queueEmbed] });
    }

    if (command === "p") {
      if (!voiceChannel) {
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("You need to join a voice channel first!");
        return message.channel.send({ embeds: [embed] });
      }

      const botChannelId = getVoiceConnection(message.guild.id)?.joinConfig?.channelId || message.guild.members.me?.voice?.channelId;
      if (botChannelId && voiceChannel.id !== botChannelId) {
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("You must be in the same voice channel as the bot to play music!");
        return message.channel.send({ embeds: [embed] });
      }

      const query = args.join(" ");
      if (!query) {
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("Please provide a song title, playlist URL, or song URL!");
        return message.channel.send({ embeds: [embed] });
      }

      try {
        const isPlaylist = query.includes("list=") || query.includes("playlist");
        let songsToAdd = [];

        const requesterData = {
          id: message.author.id,
          tag: message.author.tag,
        };

        const directVideoId = extractVideoId(query);

        if (isPlaylist && !query.includes("watch?v=")) {
          log(`[Playlist Processing]: ${query}`);
          const playlistTracks = await getPlaylistTracks(query);

          if (playlistTracks && playlistTracks.length > 0) {
            songsToAdd = playlistTracks.map((t) => ({
              title: t.title,
              artist: t.artist || "Unknown Artist",
              url: t.url,
              id: t.id || extractVideoId(t.url),
              requestedBy: requesterData,
              isManual: true,
            }));
          } else {
            try {
              const playlistInfo = await play.playlist_info(query, { incomplete: true }).catch((e) => {
                log(`play-dl Playlist Catch: ${e.message}`);
                return null;
              });
              if (playlistInfo) {
                const videos = await playlistInfo.all_videos();
                songsToAdd = videos.map((v) => {
                  let artistName = "Unknown Artist";
                  if (v.channel && v.channel.name) artistName = v.channel.name;
                  return {
                    title: v.title,
                    artist: artistName,
                    url: v.url,
                    id: v.id || extractVideoId(v.url),
                    requestedBy: requesterData,
                    isManual: true,
                  };
                });
              }
            } catch (pErr) {
              log(`play-dl Playlist Fallback Error: ${pErr.message}`);
            }
          }

          if (songsToAdd.length === 0) {
            const embed = new EmbedBuilder()
              .setColor("#FF0000")
              .setDescription("Could not load any tracks from the playlist.");
            return message.channel.send({ embeds: [embed] });
          }
        } else if (directVideoId) {
          const normUrl = `https://www.youtube.com/watch?v=${directVideoId}`;
          
          let trackInfo = await fetchOembedInfo(directVideoId, requesterData);

          if (!trackInfo) {
            try {
              const { stdout } = await execPromise(
                `yt-dlp --extractor-args "youtube:player_client=ios,android" -j --no-warnings "${normUrl}"`
              );
              const json = JSON.parse(stdout);
              trackInfo = {
                title: json.title,
                artist: json.artist || json.uploader || json.channel || "Unknown Artist",
                url: normUrl,
                id: directVideoId,
                durationInSec: json.duration || 0,
                requestedBy: requesterData,
                isManual: true,
              };
            } catch (yErr) {
              log(`yt-dlp Direct URL Fetch Error: ${yErr.message}`);
            }
          }

          if (!trackInfo) {
            try {
              const info = await play.video_basic_info(normUrl).catch((e) => {
                log(`play-dl video_basic_info Catch: ${e.message}`);
                return null;
              });
              if (info && info.video_details) {
                const v = info.video_details;
                let artistName = "Unknown Artist";
                if (v.channel && v.channel.name) artistName = v.channel.name.replace(/ - Topic$/i, "");
                trackInfo = {
                  title: v.title,
                  artist: artistName,
                  url: normUrl,
                  id: directVideoId,
                  durationInSec: v.durationInSec || 0,
                  requestedBy: requesterData,
                  isManual: true,
                };
              }
            } catch (e) {
              log(`play-dl video_basic_info Error: ${e.message}`);
            }
          }

          if (trackInfo) {
            songsToAdd.push(trackInfo);
          } else {
            const embed = new EmbedBuilder()
              .setColor("#FF0000")
              .setDescription("Could not fetch video from the provided URL.");
            return message.channel.send({ embeds: [embed] });
          }
        } else {
          const searchResult = await searchYtMusic(query, 10);

          if (searchResult && searchResult.length > 0) {
            const validVideo = searchResult.find((v) => isSingleSong(v)) || searchResult[0];
            songsToAdd.push({
              title: validVideo.title,
              artist: validVideo.artist || "Unknown Artist",
              url: validVideo.url,
              id: validVideo.id || extractVideoId(validVideo.url),
              requestedBy: requesterData,
              isManual: true,
            });
          } else {
            const embed = new EmbedBuilder()
              .setColor("#FF0000")
              .setDescription("No song found.");
            return message.channel.send({ embeds: [embed] });
          }
        }

        let serverQueue = queues.get(message.guild.id);

        if (!serverQueue) {
          const connection = joinVoiceChannel({
            channelId: voiceChannel.id,
            guildId: message.guild.id,
            adapterCreator: message.guild.voiceAdapterCreator,
          });

          const player = createAudioPlayer();
          connection.subscribe(player);

          serverQueue = {
            textChannel: message.channel,
            connection,
            player,
            songs: [],
            lastSong: null,
            autoplay: true,
            failCount: 0,
            playedUrls: new Set(),
            playedKeys: new Set(),
            autoplayPrefetch: null,
          };

          queues.set(message.guild.id, serverQueue);
          serverQueue.songs.push(...songsToAdd);

          const currentGuildId = message.guild.id;

          player.on(AudioPlayerStatus.Idle, () => {
            const sq = queues.get(currentGuildId);
            if (sq) {
              sq.songs.shift();
              playNextSong(currentGuildId);
            }
          });

          player.on("error", async (err) => {
            log(`Audio Player Error in Guild ${currentGuildId}: ${err.message}`);
            const sq = queues.get(currentGuildId);
            if (sq) {
              sq.failCount++;
              sq.songs.shift();
              await delay(2000);
              playNextSong(currentGuildId);
            }
          });

          if (isPlaylist && !query.includes("watch?v=")) {
            const embed = new EmbedBuilder()
              .setColor("#00FF00")
              .setDescription(`Added **${songsToAdd.length}** tracks from playlist to queue!`);
            message.channel.send({ embeds: [embed] });
          }

          playNextSong(message.guild.id);
        } else {
          serverQueue.textChannel = message.channel;
          serverQueue.songs.push(...songsToAdd);
          log(`[Queue Added] Guild: ${message.guild.id} | Added ${songsToAdd.length} tracks.`);
          
          if (isPlaylist && !query.includes("watch?v=")) {
            const embed = new EmbedBuilder()
              .setColor("#00FF00")
              .setDescription(`Added **${songsToAdd.length}** tracks from playlist to queue!`);
            return message.channel.send({ embeds: [embed] });
          } else {
            const artistText = songsToAdd[0].artist && songsToAdd[0].artist !== "Unknown Artist" ? ` by ${songsToAdd[0].artist}` : "";
            const embed = new EmbedBuilder()
              .setColor("#00FF00")
              .setDescription(`Added to queue: [${songsToAdd[0].title}](${songsToAdd[0].url})${artistText}`)
              .setThumbnail(`https://img.youtube.com/vi/${songsToAdd[0].id}/mqdefault.jpg`);
            return message.channel.send({ embeds: [embed] });
          }
        }
      } catch (err) {
        log(`Music Play Error: ${err.message}`);
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("An error occurred while searching or loading the playlist.");
        return message.channel.send({ embeds: [embed] });
      }
      return;
    }

    if (command === "ap") {
      const serverQueue = queues.get(message.guild.id);
      if (!serverQueue) {
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("Play a song first to toggle autoplay!");
        return message.channel.send({ embeds: [embed] });
      }

      if (!voiceChannel) {
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("You need to join a voice channel first!");
        return message.channel.send({ embeds: [embed] });
      }

      const botChannelId = getVoiceConnection(message.guild.id)?.joinConfig?.channelId || message.guild.members.me?.voice?.channelId;
      if (botChannelId && voiceChannel.id !== botChannelId) {
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("You must be in the same voice channel as the bot to toggle autoplay!");
        return message.channel.send({ embeds: [embed] });
      }

      serverQueue.textChannel = message.channel;

      if (!canControlQueue(message, serverQueue)) {
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("Only the person who played this song can toggle autoplay.");
        return message.channel.send({ embeds: [embed] });
      }

      serverQueue.autoplay = !serverQueue.autoplay;
      log(`[Autoplay Toggle] Guild: ${message.guild.id} | Status: ${serverQueue.autoplay}`);
      if (serverQueue.autoplay && serverQueue.lastSong && serverQueue.songs.length === 1 && !serverQueue.autoplayPrefetch) {
        warmAutoplayCandidate(serverQueue, serverQueue.lastSong);
      }
      const embed = new EmbedBuilder()
        .setColor(serverQueue.autoplay ? "#00FF00" : "#FF0000")
        .setDescription(`Autoplay has been **${serverQueue.autoplay ? "enabled" : "disabled"}**`);
      return message.channel.send({ embeds: [embed] });
    }

    if (command === "s") {
      const serverQueue = queues.get(message.guild.id);
      if (!serverQueue || serverQueue.songs.length === 0) {
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("No song is currently playing.");
        return message.channel.send({ embeds: [embed] });
      }

      if (!voiceChannel) {
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("You need to join a voice channel first!");
        return message.channel.send({ embeds: [embed] });
      }

      const botChannelId = getVoiceConnection(message.guild.id)?.joinConfig?.channelId || message.guild.members.me?.voice?.channelId;
      if (botChannelId && voiceChannel.id !== botChannelId) {
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("You must be in the same voice channel as the bot to skip!");
        return message.channel.send({ embeds: [embed] });
      }

      serverQueue.textChannel = message.channel;

      if (!canControlQueue(message, serverQueue)) {
        const reqUser = serverQueue.songs[0]?.requestedBy;
        const reqTag = reqUser ? `<@${reqUser.id}>` : "the requester";
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription(`Only ${reqTag} (or an Admin) can skip this track.`);
        return message.channel.send({ embeds: [embed] });
      }

      log(`[Skip Command] Guild: ${message.guild.id} | Skipped by: ${message.author.tag}`);
      const currentSong = serverQueue.songs[0];
      const artistText = currentSong.artist && currentSong.artist !== "Unknown Artist" ? ` by ${currentSong.artist}` : "";
      serverQueue.player.stop();
      const embed = new EmbedBuilder()
        .setColor("#FFFF00")
        .setDescription(`**${currentSong.title}**${artistText} was skipped by ${message.author}`);
      return message.channel.send({ embeds: [embed] });
    }

    if (command === "l") {
      const serverQueue = queues.get(message.guild.id);
      const botChannelId = getVoiceConnection(message.guild.id)?.joinConfig?.channelId || message.guild.members.me?.voice?.channelId;

      if (!voiceChannel) {
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("You need to join a voice channel first!");
        return message.channel.send({ embeds: [embed] });
      }

      if (botChannelId && voiceChannel.id !== botChannelId) {
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("You must be in the same voice channel as the bot to stop/leave!");
        return message.channel.send({ embeds: [embed] });
      }

      if (serverQueue) {
        if (!canControlQueue(message, serverQueue)) {
          const reqUser = serverQueue.songs[0]?.requestedBy;
          const reqTag = reqUser ? `<@${reqUser.id}>` : "the requester";
          const embed = new EmbedBuilder()
            .setColor("#FF0000")
            .setDescription(`Only ${reqTag} (or an Admin) can stop the player.`);
          return message.channel.send({ embeds: [embed] });
        }

        log(`[Stop Command] Guild: ${message.guild.id} | Stopped by: ${message.author.tag}`);
        serverQueue.songs = [];
        serverQueue.player.stop();
        serverQueue.connection.destroy();
        queues.delete(message.guild.id);
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("Music stopped and disconnected.");
        return message.channel.send({ embeds: [embed] });
      }

      const existingConnection = getVoiceConnection(message.guild.id);
      if (existingConnection) {
        log(`[Leave Command] Guild: ${message.guild.id} | Left by: ${message.author.tag}`);
        existingConnection.destroy();
        const embed = new EmbedBuilder()
          .setColor("#FF0000")
          .setDescription("Disconnected from voice channel.");
        return message.channel.send({ embeds: [embed] });
      }

      const embed = new EmbedBuilder()
        .setColor("#FF0000")
        .setDescription("Bot is not in a voice channel.");
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
