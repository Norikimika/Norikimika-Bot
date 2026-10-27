//! GitHub/GitLab release + commit watcher (port of repo.js).
use anyhow::Result;
use serde_json::Value;
use std::time::Duration;

use crate::state::State;
use crate::util::{escape_html, truncate};

const MOD: &str = "repo";
const ARROW: &str = "<i>-&gt;</i>";
const MAX_COMMIT_LENGTH: usize = 150;

struct Repo {
    owner: &'static str,
    repo: &'static str,
    branch: &'static str,
    path: Option<&'static str>,
}

const GITHUB_REPOS: &[Repo] = &[
    Repo { owner: "PerformanC", repo: "ReZygisk", branch: "main", path: None },
    Repo { owner: "JuanArton", repo: "reYAMF", branch: "main", path: None },
    Repo { owner: "termux", repo: "termux-app", branch: "master", path: None },
    Repo { owner: "termux", repo: "termux-api", branch: "master", path: None },
    Repo { owner: "zwc456baby", repo: "ScrcpyForAndroid", branch: "main", path: None },
    Repo { owner: "tak-bro", repo: "aicommit2", branch: "main", path: None },
    Repo { owner: "anasfanani", repo: "Magisk-Tailscaled", branch: "main", path: None },
    Repo { owner: "Dev4Mod", repo: "WaEnhancer", branch: "master", path: None },
    Repo { owner: "NexAlloy", repo: "NexAlloy", branch: "main", path: None },
    Repo { owner: "LSPosed", repo: "DisableFlagSecure", branch: "main", path: None },
    Repo { owner: "KernelSU-Next", repo: "KernelSU-Next", branch: "dev", path: None },
    Repo { owner: "tiann", repo: "KernelSU", branch: "main", path: None },
    Repo { owner: "canaria3406", repo: "hoyolab-auto-sign", branch: "main", path: None },
    Repo { owner: "5ec1cff", repo: "TrickyStore", branch: "release", path: None },
    Repo { owner: "KOWX712", repo: "PlayIntegrityFix", branch: "inject_s", path: None },
    Repo { owner: "yt-dlp", repo: "yt-dlp", branch: "master", path: None },
    Repo { owner: "decolua", repo: "9router", branch: "master", path: None },
    Repo { owner: "NousResearch", repo: "hermes-agent", branch: "main", path: Some("hermes_cli/__init__.py") },
];

const GITLAB_REPOS: &[Repo] = &[];

fn http() -> Result<reqwest::Client> {
    Ok(crate::util::http()
        .user_agent("Norikimika-Bot")
        .timeout(Duration::from_secs(30))
        .build()?)
}

fn gh_headers(token: Option<&str>) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    h.insert("Accept", "application/vnd.github+json".parse().unwrap());
    if let Some(t) = token {
        if let Ok(v) = format!("Bearer {t}").parse() {
            h.insert("Authorization", v);
        }
    }
    h
}

fn gl_headers(token: Option<&str>) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    if let Some(t) = token {
        if let Ok(v) = t.parse() {
            h.insert("PRIVATE-TOKEN", v);
        }
    }
    h
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub struct Watcher {
    http: reqwest::Client,
    tg: Option<crate::telegram::Client>,
    chat_id: i64,
    thread_id: Option<i64>,
    gh_token: Option<String>,
    gl_token: Option<String>,
    dry_run: bool,
}

impl Watcher {
    pub fn from_env(dry_run: bool) -> Result<Self> {
        let chat_id = crate::config::chat_id().unwrap_or(0);
        Ok(Self {
            http: http()?,
            tg: if dry_run { None } else { Some(crate::telegram::Client::from_env()?) },
            chat_id,
            thread_id: crate::config::topic("TOPIC_ID_UPDATE"),
            gh_token: crate::config::get("GITHUB_TOKEN"),
            gl_token: crate::config::get("GITLAB_TOKEN"),
            dry_run,
        })
    }

