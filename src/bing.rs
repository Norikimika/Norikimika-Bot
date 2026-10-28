//! Bing Rewards automation via Android UI automation (port of bing.js).
//! One pass = run `Norikimika-Bot bing`; schedule it externally (cron) at 01:00 like the JS did.
use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::time::Duration;

const MOD: &str = "bing";
const PRIMARY_PACKAGE: &str = "com.brave.browser";
const CLONE_PACKAGE: &str = "com.multibox.clone.hub";
const SEARCH_COUNT: usize = 25;
/// `su` blocks on an unanswered root prompt and device commands can hang with the screen off.
const CMD_TIMEOUT: Duration = Duration::from_secs(60);
const URL_BAR_ATTEMPTS: usize = 6;
const ARTIFACTS: &[&str] = &[
    "brave-current.xml",
    "clone-current.xml",
    "clone-home.xml",
    "clone-tabs.xml",
    "tabs.xml",
    "rewards-current.png",
    "rewards-current.tsv",
];

/// Absolute path only: `uiautomator dump` and `su -c` run with their own cwd, so a relative
/// BING_WORK_DIR (e.g. `Logs`) made root write somewhere else and the read back fail.
fn work_dir() -> PathBuf {
    let dir = crate::config::get("BING_WORK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(crate::config::state_dir);
    let dir = if dir.is_absolute() {
        dir
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(dir)
    };
    let _ = std::fs::create_dir_all(&dir);
    std::fs::canonicalize(&dir).unwrap_or(dir)
}

/// Where root-owned tools write. Pulled back into `work_dir` afterwards, because a file created
/// by `su` is root:root 0600 and the Termux app cannot read it.
const ROOT_TMP: &str = "/data/local/tmp/norikimika-bing";

fn root_tmp(name: &str) -> String {
    format!("{ROOT_TMP}/{name}")
}

fn path_in(name: &str) -> PathBuf {
    work_dir().join(name)
}

async fn sleep(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// Spawn helper: a missing cwd makes spawn fail with a bare "No such file or directory"
/// that named neither the program nor the directory, so both are checked here.
async fn spawn(program: &str, args: &[&str]) -> Result<String> {
    let dir = work_dir();
    let mut command = tokio::process::Command::new(program);
    command.args(args);
    if dir.is_dir() {
        command.current_dir(&dir);
    } else {
        logln!(MOD, "work dir {} is not usable, running without cwd", dir.display());
    }
    let out = tokio::time::timeout(CMD_TIMEOUT, command.output())
        .await
        .map_err(|_| anyhow!("{program} timed out after {}s", CMD_TIMEOUT.as_secs()))?
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => anyhow!("{program} not found in PATH ({e})"),
            _ => anyhow!("{program}: {e}"),
        })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let code = out.status.code().map(|c| c.to_string()).unwrap_or_else(|| out.status.to_string());
        return Err(anyhow!("{program} exit {code}: {err}"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Run a binary directly (no shell), cwd = work dir.
async fn run_cmd(cmd: &str, args: &[&str]) -> Result<String> {
    spawn(cmd, args).await
}

/// Run a command as root through `su -c`.
async fn shell(cmd: &str) -> Result<String> {
    spawn(&crate::util::su(), &["-c", cmd])
        .await
        .map_err(|e| anyhow!("root shell needed for `{cmd}`: {e}"))
}

async fn shell_best_effort(cmd: &str) {
    let _ = shell(cmd).await;
}

// ---------- pure helpers ----------

fn random_bytes(n: usize) -> Vec<u8> {
    // ponytail: /dev/urandom instead of a rand crate — only used for search queries
    // (must read *exactly* n bytes: fs::read() on a char device never hits EOF)
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ (std::process::id() as u128);
    (0..n).map(|i| (seed >> (i * 8)) as u8).collect()
}

fn random_query() -> String {
    const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let bytes = random_bytes(16);
    bytes.iter().map(|b| ALNUM[*b as usize % ALNUM.len()] as char).take(4).collect()
}

fn random_cvid() -> String {
    let b = random_bytes(16);
    let hex: String = b.iter().map(|x| format!("{x:02X}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

fn build_search_url(query: &str, previous_query: &str) -> String {
    format!(
        "https://www.bing.com/search?q={query}&form=QBLH&sp=-1&lq=0&pq={previous_query}&sc=12-4&qs=n&sk=&cvid={}",
        random_cvid()
    )
}

fn xml_escape(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Attribute value from a single tag string.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// "[x1,y1][x2,y2]" -> centre point.
fn bounds_center(tag: &str) -> Option<(i32, i32)> {
    let raw = attr(tag, "bounds")?;
    let nums: Vec<i32> = raw
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    if nums.len() < 4 {
        return None;
    }
    Some(((nums[0] + nums[2]) / 2, (nums[1] + nums[3]) / 2))
}

fn tags(xml: &str) -> Vec<&str> {
    xml.split_inclusive('>').collect()
}

fn find_node_by_description(xml: &str, description: &str) -> Option<(i32, i32)> {
    for tag in tags(xml) {
        if attr(tag, "content-desc") == Some(description) {
            if let Some(point) = bounds_center(tag) {
                return Some(point);
            }
        }
    }
    for tag in tags(xml) {
        if attr(tag, "text") == Some(description) {
            if let Some(point) = bounds_center(tag) {
                return Some(point);
            }
        }
    }
    None
}

fn find_url_bar(xml: &str) -> Option<(i32, i32)> {
    const IDS: &[&str] = &[
        "url_bar",
        "urlbar",
        "omnibox",
        "url_bar_container",
        "search_bar",
        "toolbar_url_box",
    ];
    for tag in tags(xml) {
        if let Some(id) = attr(tag, "resource-id") {
            if let Some(suffix) = id.rsplit_once(":id/").map(|(_, s)| s) {
                if IDS.contains(&suffix) {
                    if let Some(point) = bounds_center(tag) {
                        return Some(point);
                    }
                }
            }
        }
    }
    const HINTS: &[&str] = &[
        "Search or enter",
        "Enter a URL",
        "Enter a web address",
        "Search or type URL",
        "Search Bing",
        "Microsoft Edge",
    ];
    for tag in tags(xml) {
        if let Some(desc) = attr(tag, "content-desc") {
            if HINTS.iter().any(|h| desc.contains(h)) {
                if let Some(point) = bounds_center(tag) {
                    return Some(point);
                }
            }
        }
    }
    None
}

/// Parse a tesseract TSV row set, looking for the "claim" word.
fn parse_claim_from_tsv(tsv: &str) -> Option<(i32, i32)> {
    for row in tsv.lines().skip(1) {
        let fields: Vec<&str> = row.split('\t').collect();
        if fields.len() > 11 && fields[11].trim().eq_ignore_ascii_case("claim") {
            let left: f64 = fields[6].parse().ok()?;
            let top: f64 = fields[7].parse().ok()?;
            let width: f64 = fields[8].parse().ok()?;
            let height: f64 = fields[9].parse().ok()?;
            return Some(((left + width / 2.0) as i32, (top + height / 2.0) as i32));
        }
    }
    None
}

// ---------- device steps ----------

async fn dump_ui(filename: &str) -> Result<String> {
    let remote = root_tmp(filename);
    shell(&format!("mkdir -p {ROOT_TMP} && uiautomator dump {remote}")).await?;
    let xml = shell(&format!("cat {remote}"))
        .await
        .map_err(|e| anyhow!("uiautomator dump left nothing at {remote}: {e}"))?;
    if xml.trim().is_empty() {
        return Err(anyhow!("uiautomator dump left {remote} empty"));
    }
    Ok(xml)
}

async fn tap(x: i32, y: i32) -> Result<()> {
    shell(&format!("input tap {x} {y}")).await?;
    sleep(500).await;
    Ok(())
}

async fn tap_url_bar(mode: &str) -> Result<()> {
    let filename = if mode == "clone" { "clone-current.xml" } else { "brave-current.xml" };
    for attempt in 1..=URL_BAR_ATTEMPTS {
        let xml = dump_ui(filename).await?;
        if let Some((x, y)) = find_url_bar(&xml) {
            return tap(x, y).await;
        }
        logln!(MOD, "url bar not found ({mode}), retry {attempt}/{URL_BAR_ATTEMPTS}");
        sleep(2000).await;
    }
    Err(anyhow!("URL bar not found in {mode} after {URL_BAR_ATTEMPTS} attempts"))
}

async fn type_url(url: &str) -> Result<()> {
    let escaped = url.replace('\'', "'\\''");
    shell(&format!("input text '{escaped}'")).await?;
    sleep(1000).await;
    shell("input keyevent KEYCODE_ENTER").await?;
    sleep(5000).await;
    Ok(())
}

async fn clear_url_bar() -> Result<()> {
    shell("input keycombination 113 29").await?;
    shell("input keyevent KEYCODE_DEL").await?;
    Ok(())
}

async fn search_again(url: &str, mode: &str) -> Result<()> {
    tap_url_bar(mode).await?;
    clear_url_bar().await?;
    type_url(url).await
}

async fn open_tab_switcher() -> Result<String> {
    let xml = dump_ui("tabs.xml").await?;
    match find_node_by_description(&xml, "Show tabs").or_else(|| find_node_by_description(&xml, "Tabs")) {
        Some((x, y)) => tap(x, y).await?,
        None => tap(766, 2268).await?,
    }
    sleep(3000).await;
    dump_ui("tabs.xml").await
}

async fn open_first_search(url: &str, mode: &str) -> Result<()> {
    if mode == "clone" {
        shell(&format!("am start -W -n {CLONE_PACKAGE}/.MainActivity")).await?;
        sleep(5000).await;
        let home = dump_ui("clone-home.xml").await?;
        if let Some((x, y)) = find_node_by_description(&home, "Brave") {
            tap(x, y).await?;
        }
        sleep(7000).await;
        open_tab_switcher().await?;
        let tabs = dump_ui("tabs.xml").await?;
        if let Some((x, y)) = find_node_by_description(&tabs, "New tab") {
            tap(x, y).await?;
        }
        sleep(4000).await;
        tap_url_bar("clone").await?;
        return type_url(url).await;
    }
    shell(&format!(
        "am start -a android.intent.action.VIEW -d '{url}' {PRIMARY_PACKAGE}/com.google.android.apps.chrome.Main"
    ))
    .await?;
    sleep(4000).await;
    Ok(())
}

async fn run_searches(mode: &str) -> Result<String> {
    let mut previous_query = String::new();
    let mut last_query = String::new();
    logln!(MOD, "START {SEARCH_COUNT} searches ({mode})");
    for index in 1..=SEARCH_COUNT {
        let query = random_query();
        let url = build_search_url(&query, &previous_query);
        previous_query = query.clone();
        last_query = query.clone();
        if index == 1 {
            open_first_search(&url, mode).await?;
        } else {
            search_again(&url, mode).await?;
        }
        logln!(MOD, "search {index}: {query}");
        sleep(2000).await;
    }
    logln!(MOD, "DONE searches ({mode})");
    Ok(last_query)
}

/// Locate tesseract without trusting PATH (same reason as `util::su`).
fn tesseract_path() -> Option<String> {
    use std::sync::OnceLock;
    static FOUND: OnceLock<Option<String>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            let mut candidates: Vec<PathBuf> = Vec::new();
            if let Some(prefix) = std::env::var_os("PREFIX") {
                candidates.push(PathBuf::from(prefix).join("bin/tesseract"));
            }
            candidates.push(PathBuf::from("/data/data/com.termux/files/usr/bin/tesseract"));
            if let Some(path) = std::env::var_os("PATH") {
                candidates.extend(std::env::split_paths(&path).map(|dir| dir.join("tesseract")));
            }
            candidates
                .into_iter()
                .find(|path| path.is_file())
                .map(|path| path.display().to_string())
        })
        .clone()
}

async fn parse_ocr_claim() -> Option<(i32, i32)> {
    let tesseract = tesseract_path()?;
    let image = path_in("rewards-current.png");
    let output = path_in("rewards-current");
    let remote = root_tmp("rewards-current.png");
    let copied = shell(&format!(
        "mkdir -p {ROOT_TMP} && screencap -p {remote} && cp {remote} {} && chmod 644 {}",
        image.display(),
        image.display()
    ))
    .await;
    if copied.is_err() {
        return None;
    }
    run_cmd(
        &tesseract,
        &[&image.display().to_string(), &output.display().to_string(), "--psm", "11", "tsv"],
    )
    .await
    .ok()?;
    let tsv = std::fs::read_to_string(path_in("rewards-current.tsv")).ok()?;
    parse_claim_from_tsv(&tsv)
}

async fn claim_if_ready() -> Result<()> {
    let Some((x, y)) = parse_ocr_claim().await else {
        match tesseract_path() {
            Some(_) => logln!(MOD, "no ready points, skip claim"),
            None => logln!(MOD, "claim skipped: tesseract is not installed (pkg install tesseract)"),
        }
        return Ok(());
    };
    logln!(MOD, "ready to claim, tapping {x},{y}");
    tap(x, y).await?;
    sleep(5000).await;
    match parse_ocr_claim().await {
        Some((hx, hy)) => {
            tap(hx, hy + 130).await?;
            logln!(MOD, "confirmed claim at {hx},{}", hy + 130);
        }
        None => {
            tap(884, 420).await?;
            logln!(MOD, "fallback claim 884,420");
        }
    }
    sleep(4000).await;
    Ok(())
}

async fn close_last_search_tab(last_query: &str) -> Result<()> {
    let xml = open_tab_switcher().await?;

    for tag in tags(&xml) {
        if let Some(desc) = attr(tag, "content-desc") {
            if desc.starts_with("Close") && desc.contains("Dashboard") {
                if let Some((x, y)) = bounds_center(tag) {
                    tap(x, y).await?;
                    logln!(MOD, "closed dashboard tab");
                    sleep(4000).await;
                    return Ok(());
                }
            }
        }
    }

    let exact = format!("Close {}", xml_escape(last_query));
    let mut target = None;
    for tag in tags(&xml) {
        if let Some(desc) = attr(tag, "content-desc") {
            if desc.starts_with(&exact) {
                target = bounds_center(tag);
                if target.is_some() {
                    break;
                }
            }
        }
    }
    if target.is_none() {
        for tag in tags(&xml) {
            if let Some(desc) = attr(tag, "content-desc") {
                if desc.starts_with("Close") {
                    target = bounds_center(tag);
                    if target.is_some() {
                        break;
                    }
                }
            }
        }
    }
    match target {
        Some((x, y)) => {
            tap(x, y).await?;
            logln!(MOD, "closed last search tab");
        }
        None => logln!(MOD, "no search tab found to close"),
    }
    sleep(2000).await;
    Ok(())
}

async fn finish_account(mode: &str) {
    for key in ["KEYCODE_BACK", "KEYCODE_BACK", "KEYCODE_HOME"] {
        shell_best_effort(&format!("input keyevent {key}")).await;
        sleep(2000).await;
    }
    shell_best_effort(&format!("am force-stop {PRIMARY_PACKAGE}")).await;
    if mode == "clone" {
        shell_best_effort(&format!("am force-stop {CLONE_PACKAGE}")).await;
    }
}

async fn run_account(mode: &str) -> Result<()> {
    let last_query = run_searches(mode).await?;
    search_again("https://rewards.bing.com/dashboard", mode).await?;
    sleep(4000).await;
    claim_if_ready().await?;
    close_last_search_tab(&last_query).await?;
    finish_account(mode).await;
    Ok(())
}

pub fn cleanup_artifacts() {
    for name in ARTIFACTS {
        let _ = std::fs::remove_file(path_in(name));
    }
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Did today's run already happen? Lives on disk because a reboot erases any in-memory flag.
pub fn ran_today() -> bool {
    crate::state::State::load("bing.json").str("last_run").map(|v| v == today()).unwrap_or(false)
}

fn mark_ran() {
    let mut state = crate::state::State::load("bing.json");
    state.set_str("last_run", today());
    if let Err(e) = state.save() {
        logln!(MOD, "could not record the run: {e}");
    }
}

pub async fn run() -> Result<()> {
    logln!(MOD, "Bing Rewards started (root shell: {})", crate::util::su());
    mark_ran();
    for mode in ["brave", "clone"] {
        if mode == "clone" {
            sleep(3000).await;
        }
        if let Err(e) = run_account(mode).await {
            logln!(MOD, "Bing Rewards failed ({mode}): {e}");
            let package = if mode == "clone" { CLONE_PACKAGE } else { PRIMARY_PACKAGE };
            shell_best_effort(&format!("am force-stop {package}")).await;
        }
    }
    cleanup_artifacts();
    logln!(MOD, "DONE all rewards");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<hierarchy><node content-desc="Brave" bounds="[100,200][300,400]" /><node resource-id="com.brave.browser:id/url_bar" bounds="[10,20][500,120]" /><node content-desc="Close abc123" bounds="[0,0][50,50]" /></hierarchy>"#;

    #[test]
    fn finds_nodes_and_url_bar() {
        assert_eq!(find_node_by_description(XML, "Brave"), Some((200, 300)));
        assert_eq!(find_url_bar(XML), Some((255, 70)));
        assert_eq!(find_node_by_description(XML, "Missing"), None);
    }

    #[test]
    fn parses_claim_tsv() {
        let tsv = "level\tpage\tblock\tpar\tline\tword\tleft\ttop\twidth\theight\tconf\ttext\n\
                   5\t1\t1\t1\t1\t1\t100\t200\t40\t20\t90\tclaim\n";
        assert_eq!(parse_claim_from_tsv(tsv), Some((120, 210)));
        assert_eq!(parse_claim_from_tsv("level\tpage\n5\t1\n"), None);
    }

    #[test]
    fn work_dir_is_absolute_even_when_configured_relative() {
        // `Logs` in .env used to stay relative, so root wrote the dump elsewhere
        std::env::set_var("BING_WORK_DIR", "target/bing-logs");
        let dir = work_dir();
        std::env::remove_var("BING_WORK_DIR");
        assert!(dir.is_absolute(), "{} is relative", dir.display());
        assert!(dir.is_dir());
    }

    #[test]
    fn tesseract_lookup_is_absolute_when_found() {
        if let Some(path) = tesseract_path() {
            assert!(std::path::Path::new(&path).is_file(), "{path} must exist");
        }
    }

    #[test]
    fn builds_search_url_and_query() {
        let url = build_search_url("abcd", "prev");
        assert!(url.starts_with("https://www.bing.com/search?q=abcd&form=QBLH"));
        assert!(url.contains("&pq=prev&"));
        let q = random_query();
        assert_eq!(q.len(), 4);
        assert!(q.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}
