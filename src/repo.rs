use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::app::App;
use crate::util::{data_dir, escape_html, log_to, now_ms};

const INTERVAL: Duration = Duration::from_secs(300);
/// index.js: `setTimeout(loop, 300000)` plus a 2s pause between repos.
const THROTTLE: Duration = Duration::from_secs(2);
const ARROW: &str = "<i>-&gt;</i>";
const MAX_COMMIT: usize = 150;

struct Repo {
    owner: &'static str,
    repo: &'static str,
    branch: Option<&'static str>,
    path: Option<&'static str>,
}

const fn gh(owner: &'static str, repo: &'static str, branch: &'static str, path: Option<&'static str>) -> Repo {
    Repo { owner, repo, branch: Some(branch), path }
}

const GITHUB_REPOS: &[Repo] = &[
    gh("PerformanC", "ReZygisk", "main", None),
    gh("JuanArton", "reYAMF", "main", None),
    gh("termux", "termux-app", "master", None),
    gh("termux", "termux-api", "master", None),
    gh("zwc456baby", "ScrcpyForAndroid", "main", None),
    gh("tak-bro", "aicommit2", "main", None),
    gh("anasfanani", "Magisk-Tailscaled", "main", None),
    gh("Dev4Mod", "WaEnhancer", "master", None),
    gh("NexAlloy", "NexAlloy", "main", None),
    gh("LSPosed", "DisableFlagSecure", "main", None),
    gh("KernelSU-Next", "KernelSU-Next", "dev", None),
    gh("tiann", "KernelSU", "main", None),
    gh("canaria3406", "hoyolab-auto-sign", "main", None),
    gh("5ec1cff", "TrickyStore", "release", None),
    gh("KOWX712", "PlayIntegrityFix", "inject_s", None),
    gh("yt-dlp", "yt-dlp", "master", None),
    gh("decolua", "9router", "master", None),
    gh("NousResearch", "hermes-agent", "main", Some("hermes_cli/__init__.py")),
];

/// Empty in repo.js too; kept so the GitLab path stays exercised if a repo is added.
const GITLAB_REPOS: &[Repo] = &[];

pub fn spawn(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            run(&app).await;
            // ponytail: awaited then slept (no overlap). Switch to spawn+immediate
            // sleep if a cycle ever needs to run longer than 5 minutes.
            tokio::time::sleep(INTERVAL).await;
        }
    });
}

async fn run(app: &App) {
    let mut state = load_state();
    check_github(app, &mut state).await;
    check_gitlab(app, &mut state).await;
    save_state(&state);
}

// ================== STATE ==================

fn state_path() -> std::path::PathBuf {
    data_dir().join("repo.json")
}

fn load_state() -> Map<String, Value> {
    let path = state_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Map::new();
    };
    let cleaned = raw.replace('\0', "");
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return Map::new();
    }
    match serde_json::from_str::<Map<String, Value>>(cleaned) {
        Ok(state) => state,
        Err(_) => {
            log_to("repo.log", "⚠️ State file corrupted. Resetting.");
            let backup = std::path::PathBuf::from(format!("{}.corrupt-{}", path.display(), now_ms()));
            let _ = std::fs::rename(&path, backup);
            Map::new()
        }
    }
}

