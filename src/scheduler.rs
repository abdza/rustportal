//! Scheduled runable pages + `/portalScheduler/*` admin screens (matching g6ikram's
//! `PortalScheduler`, `PortalSchedulerRun`, `PortalSchedulerController`, and
//! `views/portalScheduler/*.gsp`).

use crate::db::{self, b, csv, i, lit, s, Row};
use crate::row;
use crate::script::Sc;
use crate::web::{self, enc, esc, now_str, Ctx, St};
use axum::http::StatusCode;
use axum::response::Response;
use chrono::{Datelike, Duration, Local, NaiveDateTime, Timelike};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

const MISSED_LOOKBACK_HOURS: i64 = 48;
const STUCK_AFTER_HOURS: i64 = 6;
const MAX_ERROR: usize = 4000;
const MAX_RESULT: usize = 1000;
const SEVERITY: &[&str] = &["Failed", "Page missing", "Interrupted", "Missed", "Running", "Success"];

pub fn sched_hour(r: &Row) -> String {
    let h = s(r, "hour_of_day");
    if !h.trim().is_empty() {
        h
    } else {
        s(r, "hour")
    }
}

/// Match g6ikram's `PortalScheduler.fieldMatches`:
/// null/blank never matches, '*' matches anything, else comma-separated exact integers
/// (also tolerates legacy `HH:MM` hour tokens by taking the hour part).
pub fn field_matches(spec: &str, val: u32) -> bool {
    let sp = spec.trim();
    if sp.is_empty() {
        return false;
    }
    if sp == "*" {
        return true;
    }
    sp.split(',').any(|tok| {
        let t = tok.trim();
        let num = t.split_once(':').map(|(a, _)| a.trim()).unwrap_or(t);
        num.parse::<u32>().ok() == Some(val)
    })
}

/// Exact match with g6ikram's `PortalScheduler.isDue(Date)`:
/// `day_of_week` uses 0 = Sunday .. 6 = Saturday (`Calendar.DAY_OF_WEEK - 1`).
pub fn is_due(sch: &Row, when: NaiveDateTime) -> bool {
    if !b(sch, "enabled") {
        return false;
    }
    let dow = when.weekday().num_days_from_sunday();
    field_matches(&sched_hour(sch), when.hour())
        && field_matches(&s(sch, "day_of_week"), dow)
        && field_matches(&s(sch, "day_of_month"), when.day())
}

pub fn truncate_to_hour(dt: NaiveDateTime) -> NaiveDateTime {
    dt.date().and_hms_opt(dt.hour(), 0, 0).unwrap_or(dt)
}

/// Hour slots in `[from, to)` at which this schedule was due, oldest first.
pub fn due_slots(sch: &Row, from: NaiveDateTime, to: NaiveDateTime) -> Vec<NaiveDateTime> {
    if !b(sch, "enabled") || !(from < to) {
        return vec![];
    }
    let mut cur = truncate_to_hour(from);
    if cur < from {
        cur += Duration::hours(1);
    }
    let mut out = vec![];
    while cur < to {
        if is_due(sch, cur) {
            out.push(cur);
        }
        cur += Duration::hours(1);
    }
    out
}

/// Next hour slot at or after `from` this schedule is due, looking up to 62 days ahead.
pub fn next_due(sch: &Row, from: NaiveDateTime) -> Option<NaiveDateTime> {
    if !b(sch, "enabled") {
        return None;
    }
    let mut cur = truncate_to_hour(from);
    if cur < from {
        cur += Duration::hours(1);
    }
    for _ in 0..(24 * 62) {
        if is_due(sch, cur) {
            return Some(cur);
        }
        cur += Duration::hours(1);
    }
    None
}

pub fn worst_status<'a>(statuses: &[&'a str]) -> Option<&'a str> {
    for sev in SEVERITY {
        if statuses.iter().any(|s| s == sev) {
            return Some(*sev);
        }
    }
    statuses.first().copied()
}

pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{} ...[truncated]", head)
    }
}

/// Match g6ikram's `PortalSchedulerRun.summarise`: strip scripts/styles/HTML tags & collapse whitespace.
pub fn summarise_result(content: &str) -> Option<String> {
    let mut s = content.to_string();
    for tag in ["script", "style"] {
        loop {
            let low = s.to_ascii_lowercase();
            let open = format!("<{}", tag);
            let close = format!("</{}>", tag);
            let Some(start) = low.find(&open) else { break };
            if let Some(end_rel) = low[start..].find(&close) {
                s.replace_range(start..(start + end_rel + close.len()), " ");
            } else {
                s.truncate(start);
                break;
            }
        }
    }
    let mut plain = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => {
                in_tag = true;
                plain.push(' ');
            }
            '>' => in_tag = false,
            _ if !in_tag => plain.push(ch),
            _ => {}
        }
    }
    let plain = plain.replace("&nbsp;", " ");
    let collapsed = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        None
    } else {
        Some(clip(&collapsed, MAX_RESULT))
    }
}

fn hostname() -> Option<String> {
    if let Ok(h) = std::fs::read_to_string("/etc/hostname") {
        let t = h.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    std::env::var("HOSTNAME").ok().filter(|h| !h.trim().is_empty())
}

fn fmt_dt_full(dt: NaiveDateTime) -> String {
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn fmt_dt_str(raw: &str, pat: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return String::new();
    }
    if let Some(dt) = crate::script::parse_dt(t) {
        return dt.format(pat).to_string();
    }
    let norm = t.replace('T', " ");
    let base = norm.split('.').next().unwrap_or(&norm).trim_end_matches('Z');
    let base = if base.len() > 19 { &base[..19] } else { base };
    match crate::script::parse_dt(base) {
        Some(dt) => dt.format(pat).to_string(),
        None => t.to_string(),
    }
}

fn parse_dt_any(raw: &str) -> Option<NaiveDateTime> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(dt) = crate::script::parse_dt(t) {
        return Some(dt);
    }
    let norm = t.replace('T', " ");
    let base = norm.split('.').next().unwrap_or(&norm).trim_end_matches('Z');
    let base = if base.len() > 19 { &base[..19] } else { base };
    crate::script::parse_dt(base)
}

#[derive(Default)]
struct TickCounts {
    due: usize,
    succeeded: usize,
    failed: usize,
    missing: usize,
    missed: usize,
    caughtup: usize,
}

async fn log_run(st: &St, props: &Row) -> Option<i64> {
    match st.db.insert("portal_scheduler_run", props).await {
        Ok(id) => Some(id),
        Err(e) => {
            eprintln!("[scheduler] could not write run log: {}", e);
            None
        }
    }
}

async fn close_run(st: &St, run_id: Option<i64>, started_at: std::time::Instant, status: &str, error: Option<&str>, content: Option<&str>) {
    let Some(id) = run_id else { return };
    let err_v = error.map(|e| json!(clip(e, MAX_ERROR))).unwrap_or(Value::Null);
    let res_v = content.and_then(summarise_result).map(|x| json!(x)).unwrap_or(Value::Null);
    let _ = st
        .db
        .update(
            "portal_scheduler_run",
            id,
            &row! {
                "status" => status,
                "finished" => now_str(),
                "duration_ms" => started_at.elapsed().as_millis() as i64,
                "error" => err_v,
                "result" => res_v,
            },
        )
        .await;
}

