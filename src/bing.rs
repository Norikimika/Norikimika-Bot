//! Bing Rewards UI automation ported from bing.js.
//!
//! Runs once a day at 01:00 local via su/uiautomator/input: 25 searches in
//! Brave, then the same in the multibox clone, claims dashboard points, closes
//! the leftover tab and deletes its dump artifacts.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use chrono::{Duration as ChronoDuration, Local, NaiveTime, TimeZone};

use crate::app::App;
use crate::util::{data_dir, log_to, run};

const SCHEDULE: &str = "0 1 * * *";
const PRIMARY_PACKAGE: &str = "com.brave.browser";
const CLONE_PACKAGE: &str = "com.multibox.clone.hub";
const SEARCH_COUNT: usize = 25;
const URL_BAR_ATTEMPTS: usize = 6;
const BOUNDS: &str = r"\[[0-9]+,[0-9]+\]\[[0-9]+,[0-9]+\]";

const ARTIFACT_NAMES: [&str; 7] = [
    "brave-current.xml",
    "clone-current.xml",
    "clone-home.xml",
    "clone-tabs.xml",
    "tabs.xml",
    "rewards-current.png",
    "rewards-current.tsv",
];

static RUNNING: AtomicBool = AtomicBool::new(false);

fn work_dir() -> PathBuf {
    data_dir()
}

fn log(msg: impl AsRef<str>) {
    log_to("bing.log", msg.as_ref());
}

async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// Every UI command runs as root, same as bing.js.
async fn shell(command: &str) -> anyhow::Result<String> {
    let out = run("su", &["-c", command]).await?;
    if out.ok() {
        Ok(out.stdout)
    } else {
        anyhow::bail!("su -c `{command}` failed: {}", out.stderr.trim())
    }
}

fn random_query() -> String {
    let bytes: [u8; 8] = rand::random();
    base64::engine::general_purpose::STANDARD
        .encode(bytes)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(4)
        .collect()
}

/// Stand-in for crypto.randomUUID().toUpperCase().
fn random_uuid_upper() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
    .to_uppercase()
}

fn build_search_url(query: &str, previous_query: &str) -> String {
    format!(
        "https://www.bing.com/search?q={query}&form=QBLH&sp=-1&lq=0&pq={previous_query}&sc=12-4&qs=n&sk=&cvid={}",
        random_uuid_upper()
    )
}

