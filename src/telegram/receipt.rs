use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};

use crate::app::App;
use crate::config::Config;
use crate::tg::Msg;
use crate::util::{log_to, now_ms, run};

const W: usize = 32;
const RECEIPT_PROMPT: &str = "Identify the type of this receipt and extract its data.
Respond with a single JSON object and nothing else — no preamble, no safety note, no markdown, no explanation. Your entire response must start with { and end with }.

If it's a Money TRANSFER receipt (DANA, OVO, GoPay, BCA, BRI, etc):
{
  \"type\": \"transfer\",
  \"date\": \"YYYY-MM-DD\",
  \"time\": \"HH:MM:SS\",
  \"bank\": \"destination bank/wallet name e.g. DANA\",
  \"accountNumber\": \"destination account/phone number\",
  \"recipient\": \"recipient name\",
  \"amount\": \"amount transferred excluding fee fee\"
}

If it's a PLN ELECTRICITY TOKEN receipt:
{
  \"type\": \"pln\",
  \"date\": \"YYYY-MM-DD\",
  \"time\": \"HH:MM:SS\",
  \"customerId\": \"customer ID number\",
  \"meterNumber\": \"meter number\",
  \"name\": \"customer name\",
  \"tariff\": \"tariff/power e.g. R2/3500VA\",
  \"kwh\": \"kWh total amount\",
  \"token\": \"20 digits token numbers only without space or hyphen\",
  \"amount\": \"total payment paid by customer with thousands separator\"
}

If it's a phone-credit top-up receipt (Telkomsel, Tri, XL, simPATI, Indosat, etc):
{
  \"type\": \"topup\",
  \"date\": \"YYYY-MM-DD\",
  \"time\": \"HH:MM:SS\",
  \"provider\": \"destination provider/sim name e.g. SIMPATI\",
  \"phoneNumber\": \"destination account/phone number\",
  \"amount\": \"amount transferred excluding fee fee\"
}";

// ================== receipt text (byte-identical to the JS builders) ==================