fn save_state(state: &Map<String, Value>) {
    let path = state_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = std::path::PathBuf::from(format!("{}.tmp", path.display()));
    let body = serde_json::to_string_pretty(state).unwrap_or_else(|_| "{}".into());
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn get(state: &Map<String, Value>, key: &str) -> String {
    state.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

// ================== TELEGRAM ==================

async fn send(app: &App, text: &str) -> anyhow::Result<()> {
    let mut payload = json!({
        "chat_id": app.cfg.chat_id,
        "text": text,
        "parse_mode": "HTML",
        "disable_web_page_preview": true,
    });
    if let Some(topic) = app.cfg.topic_update {
        payload["message_thread_id"] = topic.into();
    }
    match app.tg.call("sendMessage", payload).await {
        Ok(_) => Ok(()),
        Err(e) => {
            log_to("repo.log", &format!("❌ Telegram failed: {e}"));
            Err(e)
        }
    }
}

// ================== HTTP ==================

async fn github(app: &App, token: &Option<String>, url: &str) -> anyhow::Result<Value> {
    let mut req = app
        .http
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(60));
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let res = req.send().await?;
    if !res.status().is_success() {
        let body = res.text().await.unwrap_or_default();
        anyhow::bail!("{}", body.trim());
    }
    Ok(res.json().await?)
}

async fn gitlab(app: &App, token: &Option<String>, url: &str) -> anyhow::Result<Value> {
    let mut req = app.http.get(url).timeout(Duration::from_secs(60));
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let res = req.send().await?;
    if !res.status().is_success() {
        let body = res.text().await.unwrap_or_default();
        anyhow::bail!("{}", body.trim());
    }
    Ok(res.json().await?)
}

fn pct(s: &str) -> String {
    s.replace('/', "%2F")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        format!("{}...", s.chars().take(max).collect::<String>())
    } else {
        s.to_string()
    }
}

// ================== GITHUB ==================

