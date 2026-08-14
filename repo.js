// ====================== CONFIG ======================
require('dotenv').config();
const fetch = (...args) => import('node-fetch').then(({ default: fetch }) => fetch(...args));
const fs = require('fs');
const path = require('path');
const os = require('os');

// ====== GitHub Repos ======
const GITHUB_REPOS = [
  {
    owner: "PerformanC",
    repo: "ReZygisk",
    branch: "main"
  },
  {
    owner: "JuanArton",
    repo: "reYAMF",
    branch: "main"
  },
  {
    owner: "termux",
    repo: "termux-app",
    branch: "master"
  },
  {
    owner: "termux",
    repo: "termux-api",
    branch: "master"
  },
  {
    owner: "zwc456baby",
    repo: "ScrcpyForAndroid",
    branch: "main"
  },
  {
    owner: "tak-bro",
    repo: "aicommit2",
    branch: "main"
  },
  {
    owner: "anasfanani",
    repo: "Magisk-Tailscaled",
    branch: "main"
  },
  {
    owner: "Dev4Mod",
    repo: "WaEnhancer",
    branch: "master"
  },
  {
    owner: "NexAlloy",
    repo: "NexAlloy",
    branch: "main"
  },
  {
    owner: "LSPosed",
    repo: "DisableFlagSecure",
    branch: "main"
  },
  {
    owner: "backslashxx",
    repo: "KernelSU",
    branch: "staging"
  },
  {
    owner: "KernelSU-Next",
    repo: "KernelSU-Next",
    branch: "dev"
  },
  {
    owner: "tiann",
    repo: "KernelSU",
    branch: "main"
  },
  {
    owner: "canaria3406",
    repo: "hoyolab-auto-sign",
    branch: "main"
  },
  {
    owner: "5ec1cff",
    repo: "TrickyStore",
    branch: "release"
  },
  {
    owner: "KOWX712",
    repo: "PlayIntegrityFix",
    branch: "inject_s"
  },
  {
    owner: "yt-dlp",
    repo: "yt-dlp",
    branch: "master"
  },
  {
    owner: "decolua",
    repo: "9router",
    branch: "master"
  }
];

// ====== GitLab Repos ======
const GITLAB_REPOS = [
  // { 
    // owner: "simonpunk",
    // repo: "susfs4ksu" 
  // }
];

// ====== Tokens & IDs ======
const TELEGRAM_BOT_TOKEN = process.env.BOT_TOKEN;
const TELEGRAM_CHAT_ID = process.env.CHAT_ID;
const TELEGRAM_TOPIC_ID = process.env.TOPIC_ID_UPDATE;
const GITHUB_TOKEN = process.env.GITHUB_TOKEN;
const GITLAB_TOKEN = process.env.GITLAB_TOKEN;
const ARROW = "<i>-&gt;</i>";

// ====== State File ======
const STATE_FILE = path.join(os.homedir(), 'SERVER', 'Backup', 'repo.json');
const LOG_FILE = path.join(os.homedir(), 'SERVER', 'Backup', 'repo.log');

function log(msg) {
  fs.appendFileSync(
    LOG_FILE,
    `[${new Date().toLocaleString()}] ${msg}\n`
  );
}

// ====================== UTILITIES ======================
function loadState() {
  try {
    if (!fs.existsSync(STATE_FILE)) return {};

    const raw = fs.readFileSync(STATE_FILE, 'utf8')
      .replace(/\0/g, '')
      .trim();

    if (!raw) return {};

    return JSON.parse(raw);

  } catch (err) {
    log("⚠️ State file corrupted. Resetting.");

    try {
      fs.renameSync(
        STATE_FILE,
        STATE_FILE + ".corrupt-" + Date.now()
      );
    } catch {}

    return {};
  }
}

function saveState(state) {
  const tmp = STATE_FILE + ".tmp";
  fs.writeFileSync(tmp, JSON.stringify(state, null, 2));
  fs.renameSync(tmp, STATE_FILE);
}

function escapeHtml(text) {
  return String(text)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

async function sendToTelegram(message) {
  const telegramUrl = `https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/sendMessage`;
  const payload = {
    chat_id: TELEGRAM_CHAT_ID,
    text: message,
    parse_mode: "HTML",
    message_thread_id: TELEGRAM_TOPIC_ID,
    disable_web_page_preview: true
  };

  const res = await fetch(telegramUrl, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload)
  });

  if (!res.ok) {
    const errorText = await res.text();
    log(`❌ Telegram failed: ${errorText}`);
    throw new Error(`Telegram API error: ${res.status} - ${errorText}`);
  }

  return true;
}

