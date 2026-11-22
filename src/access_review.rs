//! Monthly module access reviews (/accessReview/index, mine, show, addRole, removeRole, approve, restore, runNow, clearPeriod).
//! Matches g6ikram's AccessReviewController + PortalAccessReviewService + views/accessReview/*.gsp.

use crate::db::{self, i, s, Db, Row};
use crate::row;
use crate::web::{self, enc, esc, now_str, Ctx};
use axum::response::Response;
use chrono::{Datelike, Duration, Local, NaiveDate};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

const STYLES: &str = r#"<style>
.ar-page { --ar-line: #e4e6eb; --ar-muted: #6b7280; }
.ar-page h1 { font-size: 1.5rem; font-weight: 600; margin: 0; }
.ar-page h3 { font-size: 1.05rem; font-weight: 600; margin: 0 0 .6rem; }
.ar-page h4 { font-size: .95rem; font-weight: 600; margin: 1rem 0 .5rem; }
.ar-muted, .ar-page .ar-sub { color: var(--ar-muted); font-weight: 400; font-size: .85rem; }
.ar-head { display: flex; flex-wrap: wrap; justify-content: space-between; gap: 1rem; align-items: flex-end;
           border-bottom: 1px solid var(--ar-line); padding-bottom: 1rem; margin-bottom: 1rem; }
.ar-due { text-align: right; font-size: .85rem; color: var(--ar-muted); }
.ar-due-n { font-size: 2rem; font-weight: 600; color: #222; line-height: 1; }
.ar-due-n.ar-urgent { color: #c0392b; }
.ar-badge { font-size: .7rem; font-weight: 600; padding: .2rem .5rem; border-radius: 999px; vertical-align: middle; }
.ar-pending { background: #fff4d6; color: #8a6100; }
.ar-approved { background: #dcf5e8; color: #146c43; }
.ar-revoked { background: #fde2df; color: #a4281c; }
.ar-cancelled { background: #eceef1; color: #555; }
.ar-steps { background: #eef4fc; color: #1e4f8a; border-radius: 8px; padding: .7rem 1rem; font-size: .9rem; margin-bottom: 1rem; }
.ar-card { background: #fff; border: 1px solid var(--ar-line); border-radius: 12px; padding: 1rem 1.25rem; margin-bottom: 1.25rem; }
.ar-card-quiet { background: #fafbfc; }
.ar-approve { border-color: #9fd8b8; }
.ar-page table.ar-table { font-size: .875rem; margin: 0; border: 0; }
.ar-page table.ar-table th { background: #f7f8fa; background-image: none; color: var(--ar-muted); font-size: .75rem; font-weight: 600;
                             text-transform: uppercase; white-space: normal; border: 0; border-bottom: 1px solid var(--ar-line); }
.ar-page table.ar-table td { border: 0; border-bottom: 1px solid #f0f1f3; vertical-align: middle; }
.ar-inactive td { color: #999; }
.ar-flag { font-size: .7rem; background: #fde2df; color: #a4281c; border-radius: 4px; padding: 0 .3rem; }
.ar-add { display: flex; flex-wrap: wrap; gap: .5rem; align-items: center; }
.ar-add-user { flex: 1 1 280px; }
.ar-add select.form-select { width: auto; min-width: 180px; }
.ar-confirm { display: flex; gap: .5rem; align-items: flex-start; margin-bottom: .6rem; font-weight: 500; }
.ar-pre { white-space: pre-line; margin: 0; }
.ar-stats { display: grid; grid-template-columns: repeat(auto-fit, minmax(140px, 1fr)); gap: .75rem; margin-bottom: 1.25rem; }
.ar-stat { border: 1px solid var(--ar-line); border-radius: 10px; padding: .75rem 1rem; background: #fff; }
.ar-stat .v { font-size: 1.6rem; font-weight: 600; line-height: 1.1; }
.ar-stat .l { font-size: .8rem; color: var(--ar-muted); }
</style>"#;

const MONTHS: &[&str] = &[
    "January", "February", "March", "April", "May", "June",
    "July", "August", "September", "October", "November", "December",
];
const MONTHS_SHORT: &[&str] = &[
    "Jan", "Feb", "Mar", "Apr", "May", "Jun",
    "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

pub fn period_label(period: &str) -> String {
    let parts: Vec<&str> = period.trim().split('-').collect();
    if parts.len() == 2 {
        if let (Ok(y), Ok(m)) = (parts[0].parse::<i32>(), parts[1].parse::<usize>()) {
            if (1..=12).contains(&m) {
                return format!("{} {}", MONTHS[m - 1], y);
            }
        }
    }
    period.to_string()
}

/// Format an ISO date/datetime string as `d MMM yyyy` (e.g. `15 Nov 2026`).
pub fn format_date_short(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() >= 10 {
        if let Ok(d) = NaiveDate::parse_from_str(&trimmed[..10], "%Y-%m-%d") {
            let m = d.month() as usize;
            if (1..=12).contains(&m) {
                return format!("{} {} {}", d.day(), MONTHS_SHORT[m - 1], d.year());
            }
        }
    }
    trimmed.to_string()
}

pub fn format_datetime_short(raw: &str) -> String {
    let trimmed = raw.trim();
    let date = format_date_short(trimmed);
    if trimmed.len() >= 16 && date != trimmed {
        format!("{}, {}", date, &trimmed[11..16])
    } else {
        date
    }
}

fn valid_period(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() == 7 && b[4] == b'-' && b[..4].iter().all(|c| c.is_ascii_digit()) && b[5..].iter().all(|c| c.is_ascii_digit())
}

fn current_period() -> String {
    Local::now().format("%Y-%m").to_string()
}

async fn setting_bool(db: &Db, name: &str) -> bool {
    matches!(
        web::setting_str(db, name).await.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("true" | "1" | "yes" | "on")
    )
}

async fn grace_days(db: &Db) -> i64 {
    web::setting_str(db, "portal.access_review_grace_days")
        .await
        .and_then(|v| v.trim().parse::<i64>().ok())
        .map(|n| n.max(1))
        .unwrap_or(30)
}

async fn reminder_days(db: &Db) -> Vec<i64> {
    let g = grace_days(db).await;
    let raw = web::setting_str(db, "portal.access_review_reminder_days").await.unwrap_or_else(|| "10,20,27".into());
    let mut days: Vec<i64> = raw
        .split(',')
        .filter_map(|x| x.trim().parse::<i64>().ok())
        .filter(|d| *d > 0 && *d < g)
        .collect();
    days.sort();
    days.dedup();
    days
}

async fn exempt_roles(db: &Db) -> Vec<String> {
    let raw = web::setting_str(db, "portal.access_review_exempt_roles").await.unwrap_or_else(|| "Developer".into());
    raw.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

fn is_exempt(role: &str, exempt: &[String]) -> bool {
    let r = role.trim();
    !r.is_empty() && exempt.iter().any(|e| e.eq_ignore_ascii_case(r))
}

async fn cc_addresses(db: &Db) -> Vec<String> {
    let raw = web::setting_str(db, "portal.access_review_cc").await.unwrap_or_default();
    raw.split([',', ';']).map(|x| x.trim().to_string()).filter(|x| x.contains('@')).collect()
}

async fn module_owners(db: &Db, module_id: i64) -> Vec<Row> {
    db.rows(
        "select u.id, u.userid, u.name, u.email from portal_module_contact c join portal_user u on u.id=c.user_id where c.module_id=:m and c.role='Owner' order by c.id",
        &row! {"m"=>module_id},
    )
    .await
    .unwrap_or_default()
}

async fn can_act(ctx: &Ctx, module_id: i64) -> bool {
    if ctx.is_admin() {
        return true;
    }
    let Some(uid) = ctx.uid() else { return false };
    ctx.db()
        .count("select count(*) from portal_module_contact where module_id=:m and role='Owner' and user_id=:u", &row! {"m"=>module_id,"u"=>uid})
        .await
        .unwrap_or(0)
        > 0
}

fn mod_display(m: &Row) -> String {
    let t = s(m, "title");
    if t.trim().is_empty() { s(m, "name") } else { t }
}

async fn grantable_roles(db: &Db, module_name: &str, exempt: &[String]) -> Vec<String> {
    let mut set: Vec<String> = vec![];
    for r in db
        .rows(
            "select distinct r.name from portal_tracker_role r join portal_tracker t on t.id=r.tracker_id where t.module=:m and r.role_type='User Role'",
            &row! {"m"=>module_name},
        )
        .await
        .unwrap_or_default()
    {
        let n = s(&r, "name").trim().to_string();
        if !n.is_empty() && !is_exempt(&n, exempt) {
            set.push(n);
        }
    }
    for r in db
        .rows("select distinct role from user_role where module=:m", &row! {"m"=>module_name})
        .await
        .unwrap_or_default()
    {
        let n = s(&r, "role").trim().to_string();
        if !n.is_empty() && !is_exempt(&n, exempt) {
            set.push(n);
        }
    }
    set.sort_by_key(|a| a.to_ascii_lowercase());
    set.dedup();
    set
}

async fn review_log(db: &Db, review_id: i64, action: &str, user_id: Option<i64>, role: Option<&str>, actor_id: Option<i64>, detail: Option<&str>) {
    let _ = db
        .insert(
            "portal_module_access_review_log",
            &row! {
                "review_id"=>review_id,
                "action"=>action,
                "user_id"=>user_id,
                "role"=>role,
                "actor_id"=>actor_id,
                "detail"=>detail.map(|x| x.chars().take(2000).collect::<String>()),
                "date_created"=>now_str()
            },
        )
        .await;
}

async fn scope_modules(db: &Db, exempt: &[String]) -> (Vec<Row>, Vec<Row>, HashMap<String, i64>) {
    let mut role_counts: HashMap<String, i64> = HashMap::new();
    for r in db
        .rows("select module, role, count(id) as n from user_role group by module, role", &db::no_params())
        .await
        .unwrap_or_default()
    {
        if !is_exempt(&s(&r, "role"), exempt) {
            *role_counts.entry(s(&r, "module")).or_insert(0) += i(&r, "n").unwrap_or(0);
        }
    }
    let owned_ids: HashSet<i64> = db
        .rows("select distinct module_id from portal_module_contact where role='Owner'", &db::no_params())
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| i(&r, "module_id"))
        .collect();
    let candidates = db
        .rows("select * from portal_module where status is null or status='' or status='Active' order by name", &db::no_params())
        .await
        .unwrap_or_default();
    let (mut in_scope, mut no_owner) = (vec![], vec![]);
    for m in candidates {
        let mname = s(&m, "name");
        if *role_counts.get(&mname).unwrap_or(&0) == 0 {
            continue;
        }
        let mid = i(&m, "id").unwrap_or(0);
        if owned_ids.contains(&mid) {
            in_scope.push(m);
        } else {
            no_owner.push(m);
        }
    }
    (in_scope, no_owner, role_counts)
}

pub async fn action(ctx: Ctx, act: &str, id: Option<i64>) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    match act {
        "" | "index" | "list" => index(ctx).await,
        "mine" => mine(ctx).await,
        "show" => show(ctx, id.unwrap_or(0)).await,
        "addRole" => add_role(ctx, id.unwrap_or(0)).await,
        "removeRole" => remove_role(ctx, id.unwrap_or(0)).await,
        "approve" => approve(ctx, id.unwrap_or(0)).await,
        "restore" => restore(ctx, id.unwrap_or(0)).await,
        "clearPeriod" => clear_period(ctx).await,
        "runNow" => run_now(ctx).await,
        _ => index(ctx).await,
    }
}

async fn index(mut ctx: Ctx) -> Response {
    if !ctx.is_admin() {
        return ctx.redirect("/accessReview/mine").await;
    }
    let db = &ctx.st.db.clone();
    let p_in = ctx.p("period");
    let period = if valid_period(&p_in) { p_in } else { current_period() };
    let plabel = period_label(&period);
    let enabled = setting_bool(db, "portal.access_review_enabled").await;
    let grace = grace_days(db).await;
    let rem = reminder_days(db).await;
    let exempt = exempt_roles(db).await;
    let cc = cc_addresses(db).await;
    let (in_scope, no_owner, role_counts) = scope_modules(db, &exempt).await;

    let periods: Vec<String> = db
        .rows("select distinct period from portal_module_access_review order by period desc", &db::no_params())
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| s(&r, "period"))
        .filter(|x| !x.is_empty())
        .collect();

    let reviews = db
        .rows(
            "select r.*, m.name as mod_name, m.title as mod_title, u.name as approver_name \
             from portal_module_access_review r \
             join portal_module m on m.id=r.module_id \
             left join portal_user u on u.id=r.approved_by_id \
             where r.period=:p order by lower(m.name)",
            &row! {"p"=>period.clone()},
        )
        .await
        .unwrap_or_default();

    let (mut c_app, mut c_pend, mut c_rev) = (0usize, 0usize, 0usize);
    for r in &reviews {
        match s(r, "status").as_str() {
            "Approved" => c_app += 1,
            "Pending" => c_pend += 1,
            "Revoked" => c_rev += 1,
            _ => {}
        }
    }

    let flash = s(&ctx.sess, "flash");
    let csrf = ctx.csrf_field();
    let mut period_opts = String::new();
    if !periods.contains(&period) {
        period_opts.push_str(&format!("<option value='{}' selected>{}</option>", esc(&period), esc(&plabel)));
    }
    for p in &periods {
        period_opts.push_str(&format!(
            "<option value='{}'{}>{}</option>",
            esc(p),
            if p == &period { " selected" } else { "" },
            esc(&period_label(p))
        ));
    }

    let mut rows_html = String::new();
    for (idx, r) in reviews.iter().enumerate() {
        let rid = i(r, "id").unwrap_or(0);
        let mid = i(r, "module_id").unwrap_or(0);
        let owners = module_owners(db, mid).await.iter().map(|u| esc(&s(u, "name"))).collect::<Vec<_>>().join(", ");
        let mtitle = { let t = s(r, "mod_title"); if t.trim().is_empty() { s(r, "mod_name") } else { t } };
        let st = s(r, "status");
        rows_html.push_str(&format!(
            "<tr><td class='ar-muted'>{}</td><td><a href='/accessReview/show/{}'>{}</a> <span class='ar-muted'>{}</span></td><td>{}</td><td><span class='ar-badge ar-{}'>{}</span></td><td class='text-nowrap'>{}</td><td>{}</td><td>{}</td></tr>",
            idx + 1, rid, esc(&mtitle), esc(&s(r, "mod_name")), owners, esc(&st.to_ascii_lowercase()), esc(&st),
            esc(&format_date_short(&s(r, "due_date"))), i(r, "reminders_sent").unwrap_or(0), esc(&s(r, "approver_name"))
        ));
    }
    if reviews.is_empty() {
        rows_html.push_str("<tr><td colspan='7' class='ar-muted'>No reviews opened for this period yet.</td></tr>");
    }

    let clear_form = if !reviews.is_empty() {
        let rev_box = if c_rev > 0 {
            format!(
                "<label class='ar-muted' title='A revoked review&#39;s history is the only record of the roles it removed; clearing it means they can no longer be restored from here.'><input type='checkbox' name='includeRevoked' value='yes'/> include {} revoked</label> ",
                c_rev
            )
        } else {
            String::new()
        };
        format!(
            "<form action='/accessReview/clearPeriod' method='post' class='d-inline ar-clear' onsubmit=\"return confirm('Delete all {} review(s) for {} and their history? Roles are not changed.');\">{csrf}<input type='hidden' name='period' value='{}'/>{rev_box}<button type='submit' class='btn btn-sm btn-outline-secondary'>Clear this period</button></form>",
            reviews.len(), esc(&plabel), esc(&period)
        )
    } else {
        String::new()
    };

    let no_owner_html = if !no_owner.is_empty() {
        let mut nr = String::new();
        for (idx, m) in no_owner.iter().enumerate() {
            let mid = i(m, "id").unwrap_or(0);
            let mn = s(m, "name");
            let st = { let x = s(m, "status"); if x.is_empty() { "Not recorded".into() } else { x } };
            nr.push_str(&format!(
                "<tr><td class='ar-muted'>{}</td><td><a href='/portalModule/show/{}'>{}</a> <span class='ar-muted'>{}</span></td><td>{}</td><td>{}</td></tr>",
                idx + 1, mid, esc(&mod_display(m)), esc(&mn), role_counts.get(&mn).copied().unwrap_or(0), esc(&st)
            ));
        }
        format!(
            "<div class='ar-card ar-card-quiet'><h3>Need an owner <span class='ar-muted'>{}</span></h3>\
             <p class='ar-muted'>These modules have roles but no owner, so they are not reviewed - and never revoked. Set an owner on the module and it joins the next run.</p>\
             <table class='table table-sm ar-table'><thead><tr><th>No.</th><th>Module</th><th>Reviewable roles</th><th>Status</th></tr></thead><tbody>{}</tbody></table></div>",
            no_owner.len(), nr
        )
    } else {
        String::new()
    };

    let cc_part = if cc.is_empty() { String::new() } else { format!(" &middot; cc {}", esc(&cc.join(", "))) };
    let rem_str = rem.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(", ");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class='alert alert-info'>{}</div>", esc(&flash)) };
    let sched_badge = if enabled {
        "<span class='ar-badge ar-approved'>Scheduled run on</span>".to_string()
    } else {
        "<span class='ar-badge ar-cancelled'>Scheduled run off</span> (setting <code>portal.access_review_enabled</code>)".to_string()
    };

    let body = format!(
        r#"<div id="content" role="main"><div class="container">
<section class="row"><div class="nav" role="navigation"><ul>
  <li><a class="list" href="/portalModule/index">Modules</a></li>
  <li><a class="list" href="/accessReview/mine">My access reviews</a></li>
</ul></div></section>
<section class="row"><div class="col-12 content ar-page">
{flash_html}
<div class="ar-head">
  <div>
    <h1>Access reviews &middot; {plabel}</h1>
    <div class="ar-sub">{sched_badge} &middot; grace {grace} days &middot; reminders on day {rem_str} &middot; never reviewed: {exempt}{cc_part}</div>
  </div>
  <div class="d-flex gap-2 align-items-center">
    <form action="/accessReview/index" method="get" class="d-inline">
      <select name="period" class="form-select form-select-sm" onchange="this.form.submit()">{period_opts}</select>
    </form>
    <form action="/accessReview/runNow" method="post" class="d-inline" onsubmit="return confirm('Run the access review now? It opens the current month reviews, sends any reminders that are due and revokes overdue reviews.');">
      {csrf}<button type="submit" class="btn btn-sm btn-outline-danger">Run now</button>
    </form>
    {clear_form}
  </div>
</div>
<div class="ar-stats">
  <div class="ar-stat"><div class="v">{in_scope}</div><div class="l">Modules in scope today</div></div>
  <div class="ar-stat"><div class="v">{nrev}</div><div class="l">Reviews this period</div></div>
  <div class="ar-stat"><div class="v">{c_app}</div><div class="l">Approved</div></div>
  <div class="ar-stat"><div class="v">{c_pend}</div><div class="l">Pending</div></div>
  <div class="ar-stat"><div class="v"{rev_style}>{c_rev}</div><div class="l">Revoked</div></div>
  <div class="ar-stat"><div class="v"{no_style}>{no_cnt}</div><div class="l">Need an owner</div></div>
</div>
<div class="ar-card">
  <h3>Reviews for {plabel}</h3>
  <table class="table table-sm ar-table">
    <thead><tr><th>No.</th><th>Module</th><th>Owners</th><th>Status</th><th>Due</th><th>Reminders</th><th>Approved by</th></tr></thead>
    <tbody>{rows_html}</tbody>
  </table>
</div>
{no_owner_html}
</div></section></div></div>{STYLES}"#,
        plabel = esc(&plabel),
        exempt = esc(&exempt.join(", ")),
        in_scope = in_scope.len(),
        nrev = reviews.len(),
        rev_style = if c_rev > 0 { " style='color:#c0392b'" } else { "" },
        no_style = if !no_owner.is_empty() { " style='color:#b7791f'" } else { "" },
        no_cnt = no_owner.len(),
    );
    ctx.html("Access review dashboard", body).await
}

async fn mine(ctx: Ctx) -> Response {
    let Some(uid) = ctx.uid() else { return ctx.need_login().await };
    let db = ctx.db();
    let reviews = db
        .rows(
            "select r.*, m.name as mod_name, m.title as mod_title \
             from portal_module_access_review r \
             join portal_module m on m.id=r.module_id \
             where r.module_id in (select module_id from portal_module_contact where role='Owner' and user_id=:u) \
             order by r.period desc, r.id desc limit 200",
            &row! {"u"=>uid},
        )
        .await
        .unwrap_or_default();
    let today = Local::now().date_naive();
    let mut pending: Vec<&Row> = reviews.iter().filter(|r| s(r, "status") == "Pending").collect();
    pending.sort_by_key(|r| s(r, "due_date"));
    let recent: Vec<&Row> = reviews.iter().filter(|r| s(r, "status") != "Pending").take(30).collect();

    let mut prow = String::new();
    for (idx, r) in pending.iter().enumerate() {
        let rid = i(r, "id").unwrap_or(0);
        let due_raw = s(r, "due_date");
        let urgent = due_raw.get(..10).and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()).map(|d| (d - today).num_days() <= 7).unwrap_or(false);
        let mtitle = { let t = s(r, "mod_title"); if t.trim().is_empty() { s(r, "mod_name") } else { t } };
        prow.push_str(&format!(
            "<tr><td class='ar-muted'>{}</td><td>{}</td><td>{}</td><td class='{}'>{}</td><td>{}</td><td class='text-end'><a href='/accessReview/show/{}' class='btn btn-sm btn-primary'>Review</a></td></tr>",
            idx + 1, esc(&mtitle), esc(&period_label(&s(r, "period"))), if urgent { "text-danger fw-bold" } else { "" },
            esc(&format_date_short(&due_raw)), i(r, "reminders_sent").unwrap_or(0), rid
        ));
    }
    if pending.is_empty() {
        prow.push_str("<tr><td colspan='6' class='ar-muted'>Nothing waiting - all your reviews are done.</td></tr>");
    }

    let recent_html = if !recent.is_empty() {
        let mut rr = String::new();
        for (idx, r) in recent.iter().enumerate() {
            let rid = i(r, "id").unwrap_or(0);
            let mtitle = { let t = s(r, "mod_title"); if t.trim().is_empty() { s(r, "mod_name") } else { t } };
            let st = s(r, "status");
            rr.push_str(&format!(
                "<tr><td class='ar-muted'>{}</td><td><a href='/accessReview/show/{}'>{}</a></td><td>{}</td><td><span class='ar-badge ar-{}'>{}</span></td><td>{}</td></tr>",
                idx + 1, rid, esc(&mtitle), esc(&period_label(&s(r, "period"))), esc(&st.to_ascii_lowercase()), esc(&st),
                esc(&format_date_short(&s(r, "closed_date")))
            ));
        }
        format!(
            "<div class='ar-card ar-card-quiet'><h3>Recent</h3><table class='table table-sm ar-table'>\
             <thead><tr><th>No.</th><th>Module</th><th>Period</th><th>Result</th><th>When</th></tr></thead><tbody>{rr}</tbody></table></div>"
        )
    } else {
        String::new()
    };

    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class='alert alert-info'>{}</div>", esc(&flash)) };
    let dash_btn = if ctx.is_admin() { "<a href='/accessReview/index' class='btn btn-sm btn-outline-primary'>Review dashboard</a>" } else { "" };

    let body = format!(
        r#"<div id="content" role="main"><div class="container"><section class="row"><div class="col-12 content ar-page">
{flash_html}
<div class="ar-head">
  <div><h1>My access reviews</h1><div class="ar-sub">Modules you own. Each month, confirm who has access to each of them.</div></div>
  {dash_btn}
</div>
<div class="ar-card">
  <h3>Waiting for you <span class="ar-muted">{pcnt}</span></h3>
  <table class="table table-sm ar-table">
    <thead><tr><th>No.</th><th>Module</th><th>Period</th><th>Due</th><th>Reminders</th><th></th></tr></thead>
    <tbody>{prow}</tbody>
  </table>
</div>
{recent_html}
</div></section></div></div>{STYLES}"#,
        pcnt = pending.len()
    );
    ctx.html("My access reviews", body).await
}

async fn show(mut ctx: Ctx, id: i64) -> Response {
    let db = &ctx.st.db.clone();
    let Ok(Some(review)) = db.get("portal_module_access_review", id).await else {
        ctx.flash("That access review does not exist.");
        return ctx.redirect("/accessReview/mine").await;
    };
    let mid = i(&review, "module_id").unwrap_or(0);
    let Ok(Some(module)) = db.get("portal_module", mid).await else {
        ctx.flash("Module not found.");
        return ctx.redirect("/accessReview/mine").await;
    };
    let mname = s(&module, "name");
    let act_ok = can_act(&ctx, mid).await;
    let can_view = act_ok || ctx.can_admin(&mname).await;
    if !can_view {
        ctx.flash("Only the owners of this module can open its access review.");
        return ctx.redirect("/accessReview/mine").await;
    }

    let status = s(&review, "status");
    let is_pending = status == "Pending";
    let editable = act_ok && is_pending;
    let period = s(&review, "period");
    let plabel = period_label(&period);
    let owners = module_owners(db, mid).await.iter().map(|u| esc(&s(u, "name"))).collect::<Vec<_>>().join(", ");
    let exempt = exempt_roles(db).await;

    let all_roles = db
        .rows(
            "select ur.id, ur.role, u.id as uid, u.name, u.userid, u.email, u.is_active \
             from user_role ur left join portal_user u on u.id=ur.user_id \
             where ur.module=:m order by lower(coalesce(u.name,'')), ur.role",
            &row! {"m"=>mname.clone()},
        )
        .await
        .unwrap_or_default();
    let rev_roles: Vec<&Row> = all_roles.iter().filter(|r| !is_exempt(&s(r, "role"), &exempt)).collect();
    let ex_roles: Vec<&Row> = all_roles.iter().filter(|r| is_exempt(&s(r, "role"), &exempt)).collect();
    let grantable = if editable { grantable_roles(db, &mname, &exempt).await } else { vec![] };

    let due_html = if is_pending {
        let due_raw = s(&review, "due_date");
        let today = Local::now().date_naive();
        let days_left = due_raw
            .get(..10)
            .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            .map(|d| (d - today).num_days())
            .unwrap_or(0);
        format!(
            "<div class='ar-due-n {}'>{}</div><div>day(s) left &middot; due {}</div>",
            if days_left <= 7 { "ar-urgent" } else { "" },
            days_left,
            esc(&format_date_short(&due_raw))
        )
    } else if status == "Approved" {
        let approver = match i(&review, "approved_by_id") {
            Some(aid) => db.get("portal_user", aid).await.ok().flatten().map(|u| s(&u, "name")).unwrap_or_default(),
            None => String::new(),
        };
        format!("Approved by {}<br/>{}", esc(&approver), esc(&format_datetime_short(&s(&review, "approved_date"))))
    } else {
        format!("{} {}", esc(&status), esc(&format_date_short(&s(&review, "closed_date"))))
    };

    let steps_html = if editable {
        format!(
            "<div class='ar-steps'><strong>What to do:</strong> check everyone below still needs their role. Remove anyone who should not have access, add anyone missing, then confirm and approve at the bottom. If this review is not approved by {}, every role listed here will be removed from the module automatically.</div>",
            esc(&format_date_short(&s(&review, "due_date")))
        )
    } else if is_pending && !act_ok {
        "<div class='ar-steps'>You can view this review; only the module's owners can change or approve it.</div>".into()
    } else {
        String::new()
    };

    // Approved snapshot card
    let snap_raw = s(&review, "approved_snapshot");
    let snap_html = if let Ok(Value::Object(snap)) = serde_json::from_str::<Value>(&snap_raw) {
        let sroles = snap.get("roles").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let mut sr_rows = String::new();
        for (idx, r) in sroles.iter().enumerate() {
            let active = r.get("active").and_then(|v| v.as_bool()).unwrap_or(true);
            let ex = r.get("exempt").and_then(|v| v.as_bool()).unwrap_or(false);
            sr_rows.push_str(&format!(
                "<tr><td class='ar-muted'>{}</td><td>{}{}</td><td>{}</td><td>{}</td><td>{}{}</td></tr>",
                idx + 1,
                esc(r.get("name").and_then(|v| v.as_str()).unwrap_or("")),
                if !active { " <span class='ar-flag'>inactive account</span>" } else { "" },
                esc(r.get("userID").and_then(|v| v.as_str()).unwrap_or("")),
                esc(r.get("email").and_then(|v| v.as_str()).unwrap_or("")),
                esc(r.get("role").and_then(|v| v.as_str()).unwrap_or("")),
                if ex { " <span class='ar-muted'>(not part of the review)</span>" } else { "" }
            ));
        }
        if sroles.is_empty() {
            sr_rows.push_str("<tr><td colspan='5' class='ar-muted'>The module had no roles when this was approved.</td></tr>");
        }
        let app_name = snap.get("approver").and_then(|a| a.get("name")).and_then(|v| v.as_str()).unwrap_or("");
        let app_uid = snap.get("approver").and_then(|a| a.get("userID")).and_then(|v| v.as_str()).unwrap_or("");
        let taken_at = snap.get("takenAt").and_then(|v| v.as_str()).unwrap_or("").replace('T', " ");
        format!(
            "<div class='ar-card'><h3>Roles as approved <span class='ar-muted'>{}</span></h3>\
             <p class='ar-muted'>Recorded when {} ({}) approved this review, on {}. Names and user IDs are as they were then.</p>\
             <table class='table table-sm ar-table'><thead><tr><th>No.</th><th>Name</th><th>User ID</th><th>E-mail</th><th>Role</th></tr></thead><tbody>{}</tbody></table></div>",
            sroles.len(), esc(app_name), esc(app_uid), esc(&taken_at), sr_rows
        )
    } else if status == "Approved" {
        "<div class='ar-steps'>This review was approved before the approved list was recorded on reviews, so only the module's current roles can be shown below.</div>".into()
    } else {
        String::new()
    };

    let csrf = ctx.csrf_field();
    let mut role_rows = String::new();
    for (idx, r) in rev_roles.iter().enumerate() {
        let urid = i(r, "id").unwrap_or(0);
        let inactive = r.get("is_active") == Some(&json!(false));
        let act_td = if editable {
            format!(
                "<td class='text-end'><form action='/accessReview/removeRole/{}' method='post' class='d-inline' onsubmit=\"return confirm('Remove {} from {}?');\">{csrf}<input type='hidden' name='userRoleId' value='{}'/><button type='submit' class='btn btn-sm btn-outline-danger'>Remove</button></form></td>",
                id, esc(&s(r, "role")).replace('\'', "\\'"), esc(&s(r, "name")).replace('\'', "\\'"), urid
            )
        } else {
            String::new()
        };
        role_rows.push_str(&format!(
            "<tr class='{}'><td class='ar-muted'>{}</td><td>{}{}</td><td>{}</td><td>{}</td><td>{}</td>{}</tr>",
            if inactive { "ar-inactive" } else { "" },
            idx + 1,
            esc(&s(r, "name")),
            if inactive { " <span class='ar-flag'>inactive account</span>" } else { "" },
            esc(&s(r, "userid")),
            esc(&s(r, "email")),
            esc(&s(r, "role")),
            act_td
        ));
    }
    if rev_roles.is_empty() {
        role_rows.push_str("<tr><td colspan='6' class='ar-muted'>No roles in this module.</td></tr>");
    }

    let add_role_html = if editable {
        if grantable.is_empty() {
            "<h4>Add a role</h4><p class='ar-muted'>This module's trackers define no user roles, so there is nothing to grant from here.</p>".into()
        } else {
            let opts: String = grantable.iter().map(|rn| format!("<option value='{}'>{}</option>", esc(rn), esc(rn))).collect();
            format!(
                "<h4>Add a role</h4><form action='/accessReview/addRole/{}' method='post' class='ar-add'>{csrf}\
                 <div id='addUser_div' class='ar-add-user fieldcontain' style='margin:0'><select class='ajax-select' data-url='/user/lookup' name='userId' id='addUser' style='width:100%;'></select></div>\
                 <select name='role' id='addRoleName' class='form-select form-select-sm'>{opts}</select>\
                 <button type='submit' class='btn btn-sm btn-primary'>Add</button></form>",
                id
            )
        }
    } else {
        String::new()
    };

    let exempt_html = if !ex_roles.is_empty() {
        let mut er = String::new();
        for (idx, r) in ex_roles.iter().enumerate() {
            er.push_str(&format!(
                "<tr><td class='ar-muted'>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                idx + 1, esc(&s(r, "name")), esc(&s(r, "userid")), esc(&s(r, "role"))
            ));
        }
        format!(
            "<div class='ar-card ar-card-quiet'><h3>Not part of the review <span class='ar-muted'>{}</span></h3>\
             <p class='ar-muted'>These roles are managed separately and are never removed by the review.</p>\
             <table class='table table-sm ar-table'><tbody>{er}</tbody></table></div>",
            ex_roles.len()
        )
    } else {
        String::new()
    };

    let approve_html = if editable {
        format!(
            "<div class='ar-card ar-approve'><h3>Approve</h3><form action='/accessReview/approve/{id}' method='post'>{csrf}\
             <label class='ar-confirm'><input type='checkbox' name='confirm' value='yes' required/> I have checked every user and role listed above and confirm they still need this access.</label>\
             <textarea name='note' rows='2' class='form-control' maxlength='2000' placeholder='Note (optional)'></textarea>\
             <button type='submit' class='btn btn-success mt-2'>Approve access for {}</button></form></div>",
            esc(&plabel)
        )
    } else {
        String::new()
    };

    let note_html = {
        let n = s(&review, "approval_note");
        if n.is_empty() { String::new() } else { format!("<div class='ar-card'><h3>Approval note</h3><p class='ar-pre'>{}</p></div>", esc(&n)) }
    };

    let restore_html = if ctx.is_admin() && status == "Revoked" {
        format!(
            "<div class='ar-card'><h3>Restore</h3><p>Puts back every role this review removed.</p>\
             <form action='/accessReview/restore/{id}' method='post' onsubmit=\"return confirm('Restore all roles removed by this review?');\">{csrf}\
             <button type='submit' class='btn btn-outline-primary'>Restore removed roles</button></form></div>"
        )
    } else {
        String::new()
    };

    let logs = db
        .rows(
            "select l.*, u.name as uname, a.name as aname \
             from portal_module_access_review_log l \
             left join portal_user u on u.id=l.user_id \
             left join portal_user a on a.id=l.actor_id \
             where l.review_id=:r order by l.id desc",
            &row! {"r"=>id},
        )
        .await
        .unwrap_or_default();
    let mut log_rows = String::new();
    for (idx, l) in logs.iter().enumerate() {
        let actor = { let a = s(l, "aname"); if a.is_empty() { "System".into() } else { a } };
        log_rows.push_str(&format!(
            "<tr><td class='ar-muted'>{}</td><td class='text-nowrap'>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            idx + 1, esc(&format_datetime_short(&s(l, "date_created"))), esc(&s(l, "action")),
            esc(&s(l, "uname")), esc(&s(l, "role")), esc(&actor), esc(&s(l, "detail"))
        ));
    }

    let flash = s(&ctx.sess, "flash");
    let err = ctx.sess.remove("flash_error").map(|v| db::vs(&v)).unwrap_or_default();
    let flash_html = format!(
        "{}{}",
        if flash.is_empty() { String::new() } else { format!("<div class='alert alert-success'>{}</div>", esc(&flash)) },
        if err.is_empty() { String::new() } else { format!("<div class='alert alert-danger'>{}</div>", esc(&err)) }
    );
    let dash_li = if ctx.is_admin() {
        format!("<li><a class='list' href='/accessReview/index?period={}'>Review dashboard</a></li>", enc(&period))
    } else {
        String::new()
    };

    let body = format!(
        r#"<div id="content" role="main"><div class="container">
<section class="row"><div class="nav" role="navigation"><ul>
  <li><a class="list" href="/accessReview/mine">My access reviews</a></li>
  {dash_li}
</ul></div></section>
<section class="row"><div class="col-12 content ar-page">
{flash_html}
<div class="ar-head">
  <div>
    <h1>{mdisp} <span class="ar-badge ar-{stlow}">{st}</span></h1>
    <div class="ar-sub">Access review for <strong>{plabel}</strong> &middot; <code>{mname}</code> &middot; owners: {owners}</div>
  </div>
  <div class="ar-due">{due_html}</div>
</div>
{steps_html}
{snap_html}
<div class="ar-card">
  <h3>{rhead} <span class="ar-muted">{rcnt}</span></h3>
  <table class="table table-sm ar-table">
    <thead><tr><th>No.</th><th>Name</th><th>User ID</th><th>E-mail</th><th>Role</th>{act_th}</tr></thead>
    <tbody>{role_rows}</tbody>
  </table>
  {add_role_html}
</div>
{exempt_html}
{approve_html}
{note_html}
{restore_html}
<div class="ar-card">
  <h3>History</h3>
  <table class="table table-sm ar-table">
    <thead><tr><th>No.</th><th>When</th><th>What</th><th>User</th><th>Role</th><th>By</th><th>Detail</th></tr></thead>
    <tbody>{log_rows}</tbody>
  </table>
</div>
</div></section></div></div>{STYLES}{form_js}"#,
        mdisp = esc(&mod_display(&module)),
        stlow = esc(&status.to_ascii_lowercase()),
        st = esc(&status),
        plabel = esc(&plabel),
        mname = esc(&mname),
        rhead = if is_pending { "Roles under review" } else { "Roles in the module now" },
        rcnt = rev_roles.len(),
        act_th = if editable { "<th></th>" } else { "" },
        form_js = if editable && !grantable.is_empty() { crate::tags::form_js() } else { "" },
    );
    ctx.html(&format!("Access review - {}", mod_display(&module)), body).await
}

async fn add_role(mut ctx: Ctx, id: i64) -> Response {
    if ctx.method != "POST" {
        return ctx.redirect(&format!("/accessReview/show/{}", id)).await;
    }
    let db = ctx.db();
    let Ok(Some(review)) = db.get("portal_module_access_review", id).await else { return ctx.redirect("/accessReview/mine").await };
    let mid = i(&review, "module_id").unwrap_or(0);
    let Ok(Some(module)) = db.get("portal_module", mid).await else { return ctx.redirect("/accessReview/mine").await };
    let mname = s(&module, "name");
    if !can_act(&ctx, mid).await || s(&review, "status") != "Pending" {
        ctx.sess.insert("flash_error".into(), json!("Cannot modify this review."));
        return ctx.redirect(&format!("/accessReview/show/{}", id)).await;
    }
    let exempt = exempt_roles(db).await;
    let role = ctx.p("role").trim().to_string();
    let uid = ctx.pi("userId");
    let target = match uid {
        Some(x) => db.get("portal_user", x).await.ok().flatten(),
        None => None,
    };
    let grantable = grantable_roles(db, &mname, &exempt).await;
    if let Some(t) = target {
        let tid = i(&t, "id").unwrap_or(0);
        if role.is_empty() || !grantable.contains(&role) {
            ctx.sess.insert("flash_error".into(), json!(format!("'{}' is not a role this module offers.", role)));
        } else if db.count("select count(*) from user_role where user_id=:u and module=:m and role=:r", &row! {"u"=>tid,"m"=>mname.clone(),"r"=>role.clone()}).await.unwrap_or(0) > 0 {
            ctx.sess.insert("flash_error".into(), json!(format!("{} already has {}.", s(&t, "name"), role)));
        } else {
            let _ = db.insert("user_role", &row! {"user_id"=>tid,"module"=>mname,"role"=>role.clone()}).await;
            review_log(db, id, "RoleAdded", Some(tid), Some(&role), ctx.uid(), None).await;
            ctx.flash(&format!("Added {} for {}.", role, s(&t, "name")));
        }
    } else {
        ctx.sess.insert("flash_error".into(), json!("Choose a user."));
    }
    ctx.redirect(&format!("/accessReview/show/{}", id)).await
}

async fn remove_role(mut ctx: Ctx, id: i64) -> Response {
    if ctx.method != "POST" {
        return ctx.redirect(&format!("/accessReview/show/{}", id)).await;
    }
    let db = ctx.db();
    let Ok(Some(review)) = db.get("portal_module_access_review", id).await else { return ctx.redirect("/accessReview/mine").await };
    let mid = i(&review, "module_id").unwrap_or(0);
    let Ok(Some(module)) = db.get("portal_module", mid).await else { return ctx.redirect("/accessReview/mine").await };
    let mname = s(&module, "name");
    if !can_act(&ctx, mid).await || s(&review, "status") != "Pending" {
        ctx.sess.insert("flash_error".into(), json!("Cannot modify this review."));
        return ctx.redirect(&format!("/accessReview/show/{}", id)).await;
    }
    let exempt = exempt_roles(db).await;
    if let Some(urid) = ctx.pi("userRoleId") {
        if let Ok(Some(ur)) = db.get("user_role", urid).await {
            let role = s(&ur, "role");
            if s(&ur, "module") == mname && !is_exempt(&role, &exempt) {
                let target_uid = i(&ur, "user_id");
                let uname = match target_uid {
                    Some(u) => db.get("portal_user", u).await.ok().flatten().map(|x| s(&x, "name")).unwrap_or_default(),
                    None => String::new(),
                };
                review_log(db, id, "RoleRemoved", target_uid, Some(&role), ctx.uid(), None).await;
                let _ = db.delete("user_role", urid).await;
                ctx.flash(&format!("Removed {} for {}.", role, uname));
            }
        }
    }
    ctx.redirect(&format!("/accessReview/show/{}", id)).await
}

async fn approve(mut ctx: Ctx, id: i64) -> Response {
    if ctx.method != "POST" {
        return ctx.redirect(&format!("/accessReview/show/{}", id)).await;
    }
    let db = ctx.db();
    let Ok(Some(review)) = db.get("portal_module_access_review", id).await else { return ctx.redirect("/accessReview/mine").await };
    let mid = i(&review, "module_id").unwrap_or(0);
    let Ok(Some(module)) = db.get("portal_module", mid).await else { return ctx.redirect("/accessReview/mine").await };
    if !can_act(&ctx, mid).await || s(&review, "status") != "Pending" {
        ctx.sess.insert("flash_error".into(), json!("Cannot approve this review."));
        return ctx.redirect(&format!("/accessReview/show/{}", id)).await;
    }
    if ctx.p("confirm") != "yes" {
        ctx.sess.insert("flash_error".into(), json!("Tick the confirmation box to approve."));
        return ctx.redirect(&format!("/accessReview/show/{}", id)).await;
    }
    let mname = s(&module, "name");
    let exempt = exempt_roles(db).await;
    let roles = db
        .rows(
            "select u.id as uid, u.userid, u.name, u.email, u.is_active, ur.role from user_role ur left join portal_user u on u.id=ur.user_id where ur.module=:m order by lower(coalesce(u.name,'')), ur.role",
            &row! {"m"=>mname.clone()},
        )
        .await
        .unwrap_or_default();
    let snap_roles: Vec<Value> = roles
        .iter()
        .map(|r| {
            let rn = s(r, "role");
            json!({
                "uid": i(r, "uid"),
                "userID": s(r, "userid"),
                "name": s(r, "name"),
                "email": s(r, "email"),
                "active": r.get("is_active") != Some(&json!(false)),
                "role": rn,
                "exempt": is_exempt(&rn, &exempt)
            })
        })
        .collect();
    let actor_u = ctx.user.as_ref();
    let snapshot = json!({
        "takenAt": Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        "module": mname,
        "approver": {
            "id": ctx.uid(),
            "userID": actor_u.map(|u| s(u, "userid")).unwrap_or_default(),
            "name": actor_u.map(|u| s(u, "name")).unwrap_or_default()
        },
        "roles": snap_roles
    })
    .to_string();
    let note = ctx.p("note").trim().chars().take(2000).collect::<String>();
    let now = now_str();
    let _ = db
        .update(
            "portal_module_access_review",
            id,
            &row! {
                "status"=>"Approved",
                "approved_by_id"=>ctx.uid(),
                "approved_date"=>now.clone(),
                "closed_date"=>now.clone(),
                "approval_note"=>if note.is_empty() { Value::Null } else { json!(note) },
                "approved_snapshot"=>snapshot.clone(),
                "last_updated"=>now.clone()
            },
        )
        .await;
    let changes = db
        .count("select count(*) from portal_module_access_review_log where review_id=:r and action in ('RoleAdded','RoleRemoved')", &row! {"r"=>id})
        .await
        .unwrap_or(0);
    let detail = if changes > 0 { format!("Approved with {} change(s)", changes) } else { "Approved without changes".into() };
    review_log(db, id, "Approved", None, None, ctx.uid(), Some(&detail)).await;
    ctx.flash(&format!("Approved. Thank you - the {} review is complete.", period_label(&s(&review, "period"))));
    ctx.redirect(&format!("/accessReview/show/{}", id)).await
}

async fn restore(mut ctx: Ctx, id: i64) -> Response {
    if ctx.method != "POST" || !ctx.is_admin() {
        return ctx.redirect(&format!("/accessReview/show/{}", id)).await;
    }
    let db = ctx.db();
    let Ok(Some(review)) = db.get("portal_module_access_review", id).await else { return ctx.redirect("/accessReview/mine").await };
    if s(&review, "status") != "Revoked" {
        ctx.sess.insert("flash_error".into(), json!("Only a revoked review can be restored."));
        return ctx.redirect(&format!("/accessReview/show/{}", id)).await;
    }
    let mid = i(&review, "module_id").unwrap_or(0);
    let mname = db.get("portal_module", mid).await.ok().flatten().map(|m| s(&m, "name")).unwrap_or_default();
    let logs = db
        .rows("select user_id, role from portal_module_access_review_log where review_id=:r and action='RoleRevoked'", &row! {"r"=>id})
        .await
        .unwrap_or_default();
    let mut put = 0;
    for l in logs {
        let (Some(uid), role) = (i(&l, "user_id"), s(&l, "role")) else { continue };
        if !role.is_empty() && db.count("select count(*) from user_role where user_id=:u and module=:m and role=:r", &row! {"u"=>uid,"m"=>mname.clone(),"r"=>role.clone()}).await.unwrap_or(0) == 0 {
            let _ = db.insert("user_role", &row! {"user_id"=>uid,"module"=>mname.clone(),"role"=>role}).await;
            put += 1;
        }
    }
    review_log(db, id, "Restored", None, None, ctx.uid(), Some(&format!("{} role(s) restored", put))).await;
    ctx.flash(&format!("{} role(s) restored.", put));
    ctx.redirect(&format!("/accessReview/show/{}", id)).await
}

async fn clear_period(mut ctx: Ctx) -> Response {
    let period = ctx.p("period");
    if ctx.is_admin() && ctx.method == "POST" && valid_period(&period) {
        let db = ctx.db();
        let include_revoked = ctx.p("includeRevoked") == "yes";
        let revs = db.rows("select id, status from portal_module_access_review where period=:p", &row! {"p"=>period.clone()}).await.unwrap_or_default();
        let mut blocked = 0;
        for r in &revs {
            if s(r, "status") == "Revoked" && !include_revoked {
                let rid = i(r, "id").unwrap_or(0);
                if db.count("select count(*) from portal_module_access_review_log where review_id=:r and action='Restored'", &row! {"r"=>rid}).await.unwrap_or(0) == 0 {
                    blocked += 1;
                }
            }
        }
        if blocked > 0 {
            ctx.flash(&format!("{} revoked review(s) in {} still hold the only record of removed roles. Restore them first or tick include revoked.", blocked, period_label(&period)));
        } else {
            let n = revs.len();
            let _ = db.exec("delete from portal_module_access_review_log where review_id in (select id from portal_module_access_review where period=:p)", &row! {"p"=>period.clone()}).await;
            let _ = db.exec("delete from portal_module_access_review where period=:p", &row! {"p"=>period.clone()}).await;
            ctx.flash(&format!("Cleared {} review(s) for {}. Press Run now to open the period again.", n, period_label(&period)));
        }
    }
    ctx.redirect(&format!("/accessReview/index?period={}", enc(&period))).await
}

async fn run_now(mut ctx: Ctx) -> Response {
    if ctx.is_admin() && ctx.method == "POST" {
        let db = ctx.db();
        let exempt = exempt_roles(db).await;
        let grace = grace_days(db).await;
        let (in_scope, no_owner, _) = scope_modules(db, &exempt).await;
        let period = current_period();
        let now = now_str();
        let due = (Local::now().date_naive() + Duration::days(grace)).format("%Y-%m-%d 00:00:00").to_string();
        let existing: HashSet<i64> = db
            .rows("select module_id from portal_module_access_review where period=:p", &row! {"p"=>period.clone()})
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| i(&r, "module_id"))
            .collect();
        let mut opened = 0;
        for m in &in_scope {
            let mid = i(m, "id").unwrap_or(0);
            if existing.contains(&mid) {
                continue;
            }
            if let Ok(rid) = db
                .insert(
                    "portal_module_access_review",
                    &row! {
                        "module_id"=>mid,
                        "period"=>period.clone(),
                        "status"=>"Pending",
                        "opened_date"=>now.clone(),
                        "due_date"=>due.clone(),
                        "reminders_sent"=>0,
                        "date_created"=>now.clone(),
                        "last_updated"=>now.clone()
                    },
                )
                .await
            {
                review_log(db, rid, "Opened", None, None, ctx.uid(), Some(&format!("due {}", format_date_short(&due)))).await;
                opened += 1;
            }
        }
        ctx.flash(&format!("Run complete: {} opened; {} module(s) need an owner.", opened, no_owner.len()));
    }
    ctx.redirect("/accessReview/index").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_and_date_formatting() {
        assert_eq!(period_label("2026-11"), "November 2026");
        assert_eq!(period_label("2026-01"), "January 2026");
        assert_eq!(period_label("bad"), "bad");
        assert_eq!(format_date_short("2026-11-22 05:49:48"), "22 Nov 2026");
        assert_eq!(format_datetime_short("2026-11-22 05:49:48"), "22 Nov 2026, 05:49");
    }
}