    async fn deliver(&self, message: &str) -> Result<()> {
        if self.dry_run {
            println!("--- DRY RUN message ---\n{message}\n-----------------------");
            return Ok(());
        }
        self.tg
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("telegram client not initialised"))?
            .send_html(self.chat_id, self.thread_id, message)
            .await
    }

    pub async fn run(&self, state: &mut State) -> Result<()> {
        self.check_github(state).await;
        self.check_gitlab(state).await;
        if !self.dry_run {
            state.save()?;
        }
        Ok(())
    }

    async fn check_github(&self, state: &mut State) {
        let headers = gh_headers(self.gh_token.as_deref());

        for repo in GITHUB_REPOS {
            let sha_key = format!("GITHUB_{}_{}_LAST_COMMIT_SHA", repo.owner, repo.repo);
            let release_key = format!("GITHUB_{}_{}_LAST_RELEASE_ID", repo.owner, repo.repo);
            let asset_key = format!("GITHUB_{}_{}_LAST_RELEASE_ASSETS", repo.owner, repo.repo);
            let tag_key = format!("GITHUB_{}_{}_LAST_TAG", repo.owner, repo.repo);
            let branch = urlencode(repo.branch);
            let path_param = repo.path.map(|p| format!("&path={}", urlencode(p))).unwrap_or_default();
            let commit_url = format!(
                "https://api.github.com/repos/{}/{}/commits?sha={branch}{path_param}",
                repo.owner, repo.repo
            );

            let commits: Value = match self.http.get(&commit_url).headers(headers.clone()).send().await {
                Ok(r) if r.status().is_success() => r.json().await.unwrap_or(Value::Null),
                Ok(r) => {
                    logln!(MOD, "❌ Error on {}: {}", repo.repo, r.status());
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
                Err(e) => {
                    logln!(MOD, "❌ Fetch error on {}: {e}", repo.repo);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };

            let Some(latest) = commits.get(0) else {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            };
            let latest_sha = latest["sha"].as_str().unwrap_or_default().to_string();

            // Release: id + asset signature (asset re-uploads bump updated_at).
            let mut release_changed = false;
            let mut release_data = Value::Null;
            if let Ok(r) = self
                .http
                .get(format!("https://api.github.com/repos/{}/{}/releases/latest", repo.owner, repo.repo))
                .headers(headers.clone())
                .send()
                .await
            {
                if r.status().is_success() {
                    if let Ok(data) = r.json::<Value>().await {
                        let id = data["id"].as_i64().unwrap_or(0).to_string();
                        let sig = data["assets"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .map(|x| format!("{}:{}", x["id"], x["updated_at"].as_str().unwrap_or("")))
                                    .collect::<Vec<_>>()
                                    .join("|")
                            })
                            .unwrap_or_default();
                        if id != "0" && (Some(id.as_str()) != state.str(&release_key) || Some(sig.as_str()) != state.str(&asset_key)) {
                            release_changed = true;
                            state.set_str(&release_key, id);
                            state.set_str(&asset_key, sig);
                        }
                        release_data = data;
                    }
                }
            }

            // Tags
            let mut tag_changed = false;
            let mut latest_tag: Option<String> = None;
            if let Ok(r) = self
                .http
                .get(format!("https://api.github.com/repos/{}/{}/tags?per_page=1", repo.owner, repo.repo))
                .headers(headers.clone())
                .send()
                .await
            {
                if r.status().is_success() {
                    if let Ok(data) = r.json::<Value>().await {
                        if let Some(name) = data.get(0).and_then(|t| t["name"].as_str()) {
                            latest_tag = Some(name.to_string());
                            tag_changed = state.str(&tag_key) != Some(name);
                        }
                    }
                }
            }

            let has_new_commit = state.str(&sha_key) != Some(latest_sha.as_str());
            if !has_new_commit && !release_changed && !tag_changed {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }

            // Workflow run for the new head sha
            let mut actions_text = String::new();
            if has_new_commit {
                let url = format!(
                    "https://api.github.com/repos/{}/{}/actions/runs?per_page=5&branch={branch}",
                    repo.owner, repo.repo
                );
                if let Ok(r) = self.http.get(&url).headers(headers.clone()).send().await {
                    if r.status().is_success() {
                        if let Ok(data) = r.json::<Value>().await {
                            if let Some(runs) = data["workflow_runs"].as_array() {
                                if let Some(run) = runs.iter().find(|x| x["head_sha"].as_str() == Some(latest_sha.as_str())) {
                                    actions_text = format!(
                                        "<b>✹ Workflow</b>\n{ARROW} <i><a href=\"{}\">Run</a></i>",
                                        escape_html(run["html_url"].as_str().unwrap_or(""))
                                    );
                                }
                            }
                        }
                    }
                }
            }

            let mut release_text = String::new();
            if release_changed && release_data.is_object() {
                let name = release_data["name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .or_else(|| release_data["tag_name"].as_str())
                    .unwrap_or("");
                release_text = format!(
                    "<b>✹ Release</b>\n{ARROW} <i><a href=\"{}\">{}</a></i>",
                    escape_html(release_data["html_url"].as_str().unwrap_or("")),
                    escape_html(name)
                );
            }

            if tag_changed {
                if let Some(tag) = &latest_tag {
                    let tag_url = format!(
                        "https://github.com/{}/{}/releases/tag/{}",
                        repo.owner, repo.repo, urlencode(tag)
                    );
                    if !release_text.is_empty() {
                        release_text.push('\n');
                    }
                    release_text.push_str(&format!(
                        "<b>✹ Tag</b>\n{ARROW} <i><a href=\"{}\">{}</a></i>",
                        escape_html(&tag_url),
                        escape_html(tag)
                    ));
                }
            }

            let commit_message = truncate(latest["commit"]["message"].as_str().unwrap_or(""), MAX_COMMIT_LENGTH);

            let mut parts: Vec<String> = vec![
                "<b>🎉 GitHub Update Detected</b>".into(),
                "<b>✹ Username</b>".into(),
                format!("{ARROW} <i>{}</i>", escape_html(repo.owner)),
                "<b>✹ Repository</b>".into(),
                format!(
                    "{ARROW} <i><a href=\"{}\">{}</a></i>",
                    escape_html(&format!("https://github.com/{}/{}", repo.owner, repo.repo)),
                    escape_html(repo.repo)
                ),
                "<b>✹ Branch</b>".into(),
                format!("{ARROW} <i>{}</i>", escape_html(repo.branch)),
            ];
            if let Some(p) = repo.path {
                parts.push(format!("<b>✹ Path</b>\n{ARROW} <i>{}</i>", escape_html(p)));
            }
            if has_new_commit {
                parts.push("<b>✹ Commit</b>".into());
                parts.push(format!("<blockquote>{}</blockquote>", escape_html(&commit_message)));
                parts.push(format!(
                    "{ARROW} <i><a href=\"{}\">View</a></i>",
                    escape_html(latest["html_url"].as_str().unwrap_or(""))
                ));
                if !actions_text.is_empty() {
                    parts.push(actions_text);
                }
            }
            if !release_text.is_empty() {
                parts.push(release_text);
            }

            match self.deliver(&parts.join("\n")).await {
                Ok(()) => {
                    if has_new_commit {
                        state.set_str(&sha_key, latest_sha.clone());
                    }
                    if tag_changed {
                        if let Some(tag) = &latest_tag {
                            state.set_str(&tag_key, tag.clone());
                        }
                    }
                    if self.dry_run {
                        logln!(MOD, "Dry-run update for {} (not sent)", repo.repo);
                    } else {
                        logln!(MOD, "📨 Sent update for {}", repo.repo);
                    }
                }
                Err(e) => logln!(MOD, "❌ Telegram failed for {}: {e} (state not saved)", repo.repo),
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn check_gitlab(&self, state: &mut State) {
        let headers = gl_headers(self.gl_token.as_deref());

        for repo in GITLAB_REPOS {
            let encoded_path = urlencode(&format!("{}/{}", repo.owner, repo.repo));
            let branches: Vec<String> = match repo.branch {
                b if !b.is_empty() => vec![b.to_string()],
                _ => {
                    let url = format!("https://gitlab.com/api/v4/projects/{encoded_path}/repository/branches");
                    match self.http.get(&url).headers(headers.clone()).send().await {
                        Ok(r) if r.status().is_success() => r
                            .json::<Value>()
                            .await
                            .ok()
                            .and_then(|d| d.as_array().map(|a| a.iter().filter_map(|b| b["name"].as_str().map(str::to_string)).collect()))
                            .unwrap_or_default(),
                        _ => Vec::new(),
                    }
                }
            };

            for branch in branches {
                let sha_key = format!("GITLAB_{}_{}_LAST_COMMIT_SHA_{branch}", repo.owner, repo.repo);
                let url = format!(
                    "https://gitlab.com/api/v4/projects/{encoded_path}/repository/commits?ref_name={}",
                    urlencode(&branch)
                );
                let data: Value = match self.http.get(&url).headers(headers.clone()).send().await {
                    Ok(r) if r.status().is_success() => r.json().await.unwrap_or(Value::Null),
                    Ok(r) => {
                        logln!(MOD, "❌ GitLab error on {}@{branch}: {}", repo.repo, r.status());
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    Err(e) => {
                        logln!(MOD, "❌ GitLab fetch error on {}@{branch}: {e}", repo.repo);
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                };
                let Some(latest) = data.get(0) else {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                };
                let latest_sha = latest["id"].as_str().unwrap_or_default().to_string();
                if state.str(&sha_key) == Some(latest_sha.as_str()) {
                    logln!(MOD, "✅ GitLab {} ({branch}): no new commit", repo.repo);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }

                let message = format!(
                    "<b>🎉 GitLab Update Detected</b>\n<b>✹ Username</b>\n{ARROW} <i>{owner}</i>\n\
                     <b>✹ Repository</b>\n{ARROW} <i><a href=\"{repo_url}\">{repo}</a></i>\n\
                     <b>✹ Branch</b>\n{ARROW} <i>{branch_esc}</i>\n\
                     <b>✹ Commit</b>\n<blockquote>{msg}</blockquote>{ARROW} <i><a href=\"{commit_url}\">View</a></i>",
                    owner = escape_html(repo.owner),
                    repo_url = escape_html(&format!("https://gitlab.com/{}/{}", repo.owner, repo.repo)),
                    repo = escape_html(repo.repo),
                    branch_esc = escape_html(&branch),
                    msg = escape_html(&truncate(latest["title"].as_str().unwrap_or(""), MAX_COMMIT_LENGTH)),
                    commit_url = escape_html(latest["web_url"].as_str().unwrap_or("")),
                );

                match self.deliver(&message).await {
                    Ok(()) => {
                        state.set_str(&sha_key, latest_sha);
                        logln!(MOD, "📨 Sent GitLab update for {}", repo.repo);
                    }
                    Err(e) => logln!(MOD, "❌ Telegram failed for {}: {e} (state not saved)", repo.repo),
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

pub async fn run(dry_run: bool) -> Result<()> {
    let mut state = State::load("repo.json");
    let watcher = Watcher::from_env(dry_run)?;
    watcher.run(&mut state).await
}
