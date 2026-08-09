require("dotenv").config();
const fs = require("fs");
const path = require("path");
const os = require("os");
const { Client, GatewayIntentBits, Partials } = require("discord.js");

const LOG_FILE = path.join(os.homedir(), "SERVER", "Backup", "discord.log");

function log(msg) {
  try {
    fs.appendFileSync(
      LOG_FILE,
      `[${new Date().toLocaleString("en-US")}] ${msg}\n`
    );
  } catch (err) {
    console.error("Failed to write log:", err.message);
  }
}

function chunkText(text, maxLength = 1900) {
  if (!text) return [];
  return text.match(new RegExp(`.{1,${maxLength}}`, "gs")) || [text];
}

async function processOpenRouterRequest(prompt) {
  try {
    const res = await fetch("https://openrouter.ai/api/v1/chat/completions", {
      method: "POST",
      headers: {
        Authorization: `Bearer ${process.env.OPENROUTER_API_KEY}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({
        model: process.env.OPENROUTER_MODEL || "meta-llama/llama-3.3-70b-instruct:free",
        messages: [{ role: "user", content: prompt }],
        temperature: 0.7,
      }),
    });

    if (!res.ok) throw new Error(`OpenRouter HTTP ${res.status}`);
    const data = await res.json();
    return data?.choices?.[0]?.message?.content || null;
  } catch (err) {
    log(`OpenRouter Error: ${err.message}`);
    throw err;
  }
}

function runDiscord() {
  const discordClient = new Client({
    intents: [
      GatewayIntentBits.Guilds,
      GatewayIntentBits.GuildMessages,
      GatewayIntentBits.MessageContent,
      GatewayIntentBits.DirectMessages,
    ],
    partials: [Partials.Channel],
  });

  discordClient.once("clientReady", () => {
    log(`🤖 Discord Bot online as: ${discordClient.user.tag}`);
  });

  discordClient.on("messageCreate", async (message) => {
    if (message.author.bot) return;

    if (message.content.startsWith("m!")) return;

    let argPrompt = message.content.trim();
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

    if (!prompt) return;

    log(`[Discord] From ${message.author.tag}: ${prompt}`);

    try {
      const responseText = await processOpenRouterRequest(prompt);
      if (!responseText) throw new Error("Empty response from OpenRouter");

      const parts = chunkText(responseText, 1900);
      for (const part of parts) {
        await message.reply(part);
      }
    } catch (err) {
      log(`Discord AI Error: ${err.message}`);
      await message.reply("❌ Failed to process request.");
    }
  });

  if (process.env.DISCORD_BOT_TOKEN) {
    discordClient.login(process.env.DISCORD_BOT_TOKEN).catch((err) => {
      log(`Discord Login Error: ${err.message}`);
    });
  } else {
    log("❌ DISCORD_BOT_TOKEN is missing in .env file!");
  }
}

module.exports = runDiscord;
