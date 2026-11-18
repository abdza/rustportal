//! Audit trail writer. Requests never wait on an audit insert: rows are queued in
//! memory and written in batches by one background task, like the Grails portal's
//! PortalAuditWriter.
//!
//! - rows are written every FLUSH_INTERVAL, up to BATCH_SIZE rows per INSERT;
//! - a crash loses at most the last FLUSH_INTERVAL of rows (a normal shutdown
//!   writes out whatever is queued);
//! - if the database falls behind and the queue fills, new rows are dropped rather
//!   than slowing requests down, and the number dropped goes to the error log.

use crate::db::{ident, lit, Db, Row};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::mpsc;

const QUEUE_CAPACITY: usize = 20_000;
const BATCH_SIZE: usize = 500;
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);

static QUEUE: OnceLock<mpsc::Sender<Row>> = OnceLock::new();
static RECEIVER: tokio::sync::Mutex<Option<mpsc::Receiver<Row>>> = tokio::sync::Mutex::const_new(None);
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Queue an audit row (never blocks).
pub fn enqueue(row: Row) {
    match QUEUE.get() {
        Some(q) => {
            if q.try_send(row).is_err() {
                DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }
        None => {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Start the writer. Call once at startup.
pub async fn start(db: Db) {
    let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
    let _ = QUEUE.set(tx);
    *RECEIVER.lock().await = Some(rx);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(FLUSH_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            drain(&db).await;
        }
    });
}

/// Write everything queued so far. Also called on shutdown.
pub async fn drain(db: &Db) {
    let mut guard = RECEIVER.lock().await;
    let Some(rx) = guard.as_mut() else { return };
    loop {
        let mut batch = Vec::with_capacity(BATCH_SIZE);
        while batch.len() < BATCH_SIZE {
            match rx.try_recv() {
                Ok(r) => batch.push(r),
                Err(_) => break,
            }
        }
        if batch.is_empty() {
            break;
        }
        let n = batch.len();
        if let Err(e) = write(db, &batch).await {
            crate::web::log_error(db, "audit", "write", &format!("{} audit rows not written: {}", n, e), None, None, None).await;
        }
        if n < BATCH_SIZE {
            break;
        }
    }
    let lost = DROPPED.swap(0, Ordering::Relaxed);
    if lost > 0 {
        crate::web::log_error(db, "audit", "queue", &format!("Audit queue was full; dropped {} audit rows", lost), None, None, None).await;
    }
}

/// One INSERT for the whole batch (one commit, one flush to disk).
async fn write(db: &Db, rows: &[Row]) -> anyhow::Result<()> {
    let mut cols: Vec<String> = vec![];
    for r in rows {
        for k in r.keys() {
            if !cols.contains(k) {
                cols.push(k.clone());
            }
        }
    }
    let names = cols.iter().map(|c| ident(c)).collect::<anyhow::Result<Vec<_>>>()?.join(",");
    let values = rows
        .iter()
        .map(|r| format!("({})", cols.iter().map(|c| lit(r.get(c).unwrap_or(&serde_json::Value::Null))).collect::<Vec<_>>().join(",")))
        .collect::<Vec<_>>()
        .join(",");
    db.exec(&format!("insert into portal_audit_trail ({}) values {}", names, values), &Row::new()).await?;
    Ok(())
}