fn digits(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// parseInt(str.replace(/\./g, ""), 10) || 0
fn num(s: &str) -> i64 {
    digits(s).parse().unwrap_or(0)
}

/// JS truthiness for the fields the model returns ("" is falsy).
fn truthy(v: &Value) -> bool {
    match v {
        Value::String(s) => !s.is_empty(),
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        _ => true,
    }
}

fn sval(d: &Value, key: &str) -> String {
    match &d[key] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// num.toString().replace(/\B(?=(?:\d{3})+(?!\d))/g, ".")
fn rupiah(n: i64) -> String {
    let neg = n < 0;
    let d = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(d.len() + d.len() / 3);
    for (i, c) in d.chars().enumerate() {
        if i > 0 && (d.len() - i) % 3 == 0 {
            out.push('.');
        }
        out.push(c);
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

/// truncateMiddle(): `String(text || "-")`, hard slice at maxLen.
fn truncate(text: &str, max: usize) -> String {
    let s = if text.is_empty() { "-" } else { text };
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

fn pad_c(text: &str, w: usize) -> String {
    let s = truncate(text, w);
    let n = s.chars().count();
    if n >= w {
        return s;
    }
    format!("{}{}", " ".repeat((w - n) / 2), s)
}

fn pad_r(label: &str, value: &str, w: usize) -> String {
    let spaces = w as isize - label.chars().count() as isize - value.chars().count() as isize;
    if spaces <= 0 {
        format!("{label}{value}")
    } else {
        format!("{label}{}{value}", " ".repeat(spaces as usize))
    }
}

/// (fee, "5.000") — 5k up to 500k, then +5k per started million.
fn calc_fee(amount: &str) -> (i64, String) {
    let n = num(amount);
    if n <= 500_000 {
        return (5_000, "5.000".to_string());
    }
    let steps = (n - 500_000 + 999_999) / 1_000_000;
    let fee = 5_000 + steps * 5_000;
    (fee, rupiah(fee))
}

fn round_token_amount(amount: &str) -> String {
    let n = num(amount);
    if n <= 0 {
        return "0".to_string();
    }
    let rem = n % 10_000;
    if rem > 0 && rem <= 5_000 {
        rupiah(n - rem)
    } else {
        rupiah(n)
    }
}

/// parseInt() prefix semantics: leading spaces, optional sign, leading digits.
fn parse_lead_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (sign, t) = if let Some(r) = t.strip_prefix('-') {
        (-1, r)
    } else if let Some(r) = t.strip_prefix('+') {
        (1, r)
    } else {
        (1, t)
    };
    let d: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if d.is_empty() {
        None
    } else {
        d.parse::<i64>().ok().map(|v| sign * v)
    }
}

/// WITA = WIB + 1h; minutes/seconds kept verbatim like the JS template.
fn adjust_to_wita(time: &str) -> String {
    if time.is_empty() || !time.contains(':') {
        return "-".to_string();
    }
    let parts: Vec<&str> = time.trim().split(':').collect();
    let hours = match parse_lead_int(parts[0]) {
        Some(h) => h,
        None => return time.to_string(),
    };
    let pick = |i: usize| -> String {
        match parts.get(i) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => "00".to_string(),
        }
    };
    format!("{:02}:{}:{}", (hours + 1).rem_euclid(24), pick(1), pick(2))
}

fn date_or_dash(d: &Value) -> String {
    let t = sval(d, "date");
    if t.is_empty() {
        "-".to_string()
    } else {
        t
    }
}

fn build_receipt_pln(d: &Value, store_name: &str, store_addr: &str) -> String {
    let eq = "=".repeat(W);
    let da = "-".repeat(W);
    let rounded = round_token_amount(&sval(d, "amount"));
    let fee = calc_fee(&rounded);
    let total = rupiah(num(&rounded) + fee.0);
    let wita = adjust_to_wita(&sval(d, "time"));

    let tk: String = sval(d, "token")
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    let groups: Vec<String> = tk.chars().collect::<Vec<char>>().chunks(4).map(|c| c.iter().collect()).collect();
    let tl1 = groups.iter().take(3).cloned().collect::<Vec<_>>().join(" ");
    let tl2 = groups.iter().skip(3).cloned().collect::<Vec<_>>().join(" ");

    let mut out: Vec<String> = vec![
        pad_c(store_name, W),
        pad_c(store_addr, W),
        eq.clone(),
        pad_c("STRUK PEMBELIAN TOKEN LISTRIK", W),
        eq.clone(),
        pad_r("TANGGAL", &date_or_dash(d), W),
        pad_r("WAKTU", &wita, W),
        da.clone(),
        format!("ID PEL     : {}", truncate(&sval(d, "customerId"), 18)),
        format!("NO METER   : {}", truncate(&sval(d, "meterNumber"), 18)),
        format!("NAMA       : {}", truncate(&sval(d, "name"), 18)),
        format!("TARIF/DAYA : {}", truncate(&sval(d, "tariff"), 18)),
        format!("JML KWH    : {}", truncate(&sval(d, "kwh"), 18)),
        da.clone(),
        pad_c("TOKEN", W),
        pad_c(&tl1, W),
    ];
    if !tl2.is_empty() {
        out.push(pad_c(&tl2, W));
    }
    out.extend([
        da,
        pad_r("NOMINAL", &format!("Rp {rounded}"), W),
        pad_r("ADMIN", &format!("Rp {}", fee.1), W),
        eq.clone(),
        pad_r("TOTAL", &format!("Rp {total}"), W),
        eq,
        String::new(),
        pad_c("** TRANSAKSI BERHASIL **", W),
        String::new(),
        pad_c("TERIMA KASIH", W),
    ]);
    out.join("\n")
}

fn build_receipt_transfer(d: &Value, store_name: &str, store_addr: &str) -> String {
    let eq = "=".repeat(W);
    let da = "-".repeat(W);
    let amount = sval(d, "amount");
    let fee = calc_fee(&amount);
    let total = rupiah(num(&amount) + fee.0);
    let wita = adjust_to_wita(&sval(d, "time"));

    [
        pad_c(store_name, W),
        pad_c(store_addr, W),
        eq.clone(),
        pad_r("TANGGAL", &date_or_dash(d), W),
        pad_r("WAKTU", &wita, W),
        da.clone(),
        pad_c("DATA PENERIMA", W),
        format!("TUJUAN   : {}", truncate(&sval(d, "bank"), 10)),
        format!("NOMOR    : {}", truncate(&sval(d, "accountNumber"), 15)),
        format!("PENERIMA : {}", truncate(&sval(d, "recipient"), 21)),
        da,
        pad_r("NOMINAL", &format!("Rp {}", if amount.is_empty() { "0".to_string() } else { amount }), W),
        pad_r("ADMIN", &format!("Rp {}", fee.1), W),
        eq.clone(),
        pad_r("TOTAL", &format!("Rp {total}"), W),
        eq,
        String::new(),
        pad_c("** TRANSAKSI BERHASIL **", W),
        String::new(),
        pad_c("TERIMA KASIH", W),
    ]
    .join("\n")
}

fn build_receipt_topup(d: &Value, store_name: &str, store_addr: &str) -> String {
    let eq = "=".repeat(W);
    let da = "-".repeat(W);
    let rounded = round_token_amount(&sval(d, "amount"));
    let total = rupiah(num(&rounded) + 3_000);
    let wita = adjust_to_wita(&sval(d, "time"));

    [
        pad_c(store_name, W),
        pad_c(store_addr, W),
        eq.clone(),
        pad_r("TANGGAL", &date_or_dash(d), W),
        pad_r("WAKTU", &wita, W),
        da.clone(),
        pad_c("DATA PENERIMA", W),
        format!("TUJUAN   : {}", truncate(&sval(d, "provider"), 18)),
        format!("NOMOR    : {}", truncate(&sval(d, "phoneNumber"), 18)),
        da,
        pad_r("NOMINAL", &format!("Rp {rounded}"), W),
        pad_r("ADMIN", "Rp 3.000", W),
        eq.clone(),
        pad_r("TOTAL", &format!("Rp {total}"), W),
        eq,
        String::new(),
        pad_c("** TRANSAKSI BERHASIL **", W),
        String::new(),
        pad_c("TERIMA KASIH", W),
    ]
    .join("\n")
}

fn build_receipt(d: &Value, store_name: &str, store_addr: &str) -> String {
    match d["type"].as_str().unwrap_or("") {
        "transfer" => build_receipt_transfer(d, store_name, store_addr),
        "topup" => build_receipt_topup(d, store_name, store_addr),
        _ => build_receipt_pln(d, store_name, store_addr),
    }
}

// ================== image rendering (SVG -> resvg) ==================

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Monospace advance ratio; a real mono face sits at 0.6em.
const ADV: f32 = 0.6;

fn build_svg(text: &str, size: f32, family: &str) -> String {
    let mut lines = text.split('\n');
    let name = lines.next().unwrap_or("").trim().to_string();
    let addr = lines.next().unwrap_or("").trim().to_string();
    let body: Vec<String> = lines.map(|l| l.to_string()).collect();

    let small = (size - 4.0).max(9.0);
    let large = size + 8.0;

    let widest = body.iter().map(|l| l.chars().count()).max().unwrap_or(W);
    let body_w = widest as f32 * size * ADV;
    let name_w = name.chars().count() as f32 * large * ADV;
    let canvas_w = (body_w.max(name_w + 20.0) + 50.0).clamp(380.0, 460.0);

    let mut rows: Vec<(String, f32)> = vec![(name, large), (addr, small)];
    rows.extend(body.into_iter().map(|l| (l, size)));

    let mut height = 40.0; // 20px border top + bottom, like ImageMagick -border 25x20
    for (_, fs) in &rows {
        height += fs * 1.3;
    }

    let cx = canvas_w / 2.0;
    let mut y = 20.0;
    let mut parts: Vec<String> = Vec::new();
    for (line, fs) in rows {
        y += fs * 1.3;
        if line.is_empty() {
            continue;
        }
        parts.push(format!(
            "<text x=\"{cx:.1}\" y=\"{:.1}\" font-family=\"{}\" font-size=\"{fs:.1}\" text-anchor=\"middle\" fill=\"#000000\">{}</text>",
            y - fs * 0.35,
            xml_escape(family),
            xml_escape(&line)
        ));
    }

    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{canvas_w:.0}\" height=\"{height:.0}\" viewBox=\"0 0 {canvas_w:.0} {height:.0}\">\
<rect width=\"100%\" height=\"100%\" fill=\"#ffffff\"/>{}</svg>",
        parts.join("")
    )
}

fn font_size(cfg: &Config) -> f32 {
    cfg.receipt_fontsize
        .as_deref()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(18.0)
}

/// Rasterise the receipt to PNG bytes. Custom STRUK_FONT is embedded when readable,
/// otherwise the system monospace face is used.
fn render_receipt(cfg: &Config, text: &str) -> anyhow::Result<Vec<u8>> {
    use resvg::usvg::fontdb;

    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    let mut family = "monospace".to_string();
    if !cfg.receipt_font.trim().is_empty() {
        match db.load_font_file(&cfg.receipt_font) {
            Ok(()) => {
                if let Some(face) = db.faces().last() {
                    if let Some((name, _)) = face.families.first() {
                        family = name.clone();
                    }
                }
            }
            Err(e) => log_to(
                "telegram.log",
                &format!("STRUK_FONT {} not loaded ({e}); falling back to monospace", cfg.receipt_font),
            ),
        }
    }

    let svg = build_svg(text, font_size(cfg), &family);
    let mut opt = resvg::usvg::Options::default();
    opt.fontdb = Arc::new(db);
    let tree = resvg::usvg::Tree::from_str(&svg, &opt)?;

    let size = tree.size();
    let (w, h) = (size.width().ceil().max(1.0) as u32, size.height().ceil().max(1.0) as u32);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h)
        .ok_or_else(|| anyhow::anyhow!("pixmap {w}x{h}"))?;
    resvg::render(&tree, resvg::tiny_skia::Transform::default(), &mut pixmap.as_mut());
    Ok(pixmap.encode_png()?)
}

// ================== OpenRouter / 9Router vision ==================

fn endpoint(raw: &str) -> String {
    let base = raw.trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

/// First balanced `{...}` that parses and carries a string `type` field.
/// Models like to wrap the object in prose or `<|im_end|>` markers.
fn extract_receipt_json(text: &str) -> Option<Value> {
    let bytes: Vec<(usize, char)> = text.char_indices().collect();
    for (start_idx, &(start_off, ch)) in bytes.iter().enumerate() {
        if ch != '{' {
            continue;
        }
        let mut depth = 0i32;
        let mut in_str = false;
        let mut esc = false;
        for &(off, c) in &bytes[start_idx..] {
            if in_str {
                if esc {
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else if c == '"' {
                    in_str = false;
                }
                continue;
            }
            match c {
                '"' => in_str = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        let candidate = &text[start_off..off + c.len_utf8()];
                        if let Ok(v) = serde_json::from_str::<Value>(candidate) {
                            if v["type"].is_string() {
                                return Some(v);
                            }
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn strip_fences(s: &str) -> String {
    s.replace("```json", "").replace("```", "").trim().to_string()
}

async fn analyze(app: &App, image: &[u8], mime: &str) -> anyhow::Result<Value> {
    let out = analyze_inner(app, image, mime).await;
    if let Err(e) = &out {
        log_to("telegram.log", &format!("Vision Error: {e}"));
    }
    out
}

async fn analyze_inner(app: &App, image: &[u8], mime: &str) -> anyhow::Result<Value> {
    let raw = app
        .cfg
        .nine_router_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| anyhow::anyhow!("NINE_ROUTER_URL is missing in .env file!"))?;
    let key = app
        .cfg
        .nine_router_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .ok_or_else(|| anyhow::anyhow!("NINE_ROUTER_API_KEY is missing in .env file!"))?;
    let model = app.cfg.nine_router_model.trim();
    if model.is_empty() {
        anyhow::bail!("NINE_ROUTER_MODEL is missing in .env file!");
    }

    let b64 = base64::engine::general_purpose::STANDARD.encode(image);
    let mime = if mime.trim().is_empty() { "image/jpeg" } else { mime.trim() };
    let body = json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{b64}")}},
                {"type": "text", "text": RECEIPT_PROMPT}
            ]
        }],
        "stream": false,
        "response_format": {"type": "json_object"}
    });

    let res = app
        .http
        .post(endpoint(raw))
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(60))
        .json(&body)
        .send()
        .await?;

    let status = res.status();
    let raw_text = res.text().await?;
    if !status.is_success() {
        anyhow::bail!("9Router API HTTP {}: {}", status.as_u16(), raw_text);
    }

    let mut content = String::new();
    match serde_json::from_str::<Value>(&raw_text) {
        Ok(data) => {
            if let Some(c) = data["choices"][0]["message"]["content"].as_str() {
                content = c.to_string();
            }
        }
        Err(_) => {
            // stream:false was asked for, but some routers still emit SSE.
            for line in raw_text.lines() {
                let t = line.trim();
                if !t.starts_with("data: ") || t == "data: [DONE]" {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(&t[6..]) {
                    for path in [["choices", "0", "delta", "content"], ["choices", "0", "message", "content"]] {
                        let mut node = &v;
                        for p in path {
                            node = if let Ok(i) = p.parse::<usize>() { &node[i] } else { &node[p] };
                        }
                        if let Some(s) = node.as_str() {
                            content.push_str(s);
                            break;
                        }
                    }
                }
            }
        }
    }

    if content.is_empty() {
        anyhow::bail!("9Router returned empty response for vision analysis");
    }
    log_to("telegram.log", "Vision: 9Router OK");

    let cleaned = strip_fences(&content);
    match extract_receipt_json(&cleaned) {
        Some(v) => Ok(v),
        None => {
            let head: String = cleaned.chars().take(200).collect();
            log_to("telegram.log", &format!("[JSON Parse Failed] Content received:\n{cleaned}"));
            anyhow::bail!("Invalid JSON format from model: no JSON object with \"type\" field in: {head}")
        }
    }
}

/// Vision analysis; failures are logged (same texts as the JS bot) and yield None.
pub async fn analyze_receipt(app: &App, image: &[u8], mime: &str) -> Option<Value> {
    match analyze(app, image, mime).await {
        Ok(v) => Some(v),
        Err(e) => {
            log_to("telegram.log", &format!("Receipt analysis error: {e}"));
            None
        }
    }
}

// ================== photo flow ==================

/// Clean the recipient name: drop wallet markers the model likes to prepend.
fn clean_recipient(name: &str) -> String {
    let re = regex::Regex::new(r"(?-u:\b)(?:Dana|Dnid|Dnidl|Dnidd|Dnidx|Dnvid)(?-u:\b)").unwrap();
    let ws = regex::Regex::new(r"\s+").unwrap();
    ws.replace_all(&re.replace_all(name, ""), " ").trim().to_string()
}

fn valid_receipt(parsed: &Value) -> bool {
    let kind = parsed["type"].as_str().unwrap_or("");
    match kind {
        "pln" => digits(parsed["token"].as_str().unwrap_or("")).len() >= 10,
        "transfer" => truthy(&parsed["accountNumber"]) || truthy(&parsed["bank"]),
        "topup" => truthy(&parsed["provider"]) || truthy(&parsed["phoneNumber"]),
        _ => false,
    }
}

async fn process(app: &App, msg: &Msg, target: Option<i64>) -> anyhow::Result<()> {
    let file_id = msg
        .photo
        .last()
        .and_then(|p| p["file_id"].as_str())
        .ok_or_else(|| anyhow::anyhow!("photo has no file_id"))?
        .to_string();

    let image = app.tg.download_file(&file_id).await?;

    let Some(mut parsed) = analyze_receipt(app, &image, "image/jpeg").await else {
        return Ok(());
    };

    if !valid_receipt(&parsed) {
        log_to("telegram.log", "Receipt validation failed: Image is not recognized as a valid receipt.");
        return Ok(());
    }

    if let Some(p) = parsed["recipient"].as_str() {
        let cleaned = clean_recipient(p);
        parsed["recipient"] = json!(cleaned);
    }
    if truthy(&parsed["amount"]) {
        parsed["amount"] = json!(rupiah(num(&sval(&parsed, "amount"))));
    }

    let receipt = build_receipt(&parsed, &app.cfg.store_name, &app.cfg.store_address);
    let png = match render_receipt(&app.cfg, &receipt) {
        Ok(p) => p,
        Err(e) => {
            log_to("telegram.log", &format!("ImageMagick error: {e}"));
            return Ok(());
        }
    };

    let path = std::env::temp_dir().join(format!("receipt_{}.png", now_ms()));
    tokio::fs::write(&path, &png).await?;

    if let Err(e) = app.tg.send_photo_bytes(msg.chat_id, target, png, "").await {
        log_to("telegram.log", &format!("Failed to send receipt to Telegram: {e}"));
    }

    if app.cfg.print_cmd.trim().is_empty() {
        log_to("telegram.log", "Print error: PRINT_CMD is not configured in .env.");
        let _ = tokio::fs::remove_file(&path).await;
    } else {
        let cmd = format!("{} \"{}\"", app.cfg.print_cmd.trim(), path.display());
        tokio::spawn(async move {
            match run("sh", &["-c", &cmd]).await {
                Ok(out) if !out.ok() => log_to(
                    "telegram.log",
                    &format!("Print execution error: {}", out.stderr.trim()),
                ),
                Err(e) => log_to("telegram.log", &format!("Print execution error: {e}")),
                _ => {}
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
            let _ = tokio::fs::remove_file(&path).await;
        });
    }
    Ok(())
}

pub async fn handle_photo(app: Arc<App>, msg: &Msg) {
    if !app.is_admin(msg.chat_id, msg.user_id).await {
        app.deny(msg.chat_id, msg.thread, Some(msg.id), None, msg.user_id).await;
        return;
    }

    // Photo intake topic only, like `bot.on("photo", adminOnly, ...)`.
    if app.cfg.topic_intake.is_some() && msg.thread != app.cfg.topic_intake {
        return;
    }
    let target = app.cfg.topic_preview.or(msg.thread);

    let typing = app.typing(msg.chat_id, target, "upload_photo");
    if let Err(e) = process(&app, msg, target).await {
        log_to("telegram.log", &format!("Receipt process error: {e}"));
    }
    typing.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLN_EXPECTED: &str = "           TOKO TEST\n         JL. TEST NO 1\n================================\n STRUK PEMBELIAN TOKEN LISTRIK\n================================\nTANGGAL               2026-10-27\nWAKTU                   11:05:09\n--------------------------------\nID PEL     : 123456789012\nNO METER   : 987654321\nNAMA       : BUDI\nTARIF/DAYA : R1/1300VA\nJML KWH    : 13,2\n--------------------------------\n             TOKEN\n         1234 5678 9012\n           3456 7890\n--------------------------------\nNOMINAL                Rp 20.000\nADMIN                   Rp 5.000\n================================\nTOTAL                  Rp 25.000\n================================\n\n    ** TRANSAKSI BERHASIL **\n\n          TERIMA KASIH";

    const TRANSFER_EXPECTED: &str = "           TOKO TEST\n         JL. TEST NO 1\n================================\nTANGGAL               2026-10-27\nWAKTU                   10:00:00\n--------------------------------\n         DATA PENERIMA\nTUJUAN   : DANA\nNOMOR    : 081234567890\nPENERIMA : Dana  BUDI\n--------------------------------\nNOMINAL               Rp 100.000\nADMIN                   Rp 5.000\n================================\nTOTAL                 Rp 105.000\n================================\n\n    ** TRANSAKSI BERHASIL **\n\n          TERIMA KASIH";

    const TOPUP_EXPECTED: &str = "           TOKO TEST\n         JL. TEST NO 1\n================================\nTANGGAL               2026-10-27\nWAKTU                   10:00:00\n--------------------------------\n         DATA PENERIMA\nTUJUAN   : SIMPATI\nNOMOR    : 08123\n--------------------------------\nNOMINAL                Rp 20.000\nADMIN                   Rp 3.000\n================================\nTOTAL                  Rp 23.000\n================================\n\n    ** TRANSAKSI BERHASIL **\n\n          TERIMA KASIH";

    const PLN_NO_TOKEN_EXPECTED: &str = "           TOKO TEST\n         JL. TEST NO 1\n================================\n STRUK PEMBELIAN TOKEN LISTRIK\n================================\nTANGGAL               2026-10-27\nWAKTU                   11:05:09\n--------------------------------\nID PEL     : 123456789012\nNO METER   : 987654321\nNAMA       : BUDI\nTARIF/DAYA : R1/1300VA\nJML KWH    : 13,2\n--------------------------------\n             TOKEN\n               -\n--------------------------------\nNOMINAL                Rp 20.000\nADMIN                   Rp 5.000\n================================\nTOTAL                  Rp 25.000\n================================\n\n    ** TRANSAKSI BERHASIL **\n\n          TERIMA KASIH";

    fn pln_sample() -> Value {
        json!({
            "type": "pln", "date": "2026-10-27", "time": "10:05:09",
            "customerId": "123456789012", "meterNumber": "987654321", "name": "BUDI",
            "tariff": "R1/1300VA", "kwh": "13,2",
            "token": "1234 5678 9012 3456 7890", "amount": "20.000"
        })
    }

    fn transfer_sample() -> Value {
        json!({
            "type": "transfer", "date": "2026-10-27", "time": "09:00:00",
            "bank": "DANA", "accountNumber": "081234567890",
            "recipient": "Dana  BUDI", "amount": "100.000"
        })
    }

    fn topup_sample() -> Value {
        json!({
            "type": "topup", "date": "2026-10-27", "time": "09:00:00",
            "provider": "SIMPATI", "phoneNumber": "08123", "amount": "25.000"
        })
    }

    // Expected strings pin the exact rendered receipt (Indonesian labels, as printed) for fixed
    // fixtures, so padding/fee/rounding regressions fail loudly.
    #[test]
    fn receipt_text_matches_expected() {
        assert_eq!(build_receipt(&pln_sample(), "TOKO TEST", "JL. TEST NO 1"), PLN_EXPECTED);
        assert_eq!(build_receipt(&transfer_sample(), "TOKO TEST", "JL. TEST NO 1"), TRANSFER_EXPECTED);
        assert_eq!(build_receipt(&topup_sample(), "TOKO TEST", "JL. TEST NO 1"), TOPUP_EXPECTED);
    }

    #[test]
    fn empty_token_centers_dash() {
        let mut v = pln_sample();
        v["token"] = json!("");
        assert_eq!(build_receipt(&v, "TOKO TEST", "JL. TEST NO 1"), PLN_NO_TOKEN_EXPECTED);
    }

    #[test]
    fn helpers_match_js() {
        assert_eq!(rupiah(1_000_000), "1.000.000");
        assert_eq!(rupiah(-5_000), "-5.000");
        assert_eq!(round_token_amount("23.000"), "20.000");
        assert_eq!(round_token_amount("26.000"), "26.000");
        assert_eq!(round_token_amount("0"), "0");
        assert_eq!(calc_fee("500.000"), (5_000, "5.000".to_string()));
        assert_eq!(calc_fee("1.000.000"), (10_000, "10.000".to_string()));
        assert_eq!(calc_fee("1.500.000"), (10_000, "10.000".to_string()));
        assert_eq!(calc_fee("1.500.001"), (15_000, "15.000".to_string()));
        assert_eq!(adjust_to_wita("10:05:09"), "11:05:09");
        assert_eq!(adjust_to_wita("23:30:00"), "00:30:00");
        assert_eq!(adjust_to_wita("9:5:3"), "10:5:3");
        assert_eq!(adjust_to_wita(""), "-");
        assert_eq!(adjust_to_wita("nope"), "-");
        assert_eq!(truncate("123456789012345678901", 18), "123456789012345678");
        assert_eq!(pad_c("", 32), format!("{}-", " ".repeat(15)));
    }

    #[test]
    fn svg_and_json_extraction() {
        let svg = build_svg("TOKO\nALAMAT\n====", 18.0, "monospace");
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("TOKO"));

        let v = extract_receipt_json("User Safety: safe\n{\"type\":\"pln\",\"token\":\"1\"} <|im_end|>").unwrap();
        assert_eq!(v["type"], "pln");
        assert!(extract_receipt_json("no json here").is_none());
        assert_eq!(clean_recipient("Dana  BUDI  S"), "BUDI S");
        assert!(!valid_receipt(&json!({"type": "pln", "token": "12345"})));
        assert!(valid_receipt(&json!({"type": "pln", "token": "1234 5678 90"})));
    }
}