async fn check_github(app: &App, state: &mut Map<String, Value>) {
    let token = app.cfg.github_token.clone();

    for repo in GITHUB_REPOS {
        let owner = repo.owner;
        let name = repo.repo;
        let branch = repo.branch.unwrap_or("main");
        let last_sha_key = format!("GITHUB_{owner}_{name}_LAST_COMMIT_SHA");
        let release_key = format!("GITHUB_{owner}_{name}_LAST_RELEASE_ID");
        let assets_key = format!("GITHUB_{owner}_{name}_LAST_RELEASE_ASSETS");
        let tag_key = format!("GITHUB_{owner}_{name}_LAST_TAG");
        let encoded_branch = pct(branch);
        let path_param = repo.path.map(|p| format!("&path={}", pct(p))).unwrap_or_default();

        let commit_url = format!(
            "https://api.github.com/repos/{owner}/{name}/commits?sha={encoded_branch}{path_param}"
        );
        let commits = match github(app, &token, &commit_url).await {
            Ok(v) => v,
            Err(e) => {
                log_to("repo.log", &format!("❌ Fetch error on {name}: {e}"));
                tokio::time::sleep(THROTTLE).await;
                continue;
            }
        };
        let Some(latest) = commits.as_array().and_then(|a| a.first()).cloned() else {
            tokio::time::sleep(THROTTLE).await;
            continue;
        };
        let latest_sha = latest["sha"].as_str().unwrap_or_default().to_string();

        // --- release ---
        let mut release_changed = false;
        let mut release_data: Option<Value> = None;
        match github(app, &token, &format!("https://api.github.com/repos/{owner}/{name}/releases/latest")).await {
            Ok(v) => {
                let release_id = v["id"].as_i64().map(|i| i.to_string()).unwrap_or_default();
                let asset_sig = v["assets"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|x| {
                                format!(
                                    "{}:{}",
                                    x["id"].as_i64().unwrap_or(0),
                                    x["updated_at"].as_str().unwrap_or_default()
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("|")
                    })
                    .unwrap_or_default();
                if !release_id.is_empty()
                    && (release_id != get(state, &release_key) || asset_sig != get(state, &assets_key))
                {
                    release_changed = true;
                    state.insert(release_key.clone(), Value::String(release_id));
                    state.insert(assets_key.clone(), Value::String(asset_sig));
                }
                release_data = Some(v);
            }
            Err(e) => log_to("repo.log", &format!("⚠️ Release check failed: {e}")),
        }

        // --- tag ---
        let mut tag_changed = false;
        let mut top_tag: Option<String> = None;
        match github(app, &token, &format!("https://api.github.com/repos/{owner}/{name}/tags?per_page=1")).await {
            Ok(v) => {
                if let Some(tag) = v.as_array().and_then(|a| a.first()).and_then(|t| t["name"].as_str()) {
                    if tag != get(state, &tag_key) {
                        tag_changed = true;
                    }
                    top_tag = Some(tag.to_string());
                }
            }
            Err(e) => log_to("repo.log", &format!("⚠️ Tag check failed: {e}")),
        }

        let has_new_commit = latest_sha != get(state, &last_sha_key);
        if !has_new_commit && !release_changed && !tag_changed {
            tokio::time::sleep(THROTTLE).await;
            continue;
        }

        // --- workflow run for the new commit ---
        let mut actions_text = String::new();
        if has_new_commit {
            let url = format!(
                "https://api.github.com/repos/{owner}/{name}/actions/runs?per_page=5&branch={encoded_branch}"
            );
            match github(app, &token, &url).await {
                Ok(v) => {
                    let run = v["workflow_runs"]
                        .as_array()
                        .and_then(|a| a.iter().find(|r| r["head_sha"].as_str() == Some(latest_sha.as_str())));
                    if let Some(run) = run {
                        actions_text = format!(
                            "<b>✹ Workflow</b>\n{ARROW} <i><a href=\"{}\">Run</a></i>",
                            escape_html(run["html_url"].as_str().unwrap_or_default())
                        );
                    }
                }
                Err(e) => log_to("repo.log", &format!("⚠️ Workflow check failed: {e}")),
            }
        }

        // --- release / tag lines ---
        let mut release_text = String::new();
        if release_changed {
            if let Some(rel) = &release_data {
                let name_text = rel["name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .or_else(|| rel["tag_name"].as_str())
                    .unwrap_or_default();
                release_text = format!(
                    "<b>✹ Release</b>\n{ARROW} <i><a href=\"{}\">{}</a></i>",
                    escape_html(rel["html_url"].as_str().unwrap_or_default()),
                    escape_html(name_text)
                );
            }
        }
        if tag_changed {
            if let Some(tag) = &top_tag {
                let link = format!(
                    "<i><a href=\"{}\">{}</a></i>",
                    escape_html(&format!("https://github.com/{owner}/{name}/releases/tag/{}", pct(tag))),
                    escape_html(tag)
                );
                if !release_text.is_empty() {
                    release_text.push('\n');
                }
                release_text.push_str(&format!("<b>✹ Tag</b>\n{ARROW} {link}"));
            }
        }

        // --- message ---
        let commit_message = truncate(
            latest["commit"]["message"].as_str().unwrap_or_default(),
            MAX_COMMIT,
        );
        let mut parts: Vec<String> = vec![
            "<b>🎉 GitHub Update Detected</b>".into(),
            "<b>✹ Username</b>".into(),
            format!("{ARROW} <i>{}</i>", escape_html(owner)),
            "<b>✹ Repository</b>".into(),
            format!(
                "{ARROW} <i><a href=\"{}\">{}</a></i>",
                escape_html(&format!("https://github.com/{owner}/{name}")),
                escape_html(name)
            ),
            "<b>✹ Branch</b>".into(),
            format!("{ARROW} <i>{}</i>", escape_html(branch)),
        ];
        if let Some(path) = repo.path {
            parts.push(format!("<b>✹ Path</b>\n{ARROW} <i>{}</i>", escape_html(path)));
        }
        if has_new_commit {
            parts.push("<b>✹ Commit</b>".into());
            parts.push(format!("<blockquote>{}</blockquote>", escape_html(&commit_message)));
            parts.push(format!(
                "{ARROW} <i><a href=\"{}\">View</a></i>",
                escape_html(latest["html_url"].as_str().unwrap_or_default())
            ));
            if !actions_text.is_empty() {
                parts.push(actions_text);
            }
        }
        if !release_text.is_empty() {
            parts.push(release_text);
        }

        match send(app, &parts.join("\n")).await {
            Ok(()) => {
                if has_new_commit {
                    state.insert(last_sha_key, Value::String(latest_sha));
                }
                if tag_changed {
                    if let Some(tag) = top_tag {
                        state.insert(tag_key, Value::String(tag));
                    }
                }
                log_to("repo.log", &format!("📨 Sent update for {name} and saved state"));
            }
            Err(e) => {
                log_to("repo.log", &format!("❌ Telegram failed for {name}: {e}"));
                log_to("repo.log", &format!("⚠️ State NOT saved for {name} due to Telegram failure"));
            }
        }

        tokio::time::sleep(THROTTLE).await;
    }
}

// ================== GITLAB ==================

async fn check_gitlab(app: &App, state: &mut Map<String, Value>) {
    let token = app.cfg.gitlab_token.clone();

    for repo in GITLAB_REPOS {
        let owner = repo.owner;
        let name = repo.repo;
        let last_sha_prefix = format!("GITLAB_{owner}_{name}_LAST_COMMIT_SHA");
        let encoded_path = format!("{}/{}", owner, name).replace('/', "%2F");

        let branches: Vec<String> = match repo.branch {
            Some(b) => vec![b.to_string()],
            None => {
                let url = format!("https://gitlab.com/api/v4/projects/{encoded_path}/repository/branches");
                match gitlab(app, &token, &url).await {
                    Ok(v) => v
                        .as_array()
                        .map(|a| a.iter().filter_map(|b| b["name"].as_str().map(String::from)).collect())
                        .unwrap_or_default(),
                    Err(e) => {
                        log_to("repo.log", &format!("❌ Failed to fetch branches for {name}: {e}"));
                        tokio::time::sleep(THROTTLE).await;
                        continue;
                    }
                }
            }
        };

        for branch in branches {
            let last_sha_key = format!("{last_sha_prefix}_{branch}");
            let url = format!(
                "https://gitlab.com/api/v4/projects/{encoded_path}/repository/commits?ref_name={}",
                pct(&branch)
            );

            let data = match gitlab(app, &token, &url).await {
                Ok(v) => v,
                Err(e) => {
                    log_to("repo.log", &format!("❌ GitLab error on {name}@{branch}: {e}"));
                    tokio::time::sleep(THROTTLE).await;
                    continue;
                }
            };
            let Some(latest) = data.as_array().and_then(|a| a.first()).cloned() else {
                log_to("repo.log", &format!("⚠️ No commits for {name}@{branch}"));
                tokio::time::sleep(THROTTLE).await;
                continue;
            };
            let latest_sha = latest["id"].as_str().unwrap_or_default().to_string();

            if latest_sha != get(state, &last_sha_key) {
                let commit_message = truncate(latest["title"].as_str().unwrap_or_default(), MAX_COMMIT);
                let commit_url = latest["web_url"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| format!("https://gitlab.com/{owner}/{name}/-/commit/{latest_sha}"));

                let message = format!(
                    "<b>🎉 GitLab Update Detected</b>\n\
                     <b>✹ Username</b>\n{ARROW} <i>{}</i>\n\
                     <b>✹ Repository</b>\n{ARROW} <i><a href=\"{}\">{}</a></i>\n\
                     <b>✹ Branch</b>\n{ARROW} <i>{}</i>\n\
                     <b>✹ Commit</b>\n<blockquote>{}</blockquote>{ARROW} <i><a href=\"{}\">View</a></i>",
                    escape_html(owner),
                    escape_html(&format!("https://gitlab.com/{owner}/{name}")),
                    escape_html(name),
                    escape_html(&branch),
                    escape_html(&commit_message),
                    escape_html(&commit_url)
                );

                match send(app, &message).await {
                    Ok(()) => {
                        state.insert(last_sha_key, Value::String(latest_sha));
                        log_to("repo.log", &format!("📨 Sent GitLab update for {name} and saved state"));
                    }
                    Err(e) => {
                        log_to("repo.log", &format!("❌ Telegram failed for {name}: {e}"));
                        log_to("repo.log", &format!("⚠️ State NOT saved for {name} due to Telegram failure"));
                    }
                }
            } else {
                log_to("repo.log", &format!("✅ GitLab {name} ({branch}): No new commit"));
            }

            tokio::time::sleep(THROTTLE).await;
        }

        tokio::time::sleep(THROTTLE).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trip() {
        let mut state = Map::new();
        state.insert("k".into(), Value::String("v".into()));
        let body = serde_json::to_string_pretty(&state).unwrap();
        let parsed: Map<String, Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(get(&parsed, "k"), "v");
        assert_eq!(get(&parsed, "missing"), "");
        assert_eq!(truncate("abcdef", 3), "abc...");
    }
}