async fn run_job(
    st: &St,
    sch: &Row,
    script: &str,
    due_slot: NaiveDateTime,
    tick_id: Option<i64>,
    triggered_by: &str,
    host: Option<&str>,
    counts: &mut TickCounts,
    problems: &mut Vec<String>,
) {
    let default_mod = s(sch, "module");
    let (m, sl) = match script.split_once(':') {
        Some((a, b2)) => (a.trim().to_string(), b2.trim().to_string()),
        None => (default_mod, script.trim().to_string()),
    };
    let sname = s(sch, "name");
    let sid = i(sch, "id");
    let started_inst = std::time::Instant::now();
    let job_id = log_run(
        st,
        &row! {
            "run_type" => "Job",
            "status" => "Running",
            "scheduler_id" => sid,
            "tick_id" => tick_id,
            "name" => sname.clone(),
            "module" => m.clone(),
            "slug" => sl.clone(),
            "due_slot" => fmt_dt_full(due_slot),
            "started" => now_str(),
            "triggered_by" => triggered_by,
            "host" => host,
        },
    )
    .await;

    let page_exists = st
        .db
        .first(
            &format!("select id from {} where module=:m and slug=:s limit 1", db::page_t()),
            &row! {"m"=>m.clone(),"s"=>sl.clone()},
        )
        .await
        .ok()
        .flatten()
        .is_some();
    if !page_exists {
        counts.missing += 1;
        let msg = format!("No page '{}' in module '{}'", sl, m);
        close_run(st, job_id, started_inst, "Page missing", Some(&msg), None).await;
        problems.push(format!("{} - {}: {}", sname, sl, msg));
        return;
    }

    let sc = Sc::system(st.clone());
    match crate::script::run_page(&sc.inner, &m, &sl, row! {"scheduled_run"=>true,"scheduled"=>true}).await {
        Ok(v) => {
            counts.succeeded += 1;
            let content = match &v {
                Value::String(x) => x.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            close_run(st, job_id, started_inst, "Success", None, Some(&content)).await;
            if let Some(id) = sid {
                // Only update lastrun; leave last_updated untouched (bounds missed-run check)
                let _ = st.db.exec("update portal_scheduler set lastrun=:d where id=:id", &row! {"d"=>now_str(),"id"=>id}).await;
            }
        }
        Err(e) => {
            counts.failed += 1;
            let desc = e.to_string();
            eprintln!("[scheduler] error running {}-{}: {}", sname, sl, desc);
            close_run(st, job_id, started_inst, "Failed", Some(&desc), None).await;
            problems.push(format!("{} - {}: FAILED\n{}", sname, sl, desc));
            crate::web::log_error(&st.db, "portalScheduler", "run", &desc, Some(&m), Some(&sl), None).await;
        }
    }
}

async fn find_missed(
    st: &St,
    schedules: &[Row],
    slot: NaiveDateTime,
    now: NaiveDateTime,
    host: Option<&str>,
    tick_id: Option<i64>,
    counts: &mut TickCounts,
    problems: &mut Vec<String>,
) -> Vec<(Row, NaiveDateTime)> {
    let mut catchups: Vec<(Row, NaiveDateTime)> = vec![];
    let first_tick = st
        .db
        .first("select min(due_slot) as m from portal_scheduler_run where run_type='Tick'", &db::no_params())
        .await
        .ok()
        .flatten()
        .and_then(|r| parse_dt_any(&s(&r, "m")));
    let Some(first_tick) = first_tick else { return catchups };
    let mut from = slot - Duration::hours(MISSED_LOOKBACK_HOURS);
    if first_tick > from {
        from = first_tick;
    }
    if !(from < slot) {
        return catchups;
    }
    let from_s = fmt_dt_full(from);
    let to_s = fmt_dt_full(slot);

    let acc_rows = st
        .db
        .rows(
            "select scheduler_id, due_slot from portal_scheduler_run where run_type in ('Job','Missed') and due_slot >= :from and due_slot < :to",
            &row! {"from"=>from_s.clone(),"to"=>to_s.clone()},
        )
        .await
        .unwrap_or_default();
    let mut accounted: HashSet<(i64, String)> = HashSet::new();
    for r in &acc_rows {
        if let (Some(sid), Some(ds)) = (i(r, "scheduler_id"), parse_dt_any(&s(r, "due_slot"))) {
            accounted.insert((sid, fmt_dt_full(truncate_to_hour(ds))));
        }
    }

    let tick_rows = st
        .db
        .rows(
            "select distinct due_slot from portal_scheduler_run where run_type='Tick' and due_slot >= :from and due_slot < :to",
            &row! {"from"=>from_s,"to"=>to_s},
        )
        .await
        .unwrap_or_default();
    let mut ticked: HashSet<String> = HashSet::new();
    for r in &tick_rows {
        if let Some(ds) = parse_dt_any(&s(r, "due_slot")) {
            ticked.insert(fmt_dt_full(truncate_to_hour(ds)));
        }
    }

    let today = now.date();
    let mut catchup_ids: HashSet<i64> = HashSet::new();
    for sch in schedules {
        let Some(sid) = i(sch, "id") else { continue };
        let sfrom = match parse_dt_any(&s(sch, "last_updated")) {
            Some(lu) if lu > from => lu,
            _ => from,
        };
        let ranslot = parse_dt_any(&s(sch, "lastrun")).map(|lr| fmt_dt_full(truncate_to_hour(lr)));
        for ds in due_slots(sch, sfrom, slot) {
            let ds_key = fmt_dt_full(ds);
            if ranslot.as_deref() == Some(ds_key.as_str()) || accounted.contains(&(sid, ds_key.clone())) {
                continue;
            }
            let why = if ticked.contains(&ds_key) {
                "The scheduler ran in that hour but did not start this job"
            } else {
                "The scheduler was not called in that hour - check the cron that calls /portalScheduler/run"
            };
            let now_s = fmt_dt_full(now);
            let _ = log_run(
                st,
                &row! {
                    "run_type" => "Missed",
                    "status" => "Missed",
                    "scheduler_id" => sid,
                    "tick_id" => tick_id,
                    "name" => s(sch, "name"),
                    "module" => s(sch, "module"),
                    "slug" => s(sch, "slugs"),
                    "due_slot" => ds_key,
                    "started" => now_s.clone(),
                    "finished" => now_s,
                    "duration_ms" => 0i64,
                    "error" => why,
                    "host" => host,
                },
            )
            .await;
            counts.missed += 1;
            let do_catchup = b(sch, "catch_up") && !is_due(sch, now) && ds.date() == today;
            if do_catchup && catchup_ids.insert(sid) {
                catchups.push((sch.clone(), ds));
            }
            problems.push(format!(
                "{}: missed the {} run. {}{}",
                s(sch, "name"),
                ds.format("%Y-%m-%d %H:00"),
                why,
                if do_catchup { " Catching it up now." } else { "" }
            ));
        }
    }
    catchups
}

async fn housekeeping(st: &St, now: NaiveDateTime) {
    let stuck_cutoff = fmt_dt_full(now - Duration::hours(STUCK_AFTER_HOURS));
    let why = format!(
        "Still marked Running after {} hours - the server probably restarted or the thread died mid-run",
        STUCK_AFTER_HOURS
    );
    let _ = st
        .db
        .exec(
            "update portal_scheduler_run set status='Interrupted', error=:why where status='Running' and started < :cutoff",
            &row! {"why"=>why,"cutoff"=>stuck_cutoff},
        )
        .await;
    let keep_days: i64 = web::setting_str(&st.db, "portal.scheduler_log_retention_days")
        .await
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|d| *d > 0)
        .unwrap_or(180);
    let prune_cutoff = fmt_dt_full(now - Duration::days(keep_days));
    let _ = st
        .db
        .exec("delete from portal_scheduler_run where started < :cutoff", &row! {"cutoff"=>prune_cutoff})
        .await;
}

/// Run a full hourly scheduler tick (matching g6ikram's `PortalSchedulerController.run()`):
/// logs a Tick row, runs due schedules, records missed slots & same-day catch-ups,
/// runs housekeeping, alerts `emailpagerror` on problems, and flushes queued emails.
pub async fn run_tick(st: &St, triggered_by: &str) -> (String, Vec<String>) {
    let now = Local::now().naive_local();
    let slot = truncate_to_hour(now);
    let host = hostname();
    let tick_inst = std::time::Instant::now();
    let tick_id = log_run(
        st,
        &row! {
            "run_type" => "Tick",
            "status" => "Running",
            "due_slot" => fmt_dt_full(slot),
            "started" => fmt_dt_full(now),
            "triggered_by" => triggered_by,
            "host" => host.clone(),
        },
    )
    .await;

    let mut counts = TickCounts::default();
    let mut problems: Vec<String> = vec![];
    let schedules = st.db.rows("select * from portal_scheduler where enabled is true order by id", &db::no_params()).await.unwrap_or_default();

    for sch in schedules.iter().filter(|x| is_due(x, now)) {
        for script in csv(&s(sch, "slugs")) {
            counts.due += 1;
            run_job(st, sch, &script, slot, tick_id, triggered_by, host.as_deref(), &mut counts, &mut problems).await;
        }
    }

    let catchups = find_missed(st, &schedules, slot, now, host.as_deref(), tick_id, &mut counts, &mut problems).await;
    let cu_by = format!("{} (catch-up)", triggered_by);
    for (sch, ds) in catchups {
        for script in csv(&s(&sch, "slugs")) {
            counts.caughtup += 1;
            run_job(st, &sch, &script, ds, tick_id, &cu_by, host.as_deref(), &mut counts, &mut problems).await;
        }
    }

    housekeeping(st, now).await;

    let summary = format!(
        "Due: {}, succeeded: {}, failed: {}, page missing: {}, missed runs found: {}, caught up: {}",
        counts.due, counts.succeeded, counts.failed, counts.missing, counts.missed, counts.caughtup
    );
    let tick_status = if problems.is_empty() { "Success" } else { "Problems" };
    let err_joined = if problems.is_empty() { None } else { Some(problems.join("\n\n")) };
    close_run(st, tick_id, tick_inst, tick_status, err_joined.as_deref(), Some(&summary)).await;

    if !problems.is_empty() {
        if let Some(to) = web::setting_str(&st.db, "emailpagerror").await.filter(|x| !x.trim().is_empty()) {
            let subj = format!(
                "Scheduler problems at {}{}",
                now.format("%Y-%m-%d %H:%M"),
                host.as_deref().map(|h| format!(" on {}", h)).unwrap_or_default()
            );
            let body = format!("<pre>{}\n\n{}</pre>", esc(&summary), esc(&problems.join("\n\n")));
            let _ = crate::mail::send(st, to.trim(), "", &subj, &body, "portal").await;
        }
    }

    crate::mail::flush(st).await;
    (summary, problems)
}