// ====================== GITHUB CHECK ======================
async function checkGithubUpdates(state) {
  const headers = {
    Authorization: `Bearer ${GITHUB_TOKEN}`,
    Accept: 'application/vnd.github+json',
  };

  for (let index = 0; index < GITHUB_REPOS.length; index++) {
    const repo = GITHUB_REPOS[index];
    const lastShaKey = `GITHUB_${repo.owner}_${repo.repo}_LAST_COMMIT_SHA_${index}`;
    const releaseIdKey = `GITHUB_${repo.owner}_${repo.repo}_LAST_RELEASE_ID_${index}`;
    const pathParam = repo.path ? `&path=${encodeURIComponent(repo.path)}` : "";
    const encodedBranch = encodeURIComponent(repo.branch);
    const commitUrl = `https://api.github.com/repos/${repo.owner}/${repo.repo}/commits?sha=${encodedBranch}${pathParam}`;

    let commitResp;
    try {
      commitResp = await fetch(commitUrl, { headers });
    } catch (e) {
      log(`❌ Fetch error on ${repo.repo}: ${e.message}`);
      await new Promise(r => setTimeout(r, 2000));
      continue;
    }

    if (!commitResp.ok) {
      log(`❌ Error on ${repo.repo}: ${await commitResp.text()}`);
      await new Promise(r => setTimeout(r, 2000));
      continue;
    }

    const commits = await commitResp.json();
    if (!commits || commits.length === 0) {
      await new Promise(r => setTimeout(r, 2000));
      continue;
    }

    const latestCommit = commits[0];
    const latestSha = latestCommit.sha;
    const lastSha = state[lastShaKey];

    if (latestSha === lastSha) {
      // log(`✅ ${repo.repo} (${repo.branch}): No new commit`);
      await new Promise(r => setTimeout(r, 2000));
      continue;
    }

    let actionsText = "";
    try {
      const actionsUrl = `https://api.github.com/repos/${repo.owner}/${repo.repo}/actions/runs?per_page=5&branch=${encodedBranch}`;
      const actionsResp = await fetch(actionsUrl, { headers });

      if (actionsResp.ok) {
        const actionsData = await actionsResp.json();
        const run = actionsData.workflow_runs.find(r => r.head_sha === latestSha);
        if (run) {
          actionsText = "<b>✹ Workflow</b>\n" + ARROW + ` <i><a href="${escapeHtml(run.html_url)}">Run</a></i>`;
        }
      }
    } catch (e) {
      log(`⚠️ Workflow check failed: ${e.message}`);
    }

    let releaseText = "";
    let latestReleaseId = null;
    try {
      const releaseResp = await fetch(`https://api.github.com/repos/${repo.owner}/${repo.repo}/releases/latest`, { headers });

      if (releaseResp.ok) {
        const releaseData = await releaseResp.json();
        latestReleaseId = releaseData.id?.toString();
        const lastReleaseId = state[releaseIdKey];

        if (latestReleaseId && latestReleaseId !== lastReleaseId) {
          const name = escapeHtml(releaseData.name || releaseData.tag_name);
          releaseText = "<b>✹ Release</b>\n" + ARROW + ` <i><a href="${escapeHtml(releaseData.html_url)}">${name}</a></i>`;
        }
      }
    } catch (e) {
      log(`⚠️ Release check failed: ${e.message}`);
    }

    const MAX_COMMIT_LENGTH = 150;
    let commitMessage = latestCommit.commit.message;
    if (commitMessage.length > MAX_COMMIT_LENGTH) {
      commitMessage = commitMessage.substring(0, MAX_COMMIT_LENGTH) + "...";
    }

    const message = [
      "<b>🎉 GitHub Update Detected</b>",
      "<b>✹ Username</b>",
      `${ARROW} <i>${escapeHtml(repo.owner)}</i>`,
      "<b>✹ Repository</b>",
      `${ARROW} <i><a href="${escapeHtml(`https://github.com/${repo.owner}/${repo.repo}`)}">${escapeHtml(repo.repo)}</a></i>`,
      "<b>✹ Branch</b>",
      `${ARROW} <i>${escapeHtml(repo.branch)}</i>`,
      repo.path ? "<b>✹ Path</b>\n" + ARROW + " <i>" + escapeHtml(repo.path) + "</i>" : "",
      "<b>✹ Commit</b>",
      "<blockquote>" + escapeHtml(commitMessage) + "</blockquote>",
      `${ARROW} <i><a href="${escapeHtml(latestCommit.html_url)}">View</a></i>`,
      actionsText,
      releaseText,
    ].filter(Boolean).join("\n");

    try {
      await sendToTelegram(message);
      
      state[lastShaKey] = latestSha;
      if (latestReleaseId) state[releaseIdKey] = latestReleaseId;
      
      log(`📨 Sent update for ${repo.repo} and saved state`);
    } catch (err) {
      if (err.response?.status === 429) {
        const retryAfter = err.response.data.parameters?.retry_after || 5;
        console.warn(`⏳ Rate limited, waiting ${retryAfter} seconds...`);
        await new Promise(r => setTimeout(r, retryAfter * 1000));
        index--;
      } else {
        log(`❌ Telegram failed for ${repo.repo}: ${err.message}`);
        log(`⚠️ State NOT saved for ${repo.repo} due to Telegram failure`);
      }
    }

    await new Promise(r => setTimeout(r, 2000));
  }
}

