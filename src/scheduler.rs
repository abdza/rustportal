//! Background loop: scheduled runable pages, queued e-mail, session clean-up.

use crate::db::{b, csv, i, s, Row};
use crate::row;
use crate::script::Sc;
use crate::web::{now_str, St};
use chrono::{Datelike, Timelike};

fn matches_list(spec: &str, val: u32) -> bool {
    let sp = spec.trim();
    sp.is_empty() || sp == "*" || csv(sp).iter().any(|x| x.parse::<u32>().ok() == Some(val))
}

/// Run every page of a scheduler entry, recording each run.
pub async fn run_scheduler(st: &St, sch: &Row, triggered_by: &str) {
    let module = s(sch, "module");
    for spec in csv(&s(sch, "slugs")) {
        let (m, sl) = match spec.split_once(':') {
            Some((a, b2)) => (a.to_string(), b2.to_string()),
            None => (module.clone(), spec.clone()),
        };
        let started = std::time::Instant::now();
        let start = now_str();
        let sc = Sc::system(st.clone());
        let res = crate::script::run_page(&sc.inner, &m, &sl, row! {"scheduled"=>true}).await;
        let (status, error, result) = match &res {
            Ok(v) => ("COMPLETED", None, Some(v.to_string().chars().take(2000).collect::<String>())),
            Err(e) => ("FAILED", Some(e.to_string()), None),
        };
        if let Some(e) = &error {
            crate::web::log_error(&st.db, "portalScheduler", "run", e, Some(&m), Some(&sl), None).await;
        }
        let _ = st
            .db
            .insert("portal_scheduler_run", &row! {"run_type"=>"page","status"=>status,"scheduler_id"=>i(sch, "id"),"name"=>s(sch, "name"),"module"=>m,"slug"=>sl,
                "started"=>start,"finished"=>now_str(),"duration_ms"=>started.elapsed().as_millis() as i64,"error"=>error,"result"=>result,"triggered_by"=>triggered_by})
            .await;
    }
    if let Some(id) = i(sch, "id") {
        let _ = st.db.exec("update portal_scheduler set lastrun=now() where id=:id", &row! {"id"=>id}).await;
    }
}

async fn tick(st: &St) {
    let now = chrono::Local::now();
    let scheds = st.db.rows("select * from portal_scheduler where enabled is true", &Row::new()).await.unwrap_or_default();
    for sch in scheds {
        if !matches_list(&s(&sch, "day_of_month"), now.day()) || !matches_list(&s(&sch, "day_of_week"), now.weekday().number_from_monday()) {
            continue;
        }
        let last = crate::script::parse_dt(&s(&sch, "lastrun"));
        let hours = { let h = csv(&s(&sch, "hour")); if h.is_empty() || h == ["*"] { (0..24).map(|x| x.to_string()).collect() } else { h } };
        for h in hours {
            let (hh, mm) = match h.split_once(':') {
                Some((a, b2)) => (a.trim().parse::<u32>().unwrap_or(99), b2.trim().parse::<u32>().unwrap_or(0)),
                None => (h.trim().parse::<u32>().unwrap_or(99), 0),
            };
            let Some(slot) = now.date_naive().and_hms_opt(hh, mm, 0) else { continue };
            let n = now.naive_local();
            let window = if b(&sch, "catch_up") { n >= slot } else { n >= slot && n < slot + chrono::Duration::minutes(10) };
            let _ = now.hour();
            if window && last.map(|l| l < slot).unwrap_or(true) {
                run_scheduler(st, &sch, "schedule").await;
                break;
            }
        }
    }
}

pub fn spawn(st: St) {
    tokio::spawn(async move {
        let mut n: u64 = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            // each tick in its own task so one failing script cannot stop the loop
            let st2 = st.clone();
            let _ = tokio::spawn(async move {
                tick(&st2).await;
                crate::mail::flush(&st2).await;
            })
            .await;
            n += 1;
            if n % 30 == 0 {
                let _ = st.db.exec(&format!("delete from portal_session where updated < now() - interval '{} hours'", st.cfg.session_hours.max(1)), &Row::new()).await;
            }
        }
    });
}
