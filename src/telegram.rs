//! Telegram Bot API integration (port of g6ikram's Telegram.groovy).
//!
//! - Account linking via /start {token} and unlinking via /stop in the long-polling daemon
//! - Outbound email mirroring via notify_emails() (called from mail::send and tracker emails)
//! - Messaging module (message/message_outbox + message/message_inbox):
//!     * queue() inserts Pending rows into message_outbox
//!     * sender daemon drains Pending rows every 20s
//!     * member replies land in message_inbox and are routed to the handler_slug runable page
//!       of the member's most recent handler-carrying outbox message sent within 48h

use crate::db::{self, b, i, s, Db, Row};
use crate::row;
use crate::script::Sc;
use crate::tracker::Tracker;
use crate::web::{self, now_str, St};
use serde_json::{json, Value};
use std::time::Duration;

const API_BASE: &str = "https://api.telegram.org/bot";
const MAX_LEN: usize = 4096;

pub async fn bot_token(db: &Db) -> String {
    web::setting(db, "telegram_bot_token").await.map(|v| db::vs(&v).trim().to_string()).unwrap_or_default()
}

pub async fn bot_name(db: &Db) -> String {
    web::setting(db, "telegram_bot_name").await.map(|v| db::vs(&v).trim().to_string()).unwrap_or_default()
}

pub async fn enabled(db: &Db) -> bool {
    !bot_token(db).await.is_empty()
}

