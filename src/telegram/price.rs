use std::sync::Arc;

use crate::app::App;
use crate::tg::Msg;
use crate::util::{escape_html, log_to};

fn parse_csv(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .map(|row| {
            let mut fields = Vec::new();
            let mut field = String::new();
            let mut in_quotes = false;
            for ch in row.chars() {
                match ch {
                    '"' => in_quotes = !in_quotes,
                    ',' if !in_quotes => {
                        fields.push(field.trim().to_string());
                        field.clear();
                    }
                    _ => field.push(ch),
                }
            }
            fields.push(field.trim().to_string());
            fields
        })
        .filter(|f| f.iter().any(|c| !c.replace('\r', "").is_empty()))
        .collect()
}

fn cell(row: &[String], i: usize) -> String {
    row.get(i).cloned().unwrap_or_default().replace('\r', "")
}

pub async fn handle(app: Arc<App>, msg: Msg, keyword: String) {
    let keyword = keyword.trim().to_ascii_lowercase();
    if keyword.is_empty() {
        let _ = app
            .tg
            .send_message(msg.chat_id, msg.thread, "❌ Please provide an item name.", Some("HTML"), None, Some(msg.id))
            .await;
        return;
    }

    let typing = app.typing(msg.chat_id, msg.thread, "typing");
    let (sheets, gid) = (app.cfg.sheets_id.clone().unwrap_or_default(), app.cfg.sheets_gid.clone().unwrap_or_else(|| "0".into()));
    let url = format!("https://docs.google.com/spreadsheets/d/{sheets}/export?format=csv&gid={gid}");

    let rows = match app.http.get(&url).send().await {
        Ok(r) if r.status().is_success() => match r.text().await {
            Ok(t) => parse_csv(&t),
            Err(e) => {
                typing.abort();
                log_to("telegram.log", &format!("Price fetch error: {e}"));
                let _ = app
                    .tg
                    .send_message(msg.chat_id, msg.thread, "❌ Failed to fetch price list.", None, None, Some(msg.id))
                    .await;
                return;
            }
        },
        Ok(r) => {
            typing.abort();
            log_to("telegram.log", &format!("Price fetch error: Sheets HTTP {}", r.status()));
            let _ = app
                .tg
                .send_message(msg.chat_id, msg.thread, "❌ Failed to fetch price list.", None, None, Some(msg.id))
                .await;
            return;
        }
        Err(e) => {
            typing.abort();
            log_to("telegram.log", &format!("Price fetch error: {e}"));
            let _ = app
                .tg
                .send_message(msg.chat_id, msg.thread, "❌ Failed to fetch price list.", None, None, Some(msg.id))
                .await;
            return;
        }
    };

    let data_rows: Vec<&Vec<String>> = rows.iter().skip(1).filter(|r| !cell(r, 0).is_empty()).collect();
    if data_rows.is_empty() {
        typing.abort();
        let _ = app
            .tg
            .send_message(msg.chat_id, msg.thread, "❌ Price list is empty.", None, None, Some(msg.id))
            .await;
        return;
    }

    let filtered: Vec<&&Vec<String>> = data_rows
        .iter()
        .filter(|r| cell(r, 0).to_ascii_lowercase().contains(&keyword) || cell(r, 1).to_ascii_lowercase().contains(&keyword))
        .collect();

    if filtered.is_empty() {
        typing.abort();
        let text = format!("❌ No items found matching \"<b>{}</b>\".", escape_html(&keyword));
        let _ = app
            .tg
            .send_message(msg.chat_id, msg.thread, &text, Some("HTML"), None, Some(msg.id))
            .await;
        return;
    }

    let clean = |s: String, is_money: bool| {
        let v = s.trim();
        let v = if is_money && v.to_uppercase() == "#VALUE!" { "-" } else { v };
        escape_html(v)
    };

    let mut chunks: Vec<String> = Vec::new();
    let mut current = "<b>💰 Norikimika Price</b>\n".to_string();
    for row in &filtered {
        let item = format!(
            "<blockquote>Name : {}\nContents : {}\nPrice : {}\nCapital : <tg-spoiler>{}</tg-spoiler>\nProfit : <tg-spoiler>{}</tg-spoiler></blockquote>\n",
            clean(cell(row, 0), false),
            clean(cell(row, 1), false),
            clean(cell(row, 2), false),
            clean(cell(row, 5), true),
            clean(cell(row, 6), true),
        );
        if current.len() + item.len() > 3800 {
            chunks.push(current.trim().to_string());
            current = item;
        } else {
            current.push_str(&item);
        }
    }
    if !current.trim().is_empty() {
        chunks.push(current.trim().to_string());
    }
    typing.abort();

    for chunk in chunks {
        let _ = app
            .tg
            .send_message(msg.chat_id, msg.thread, &chunk, Some("HTML"), None, Some(msg.id))
            .await;
    }
}