// ====================== GITLAB CHECK ======================
async function checkGitlabUpdates(state) {
  const headers = { Authorization: `Bearer ${GITLAB_TOKEN}` };

  for (let index = 0; index < GITLAB_REPOS.length; index++) {
    const repo = GITLAB_REPOS[index];
    const lastShaKeyPrefix = `GITLAB_${repo.owner}_${repo.repo}_LAST_COMMIT_SHA_${index}`;
    const encodedPath = encodeURIComponent(`${repo.owner}/${repo.repo}`);

    let branches = [];

    if (repo.branch) {
      branches = [repo.branch];
    } else {
      const branchesUrl = `https://gitlab.com/api/v4/projects/${encodedPath}/repository/branches`;
      const branchesResponse = await fetch(branchesUrl, { headers });
      if (!branchesResponse.ok) {
        log(`❌ Failed to fetch branches for ${repo.repo}: ${await branchesResponse.text()}`);
        await new Promise(r => setTimeout(r, 2000));
        continue;
      }
      const branchesData = await branchesResponse.json();
      branches = branchesData.map(b => b.name);
    }

    for (const branch of branches) {
      const lastShaKey = `${lastShaKeyPrefix}_${branch}`;
      const commitsUrl = `https://gitlab.com/api/v4/projects/${encodedPath}/repository/commits?ref_name=${encodeURIComponent(branch)}`;

      const response = await fetch(commitsUrl, { headers });
      if (!response.ok) {
        log(`❌ GitLab error on ${repo.repo}@${branch}: ${await response.text()}`);
        await new Promise(r => setTimeout(r, 2000));
        continue;
      }

      const data = await response.json();
      if (!data || data.length === 0) {
        log(`⚠️ No commits for ${repo.repo}@${branch}`);
        await new Promise(r => setTimeout(r, 2000));
        continue;
      }

      const latestCommit = data[0];
      const latestSha = latestCommit.id;
      const lastSha = state[lastShaKey];

      const MAX_COMMIT_LENGTH = 150;
      let commitMessage = latestCommit.title;
      if (commitMessage.length > MAX_COMMIT_LENGTH) {
        commitMessage = commitMessage.substring(0, MAX_COMMIT_LENGTH) + "...";
      }

      if (latestSha !== lastSha) {
        const commitUrl = latestCommit.web_url || `https://gitlab.com/${repo.owner}/${repo.repo}/-/commit/${latestSha}`;
        const message =
          "<b>🎉 GitLab Update Detected</b>\n" +
          "<b>✹ Username</b>\n" +
          ARROW + " <i>" + escapeHtml(repo.owner) + "</i>\n" +
          "<b>✹ Repository</b>\n" +
          ARROW + ` <i><a href="${escapeHtml(`https://gitlab.com/${repo.owner}/${repo.repo}`)}">${escapeHtml(repo.repo)}</a></i>\n` +
          "<b>✹ Branch</b>\n" +
          ARROW + " <i>" + escapeHtml(branch) + "</i>\n" +
          "<b>✹ Commit</b>\n" +
          "<blockquote>" + escapeHtml(commitMessage) + "</blockquote>" +
          ARROW + ` <i><a href="${escapeHtml(commitUrl)}">View</a></i>`;

        try {
          await sendToTelegram(message);
          
          state[lastShaKey] = latestSha;
          log(`📨 Sent GitLab update for ${repo.repo} and saved state`);
        } catch (e) {
          if (e.response && e.response.status === 429) {
            const retryAfter = e.response.data.parameters?.retry_after || 1;
            console.warn(`⏳ Rate limited by Telegram. Waiting ${retryAfter} seconds...`);
            await new Promise(r => setTimeout(r, retryAfter * 1000));
          } else {
            log(`❌ Telegram failed for ${repo.repo}: ${e.message}`);
            log(`⚠️ State NOT saved for ${repo.repo} due to Telegram failure`);
          }
        }
      } else {
        log(`✅ GitLab ${repo.repo} (${branch}): No new commit`);
      }

      await new Promise(r => setTimeout(r, 2000));
    }

    await new Promise(r => setTimeout(r, 2000));
  }
}

// ====================== MAIN ======================
async function runRepo() {
  const state = loadState();
  await checkGithubUpdates(state);
  await checkGitlabUpdates(state);
  saveState(state);
}

module.exports = runRepo;