pub fn esc(t: &str) -> String {
    t.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn strip_html(html: &str) -> String {
    use std::sync::OnceLock;
    static STYLE_SCRIPT: OnceLock<regex::Regex> = OnceLock::new();
    static BR: OnceLock<regex::Regex> = OnceLock::new();
    static BLOCK: OnceLock<regex::Regex> = OnceLock::new();
    static TAG: OnceLock<regex::Regex> = OnceLock::new();
    static WS: OnceLock<regex::Regex> = OnceLock::new();
    static NL_SP: OnceLock<regex::Regex> = OnceLock::new();
    static NL3: OnceLock<regex::Regex> = OnceLock::new();

    let s1 = STYLE_SCRIPT
        .get_or_init(|| regex::Regex::new(r"(?is)<(style|script)[^>]*>.*?</(?:style|script)>").unwrap())
        .replace_all(html, " ");
    let s2 = BR.get_or_init(|| regex::Regex::new(r"(?i)<br\s*/?>").unwrap()).replace_all(&s1, "\n");
    let s3 = BLOCK
        .get_or_init(|| regex::Regex::new(r"(?i)</(p|div|tr|li|h[1-6]|table)>").unwrap())
        .replace_all(&s2, "\n");
    let s4 = TAG.get_or_init(|| regex::Regex::new(r"<[^>]+>").unwrap()).replace_all(&s3, " ");
    let s5 = s4
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    let s6 = WS.get_or_init(|| regex::Regex::new(r"[ \t\r]+").unwrap()).replace_all(&s5, " ");
    let s7 = NL_SP.get_or_init(|| regex::Regex::new(r" *\n *").unwrap()).replace_all(&s6, "\n");
    NL3.get_or_init(|| regex::Regex::new(r"\n{3,}").unwrap()).replace_all(&s7, "\n\n").trim().to_string()
}

pub struct ApiResult {
    pub status: u16,
    pub body: Value,
}

async fn api_call(token: &str, method: &str, payload: &Value, read_timeout_secs: u64, attempts: usize) -> Result<ApiResult, String> {
    if token.is_empty() {
        return Err("empty token".into());
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(read_timeout_secs))
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{}{}/{}", API_BASE, token, method);
    let mut last_err = String::new();
    for attempt in 1..=attempts.max(1) {
        match client.post(&url).json(payload).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let raw = resp.text().await.unwrap_or_default();
                let body = serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null);
                return Ok(ApiResult { status, body });
            }
            Err(e) => {
                last_err = e.to_string();
                if attempt < attempts {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
    Err(last_err)
}

pub async fn unlink_chat(db: &Db, chat_id: &str) {
    let _ = db
        .exec(
            "update portal_user set telegram_chat_id=null, telegram_username=null, telegram_linked_at=null where telegram_chat_id=:c",
            &row! {"c"=>chat_id},
        )
        .await;
}

/// Send a Telegram message, returning (ok, message_id, error).
pub async fn send_message_full(db: &Db, chat_id: &str, text: &str, parse_mode: Option<&str>) -> (bool, Option<String>, Option<String>) {
    if chat_id.is_empty() || text.is_empty() {
        return (false, None, Some("no chat id or empty text".into()));
    }
    let token = bot_token(db).await;
    if token.is_empty() {
        return (false, None, Some("telegram_bot_token not configured".into()));
    }
    let clipped = if text.chars().count() > MAX_LEN {
        format!("{} ...", text.chars().take(MAX_LEN - 4).collect::<String>())
    } else {
        text.to_string()
    };
    let mut payload = json!({"chat_id": chat_id, "text": clipped});
    if let Some(pm) = parse_mode {
        if !pm.is_empty() {
            payload["parse_mode"] = json!(pm);
        }
    }
    let res = match api_call(&token, "sendMessage", &payload, 15, 3).await {
        Ok(r) => r,
        Err(e) => return (false, None, Some(e)),
    };
    if res.body["ok"].as_bool().unwrap_or(false) {
        let mid = res.body["result"]["message_id"].as_i64().map(|v| v.to_string());
        return (true, mid, None);
    }
    if res.status == 403 {
        eprintln!("[telegram] chat {} blocked the bot (403), unlinking", chat_id);
        unlink_chat(db, chat_id).await;
        return (false, None, Some("recipient blocked the bot (403) — unlinked".into()));
    }
    let desc = res.body["description"].as_str().unwrap_or("send failed");
    if parse_mode.is_some() && desc.to_ascii_lowercase().contains("parse") {
        let plain = json!({"chat_id": chat_id, "text": clipped});
        if let Ok(r2) = api_call(&token, "sendMessage", &plain, 15, 2).await {
            if r2.body["ok"].as_bool().unwrap_or(false) {
                let mid = r2.body["result"]["message_id"].as_i64().map(|v| v.to_string());
                return (true, mid, None);
            }
        }
    }
    eprintln!("[telegram] sendMessage failed ({}): {}", res.status, desc);
    (false, None, Some(format!("{}: {}", res.status, desc)))
}

pub async fn send_message(db: &Db, chat_id: &str, text: &str) -> bool {
    send_message_full(db, chat_id, text, Some("HTML")).await.0
}

pub async fn send_to_user(db: &Db, user_id: i64, text: &str) -> bool {
    let Ok(Some(u)) = db.get("portal_user", user_id).await else { return false };
    let chat = s(&u, "telegram_chat_id");
    if chat.is_empty() {
        return false;
    }
    send_message(db, &chat, text).await
}

/// Mirror an email notification to Telegram for linked, active users matching recipient emails.
pub async fn notify_emails(db: &Db, emails: &str, title: &str, html_body: &str) {
    if emails.trim().is_empty() || !enabled(db).await {
        return;
    }
    let mut list: Vec<String> = emails
        .split([',', ';'])
        .map(|x| x.trim().to_ascii_lowercase())
        .filter(|x| !x.is_empty())
        .collect();
    list.sort();
    list.dedup();
    if list.is_empty() {
        return;
    }
    let text = if title.is_empty() {
        esc(&strip_html(html_body))
    } else {
        format!("<b>{}</b>\n\n{}", esc(title), esc(&strip_html(html_body)))
    };
    for email in list {
        let users = db
            .rows(
                "select telegram_chat_id from portal_user where lower(email)=:e and is_active is true and telegram_chat_id is not null and telegram_chat_id <> ''",
                &row! {"e"=>email},
            )
            .await
            .unwrap_or_default();
        for u in users {
            let chat = s(&u, "telegram_chat_id");
            if !chat.is_empty() {
                let _ = send_message(db, &chat, &text).await;
            }
        }
    }
}

/// Queue a Telegram message in message/message_outbox. Returns the new outbox id.
pub async fn queue(st: &St, user_id: i64, title: Option<&str>, body: &str, opts: &Row) -> Option<i64> {
    if body.is_empty() {
        return None;
    }
    let tracker = Tracker::load(&st.db, "message", "message_outbox").await?;
    let mut datas = row! {
        "recipient_id"=>user_id,
        "body"=>body,
        "channel"=>"telegram",
        "record_status"=>"Pending"
    };
    if let Some(t) = title {
        if !t.is_empty() {
            datas.insert("title".into(), json!(t));
        }
    }
    for k in ["source_module", "source_ref", "handler_slug", "deliver_after"] {
        let v = s(opts, k);
        if !v.is_empty() {
            datas.insert(k.into(), json!(v));
        }
    }
    let new_id = tracker.save(&st.db, &datas).await.ok()?;
    let src = s(opts, "source_module");
    let trail_msg = if src.is_empty() { "Queued".to_string() } else { format!("Queued by {}", src) };
    let sc = Sc::system(st.clone());
    let _ = tracker.save_trail(&sc, new_id, &trail_msg, Some("Pending"), None).await;
    Some(new_id)
}

async fn drain_outbox(st: &St) {
    let Some(tracker) = Tracker::load(&st.db, "message", "message_outbox").await else { return };
    let dt = tracker.data_table();
    let rows = st
        .db
        .rows(
            &format!("select * from {} where record_status='Pending' and (deliver_after is null or deliver_after <= now()) order by id asc limit 50", dt),
            &db::no_params(),
        )
        .await
        .unwrap_or_default();
    let sc = Sc::system(st.clone());
    for d in rows {
        let Some(rid) = i(&d, "id") else { continue };
        let rec_uid = i(&d, "recipient_id");
        let user = match rec_uid {
            Some(uid) => st.db.get("portal_user", uid).await.ok().flatten(),
            None => None,
        };
        let mut upd = row! {"id"=>rid};
        let (status, trail_msg) = match user {
            None => {
                let err = "Recipient user not found".to_string();
                upd.insert("error_msg".into(), json!(err.clone()));
                ("Failed", format!("Failed: {}", err))
            }
            Some(u) if s(&u, "telegram_chat_id").is_empty() => {
                let err = "Recipient is not linked to Telegram".to_string();
                upd.insert("error_msg".into(), json!(err.clone()));
                ("Failed", format!("Failed: {}", err))
            }
            Some(u) => {
                let chat = s(&u, "telegram_chat_id");
                let title = s(&d, "title");
                let body = s(&d, "body");
                let text = if title.is_empty() {
                    esc(&body)
                } else {
                    format!("<b>{}</b>\n\n{}", esc(&title), esc(&body))
                };
                let (ok, mid, err) = send_message_full(&st.db, &chat, &text, Some("HTML")).await;
                if ok {
                    upd.insert("sent_time".into(), json!(now_str()));
                    if let Some(m) = mid {
                        upd.insert("telegram_message_id".into(), json!(m));
                    }
                    ("Sent", "Sent via Telegram".to_string())
                } else {
                    let e = err.unwrap_or_else(|| "send failed".into());
                    upd.insert("error_msg".into(), json!(e.clone()));
                    ("Failed", format!("Failed: {}", e))
                }
            }
        };
        upd.insert("record_status".into(), json!(status));
        let _ = tracker.save(&st.db, &upd).await;
        let _ = tracker.save_trail(&sc, rid, &trail_msg, Some(status), None).await;
    }
}

pub async fn handle_member_reply(st: &St, chat_id: &str, text: &str, username: Option<&str>) -> bool {
    if text.is_empty() {
        return false;
    }
    let Some(inbox) = Tracker::load(&st.db, "message", "message_inbox").await else { return false };
    let Ok(Some(user)) = st
        .db
        .first("select * from portal_user where telegram_chat_id=:c and is_active is true order by id limit 1", &row! {"c"=>chat_id})
        .await
    else {
        return false;
    };
    let uid = i(&user, "id").unwrap_or(0);

    // Find latest Sent outbox row with handler_slug within 48h
    let context = if let Some(outbox) = Tracker::load(&st.db, "message", "message_outbox").await {
        st.db
            .first(
                &format!(
                    "select * from {} where recipient_id=:u and record_status='Sent' and handler_slug is not null and handler_slug <> '' and sent_time >= now() - interval '48 hours' order by sent_time desc limit 1",
                    outbox.data_table()
                ),
                &row! {"u"=>uid},
            )
            .await
            .ok()
            .flatten()
    } else {
        None
    };

    let mut datas = row! {
        "sender_id"=>uid,
        "chat_id"=>chat_id,
        "body"=>text,
        "received_time"=>now_str(),
        "record_status"=>"New"
    };
    if let Some(un) = username {
        if !un.is_empty() {
            datas.insert("telegram_username".into(), json!(un));
        }
    }
    if let Some(ref ctx_row) = context {
        if let Some(oid) = i(ctx_row, "id") {
            datas.insert("outbox_id".into(), json!(oid));
        }
    }
    let sc = Sc::system(st.clone());
    let inbox_id = inbox.save(&st.db, &datas).await.ok();
    if let Some(iid) = inbox_id {
        let _ = inbox.save_trail(&sc, iid, "Received from Telegram", Some("New"), None).await;
    }

    let mut replied = false;
    if let Some(ctx_row) = context {
        let hmodule = {
            let m = s(&ctx_row, "source_module");
            if m.is_empty() { "message".to_string() } else { m }
        };
        let hslug = s(&ctx_row, "handler_slug");
        if let Ok(Some(page)) = st
            .db
            .first(&format!("select * from {} where module=:m and slug=:s limit 1", db::page_t()), &row! {"m"=>hmodule.clone(),"s"=>hslug.clone()})
            .await
        {
            if b(&page, "runable") {
                let vars = row! {
                    "member"=>Value::Object(user.clone()),
                    "text"=>text,
                    "inbox_id"=>inbox_id,
                    "outbox"=>Value::Object(ctx_row.clone())
                };
                match sc.eval(&s(&page, "content"), &vars) {
                    Ok(Value::String(reply)) if !reply.is_empty() => {
                        let ref_str = format!("reply:{}", inbox_id.unwrap_or(0));
                        let qopts = row! {"source_module"=>hmodule.clone(),"source_ref"=>ref_str};
                        if queue(st, uid, None, &reply, &qopts).await.is_some() {
                            if let Some(iid) = inbox_id {
                                let handled_by = format!("{}:{}", hmodule, hslug);
                                let _ = inbox.save(&st.db, &row! {"id"=>iid,"handled_by"=>handled_by.clone(),"record_status"=>"Handled"}).await;
                                let _ = inbox.save_trail(&sc, iid, &format!("Handled by {}", handled_by), Some("Handled"), None).await;
                            }
                            replied = true;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("[telegram] reply handler error ({}:{}): {}", hmodule, hslug, e);
                        web::log_error(&st.db, "telegram", "handleMemberReply", &e.to_string(), Some(&hmodule), Some(&hslug), None).await;
                    }
                }
            }
        }
    }
    if !replied {
        let _ = send_message(&st.db, chat_id, "Terima kasih! Mesej anda telah direkodkan dan akan disemak oleh admin.").await;
    }
    true
}

async fn handle_update(st: &St, upd: &Value) {
    let msg = &upd["message"];
    let Some(chat_id_num) = msg["chat"]["id"].as_i64() else { return };
    let chat_id = chat_id_num.to_string();
    let text = msg["text"].as_str().unwrap_or("").trim();
    let lower = text.to_ascii_lowercase();
    let username = msg["from"]["username"].as_str();

    if lower.starts_with("/start") {
        let linktoken = if text.len() > 6 { text[6..].trim() } else { "" };
        if linktoken.is_empty() {
            let _ = send_message(
                &st.db,
                &chat_id,
                "Salam! Untuk menerima notifikasi portal IKRAM Serdang, sila log masuk ke portal dan gunakan butang <b>Sambung Telegram</b> di halaman Telegram.",
            )
            .await;
            return;
        }
        let found = st
            .db
            .first(
                "select id, userid, name from portal_user where telegram_link_token=:t and telegram_token_expiry > now() limit 1",
                &row! {"t"=>linktoken},
            )
            .await
            .ok()
            .flatten();
        if let Some(u) = found {
            let uid = i(&u, "id").unwrap_or(0);
            let _ = st
                .db
                .exec(
                    "update portal_user set telegram_chat_id=null, telegram_username=null, telegram_linked_at=null where telegram_chat_id=:c and id<>:id",
                    &row! {"c"=>chat_id.clone(),"id"=>uid},
                )
                .await;
            let _ = st
                .db
                .exec(
                    "update portal_user set telegram_chat_id=:c, telegram_username=:un, telegram_linked_at=now(), telegram_link_token=null, telegram_token_expiry=null where id=:id",
                    &row! {"c"=>chat_id.clone(),"un"=>username,"id"=>uid},
                )
                .await;
            eprintln!("[telegram] linked chat {} to user {}", chat_id, s(&u, "userid"));
            let _ = send_message(
                &st.db,
                &chat_id,
                &format!(
                    "Salam <b>{}</b>! Akaun portal anda telah berjaya disambungkan. Notifikasi portal akan dihantar ke sini. Hantar /stop untuk berhenti.",
                    esc(&s(&u, "name"))
                ),
            )
            .await;
        } else {
            let _ = send_message(&st.db, &chat_id, "Maaf, pautan ini tidak sah atau telah tamat tempoh. Sila jana pautan baru dari halaman Telegram di portal.").await;
        }
    } else if lower.starts_with("/stop") {
        let n = st
            .db
            .count("select count(*) from portal_user where telegram_chat_id=:c", &row! {"c"=>chat_id.clone()})
            .await
            .unwrap_or(0);
        if n > 0 {
            unlink_chat(&st.db, &chat_id).await;
            let _ = send_message(
                &st.db,
                &chat_id,
                "Akaun anda telah diputuskan dari Telegram. Anda boleh menyambung semula dari halaman Telegram di portal bila-bila masa.",
            )
            .await;
        } else {
            let _ = send_message(&st.db, &chat_id, "Tiada akaun portal yang berkaitan dengan Telegram ini.").await;
        }
    } else if !handle_member_reply(st, &chat_id, text, username).await {
        let _ = send_message(
            &st.db,
            &chat_id,
            "Bot ini hanya menghantar notifikasi dari portal IKRAM Serdang. Hantar /stop untuk berhenti menerima notifikasi.",
        )
        .await;
    }
}

/// Spawn the Telegram long-poller and outbox sender background tasks when enabled.
pub fn spawn(st: St, run_telegram: bool) {
    if !run_telegram {
        return;
    }
    let st_poll = st.clone();
    tokio::spawn(async move {
        let mut offset: i64 = 0;
        let mut connected = false;
        loop {
            let token = bot_token(&st_poll.db).await;
            if token.is_empty() {
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }
            let payload = json!({"offset": offset, "timeout": 50, "allowed_updates": ["message"]});
            match api_call(&token, "getUpdates", &payload, 65, 3).await {
                Ok(res) if res.body["ok"].as_bool().unwrap_or(false) => {
                    if !connected {
                        connected = true;
                        let bn = bot_name(&st_poll.db).await;
                        eprintln!("[telegram] poller connected as @{}", if bn.is_empty() { "?" } else { &bn });
                    }
                    if let Some(arr) = res.body["result"].as_array() {
                        for upd in arr {
                            if let Some(uid) = upd["update_id"].as_i64() {
                                offset = offset.max(uid + 1);
                            }
                            handle_update(&st_poll, upd).await;
                        }
                    }
                }
                Ok(res) if res.status == 409 => {
                    eprintln!("[telegram] poller 409 conflict — another poller is active, backing off 5 minutes");
                    tokio::time::sleep(Duration::from_secs(300)).await;
                }
                Ok(res) => {
                    eprintln!("[telegram] poller unexpected response ({}): {}", res.status, res.body["description"].as_str().unwrap_or(""));
                    tokio::time::sleep(Duration::from_secs(15)).await;
                }
                Err(e) => {
                    eprintln!("[telegram] poller error: {}", e);
                    tokio::time::sleep(Duration::from_secs(15)).await;
                }
            }
        }
    });

    tokio::spawn(async move {
        loop {
            let sleep_secs = if enabled(&st.db).await {
                drain_outbox(&st).await;
                20
            } else {
                60
            };
            tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
        }
    });
}
