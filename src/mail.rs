//! Outgoing e-mail. Every message is recorded in `portal_email` first, then
//! delivered over SMTP; failures stay queued for the scheduler to retry.

use crate::db::{i, s, Row};
use crate::row;
use crate::web::{now_str, St};
use anyhow::{anyhow, Result};
use lettre::message::{header::ContentType, Mailbox};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

fn boxes(list: &str) -> Vec<Mailbox> {
    list.split([',', ';']).map(|x| x.trim()).filter(|x| !x.is_empty()).filter_map(|x| x.parse::<Mailbox>().ok()).collect()
}

async fn deliver(st: &St, e: &Row) -> Result<()> {
    let cfg = &st.cfg.mail;
    if cfg.disabled {
        return Err(anyhow!("mail is disabled in the configuration"));
    }
    let tos = boxes(&s(e, "emailto"));
    if tos.is_empty() {
        return Err(anyhow!("no valid recipient"));
    }
    let mut b = Message::builder().from(cfg.from.parse::<Mailbox>().map_err(|e| anyhow!("bad from address: {}", e))?).subject(s(e, "title"));
    for t in tos {
        b = b.to(t);
    }
    for c in boxes(&s(e, "emailcc")) {
        b = b.cc(c);
    }
    let msg = b.header(ContentType::TEXT_HTML).body(s(e, "body"))?;
    let mut tb = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.host).port(cfg.port);
    if !cfg.username.is_empty() {
        tb = tb.credentials(lettre::transport::smtp::authentication::Credentials::new(cfg.username.clone(), cfg.password.clone()));
    }
    tb.build().send(msg).await?;
    Ok(())
}

pub async fn send_queued(st: &St, id: i64) -> Result<()> {
    let e = st.db.get("portal_email", id).await?.ok_or_else(|| anyhow!("email not found"))?;
    match deliver(st, &e).await {
        Ok(()) => {
            st.db.update("portal_email", id, &row! {"email_sent"=>true,"time_sent"=>now_str(),"error"=>None::<String>}).await?;
            Ok(())
        }
        Err(err) => {
            let _ = st.db.update("portal_email", id, &row! {"email_sent"=>false,"error"=>err.to_string()}).await;
            Err(err)
        }
    }
}

/// Record and send. Returns the portal_email id; delivery errors are logged, not raised.
pub async fn send(st: &St, to: &str, cc: &str, subject: &str, body: &str, module: &str) -> Result<i64> {
    if to.trim().is_empty() {
        return Err(anyhow!("send_email: no recipient"));
    }
    let id = st
        .db
        .insert("portal_email", &row! {"emailto"=>to,"emailcc"=>cc,"title"=>subject,"body"=>body,"module"=>module,"delivery_time"=>now_str(),"email_sent"=>false})
        .await?;
    if let Err(e) = send_queued(st, id).await {
        eprintln!("[mail] delivery of #{} failed: {}", id, e);
    }
    Ok(id)
}

/// Retry messages that are due and not sent yet (called by the scheduler).
pub async fn flush(st: &St) {
    let rows = st
        .db
        .rows("select id from portal_email where email_sent is not true and (delivery_time is null or delivery_time <= now()) and (delivery_time is null or delivery_time > now() - interval '2 days') order by id limit 50", &Row::new())
        .await
        .unwrap_or_default();
    for r in rows {
        if let Some(id) = i(&r, "id") {
            let _ = send_queued(st, id).await;
        }
    }
}