fn xml_escape(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Centre of the first bounds="[x1,y1][x2,y2]" captured by `pattern`.
fn parse_bounds(xml: &str, pattern: &str) -> Option<(i64, i64)> {
    let re = regex::Regex::new(pattern).ok()?;
    let m = re.find(xml)?;
    let nums: Vec<i64> = regex::Regex::new(r"\d+")
        .ok()?
        .find_iter(m.as_str())
        .filter_map(|n| n.as_str().parse().ok())
        .collect();
    if nums.len() < 4 {
        return None;
    }
    Some(((nums[0] + nums[2]) / 2, (nums[1] + nums[3]) / 2))
}

fn find_node_by_description(xml: &str, description: &str) -> Option<(i64, i64)> {
    let escaped = regex::escape(description);
    parse_bounds(
        xml,
        &format!(r#"content-desc="{escaped}"[^>]*bounds="{BOUNDS}""#),
    )
    .or_else(|| parse_bounds(xml, &format!(r#"text="{escaped}"[^>]*bounds="{BOUNDS}""#)))
}

fn find_url_bar(xml: &str) -> Option<(i64, i64)> {
    parse_bounds(
        xml,
        &format!(
            r#"resource-id="[a-zA-Z0-9_.]*:id/(url_bar|urlbar|omnibox|url_bar_container|search_bar|toolbar_url_box)"[^>]*bounds="{BOUNDS}""#
        ),
    )
    .or_else(|| {
        parse_bounds(
            xml,
            &format!(
                r#"content-desc="[^"]*(?:Search or enter|Enter a URL|Enter a web address|Search or type URL|Search Bing|Microsoft Edge)[^"]*"[^>]*bounds="{BOUNDS}""#
            ),
        )
    })
}

async fn dump_ui(filename: &str) -> anyhow::Result<String> {
    let target = work_dir().join(filename);
    shell(&format!("uiautomator dump {}", target.display())).await?;
    tokio::fs::read_to_string(&target)
        .await
        .map_err(|e| anyhow::anyhow!("read {} failed: {e}", target.display()))
}

async fn tap(x: i64, y: i64) {
    let _ = shell(&format!("input tap {x} {y}")).await;
    sleep_ms(500).await;
}

async fn tap_url_bar(mode: &str) -> anyhow::Result<()> {
    let filename = if mode == "clone" { "clone-current.xml" } else { "brave-current.xml" };
    for attempt in 1..=URL_BAR_ATTEMPTS {
        let xml = dump_ui(filename).await.unwrap_or_default();
        if let Some((x, y)) = find_url_bar(&xml) {
            tap(x, y).await;
            return Ok(());
        }
        log(format!(
            "url bar not found ({mode}), retry {attempt}/{URL_BAR_ATTEMPTS}"
        ));
        sleep_ms(2000).await;
    }
    anyhow::bail!("URL bar not found in {mode} after {URL_BAR_ATTEMPTS} attempts")
}

async fn type_url(url: &str) -> anyhow::Result<()> {
    let escaped = url.replace('\'', r"'\''");
    shell(&format!("input text '{escaped}'")).await?;
    sleep_ms(1000).await;
    shell("input keyevent KEYCODE_ENTER").await?;
    sleep_ms(5000).await;
    Ok(())
}

async fn clear_url_bar() -> anyhow::Result<()> {
    shell("input keycombination 113 29").await?;
    shell("input keyevent KEYCODE_DEL").await?;
    Ok(())
}

async fn search_again(url: &str, mode: &str) -> anyhow::Result<()> {
    tap_url_bar(mode).await?;
    clear_url_bar().await?;
    type_url(url).await
}

async fn open_tab_switcher() -> anyhow::Result<String> {
    let xml = dump_ui("tabs.xml").await.unwrap_or_default();
    let node = find_node_by_description(&xml, "Show tabs").or_else(|| find_node_by_description(&xml, "Tabs"));
    match node {
        Some((x, y)) => tap(x, y).await,
        None => tap(766, 2268).await,
    }
    sleep_ms(3000).await;
    dump_ui("tabs.xml").await
}

async fn open_first_search(url: &str, mode: &str) -> anyhow::Result<()> {
    if mode == "clone" {
        shell(&format!("am start -W -n {CLONE_PACKAGE}/.MainActivity")).await?;
        sleep_ms(5000).await;
        let home = dump_ui("clone-home.xml").await.unwrap_or_default();
        if let Some((x, y)) = find_node_by_description(&home, "Brave") {
            tap(x, y).await;
        }
        sleep_ms(7000).await;
        let tabs = open_tab_switcher().await?;
        if let Some((x, y)) = find_node_by_description(&tabs, "New tab") {
            tap(x, y).await;
        }
        sleep_ms(4000).await;
        tap_url_bar("clone").await?;
        return type_url(url).await;
    }

    shell(&format!(
        "am start -a android.intent.action.VIEW -d '{url}' {PRIMARY_PACKAGE}/com.google.android.apps.chrome.Main"
    ))
    .await?;
    sleep_ms(4000).await;
    Ok(())
}

async fn run_searches(mode: &str) -> anyhow::Result<String> {
    let mut previous_query = String::new();
    let mut last_query = String::new();
    log(format!("START {SEARCH_COUNT} searches ({mode})"));
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
        log(format!("search {index}: {query}"));
        sleep_ms(2000).await;
    }
    log(format!("DONE searches ({mode})"));
    Ok(last_query)
}

/// OCR the dashboard screenshot for a "claim" button and return its centre.
async fn parse_ocr_claim() -> Option<(i64, i64)> {
    let image = work_dir().join("rewards-current.png");
    let output = work_dir().join("rewards-current");
    shell(&format!("screencap -p {}", image.display())).await.ok()?;
    run(
        "tesseract",
        &[
            &image.display().to_string(),
            &output.display().to_string(),
            "--psm",
            "11",
            "tsv",
        ],
    )
    .await
    .ok()?;
    let text = tokio::fs::read_to_string(output.with_extension("tsv")).await.ok()?;
    for row in text.lines().skip(1) {
        let fields: Vec<&str> = row.split('\t').collect();
        if fields.get(11).map(|f| f.trim().to_lowercase()).as_deref() != Some("claim") {
            continue;
        }
        let (Some(left), Some(top), Some(width), Some(height)) = (
            fields.get(6).and_then(|v| v.parse::<f64>().ok()),
            fields.get(7).and_then(|v| v.parse::<f64>().ok()),
            fields.get(8).and_then(|v| v.parse::<f64>().ok()),
            fields.get(9).and_then(|v| v.parse::<f64>().ok()),
        ) else {
            continue;
        };
        return Some((
            (left + width / 2.0).floor() as i64,
            (top + height / 2.0).floor() as i64,
        ));
    }
    None
}

async fn claim_if_ready() {
    let Some((x, y)) = parse_ocr_claim().await else {
        log("no ready points, skip claim");
        return;
    };
    log(format!("ready to claim, tapping {x},{y}"));
    tap(x, y).await;
    sleep_ms(5000).await;
    match parse_ocr_claim().await {
        Some((hx, hy)) => {
            tap(hx, hy + 130).await;
            log(format!("confirmed claim at {hx},{}", hy + 130));
        }
        None => {
            tap(884, 420).await;
            log("fallback claim 884,420");
        }
    }
    sleep_ms(4000).await;
}

async fn close_last_search_tab(last_query: &str) {
    let xml = match open_tab_switcher().await {
        Ok(x) => x,
        Err(e) => {
            log(format!("tab switcher failed: {e}"));
            return;
        }
    };

    if let Some((x, y)) = parse_bounds(
        &xml,
        &format!(r#"content-desc="Close[^"]*Dashboard[^"]*"[^>]*bounds="{BOUNDS}""#),
    ) {
        tap(x, y).await;
        log("closed dashboard tab");
        sleep_ms(4000).await;
        return;
    }

    let exact = format!(
        r#"content-desc="Close {}[^"]*"[^>]*bounds="{}""#,
        regex::escape(&xml_escape(last_query)),
        BOUNDS
    );
    let any_close = format!(r#"content-desc="Close[^"]*"[^>]*bounds="{BOUNDS}""#);
    if let Some((x, y)) = parse_bounds(&xml, &exact).or_else(|| parse_bounds(&xml, &any_close)) {
        tap(x, y).await;
        log("closed last search tab");
    } else {
        log("no search tab found to close");
    }
    sleep_ms(2000).await;
}

async fn finish_account(mode: &str) {
    let _ = shell("input keyevent KEYCODE_BACK").await;
    sleep_ms(2000).await;
    let _ = shell("input keyevent KEYCODE_BACK").await;
    sleep_ms(2000).await;
    let _ = shell("input keyevent KEYCODE_HOME").await;
    sleep_ms(2000).await;
    let _ = shell(&format!("am force-stop {PRIMARY_PACKAGE}")).await;
    if mode == "clone" {
        let _ = shell(&format!("am force-stop {CLONE_PACKAGE}")).await;
    }
}

async fn run_account(mode: &str) -> anyhow::Result<()> {
    let last_query = run_searches(mode).await?;
    search_again("https://rewards.bing.com/dashboard", mode).await?;
    sleep_ms(4000).await;
    claim_if_ready().await;
    close_last_search_tab(&last_query).await;
    finish_account(mode).await;
    Ok(())
}

fn cleanup_artifacts() {
    for name in ARTIFACT_NAMES {
        let _ = std::fs::remove_file(work_dir().join(name));
    }
}

/// Same "previous run still active" guard as the JS `running` flag.
async fn run_rewards(_app: Arc<App>) {
    if RUNNING.swap(true, Ordering::SeqCst) {
        log("Bing Rewards skipped: previous run still active");
        return;
    }
    log("Bing Rewards started");
    for mode in ["brave", "clone"] {
        if mode == "clone" {
            sleep_ms(3000).await;
        }
        if let Err(e) = run_account(mode).await {
            log(format!("Bing Rewards failed ({mode}): {e}"));
            let pkg = if mode == "clone" { CLONE_PACKAGE } else { PRIMARY_PACKAGE };
            let _ = shell(&format!("am force-stop {pkg}")).await;
        }
    }
    log("DONE all rewards");
    cleanup_artifacts();
    RUNNING.store(false, Ordering::SeqCst);
}

/// Next local 01:00 as unix seconds (`0 1 * * *`, no missed-run recovery).
fn next_run_ts() -> i64 {
    let now = Local::now();
    let at = NaiveTime::from_hms_opt(1, 0, 0).expect("01:00");
    let mut day = now.date_naive();
    loop {
        if let Some(dt) = Local.from_local_datetime(&day.and_time(at)).single() {
            if dt > now {
                return dt.timestamp();
            }
        }
        match day.checked_add_days(chrono::Days::new(1)) {
            Some(next) => day = next,
            None => return (now + ChronoDuration::days(1)).timestamp(),
        }
    }
}

pub fn spawn(app: Arc<App>) {
    tokio::spawn(async move {
        log(format!("Bing Rewards scheduled: {SCHEDULE}"));
        loop {
            let wait = next_run_ts() - Local::now().timestamp();
            if wait > 0 {
                tokio::time::sleep(Duration::from_secs(wait as u64)).await;
            }
            run_rewards(app.clone()).await;
            // Keep a slow run from re-firing inside the same minute.
            tokio::time::sleep(Duration::from_secs(61)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_centre() {
        let xml = r#"<node content-desc="Brave" bounds="[10,20][110,220]" />"#;
        assert_eq!(find_node_by_description(xml, "Brave"), Some((60, 120)));
    }

    #[test]
    fn url_bar_by_resource_id() {
        let xml = r#"<node resource-id="com.brave.browser:id/url_bar" text="x" bounds="[0,100][1080,200]" />"#;
        assert_eq!(find_url_bar(xml), Some((540, 150)));
    }

    #[test]
    fn query_is_four_alnum_chars() {
        let q = random_query();
        assert_eq!(q.len(), 4);
        assert!(q.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn uuid_shape() {
        let u = random_uuid_upper();
        assert_eq!(u.len(), 36);
        assert_eq!(u.chars().filter(|c| *c == '-').count(), 4);
        assert_eq!(u, u.to_uppercase());
    }
}