/// Run every page of one schedule immediately (manual "Run Now" button on `show`).
pub async fn run_scheduler(st: &St, sch: &Row, triggered_by: &str) {
    let now = Local::now().naive_local();
    let slot = truncate_to_hour(now);
    let host = hostname();
    let mut counts = TickCounts::default();
    let mut problems = vec![];
    for script in csv(&s(sch, "slugs")) {
        run_job(st, sch, &script, slot, None, triggered_by, host.as_deref(), &mut counts, &mut problems).await;
    }
}

pub fn spawn(st: St, run_jobs: bool) {
    let st_sess = st.clone();
    tokio::spawn(async move {
        loop {
            let ttl_mins = crate::web::session_timeout_minutes(&st_sess.db, &st_sess.cfg).await;
            crate::web::prune_sessions(&st_sess.db, ttl_mins).await;
            let wait_mins = ttl_mins.clamp(5, 30) as u64;
            tokio::time::sleep(std::time::Duration::from_secs(wait_mins * 60)).await;
        }
    });
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let st2 = st.clone();
            let _ = tokio::spawn(async move {
                if !run_jobs {
                    return;
                }
                // Run one unified hourly Tick per hour slot (matching g6ikram's hourly cron),
                // and flush queued e-mails every minute.
                let slot = fmt_dt_full(truncate_to_hour(Local::now().naive_local()));
                let already = st2
                    .db
                    .first("select id from portal_scheduler_run where run_type='Tick' and due_slot=:s limit 1", &row! {"s"=>slot})
                    .await
                    .ok()
                    .flatten()
                    .is_some();
                if !already {
                    let _ = run_tick(&st2, "cron").await;
                } else {
                    crate::mail::flush(&st2).await;
                }
            })
            .await;
        }
    });
}

// ---------------------------------------------------------------------------
// /portalScheduler/* web controller (matches g6ikram PortalSchedulerController)
// ---------------------------------------------------------------------------

fn page(nav: &str, kind: &str, title: &str, flash: &str, body: &str) -> String {
    let msg = if flash.is_empty() {
        String::new()
    } else {
        format!("                    <div class=\"message\" role=\"status\">{}</div>\n", esc(flash))
    };
    format!(
        "\n    <div id=\"content\" role=\"main\">\n        <div class=\"container\">\n            <section class=\"row\">\n                <div class=\"nav\" role=\"navigation\">\n                    <ul>\n                        <li><a class=\"home\" href=\"/\">Home</a></li>\n{nav}                    </ul>\n                </div>\n            </section>\n            <section class=\"row\">\n                <div id=\"{kind}-portalScheduler\" class=\"col-12 content scaffold-{kind}\" role=\"main\">\n                    <h1>{}</h1>\n{msg}{body}\n                </div>\n            </section>\n        </div>\n    </div>\n",
        esc(title)
    )
}

fn li(class: &str, href: &str, label: &str) -> String {
    format!("                        <li><a class=\"{class}\" href=\"{href}\">{}</a></li>\n", esc(label))
}

fn fmt_bool(v: Option<&Value>) -> &'static str {
    match v {
        Some(Value::Bool(true)) => "True",
        Some(Value::Bool(false)) => "False",
        Some(Value::Number(n)) if n.as_i64() == Some(1) => "True",
        Some(Value::Number(n)) if n.as_i64() == Some(0) => "False",
        Some(Value::String(x)) if x.eq_ignore_ascii_case("true") || x == "1" => "True",
        Some(Value::String(x)) if x.eq_ignore_ascii_case("false") || x == "0" => "False",
        _ => "",
    }
}

fn fmt_dur(ms: Option<i64>) -> String {
    match ms {
        Some(v) if v < 1000 => format!("{} ms", v),
        Some(v) => format!("{:.1} s", (v as f64) / 1000.0),
        None => String::new(),
    }
}

fn status_class(st: &str) -> &'static str {
    match st {
        "Success" | "COMPLETED" => "sched-ok",
        "Running" => "sched-warn",
        _ => "sched-bad",
    }
}

async fn admin_mods(ctx: &Ctx) -> Vec<String> {
    let Some(u) = &ctx.user else { return vec![] };
    if ctx.is_admin() { vec![] } else { web::admin_list(ctx.db(), u).await }
}

async fn dev_mods(ctx: &Ctx) -> Vec<String> {
    let Some(u) = &ctx.user else { return vec![] };
    if ctx.is_admin() { vec![] } else { web::developer_list(ctx.db(), u).await }
}

async fn can_view_sched(ctx: &Ctx, sch: &Row) -> bool {
    if ctx.is_admin() {
        return true;
    }
    let m = s(sch, "module");
    !m.is_empty() && admin_mods(ctx).await.contains(&m)
}

async fn can_edit_sched(ctx: &Ctx, sch: &Row) -> bool {
    if ctx.is_admin() {
        return true;
    }
    let m = s(sch, "module");
    !m.is_empty() && dev_mods(ctx).await.contains(&m)
}

pub async fn controller(ctx: Ctx, action: &str, id: Option<i64>) -> Response {
    // /portalScheduler/run (without id) is whitelisted for external hourly cron callers,
    // matching g6ikram's SecurityInterceptor. All other actions require login.
    if action == "run" && id.is_none() {
        return run_tick_action(ctx).await;
    }
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let rid = id.unwrap_or(0);
    match action {
        "" | "index" | "list" => index_action(ctx).await,
        "status" => status_action(ctx).await,
        "runs" => runs_action(ctx, id).await,
        "run" => run_one_action(ctx, rid).await,
        "show" => show_action(ctx, rid).await,
        "create" => create_action(ctx).await,
        "save" => save_action(ctx).await,
        "edit" => edit_action(ctx, rid).await,
        "update" => update_action(ctx, rid).await,
        "delete" => delete_action(ctx, rid).await,
        _ => index_action(ctx).await,
    }
}

async fn run_tick_action(ctx: Ctx) -> Response {
    // If logged in and not admin, disallow manual global tick trigger
    if let Some(_) = &ctx.user {
        if !ctx.is_admin() {
            return ctx.status(StatusCode::FORBIDDEN, "Superuser access required").await;
        }
    }
    let triggered_by = match &ctx.user {
        Some(u) => {
            let uid = s(u, "userid");
            if uid.is_empty() {
                format!("manual: {}", i(u, "id").unwrap_or(0))
            } else {
                format!("manual: {}", uid)
            }
        }
        None => "cron".to_string(),
    };
    let (summary, problems) = run_tick(&ctx.st, &triggered_by).await;
    let mut body = format!("<p>{}</p>", esc(&summary));
    // Matching run.gsp: only show error details & status link to logged-in users
    if ctx.user.is_some() {
        if !problems.is_empty() {
            body.push_str(&format!("<pre style=\"white-space:pre-wrap;\">{}</pre>", esc(&problems.join("\n\n"))));
        }
        body.push_str("<p><a href=\"/portalScheduler/status\">Scheduler status</a></p>");
    }
    let nav = if ctx.user.is_some() {
        format!("{}{}", li("list", "/portalScheduler/index", "Scheduler List"), li("list", "/portalScheduler/status", "Status"))
    } else {
        String::new()
    };
    let h = page(&nav, "list", "Running Scheduler", "", &body);
    ctx.html("PortalScheduler List", h).await
}

async fn run_one_action(mut ctx: Ctx, id: i64) -> Response {
    if ctx.method != "POST" {
        return ctx.status(StatusCode::METHOD_NOT_ALLOWED, "POST required").await;
    }
    let Ok(Some(sch)) = ctx.db().get("portal_scheduler", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "PortalScheduler not found").await;
    };
    if !can_edit_sched(&ctx, &sch).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let by = format!(
        "manual: {}",
        ctx.user.as_ref().map(|u| s(u, "userid")).unwrap_or_default()
    );
    run_scheduler(&ctx.st, &sch, &by).await;
    ctx.flash("Scheduler run finished; see Runs for the result");
    ctx.redirect(&format!("/portalScheduler/runs/{}", id)).await
}

async fn status_action(ctx: Ctx) -> Response {
    let db = ctx.db();
    let is_admin = ctx.is_admin();
    let my_mods = admin_mods(&ctx).await;
    if !is_admin && my_mods.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view scheduler status").await;
    }
    let now = Local::now().naive_local();
    let mut schedules = db.rows("select * from portal_scheduler order by \"module\", name", &db::no_params()).await.unwrap_or_default();
    if !is_admin {
        schedules.retain(|r| my_mods.contains(&s(r, "module")));
    }
    let ids: Vec<i64> = schedules.iter().filter_map(|r| i(r, "id")).collect();

    let mut stats: HashMap<i64, BTreeMap<String, i64>> = HashMap::new();
    let mut lastjob: HashMap<i64, Vec<Row>> = HashMap::new();
    let mut lastok: HashMap<i64, String> = HashMap::new();
    let mut recent: Vec<Row> = vec![];

    if !ids.is_empty() {
        let id_csv = ids.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
        let since7 = fmt_dt_full(now - Duration::days(7));

        for r in db
            .rows(
                &format!(
                    "select scheduler_id, status, count(*) as cnt from portal_scheduler_run where run_type in ('Job','Missed') and started >= :since and scheduler_id in ({id_csv}) group by scheduler_id, status"
                ),
                &row! {"since"=>since7},
            )
            .await
            .unwrap_or_default()
        {
            if let Some(sid) = i(&r, "scheduler_id") {
                *stats.entry(sid).or_default().entry(s(&r, "status")).or_insert(0) += i(&r, "cnt").unwrap_or(0);
            }
        }

        let latest_rows = db
            .rows(
                &format!(
                    "select * from portal_scheduler_run where id in (select max(id) from portal_scheduler_run where run_type in ('Job','Missed') and scheduler_id in ({id_csv}) group by scheduler_id)"
                ),
                &db::no_params(),
            )
            .await
            .unwrap_or_default();
        let mut tick_ids: Vec<i64> = vec![];
        for r in latest_rows {
            if let Some(sid) = i(&r, "scheduler_id") {
                if s(&r, "run_type") == "Job" {
                    if let Some(tid) = i(&r, "tick_id") {
                        tick_ids.push(tid);
                    }
                }
                lastjob.insert(sid, vec![r]);
            }
        }
        tick_ids.sort();
        tick_ids.dedup();
        if !tick_ids.is_empty() {
            let tid_csv = tick_ids.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
            let mut by_sched: HashMap<i64, Vec<Row>> = HashMap::new();
            for r in db
                .rows(
                    &format!(
                        "select * from portal_scheduler_run where run_type='Job' and tick_id in ({tid_csv}) and scheduler_id in ({id_csv}) order by id"
                    ),
                    &db::no_params(),
                )
                .await
                .unwrap_or_default()
            {
                if let Some(sid) = i(&r, "scheduler_id") {
                    by_sched.entry(sid).or_default().push(r);
                }
            }
            for (sid, rs) in by_sched {
                if let Some(cur) = lastjob.get(&sid) {
                    if !cur.is_empty() && !rs.is_empty() && i(&cur[0], "tick_id") == i(&rs[0], "tick_id") {
                        lastjob.insert(sid, rs);
                    }
                }
            }
        }

        for r in db
            .rows(
                &format!(
                    "select scheduler_id, max(finished) as mf from portal_scheduler_run where run_type='Job' and status in ('Success','COMPLETED') and scheduler_id in ({id_csv}) group by scheduler_id"
                ),
                &db::no_params(),
            )
            .await
            .unwrap_or_default()
        {
            if let Some(sid) = i(&r, "scheduler_id") {
                let mf = s(&r, "mf");
                if !mf.is_empty() {
                    lastok.insert(sid, mf);
                }
            }
        }

        recent = db
            .rows(
                &format!(
                    "select * from portal_scheduler_run where run_type in ('Job','Missed') and scheduler_id in ({id_csv}) and status not in ('Success','COMPLETED','Running') order by id desc limit 30"
                ),
                &db::no_params(),
            )
            .await
            .unwrap_or_default();
    }

    let lasttick = db.first("select * from portal_scheduler_run where run_type='Tick' order by id desc limit 1", &db::no_params()).await.ok().flatten();
    let tickgap: Option<i64> = lasttick
        .as_ref()
        .and_then(|t| parse_dt_any(&s(t, "started")))
        .map(|dt| (now - dt).num_minutes().max(0));
    let since24 = fmt_dt_full(now - Duration::hours(24));
    let ticks24 = db
        .count("select count(*) from portal_scheduler_run where run_type='Tick' and started >= :since", &row! {"since"=>since24})
        .await
        .unwrap_or(0);

    let banner = match (&lasttick, tickgap) {
        (None, _) => "<div class=\"sched-banner bad\"><strong>No scheduler run has been logged yet.</strong> Scheduled pages run when something calls <code>/portalScheduler/run</code> (or the built-in scheduler tick runs).</div>".to_string(),
        (Some(lt), Some(gap)) if gap > 90 => {
            let age = if gap >= 120 { format!("{} hours", gap / 60) } else { format!("{} minutes", gap) };
            let host_txt = { let h = s(lt, "host"); if h.is_empty() { String::new() } else { format!(" on {}", esc(&h)) } };
            format!(
                "<div class=\"sched-banner bad\"><strong>The scheduler has not been called for {age}.</strong> Last call: {}{host_txt}. Nothing below is running until the cron or scheduler loop calling <code>/portalScheduler/run</code> is back.</div>",
                esc(&fmt_dt_str(&s(lt, "started"), "%Y-%m-%d %H:%M"))
            )
        }
        (Some(lt), gap) => {
            let host_txt = { let h = s(lt, "host"); if h.is_empty() { String::new() } else { format!(", {}", esc(&h)) } };
            format!(
                "<div class=\"sched-banner ok\">Scheduler last called {} ({} min ago{host_txt}) &middot; {} calls in the last 24 hours.</div>",
                esc(&fmt_dt_str(&s(lt, "started"), "%Y-%m-%d %H:%M")),
                gap.unwrap_or(0),
                ticks24
            )
        }
    };

    let mut sched_rows = String::new();
    for sch in &schedules {
        let sid = i(sch, "id").unwrap_or(0);
        let hr = sched_hour(sch);
        let hr_d = if hr.is_empty() { "-".to_string() } else { esc(&hr) };
        let dow_d = { let x = s(sch, "day_of_week"); if x.is_empty() { "-".to_string() } else { esc(&x) } };
        let dom_d = { let x = s(sch, "day_of_month"); if x.is_empty() { "-".to_string() } else { esc(&x) } };
        let disabled_tag = if !b(sch, "enabled") { "<br/><span class=\"sched-muted\">(disabled)</span>" } else { "" };

        let last_res = match lastjob.get(&sid) {
            Some(lj) if !lj.is_empty() => {
                let sts: Vec<String> = lj.iter().map(|r| s(r, "status")).collect();
                let refs: Vec<&str> = sts.iter().map(|x| x.as_str()).collect();
                let worst = worst_status(&refs).unwrap_or("");
                let when_raw = { let d = s(&lj[0], "due_slot"); if d.is_empty() { s(&lj[0], "started") } else { d } };
                let mut cell = format!(
                    "<span class=\"{}\">{}</span> <span class=\"sched-muted\">{}</span>",
                    status_class(worst),
                    esc(worst),
                    esc(&fmt_dt_str(&when_raw, "%Y-%m-%d %H:%M"))
                );
                for r in lj {
                    let err = s(r, "error");
                    if !err.is_empty() {
                        if lj.len() > 1 {
                            cell.push_str(&format!("<br/><span class=\"sched-muted\">{}:</span>", esc(&s(r, "slug"))));
                        }
                        let shown = if err.chars().count() > 400 { format!("{} ...", err.chars().take(400).collect::<String>()) } else { err };
                        cell.push_str(&format!("<pre class=\"sched-err\">{}</pre>", esc(&shown)));
                    }
                }
                if lj.len() == 1 && s(&lj[0], "error").is_empty() {
                    let res = s(&lj[0], "result");
                    if !res.is_empty() {
                        let shown = if res.chars().count() > 200 { format!("{} ...", res.chars().take(200).collect::<String>()) } else { res };
                        cell.push_str(&format!("<br/><span class=\"sched-muted\">{}</span>", esc(&shown)));
                    }
                }
                cell
            }
            _ => "<span class=\"sched-muted\">not run since logging began</span>".to_string(),
        };

        let last_ok_cell = if let Some(ok_dt) = lastok.get(&sid) {
            esc(&fmt_dt_str(ok_dt, "%Y-%m-%d %H:%M"))
        } else {
            let lr = s(sch, "lastrun");
            if !lr.is_empty() {
                format!("{} <span class=\"sched-muted\">(before logging)</span>", esc(&fmt_dt_str(&lr, "%Y-%m-%d %H:%M")))
            } else {
                "<span class=\"sched-muted\">never</span>".to_string()
            }
        };

        let st_cell = match stats.get(&sid) {
            Some(m) if !m.is_empty() => m
                .iter()
                .map(|(k, v)| format!("<span class=\"{}\">{} {}</span>", if k == "Success" { "sched-ok" } else { "sched-bad" }, v, esc(k)))
                .collect::<Vec<_>>()
                .join("<br/>"),
            _ => "<span class=\"sched-muted\">-</span>".to_string(),
        };

        let next_cell = match next_due(sch, now) {
            Some(nd) => nd.format("%Y-%m-%d %H:00").to_string(),
            None => format!("<span class=\"sched-muted\">{}</span>", if b(sch, "enabled") { "not within 2 months" } else { "-" }),
        };

        sched_rows.push_str(&format!(
            "<tr><td><a href=\"/portalScheduler/runs/{sid}\">{}</a><br/><span class=\"sched-muted\">{}: {}</span>{disabled_tag}</td>\
             <td>hour {hr_d}<br/><span class=\"sched-muted\">weekday {dow_d} &middot; date {dom_d}</span></td>\
             <td>{last_res}</td><td>{last_ok_cell}</td><td>{st_cell}</td><td>{next_cell}</td></tr>\n",
            esc(&s(sch, "name")),
            esc(&s(sch, "module")),
            esc(&s(sch, "slugs")),
        ));
    }

    let recent_html = if recent.is_empty() {
        "<p class=\"sched-muted\">None logged.</p>".to_string()
    } else {
        let mut rrows = String::new();
        for r in &recent {
            let due_raw = { let d = s(r, "due_slot"); if d.is_empty() { s(r, "started") } else { d } };
            rrows.push_str(&format!(
                "<tr><td>{}</td><td>{}<br/><span class=\"sched-muted\">{}: {}</span></td><td class=\"sched-bad\">{}</td><td><pre class=\"sched-err\">{}</pre></td></tr>\n",
                esc(&fmt_dt_str(&due_raw, "%Y-%m-%d %H:%M")),
                esc(&s(r, "name")),
                esc(&s(r, "module")),
                esc(&s(r, "slug")),
                esc(&s(r, "status")),
                esc(&s(r, "error")),
            ));
        }
        format!(
            "<table class=\"table\"><thead><tr><th>Due</th><th>Schedule</th><th>Status</th><th>Detail</th></tr></thead><tbody>{rrows}</tbody></table>"
        )
    };

    let body = format!(
        r#"<style>
  .sched-ok {{ color:#1a7f37; font-weight:600; }}
  .sched-bad {{ color:#c62828; font-weight:600; }}
  .sched-warn {{ color:#b26a00; font-weight:600; }}
  .sched-muted {{ color:#888; }}
  .sched-banner {{ padding:10px 14px; border-radius:4px; margin:10px 0 16px; }}
  .sched-banner.ok {{ background:#e8f5e9; border:1px solid #a5d6a7; }}
  .sched-banner.bad {{ background:#ffebee; border:1px solid #ef9a9a; }}
  .sched-err {{ white-space:pre-wrap; font-size:12px; max-width:520px; margin:0; }}
  table.table td {{ vertical-align:top; }}
</style>
{banner}
<table class="table">
  <thead>
    <tr><th>Schedule</th><th>Runs at</th><th>Last result</th><th>Last success</th><th>Last 7 days</th><th>Next due</th></tr>
  </thead>
  <tbody>
{sched_rows}  </tbody>
</table>
<h3>Recent problems</h3>
{recent_html}"#
    );

    let nav = format!(
        "{}{}",
        li("list", "/portalScheduler/index", "Scheduler List"),
        li("list", "/portalScheduler/runs", "Scheduler Ticks"),
    );
    let flash = s(&ctx.sess, "flash");
    let h = page(&nav, "status", "Scheduler Status", &flash, &body);
    ctx.html("Scheduler Status", h).await
}

async fn runs_action(mut ctx: Ctx, id: Option<i64>) -> Response {
    let db = ctx.db();
    let is_admin = ctx.is_admin();
    let my_mods = admin_mods(&ctx).await;
    if !is_admin && my_mods.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view scheduler runs").await;
    }
    let max = ctx.pi("max").unwrap_or(50).clamp(1, 200);
    let offset = ctx.pi("offset").unwrap_or(0).max(0);

    let schedule = match id {
        Some(sid) => {
            let Ok(Some(sch)) = db.get("portal_scheduler", sid).await else {
                ctx.flash("Schedule not found");
                return ctx.redirect("/portalScheduler/status").await;
            };
            if !is_admin && !my_mods.contains(&s(&sch, "module")) {
                ctx.flash("Schedule not found");
                return ctx.redirect("/portalScheduler/status").await;
            }
            Some(sch)
        }
        None => None,
    };

    let (rows, total, title, subtitle, page_url) = match &schedule {
        Some(sch) => {
            let sid = i(sch, "id").unwrap_or(0);
            let rows = db
                .rows(
                    &format!("select * from portal_scheduler_run where scheduler_id=:id order by id desc limit {max} offset {offset}"),
                    &row! {"id"=>sid},
                )
                .await
                .unwrap_or_default();
            let total = db.count("select count(*) from portal_scheduler_run where scheduler_id=:id", &row! {"id"=>sid}).await.unwrap_or(0);
            let sub = format!(
                "{}: {} &middot; hour {} &middot; weekday {} &middot; date {}",
                esc(&s(sch, "module")),
                esc(&s(sch, "slugs")),
                esc(&sched_hour(sch)),
                esc(&s(sch, "day_of_week")),
                esc(&s(sch, "day_of_month"))
            );
            (rows, total, format!("Runs: {}", s(sch, "name")), sub, format!("/portalScheduler/runs/{}", sid))
        }
        None => {
            let rows = db
                .rows(
                    &format!("select * from portal_scheduler_run where run_type='Tick' order by id desc limit {max} offset {offset}"),
                    &db::no_params(),
                )
                .await
                .unwrap_or_default();
            let total = db.count("select count(*) from portal_scheduler_run where run_type='Tick'", &db::no_params()).await.unwrap_or(0);
            (
                rows,
                total,
                "Scheduler Ticks".to_string(),
                "Every call to /portalScheduler/run, whether or not anything was due.".to_string(),
                "/portalScheduler/runs".to_string(),
            )
        }
    };

    let has_sched = schedule.is_some();
    let table_html = if rows.is_empty() {
        "<p class=\"sched-muted\">Nothing logged yet.</p>".to_string()
    } else {
        let page_th = if has_sched { "<th>Page</th>" } else { "" };
        let mut tbody = String::new();
        for r in &rows {
            let st_txt = s(r, "status");
            let page_td = if has_sched { format!("<td>{}</td>", esc(&s(r, "slug"))) } else { String::new() };
            let by = if s(r, "run_type") == "Missed" { String::new() } else { esc(&s(r, "triggered_by")) };
            let host = s(r, "host");
            let host_br = if host.is_empty() { String::new() } else { format!("<br/><span class=\"sched-muted\">{}</span>", esc(&host)) };
            let err = s(r, "error");
            let res = s(r, "result");
            let mut detail = String::new();
            if !err.is_empty() {
                detail.push_str(&format!("<pre class=\"sched-err\">{}</pre>", esc(&err)));
            }
            if !res.is_empty() {
                detail.push_str(&format!("<span class=\"sched-muted\">{}</span>", esc(&res)));
            }
            tbody.push_str(&format!(
                "<tr><td>{}</td>{page_td}<td class=\"{}\">{}</td><td>{}</td><td>{}</td><td>{by}{host_br}</td><td>{detail}</td></tr>\n",
                esc(&fmt_dt_str(&s(r, "due_slot"), "%Y-%m-%d %H:00")),
                status_class(&st_txt),
                esc(&st_txt),
                esc(&fmt_dt_str(&s(r, "started"), "%Y-%m-%d %H:%M:%S")),
                esc(&fmt_dur(i(r, "duration_ms"))),
            ));
        }
        let pag = if total > max {
            format!("<div class=\"pagination\">{}</div>", crate::tags::paginate(&page_url, &ctx.params, total, max, offset))
        } else {
            String::new()
        };
        format!(
            "<table class=\"table\"><thead><tr><th>Due</th>{page_th}<th>Status</th><th>Started</th><th>Took</th><th>By</th><th>Detail</th></tr></thead><tbody>{tbody}</tbody></table>{pag}"
        )
    };

    let body = format!(
        r#"<style>
  .sched-ok {{ color:#1a7f37; font-weight:600; }}
  .sched-bad {{ color:#c62828; font-weight:600; }}
  .sched-warn {{ color:#b26a00; font-weight:600; }}
  .sched-muted {{ color:#888; }}
  .sched-err {{ white-space:pre-wrap; font-size:12px; max-width:600px; margin:0; }}
  table.table td {{ vertical-align:top; }}
</style>
<p class="sched-muted">{subtitle}</p>
{table_html}"#
    );

    let mut nav = li("list", "/portalScheduler/status", "Scheduler Status");
    if let Some(sch) = &schedule {
        nav.push_str(&li("list", &format!("/portalScheduler/show/{}", i(sch, "id").unwrap_or(0)), "Schedule"));
    }
    let flash = s(&ctx.sess, "flash");
    let h = page(&nav, "runs", &title, &flash, &body);
    ctx.html(&title, h).await
}

async fn index_action(ctx: Ctx) -> Response {
    let db = ctx.db();
    let is_admin = ctx.is_admin();
    let my_admins = admin_mods(&ctx).await;
    let my_devs = dev_mods(&ctx).await;
    if !is_admin && my_admins.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this list").await;
    }

    let q = ctx.p("q");
    let m = ctx.p("module");
    let mut cond: Vec<String> = vec![];
    if !m.is_empty() && m != "All" {
        if !is_admin && !my_admins.contains(&m) {
            return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this module").await;
        }
        cond.push(format!("\"module\"={}", lit(&json!(m))));
    } else if !is_admin {
        cond.push(format!("\"module\" in {}", lit(&json!(my_admins))));
    }
    if !q.trim().is_empty() {
        let like = lit(&json!(format!("%{}%", q.trim())));
        cond.push(format!("(name ilike {l} or slugs ilike {l} or \"module\" ilike {l})", l = like));
    }
    let wh = if cond.is_empty() { String::new() } else { format!(" where {}", cond.join(" and ")) };
    let max = ctx.pi("max").unwrap_or(10).clamp(1, 500);
    let offset = ctx.pi("offset").unwrap_or(0).max(0);
    let sort_p = ctx.p("sort");
    let order_dir = if ctx.p("order") == "desc" { "desc" } else { "asc" };
    let order_sql = match sort_p.as_str() {
        "name" => format!("name {order_dir}, id asc"),
        "module" => format!("\"module\" {order_dir}, id asc"),
        "slugs" => format!("slugs {order_dir}, id asc"),
        "hour_of_day" | "hour" => format!("hour_of_day {order_dir}, id asc"),
        "day_of_week" => format!("day_of_week {order_dir}, id asc"),
        "day_of_month" => format!("day_of_month {order_dir}, id asc"),
        "lastrun" => format!("lastrun {order_dir}, id asc"),
        "enabled" => format!("enabled {order_dir}, id asc"),
        "catch_up" => format!("catch_up {order_dir}, id asc"),
        _ => "id asc".to_string(),
    };

    let total = db.count(&format!("select count(*) from portal_scheduler{wh}"), &db::no_params()).await.unwrap_or(0);
    let rows = db
        .rows(&format!("select * from portal_scheduler{wh} order by {order_sql} limit {max} offset {offset}"), &db::no_params())
        .await
        .unwrap_or_default();

    const COLS: &[(&str, &str)] = &[
        ("name", "Name"),
        ("module", "Module"),
        ("slugs", "Slugs"),
        ("hour_of_day", "Hour Of Day"),
        ("day_of_week", "Day Of Week"),
        ("day_of_month", "Day Of Month"),
        ("lastrun", "Lastrun"),
        ("enabled", "Enabled"),
        ("catch_up", "Catch Up"),
    ];
    let qs_keep = web::qs(&ctx.params, &["sort", "order", "offset", "max"]);
    let extra_q = if qs_keep.is_empty() { String::new() } else { format!("&amp;{}", esc(&qs_keep)) };
    let mut thead = String::new();
    for (col, lbl) in COLS {
        let sorted = sort_p == *col || (*col == "hour_of_day" && sort_p == "hour");
        let next_ord = if sorted && order_dir == "asc" { "desc" } else { "asc" };
        let cls = if sorted { format!("sortable sorted {}", order_dir) } else { "sortable".into() };
        thead.push_str(&format!(
            "                <th class=\"{cls}\"><a href=\"/portalScheduler/index?sort={col}&amp;max={max}&amp;order={next_ord}{extra_q}\">{lbl}</a></th>\n"
        ));
    }

    let mut tbody = String::new();
    for (rn, r) in rows.iter().enumerate() {
        let sid = i(r, "id").unwrap_or(0);
        let row_cls = if rn % 2 == 0 { "even" } else { "odd" };
        let name_txt = s(r, "name");
        let name_lbl = if name_txt.trim().is_empty() { format!("#{}", sid) } else { esc(&name_txt) };
        tbody.push_str(&format!(
            "            <tr class=\"{row_cls}\">\n                <td><a href=\"/portalScheduler/show/{sid}\">{name_lbl}</a>&nbsp;</td>\n                <td>{}&nbsp;</td>\n                <td>{}&nbsp;</td>\n                <td>{}&nbsp;</td>\n                <td>{}&nbsp;</td>\n                <td>{}&nbsp;</td>\n                <td>{}&nbsp;</td>\n                <td>{}&nbsp;</td>\n                <td>{}&nbsp;</td>\n            </tr>\n",
            esc(&s(r, "module")),
            esc(&s(r, "slugs")),
            esc(&sched_hour(r)),
            esc(&s(r, "day_of_week")),
            esc(&s(r, "day_of_month")),
            esc(&fmt_dt_str(&s(r, "lastrun"), "%Y-%m-%d %H:%M:%S")),
            fmt_bool(r.get("enabled")),
            fmt_bool(r.get("catch_up")),
        ));
    }

    let pag = if total > max {
        format!("\n                    <div class=\"pagination\">\n                        {}\n                    </div>\n", crate::tags::paginate("/portalScheduler/index", &ctx.params, total, max, offset))
    } else {
        String::new()
    };

    let mod_hidden = if !m.is_empty() && m != "All" {
        format!("<input type=\"hidden\" name=\"module\" value=\"{}\"/>", esc(&m))
    } else {
        String::new()
    };

    let body = format!(
        r#"<style>
@media only screen and (max-width: 760px), (min-device-width: 768px) and (max-device-width: 1024px) {{
  td:nth-of-type(1):before {{ content: "Name"; }}
  td:nth-of-type(2):before {{ content: "Module"; }}
  td:nth-of-type(3):before {{ content: "Slugs"; }}
  td:nth-of-type(4):before {{ content: "Hour"; }}
  td:nth-of-type(5):before {{ content: "Day Of Week"; }}
  td:nth-of-type(6):before {{ content: "Day Of Month"; }}
  td:nth-of-type(7):before {{ content: "Lastrun"; }}
  td:nth-of-type(8):before {{ content: "Enabled"; }}
  td:nth-of-type(9):before {{ content: "Catch Up"; }}
}}
</style>
                    <form action="/portalScheduler/index" method="get">
                        {mod_hidden}
                        <fieldset class="form">
                            <div class="fieldcontain" id="listsearch">
                                <label>Search:</label><input type="text" name="q" id="q" value="{q}"/><input id="dosearch" name="dosearch" type="submit" value="Search"/>
                            </div>
                        </fieldset>
                    </form>
                    <table class="responsive">
                        <thead>
                            <tr>
{thead}                            </tr>
                        </thead>
                        <tbody>
{tbody}                        </tbody>
                    </table>{pag}"#,
        q = esc(&q),
    );

    let mut nav = li("list", "/portalScheduler/status", "Status");
    if is_admin || !my_devs.is_empty() {
        let create_href = if !m.is_empty() && m != "All" {
            format!("/portalScheduler/create?module={}", enc(&m))
        } else {
            "/portalScheduler/create".to_string()
        };
        nav.push_str(&li("create", &create_href, "New PortalScheduler"));
        if is_admin {
            nav.push_str(&li("create", "/portalScheduler/run", "Run"));
        }
    }
    let flash = s(&ctx.sess, "flash");
    let h = page(&nav, "list", "PortalScheduler List", &flash, &body);
    ctx.html("PortalScheduler List", h).await
}

async fn show_action(mut ctx: Ctx, id: i64) -> Response {
    let Ok(Some(r)) = ctx.db().get("portal_scheduler", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "PortalScheduler not found").await;
    };
    if !can_view_sched(&ctx, &r).await {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this schedule").await;
    }
    let can_edit = can_edit_sched(&ctx, &r).await;
    let mod_name = s(&r, "module");
    let mod_cell = match crate::admin::module_url(ctx.db(), &mod_name).await {
        Some(u) => format!("<a href=\"{u}\">{}</a>", esc(&mod_name)),
        None => esc(&mod_name),
    };
    let csrf = ctx.csrf_field();
    let buttons = if can_edit {
        format!(
            r#"                    <form action="/portalScheduler/delete/{id}" method="post">{csrf}
                        <fieldset class="buttons">
                            <a href="/portalScheduler/edit/{id}" class="edit">Edit</a>
                            <button type="submit" formaction="/portalScheduler/run/{id}" class="save">Run Now</button>
                            <input class="delete" type="submit" value="Delete" onclick="return confirm('Are you sure?');" />
                        </fieldset>
                    </form>"#
        )
    } else {
        String::new()
    };

    let content = format!(
        r#"<ol class="property-list portalScheduler">
    <li class="fieldcontain">
        <span id="name-label" class="property-label">Name</span>
        <div class="property-value" aria-labelledby="name-label">{name}</div>
    </li>
    <li class="fieldcontain">
        <span id="module-label" class="property-label">Module</span>
        <div class="property-value" aria-labelledby="module-label">{mod_cell}</div>
    </li>
    <li class="fieldcontain">
        <span id="slugs-label" class="property-label">Slugs</span>
        <div class="property-value" aria-labelledby="slugs-label">{slugs}</div>
    </li>
    <li class="fieldcontain">
        <span id="hour_of_day-label" class="property-label">Hour Of Day</span>
        <div class="property-value" aria-labelledby="hour_of_day-label">{hr}</div>
    </li>
    <li class="fieldcontain">
        <span id="day_of_week-label" class="property-label">Day Of Week</span>
        <div class="property-value" aria-labelledby="day_of_week-label">{dow}</div>
    </li>
    <li class="fieldcontain">
        <span id="day_of_month-label" class="property-label">Day Of Month</span>
        <div class="property-value" aria-labelledby="day_of_month-label">{dom}</div>
    </li>
    <li class="fieldcontain">
        <span id="lastrun-label" class="property-label">Lastrun</span>
        <div class="property-value" aria-labelledby="lastrun-label">{lastrun}</div>
    </li>
    <li class="fieldcontain">
        <span id="enabled-label" class="property-label">Enabled</span>
        <div class="property-value" aria-labelledby="enabled-label">{enabled}</div>
    </li>
    <li class="fieldcontain">
        <span id="catch_up-label" class="property-label">Catch Up</span>
        <div class="property-value" aria-labelledby="catch_up-label">{catch_up}</div>
    </li>
    <li class="fieldcontain">
        <span id="last_updated-label" class="property-label">Last Updated</span>
        <div class="property-value" aria-labelledby="last_updated-label">{last_updated}</div>
    </li>
</ol>
{buttons}"#,
        name = esc(&s(&r, "name")),
        slugs = esc(&s(&r, "slugs")),
        hr = esc(&sched_hour(&r)),
        dow = esc(&s(&r, "day_of_week")),
        dom = esc(&s(&r, "day_of_month")),
        lastrun = esc(&fmt_dt_str(&s(&r, "lastrun"), "%Y-%m-%d %H:%M:%S")),
        enabled = fmt_bool(r.get("enabled")),
        catch_up = fmt_bool(r.get("catch_up")),
        last_updated = esc(&fmt_dt_str(&s(&r, "last_updated"), "%Y-%m-%d %H:%M:%S")),
    );

    let mut nav = format!(
        "{}{}{}",
        li("list", "/portalScheduler/index", "PortalScheduler List"),
        li("list", "/portalScheduler/status", "Status"),
        li("list", &format!("/portalScheduler/runs/{id}"), "Runs"),
    );
    if can_edit {
        nav.push_str(&li("create", "/portalScheduler/create", "New PortalScheduler"));
    }
    let flash = s(&ctx.sess, "flash");
    let h = page(&nav, "show", "Show PortalScheduler", &flash, &content);
    ctx.html("Show PortalScheduler", h).await
}

async fn render_form(mut ctx: Ctx, r: &Row, id: Option<i64>, errors: &[String]) -> Response {
    let is_admin = ctx.is_admin();
    let mut mods: Vec<String> = ctx
        .db()
        .rows("select name from portal_module order by name", &db::no_params())
        .await
        .unwrap_or_default()
        .iter()
        .map(|m| s(m, "name"))
        .collect();
    if !is_admin {
        let mine = dev_mods(&ctx).await;
        mods.retain(|m| mine.contains(m));
    }
    let cur_mod = s(r, "module");
    if !cur_mod.is_empty() && !mods.contains(&cur_mod) {
        mods.push(cur_mod.clone());
    }
    let mut mod_opts = String::from("<option value=\"\"></option>");
    for m in &mods {
        let sel = if *m == cur_mod { " selected=\"selected\"" } else { "" };
        mod_opts.push_str(&format!("<option value=\"{}\"{sel}>{}</option>", esc(m), esc(m)));
    }
    let mod_cls = if is_admin { "module-select-tags" } else { "module-select" };

    let err_html = if errors.is_empty() {
        String::new()
    } else {
        let items: String = errors.iter().map(|e| format!("<li>{}</li>", esc(e))).collect();
        format!("<ul class=\"errors\" role=\"alert\">{items}</ul>\n")
    };

    let (action_url, kind, title, nav, submit_btn) = match id {
        Some(sid) => (
            format!("/portalScheduler/update/{sid}"),
            "edit",
            "Edit PortalScheduler",
            format!("{}{}", li("list", "/portalScheduler/index", "PortalScheduler List"), li("create", "/portalScheduler/create", "New PortalScheduler")),
            "<input class=\"save\" type=\"submit\" value=\"Update\" />",
        ),
        None => (
            "/portalScheduler/save".to_string(),
            "create",
            "Create PortalScheduler",
            li("list", "/portalScheduler/index", "PortalScheduler List"),
            "<input type=\"submit\" name=\"create\" class=\"save\" value=\"Create\" id=\"create\" />",
        ),
    };

    let csrf = ctx.csrf_field();
    let select2_tags = if is_admin {
        "<script>$(function(){ if ($.fn && $.fn.select2) { $('select.module-select-tags').select2({ width: '40%', tags: true, allowClear: true, placeholder: '- choose a module -' }); } });</script>"
    } else {
        ""
    };
    let enb_chk = if b(r, "enabled") { " checked=\"checked\"" } else { "" };
    let cu_chk = if b(r, "catch_up") { " checked=\"checked\"" } else { "" };
    let body = format!(
        r#"{err_html}                    <form action="{action_url}" method="post">{csrf}
                        <fieldset class="form">
                            <div class="fieldcontain required">
                                <label for="name">Name<span class="required-indicator">*</span></label>
                                <input type="text" name="name" value="{name_val}" required="" id="name" />
                            </div>
                            <div class="fieldcontain required">
                                <label for="module">Module<span class="required-indicator">*</span></label>
                                <select name="module" id="module" class="{mod_cls}" required="">{mod_opts}</select>
                            </div>
                            <div class="fieldcontain">
                                <label for="slugs">Slugs</label>
                                <textarea name="slugs" id="slugs" rows="4" cols="40">{slugs_val}</textarea>
                                <div class="property-value" style="font-size:12px;color:#666;">Comma-separated page slugs in this module (or <code>module:slug</code>).</div>
                            </div>
                            <div class="fieldcontain">
                                <label for="hour_of_day">Hour Of Day</label>
                                <input type="text" name="hour_of_day" value="{hr_val}" id="hour_of_day" placeholder="* or 0,8,14" />
                                <div class="property-value" style="font-size:12px;color:#666;"><code>*</code> for every hour, or comma-separated hours 0&ndash;23 (blank never runs).</div>
                            </div>
                            <div class="fieldcontain">
                                <label for="day_of_week">Day Of Week</label>
                                <input type="text" name="day_of_week" value="{dow_val}" id="day_of_week" placeholder="* or 1,2,3,4,5" />
                                <div class="property-value" style="font-size:12px;color:#666;"><code>*</code> for every day, or comma-separated 0&ndash;6 (0 = Sunday, 6 = Saturday).</div>
                            </div>
                            <div class="fieldcontain">
                                <label for="day_of_month">Day Of Month</label>
                                <input type="text" name="day_of_month" value="{dom_val}" id="day_of_month" placeholder="* or 1,15" />
                                <div class="property-value" style="font-size:12px;color:#666;"><code>*</code> for every date, or comma-separated 1&ndash;31.</div>
                            </div>
                            <div class="fieldcontain">
                                <label for="enabled">Enabled</label>
                                <input type="hidden" name="_enabled" />
                                <input type="checkbox" name="enabled" id="enabled"{enb_chk} />
                            </div>
                            <div class="fieldcontain">
                                <label for="catch_up">Catch Up</label>
                                <input type="hidden" name="_catch_up" />
                                <input type="checkbox" name="catch_up" id="catch_up"{cu_chk} />
                                <div class="property-value" style="font-size:12px;color:#666;">Run a missed slot on the next tick if still the same day (opt-in for idempotent pages).</div>
                            </div>
                        </fieldset>
                        <fieldset class="buttons">
                            {submit_btn}
                        </fieldset>
                    </form>
{form_js}
{select2_tags}"#,
        name_val = esc(&s(r, "name")),
        slugs_val = esc(&s(r, "slugs")),
        hr_val = esc(&sched_hour(r)),
        dow_val = esc(&s(r, "day_of_week")),
        dom_val = esc(&s(r, "day_of_month")),
        form_js = crate::tags::form_js(),
    );

    let flash = s(&ctx.sess, "flash");
    let h = page(&nav, kind, title, &flash, &body);
    ctx.html(title, h).await
}

fn posted_sched(ctx: &Ctx) -> Row {
    let opt = |k: &str| {
        let v = ctx.p(k);
        if v.trim().is_empty() { Value::Null } else { json!(v.trim()) }
    };
    let hr_raw = {
        let a = ctx.p("hour_of_day");
        if a.trim().is_empty() { ctx.p("hour") } else { a }
    };
    let hr_val = if hr_raw.trim().is_empty() { Value::Null } else { json!(hr_raw.trim()) };
    let chk = |k: &str| ctx.params.contains_key(k) && ctx.p(k) != "false" && ctx.p(k) != "0";
    row! {
        "name" => opt("name"),
        "module" => opt("module"),
        "slugs" => opt("slugs"),
        "hour_of_day" => hr_val.clone(),
        "hour" => hr_val,
        "day_of_week" => opt("day_of_week"),
        "day_of_month" => opt("day_of_month"),
        "enabled" => json!(chk("enabled")),
        "catch_up" => json!(chk("catch_up")),
        "last_updated" => json!(now_str()),
    }
}

async fn create_action(ctx: Ctx) -> Response {
    if !ctx.is_admin() && dev_mods(&ctx).await.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "Developer access required").await;
    }
    let mut r = Row::new();
    let m = ctx.p("module");
    if !m.is_empty() {
        r.insert("module".into(), json!(m));
    }
    r.insert("enabled".into(), json!(true));
    render_form(ctx, &r, None, &[]).await
}

async fn save_action(mut ctx: Ctx) -> Response {
    if ctx.method != "POST" {
        return ctx.redirect("/portalScheduler/create").await;
    }
    let d = posted_sched(&ctx);
    let m = s(&d, "module");
    if !ctx.is_admin() && (m.is_empty() || !dev_mods(&ctx).await.contains(&m)) {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to create schedules in this module").await;
    }
    let mut errs = vec![];
    if s(&d, "name").trim().is_empty() {
        errs.push("Name cannot be blank".into());
    }
    if m.trim().is_empty() {
        errs.push("Module cannot be blank".into());
    }
    if !errs.is_empty() {
        return render_form(ctx, &d, None, &errs).await;
    }
    match ctx.db().insert("portal_scheduler", &d).await {
        Ok(sid) => {
            ctx.flash(&format!("PortalScheduler {} created", sid));
            ctx.redirect(&format!("/portalScheduler/show/{}", sid)).await
        }
        Err(e) => render_form(ctx, &d, None, &[format!("Could not save: {}", e)]).await,
    }
}

async fn edit_action(ctx: Ctx, id: i64) -> Response {
    let Ok(Some(r)) = ctx.db().get("portal_scheduler", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "PortalScheduler not found").await;
    };
    if !can_edit_sched(&ctx, &r).await {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to edit this schedule").await;
    }
    render_form(ctx, &r, Some(id), &[]).await
}

async fn update_action(mut ctx: Ctx, id: i64) -> Response {
    if ctx.method != "POST" {
        return ctx.redirect(&format!("/portalScheduler/edit/{}", id)).await;
    }
    let Ok(Some(existing)) = ctx.db().get("portal_scheduler", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "PortalScheduler not found").await;
    };
    if !can_edit_sched(&ctx, &existing).await {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to edit this schedule").await;
    }
    let d = posted_sched(&ctx);
    let m = s(&d, "module");
    if !ctx.is_admin() && (m.is_empty() || !dev_mods(&ctx).await.contains(&m)) {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to move this schedule to that module").await;
    }
    let mut errs = vec![];
    if s(&d, "name").trim().is_empty() {
        errs.push("Name cannot be blank".into());
    }
    if m.trim().is_empty() {
        errs.push("Module cannot be blank".into());
    }
    if !errs.is_empty() {
        return render_form(ctx, &d, Some(id), &errs).await;
    }
    match ctx.db().update("portal_scheduler", id, &d).await {
        Ok(_) => {
            ctx.flash(&format!("PortalScheduler {} updated", id));
            ctx.redirect(&format!("/portalScheduler/show/{}", id)).await
        }
        Err(e) => render_form(ctx, &d, Some(id), &[format!("Could not update: {}", e)]).await,
    }
}

async fn delete_action(mut ctx: Ctx, id: i64) -> Response {
    if ctx.method != "POST" {
        return ctx.status(StatusCode::METHOD_NOT_ALLOWED, "POST required").await;
    }
    let Ok(Some(existing)) = ctx.db().get("portal_scheduler", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "PortalScheduler not found").await;
    };
    if !can_edit_sched(&ctx, &existing).await {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to delete this schedule").await;
    }
    let _ = ctx.db().delete("portal_scheduler", id).await;
    ctx.flash(&format!("PortalScheduler {} deleted", id));
    ctx.redirect("/portalScheduler/index").await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn field_matches_and_sunday_is_zero() {
        assert!(!field_matches("", 0));
        assert!(!field_matches("   ", 5));
        assert!(field_matches("*", 0));
        assert!(field_matches("0, 6, 12", 6));
        assert!(!field_matches("1,2,3", 0));

        // 2026-11-22 is a Sunday -> num_days_from_sunday() == 0
        let sun = NaiveDate::from_ymd_opt(2026, 11, 22).unwrap().and_hms_opt(8, 15, 0).unwrap();
        let sch = row! {
            "enabled" => true,
            "hour_of_day" => "8",
            "day_of_week" => "0",
            "day_of_month" => "*",
        };
        assert!(is_due(&sch, sun));
        let mon = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap().and_hms_opt(8, 0, 0).unwrap();
        assert!(!is_due(&sch, mon));

        let next = next_due(&sch, mon).unwrap();
        assert_eq!(next, NaiveDate::from_ymd_opt(2026, 11, 29).unwrap().and_hms_opt(8, 0, 0).unwrap());
    }

    #[test]
    fn worst_status_and_summarise_match_g6ikram() {
        assert_eq!(worst_status(&["Success", "Missed", "Success"]), Some("Missed"));
        assert_eq!(worst_status(&["Success", "Failed", "Missed"]), Some("Failed"));
        assert_eq!(worst_status(&["Success", "Page missing"]), Some("Page missing"));
        let html = "<div>Hello<script>alert(1)</script>&nbsp;<b>world</b></div>";
        assert_eq!(summarise_result(html).as_deref(), Some("Hello world"));
    }
}
