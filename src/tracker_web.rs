//! HTTP handlers for tracker data: list, Excel export, create, display,
//! workflow transitions, audit-trail updates and the JSON lookups used by forms.

use crate::db::{self, b, csv, has, i, qcol, s, vs, Row};
use crate::page::{find_page, make_sc, render_page, result_redirect};
use crate::row;
use crate::script::Sc;
use crate::tags;
use crate::tracker::{other_format, Tracker, Trans};
use crate::web::{self, esc, Ctx, ExcelJob};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};

fn tracker_json(t: &Tracker) -> Value {
    let mut r = t.row.clone();
    r.insert("fields".into(), Value::Array(t.fields.iter().cloned().map(Value::Object).collect()));
    r.insert("statuses".into(), Value::Array(t.statuses.iter().cloned().map(Value::Object).collect()));
    r.insert("data_table".into(), json!(t.data_table()));
    Value::Object(r)
}

async fn load(ctx: &mut Ctx, module: &str, slug: &str) -> Option<Tracker> {
    ctx.module = Some(module.to_string());
    ctx.slug = Some(slug.to_string());
    ctx.params.insert("module".into(), json!(module));
    ctx.params.insert("slug".into(), json!(slug));
    let t = Tracker::load(ctx.db(), module, slug).await?;
    if has(&t.row, "side_menu") {
        ctx.side_menu = Some(s(&t.row, "side_menu"));
    }
    Some(t)
}

/// The Grails portal's tracker page frame (list.gsp / display_data.gsp / transition.gsp).
fn wrap(_t: &Tracker, nav: &str, title: &str, body: &str, flash: &str) -> String {
    format!(
        "\n    <div id=\"content\" role=\"main\">\n        <div class=\"container\">\n            <section class=\"row\">\n                <a href=\"#list-portalTracker\" class=\"skip\" tabindex=\"-1\">Skip to content&hellip;</a>\n                {}\n            </section>\n            <section class=\"row\">\n                <div id=\"list-portalTracker\" class=\"col-12 content scaffold-list\" role=\"main\">\n                    <h1>{}</h1>\n                    {}\n                    {}\n                </div>\n            </section>\n        </div>\n    </div>\n",
        nav,
        esc(title),
        if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(flash)) },
        body
    )
}

async fn page_error(ctx: &Ctx, t: &Tracker, what: &str, e: &anyhow::Error) -> String {
    web::log_error(ctx.db(), "portalTracker", what, &e.to_string(), Some(&t.module), Some(&t.slug), ctx.uid()).await;
    if ctx.is_admin() {
        format!("<div class='alert alert-danger'><b>{}</b><pre style='white-space:pre-wrap'>{}</pre></div>", esc(what), esc(&e.to_string()))
    } else {
        "<div class='alert alert-danger'>There was an error displaying this page.</div>".into()
    }
}

// ---------------- excel export ----------------

async fn excel_value(sc: &Sc, t: &Tracker, f: &Row, v: &Value, r: &Row) -> Value {
    match s(f, "field_type").as_str() {
        "Integer" | "Number" => v.clone(),
        "Checkbox" => json!(if v == &json!(true) { "Yes" } else { "No" }),
        "User" | "TreeNode" | "Branch" | "BelongsTo" | "Drop Down" | "MultiSelect" | "Date" | "DateTime" => {
            let html = tags::display_field(sc, t, f, v, r, true).await;
            json!(html.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'"))
        }
        _ => json!(vs(v)),
    }
}

/// Write the tracker's list (with the current filters) to an .xlsx file at `path`.
/// Rows are read through a database cursor 2,000 at a time and streamed into the
/// zip file by xlsxstream on a blocking thread, so memory stays flat however many
/// rows there are (a new sheet starts every 1,048,575 rows). Returns the number of data rows written.
async fn build_excel(sc: &Sc, t: &Tracker, params: &Row, path: &str) -> anyhow::Result<u64> {
    let db = sc.db();
    let names = { let e = csv(&s(&t.row, "excelfields")); if e.is_empty() { csv(&s(&t.row, "listfields")) } else { e } };
    let fields: Vec<Row> = names.iter().filter_map(|n| t.field(n).cloned()).filter(|f| !matches!(s(f, "field_type").as_str(), "File" | "HasMany" | "FieldGroup")).collect();
    let audit = b(&t.row, "excel_audit") && t.tracker_type == "Tracker";
    let mut header: Vec<String> = fields.iter().map(|f| s(f, "label")).collect();
    if t.tracker_type != "DataStore" {
        header.push("Status".into());
    }
    if audit {
        header.push("Audit Trail".into());
    }
    let mut p = params.clone();
    for k in ["max", "offset", "excel", "excel_token", "excel_status"] {
        p.remove(k);
    }
    let sheet_name: String = t.name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' }).collect::<String>().trim().chars().take(28).collect();
    let sheet_name = if sheet_name.is_empty() { "Sheet".to_string() } else { sheet_name };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<Vec<Value>>>(4);
    let out = path.to_string();
    let writer = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        let mut x = crate::xlsxstream::XlsxStream::create(&out, &sheet_name, header)?;
        let mut total = 0u64;
        while let Some(chunk) = rx.blocking_recv() {
            for line in chunk {
                x.write_row(&line)?;
                total += 1;
            }
        }
        x.finish()?;
        Ok(total)
    });
    let mut cur = db.cursor(&t.list_query(sc, &p, "select *", false).await, 2000).await?;
    loop {
        let rows = cur.next().await?;
        if rows.is_empty() {
            break;
        }
        let mut out: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
        for r in &rows {
            let mut line = Vec::with_capacity(fields.len() + 2);
            for f in &fields {
                line.push(excel_value(sc, t, f, r.get(&s(f, "name")).unwrap_or(&Value::Null), r).await);
            }
            if t.tracker_type != "DataStore" {
                line.push(json!(s(r, "record_status")));
            }
            if audit {
                let trail = db
                    .rows(&format!("select tr.update_date, tr.description, u.emp_name from {} tr left join iap_user u on u.id=tr.updater_id where tr.record_id=:id order by tr.id", qcol(&t.trail_table())), &row! {"id"=>i(r, "id")})
                    .await
                    .unwrap_or_default();
                line.push(json!(trail.iter().map(|x| format!("{} {}: {}", s(x, "update_date"), s(x, "emp_name"), s(x, "description"))).collect::<Vec<_>>().join("\n")));
            }
            out.push(line);
        }
        if tx.send(out).await.is_err() {
            break; // the writer failed; its error is reported below
        }
    }
    drop(tx);
    drop(cur);
    writer.await?
}

async fn excel(mut ctx: Ctx, t: Tracker) -> Response {
    let token = ctx.p("excel_token");
    if !token.is_empty() {
        if ctx.params.contains_key("excel_status") {
            let v = match ctx.st.excel_jobs.lock().unwrap().get(&token) {
                None => json!({"ready": false, "error": "Job not found or expired"}),
                Some(j) => match &j.error {
                    Some(e) => json!({"ready": false, "error": e}),
                    None => json!({"ready": j.done}),
                },
            };
            return ctx.json(v).await;
        }
        let job = ctx.st.excel_jobs.lock().unwrap().remove(&token);
        return match job {
            Some(ExcelJob { done: true, file: Some(path), filename, .. }) => {
                ctx.file_stream("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", Some(&format!("{}.xlsx", filename)), &path, true).await
            }
            _ => ctx.status(StatusCode::NOT_FOUND, "Export not ready or expired").await,
        };
    }
    if !b(&t.row, "downloadexcel") && !ctx.can_admin(&t.module).await {
        return ctx.status(StatusCode::FORBIDDEN, "Excel download is not enabled for this tracker").await;
    }
    let token = web::token();
    ctx.st.excel_jobs.lock().unwrap().insert(token.clone(), ExcelJob { done: false, file: None, error: None, filename: t.slug.clone() });
    let sc = make_sc(&mut ctx);
    let st = ctx.st.clone();
    let params = ctx.params.clone();
    let uid = ctx.uid();
    let tk = token.clone();
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let start = web::now_str();
        let dir = format!("{}/tmp", st.cfg.upload_dir);
        let _ = tokio::fs::create_dir_all(&dir).await;
        let path = format!("{}/{}.xlsx", dir, tk);
        let (file, error) = match build_excel(&sc, &t, &params, &path).await {
            Ok(_) => (Some(path), None),
            Err(e) => {
                let _ = tokio::fs::remove_file(&path).await;
                (None, Some(e.to_string()))
            }
        };
        let _ = st
            .db
            .insert(
                "portal_background_trail",
                &row! {"name"=>"portalTracker.excel","details"=>format!("{}:{}", t.module, t.slug),"user_id"=>uid,"start_date"=>start,
                "duration_ms"=>started.elapsed().as_millis() as i64,"success"=>error.is_none(),"error"=>error.clone()},
            )
            .await;
        if let Some(j) = st.excel_jobs.lock().unwrap().get_mut(&tk) {
            j.done = true;
            j.file = file;
            j.error = error;
        }
    });
    let html = format!(
        r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>Generating Excel...</title><style>
body{{font-family:Arial,sans-serif;text-align:center;padding:60px 20px;background:#f5f5f5;}}
.box{{background:#fff;border-radius:8px;padding:40px;display:inline-block;box-shadow:0 2px 8px rgba(0,0,0,.1);min-width:320px;}}
.spinner{{display:inline-block;width:40px;height:40px;border:4px solid #ddd;border-top-color:#0078d4;border-radius:50%;animation:spin .8s linear infinite;margin-bottom:16px;}}
@keyframes spin{{to{{transform:rotate(360deg);}}}} .error{{color:#c00;}}</style></head><body><div class="box"><div class="spinner" id="spinner"></div>
<h2>Generating Excel Report</h2><p id="status">Please wait while your report is being prepared&hellip;</p></div><script>
var token='{}';var baseUrl=window.location.href.split('?')[0];
var checkUrl=baseUrl+'?excel=1&excel_token='+token+'&excel_status=1';var downloadUrl=baseUrl+'?excel=1&excel_token='+token;
function check(){{fetch(checkUrl,{{credentials:'same-origin'}}).then(function(r){{return r.json();}}).then(function(d){{
if(d.ready){{document.getElementById('status').textContent='Report ready! Downloading…';document.getElementById('spinner').style.display='none';
var a=document.createElement('a');a.href=downloadUrl;a.download='';document.body.appendChild(a);a.click();setTimeout(function(){{window.location.href=baseUrl;}},1500);}}
else if(d.error){{document.getElementById('spinner').style.display='none';document.getElementById('status').innerHTML='<span class="error">Error: '+d.error+'</span>';}}
else{{setTimeout(check,1500);}}}}).catch(function(){{setTimeout(check,2000);}});}}
setTimeout(check,800);</script></body></html>"#,
        token
    );
    ctx.raw_html(html).await
}

// ---------------- list / create / display ----------------

pub async fn list(mut ctx: Ctx, module: &str, slug: &str) -> Response {
    let Some(t) = load(&mut ctx, module, slug).await else {
        ctx.flash("Tracker Not Found");
        return ctx.redirect("/").await;
    };
    if b(&t.row, "require_login") && ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if ctx.params.contains_key("excel") {
        return excel(ctx, t).await;
    }
    let sc = make_sc(&mut ctx);
    sc.set_cur(module, slug, None, None);
    if let Some(page) = find_page(ctx.db(), module, &format!("{}_list", slug)).await {
        let roles = t.user_roles(&sc, None).await;
        let vars = row! {"tracker"=>tracker_json(&t),"userroles"=>roles,"flash"=>s(&ctx.sess, "flash")};
        return match render_page(&sc, &page, vars).await {
            Ok((title, body)) => crate::page::page_response(ctx, &page, &title, body).await,
            Err(e) => {
                let h = page_error(&ctx, &t, "list page", &e).await;
                ctx.html(&t.name, h).await
            }
        };
    }
    let nav = tags::tracker_nav(&sc, &t, true).await;
    let body = format!("{}{}", tags::tracker_filter(&sc, &t).await, tags::tracker_list(&sc, &t, &Row::new()).await);
    let flash = s(&ctx.sess, "flash");
    ctx.html(&format!("List {}", t.name), wrap(&t, &nav, &format!("List {}", t.name), &body, &flash)).await
}

pub async fn create(mut ctx: Ctx, module: &str, slug: &str) -> Response {
    let Some(t) = load(&mut ctx, module, slug).await else {
        ctx.flash("Tracker Not Found");
        return ctx.redirect("/").await;
    };
    match t.new_transition() {
        Some(nt) => {
            let mut keep = Row::new();
            for (k, v) in &ctx.params {
                let is_field = t.field(k).map(|f| b(f, "url_value") || b(f, "params_override") || s(f, "field_type") == "BelongsTo").unwrap_or(false);
                if is_field || matches!(k.as_str(), "backto" | "backtoid" | "backtransition") {
                    keep.insert(k.clone(), v.clone());
                }
            }
            let q = web::qs(&keep, &[]);
            let to = format!("/{}/{}/{}{}", t.module, t.slug, nt.url_name(), if q.is_empty() { String::new() } else { format!("?{}", q) });
            ctx.redirect(&to).await
        }
        None => {
            ctx.flash("This tracker has no transition for creating records");
            let to = t.url("list");
            ctx.redirect(&to).await
        }
    }
}

pub async fn display(mut ctx: Ctx, module: &str, slug: &str, id: i64) -> Response {
    let Some(t) = load(&mut ctx, module, slug).await else {
        ctx.flash("Tracker Not Found");
        return ctx.redirect("/").await;
    };
    if (b(&t.row, "require_login") || !b(&t.row, "anonymous_view")) && ctx.user.is_none() {
        return ctx.need_login().await;
    }
    ctx.params.insert("id".into(), json!(id));
    let Some(datas) = t.get(ctx.db(), id).await else {
        let to = t.url("list");
        return ctx.redirect(&to).await;
    };
    let sc = make_sc(&mut ctx);
    sc.set_cur(module, slug, Some(id), None);
    if !tags::can_view(&sc, &t, &datas).await {
        // as the Grails portals' trackerDisplay tag: say so, then go to the list
        let to = t.url("list");
        let h = format!("<script>alert('You are not authorised to view that record');window.location={};</script>", serde_json::to_string(&to).unwrap_or_default());
        return ctx.html(&t.name, h).await;
    }
    let status = s(&datas, "record_status");
    let mut page = None;
    if !status.is_empty() {
        page = find_page(ctx.db(), module, &format!("{}_show_{}", slug, status.replace(' ', "_").to_lowercase())).await;
    }
    if page.is_none() {
        page = find_page(ctx.db(), module, &format!("{}_show_default", slug)).await;
    }
    if let Some(page) = page {
        let roles = t.user_roles(&sc, Some(&datas)).await;
        let vars = row! {"tracker"=>tracker_json(&t),"datas"=>Value::Object(datas.clone()),"defaultdata"=>Value::Object(datas.clone()),"userroles"=>roles,"flash"=>s(&ctx.sess, "flash")};
        return match render_page(&sc, &page, vars).await {
            Ok((title, body)) => crate::page::page_response(ctx, &page, &title, body).await,
            Err(e) => {
                let h = page_error(&ctx, &t, "show page", &e).await;
                ctx.html(&t.name, h).await
            }
        };
    }
    let nav = tags::tracker_nav(&sc, &t, false).await;
    let body = format!("{}{}", tags::tracker_display(&sc, &t, id, &Row::new()).await, tags::tracker_updates(&sc, &t, id).await);
    let flash = s(&ctx.sess, "flash");
    ctx.html(&format!("Display {}", t.name), wrap(&t, &nav, &format!("Display {}", t.name), &body, &flash)).await
}

// ---------------- transitions ----------------

async fn transition_form(mut ctx: Ctx, sc: Sc, t: Tracker, tr: Trans, id: Option<i64>, datas: Option<Row>) -> Response {
    sc.set_cur(&t.module, &t.slug, id, Some(tr.name.clone()));
    let mut page = find_page(ctx.db(), &t.module, &format!("{}_{}", t.slug, tr.url_name())).await;
    if page.is_none() && id.is_some() && !tr.name.eq_ignore_ascii_case("delete") {
        page = find_page(ctx.db(), &t.module, &format!("{}_edit", t.slug)).await;
    }
    if let Some(page) = page {
        let roles = t.user_roles(&sc, datas.as_ref()).await;
        let mut trow = tr.row.clone();
        trow.insert("url_name".into(), json!(tr.url_name()));
        let vars = row! {"tracker"=>tracker_json(&t),"datas"=>datas.clone().map(Value::Object).unwrap_or(Value::Null),"transition"=>Value::Object(trow),"userroles"=>roles,"flash"=>s(&ctx.sess, "flash")};
        return match render_page(&sc, &page, vars).await {
            Ok((title, body)) => crate::page::page_response(ctx, &page, &title, body).await,
            Err(e) => {
                let h = page_error(&ctx, &t, "transition page", &e).await;
                ctx.html(&t.name, h).await
            }
        };
    }
    let nav = tags::tracker_nav(&sc, &t, false).await;
    let body = format!("{}{}\n                     <br/><br/><br/><br/><br/>", tags::tracker_info(&sc, &t, &tr, id).await, tags::tracker_form(&sc, &t, &tr, id, &Row::new()).await);
    let title = format!("{} {}", tr.label(), t.name);
    let flash = s(&ctx.sess, "flash");
    ctx.html(&title, wrap(&t, &nav, &title, &body, &flash)).await
}

pub async fn transition_get(mut ctx: Ctx, module: &str, slug: &str, tname: &str, id: Option<i64>) -> Response {
    let Some(t) = load(&mut ctx, module, slug).await else {
        ctx.flash("Tracker Not Found");
        return ctx.redirect("/").await;
    };
    let datas = match id {
        Some(rid) => t.get(ctx.db(), rid).await,
        None => None,
    };
    if id.is_some() && datas.is_none() {
        let to = t.url("list");
        return ctx.redirect(&to).await;
    }
    if let Some(rid) = id {
        ctx.params.insert("id".into(), json!(rid));
    }
    let sc = make_sc(&mut ctx);
    match t.resolve(&sc, tname, datas.as_ref()).await {
        Some(tr) => transition_form(ctx, sc, t, tr, id, datas).await,
        None => {
            if ctx.user.is_none() {
                return ctx.need_login().await;
            }
            ctx.flash("You are not authorized to do that");
            let to = match id {
                Some(rid) => format!("{}/{}", t.url("display"), rid),
                None => t.url("list"),
            };
            ctx.redirect(&to).await
        }
    }
}

async fn eval_recipients(sc: &Sc, code: &str, datas: &Row) -> String {
    if code.trim().is_empty() {
        return String::new();
    }
    match sc.eval(code, &row! {"datas"=>Value::Object(datas.clone())}) {
        Ok(Value::Array(a)) => a.iter().map(vs).filter(|x| !x.is_empty()).collect::<Vec<_>>().join(","),
        Ok(v) => vs(&v),
        // a plain address list is fine too
        Err(_) if code.contains('@') && !code.contains('(') => code.trim().to_string(),
        Err(e) => {
            web::log_error(sc.db(), "tracker", "email recipients", &e.to_string(), None, None, None).await;
            String::new()
        }
    }
}

/// Render a tracker e-mail (body page + recipients) and send it.
pub async fn send_tracker_email(sc: &Sc, t: &Tracker, email: &Row, datas: &Row) {
    let to = eval_recipients(sc, &s(email, "emailto"), datas).await;
    if to.trim().is_empty() {
        return;
    }
    let cc = eval_recipients(sc, &s(email, "emailcc"), datas).await;
    let Some(body_id) = i(email, "body_id") else { return };
    let Ok(Some(page)) = sc.db().get(crate::db::page_t(), body_id).await else { return };
    let vars = row! {"datas"=>Value::Object(datas.clone()),"tracker"=>tracker_json(t)};
    let body = sc.render(&s(&page, "content"), &vars, true);
    let title = sc.render(&s(&page, "title"), &vars, false);
    match (title, body) {
        (Ok(title), Ok(body)) => {
            if let Err(e) = crate::mail::send(&sc.inner.st, &to, &cc, &title, &body, &t.module).await {
                web::log_error(sc.db(), "tracker", "sendemail", &e.to_string(), Some(&t.module), Some(&t.slug), None).await;
            }
        }
        (a, b2) => {
            let msg = format!("email {}: {:?} {:?}", s(email, "name"), a.err().map(|e| e.to_string()), b2.err().map(|e| e.to_string()));
            web::log_error(sc.db(), "trackerEmail", "evalbody", &msg, Some(&t.module), Some(&t.slug), None).await;
        }
    }
}

pub async fn transition_post(mut ctx: Ctx, module: &str, slug: &str, tname: &str, id: Option<i64>) -> Response {
    let Some(t) = load(&mut ctx, module, slug).await else {
        ctx.flash("Tracker Not Found");
        return ctx.redirect("/").await;
    };
    let mut datas = match id {
        Some(rid) => t.get(ctx.db(), rid).await,
        None => None,
    };
    if id.is_some() && datas.is_none() {
        let to = t.url("list");
        return ctx.redirect(&to).await;
    }
    match id {
        Some(rid) => ctx.params.insert("id".into(), json!(rid)),
        None => ctx.params.remove("id"),
    };
    let files = std::mem::take(&mut ctx.files);
    let sc = make_sc(&mut ctx);
    let Some(tr) = t.resolve(&sc, tname, datas.as_ref()).await else {
        ctx.flash("You are not authorized to do that");
        return ctx.redirect("/").await;
    };
    let edit = t.expand_groups(&csv(&s(&tr.row, "editfields")));
    // the status to move to always comes from the transition, never from the browser.
    // A g6 portal moves the status only when the form posts next_status (its own
    // forms do): a page posting without it leaves a new record in sys_draft.
    let posted_status = ctx.params.remove("next_status").is_some();
    let mut params = ctx.params.clone();
    if let Some(ns) = tr.next {
        if !b(&tr.row, "same_status") && (posted_status || !crate::db::g6()) {
            params.insert("next_status".into(), json!(ns));
        }
    }
    // server-side validation: required fields and fatal error checks
    let deleting = t.status_name(tr.next).to_lowercase() == "delete";
    if !deleting {
        let mut merged = datas.clone().unwrap_or_default();
        for n in &edit {
            let key = if n == "slug" { "indata_slug" } else { n.as_str() };
            if let Some(v) = params.get(key) {
                merged.insert(n.clone(), v.clone());
            }
        }
        let mut problems: Vec<String> = vec![];
        for n in csv(&s(&tr.row, "requiredfields")) {
            if !edit.contains(&n) {
                continue;
            }
            let Some(f) = t.field(&n) else { continue };
            if has(f, "field_default") {
                continue;
            }
            let present = if s(f, "field_type") == "File" { files.contains_key(&n) || has(&merged, &n) } else { has(&merged, &n) };
            if !present {
                problems.push(format!("{} is required", s(f, "label")));
            }
        }
        for n in &edit {
            if let Some(f) = t.field(n) {
                for (msg, fatal) in tags::field_errors(&sc, &t, f, &merged).await {
                    if fatal {
                        problems.push(msg);
                    }
                }
            }
        }
        if !problems.is_empty() {
            ctx.flash(&problems.join("; "));
            let sc2 = Sc::new(ctx.st.clone(), ctx.user.clone(), { let mut p = ctx.params.clone(); p.insert("submit".into(), json!("1")); p }, sc.inner.csrf.clone(), ctx.path.clone());
            return transition_form(ctx, sc2, t, tr, id, datas).await;
        }
    }
    let defaults: Vec<String> = edit.iter().filter(|n| t.field(n).map(|f| has(f, "field_default")).unwrap_or(false)).cloned().collect();
    let (saved, prev, deleted) = match t.update_record(&sc, &params, &files, Some(&edit), &defaults, &edit).await {
        Ok(x) => x,
        Err(e) => {
            web::log_error(ctx.db(), "portalTracker", "transition_submit", &e.to_string(), Some(module), Some(slug), ctx.uid()).await;
            ctx.flash(&format!("The record could not be saved: {}", e));
            let to = t.url("list");
            return ctx.redirect(&to).await;
        }
    };
    let rid = i(&saved, "id");
    if !deleted {
        datas = match rid {
            Some(x) => t.get(ctx.db(), x).await,
            None => None,
        };
    }
    let Some(mut datas) = datas.or(prev.clone()) else {
        let to = t.url("list");
        return ctx.redirect(&to).await;
    };
    let mut trail_id: Option<i64> = None;
    let mut desc = s(&params, "statusUpdateDesc");
    if let Some(txt) = t.trail_text(&sc, &tr, &datas) {
        desc = txt;
    }
    if !deleted {
        if let Some(x) = rid {
            let tp = row! {"id"=>x,"statusUpdateDesc"=>desc.clone(),"record_status"=>s(&datas, "record_status")};
            trail_id = t.update_trail(&sc, &tp, ctx.uid(), files.get("uploadfile")).await;
        }
    }
    // postprocess script of the transition, then of the tracker
    let mut post = Value::Null;
    for pid in [i(&tr.row, "postprocess_id"), i(&t.row, "postprocess_id")].into_iter().flatten() {
        let Ok(Some(page)) = ctx.db().get(crate::db::page_t(), pid).await else { continue };
        let mut trow = tr.row.clone();
        trow.insert("roles".into(), json!(tr.roles));
        let vars = row! {
            "datas"=>Value::Object(datas.clone()), "prevdata"=>prev.clone().map(Value::Object).unwrap_or(Value::Null),
            "transition"=>Value::Object(trow), "tracker"=>tracker_json(&t), "trail_id"=>json!({"id": trail_id}), "deleted"=>deleted,
        };
        match sc.eval(&s(&page, "content"), &vars) {
            Ok(v) => {
                if let Some(f) = v.get("flash").map(vs).filter(|x| !x.is_empty()) {
                    ctx.flash(&f);
                }
                if post.is_null() {
                    post = v;
                }
            }
            Err(e) => {
                web::log_error(ctx.db(), "portalTracker", "transition_submit postprocess", &format!("page {}: {}", s(&page, "slug"), e), Some(module), Some(slug), ctx.uid()).await;
                ctx.flash(&format!("An error occurred during processing: {}", e));
            }
        }
    }
    if !deleted {
        if let Some(x) = rid {
            if let Some(fresh) = t.get(ctx.db(), x).await {
                datas = fresh;
            }
        }
    }
    datas.insert("update_desc".into(), json!(desc));
    let emails = ctx.db().rows("select * from portal_tracker_email where transition_id=:t order by id", &row! {"t"=>tr.id}).await.unwrap_or_default();
    for e in &emails {
        send_tracker_email(&sc, &t, e, &datas).await;
    }
    // where next
    if let Some(to) = result_redirect(ctx.db(), module, &post).await {
        return ctx.redirect(&to).await;
    }
    let backto = ctx.p("backto");
    if !backto.is_empty() {
        if let Some(target) = Tracker::load_ref(ctx.db(), backto.split(',').next().unwrap_or(""), module).await {
            let bid = ctx.pi("backtoid").or_else(|| t.fields.iter().filter(|f| s(f, "field_type") == "BelongsTo").find_map(|f| i(&datas, &s(f, "name"))));
            let to = match bid {
                Some(bid) => format!("/{}/{}/display/{}", target.module, target.slug, bid),
                None => target.url("list"),
            };
            return ctx.redirect(&to).await;
        }
    }
    let ra = s(&tr.row, "redirect_after");
    if !ra.trim().is_empty() {
        let tok: Vec<&str> = ra.split(':').map(|x| x.trim()).collect();
        let (m, sl) = if tok.len() > 1 { (tok[0].to_string(), tok[1].to_string()) } else { (module.to_string(), tok[0].to_string()) };
        if let Some(p) = find_page(ctx.db(), &m, &sl).await {
            let to = format!("/{}/{}/{}?id={}", if b(&p, "runable") { "run" } else { "view" }, m, sl, rid.unwrap_or(0));
            return ctx.redirect(&to).await;
        }
    }
    if deleted || b(&tr.row, "gotoprevstatuslist") {
        let to = t.url("list");
        return ctx.redirect(&to).await;
    }
    let to = match rid {
        Some(x) => format!("{}/{}", t.url("display"), x),
        None => t.url("list"),
    };
    ctx.redirect(&to).await
}

/// POST /{module}/{slug}/updaterecord/{id}: add an audit-trail entry (with optional attachment).
pub async fn update_record(mut ctx: Ctx, module: &str, slug: &str, id: i64) -> Response {
    let Some(t) = load(&mut ctx, module, slug).await else {
        ctx.flash("Tracker Not Found");
        return ctx.redirect("/").await;
    };
    let to = format!("{}/{}", t.url("display"), id);
    if ctx.method != "POST" {
        return ctx.redirect(&to).await;
    }
    let Some(datas) = t.get(ctx.db(), id).await else { return ctx.redirect(&t.url("list")).await };
    let upload = ctx.files.remove("uploadfile");
    let sc = make_sc(&mut ctx);
    let status = t.status(&s(&datas, "record_status")).cloned();
    let uroles = t.user_roles(&sc, Some(&datas)).await;
    let allowed = match &status {
        Some(st) if b(st, "updateable") => {
            let rules = s(st, "updateallowedroles");
            rules.trim().is_empty() || rules.split(';').flat_map(|r| csv(r.split("->").next().unwrap_or(""))).any(|w| uroles.contains(&w))
        }
        _ => false,
    };
    if ctx.user.is_none() || !allowed {
        ctx.flash("You are not authorized to update this record");
        return ctx.redirect(&to).await;
    }
    let tp = row! {"id"=>id,"statusUpdateDesc"=>ctx.p("statusUpdateDesc"),"record_status"=>s(&datas, "record_status")};
    let trail_id = t.update_trail(&sc, &tp, ctx.uid(), upload.as_ref()).await;
    if let Some(st) = &status {
        if let Some(pid) = i(st, "runonupdate_id") {
            if let Ok(Some(page)) = ctx.db().get(crate::db::page_t(), pid).await {
                let vars = row! {"datas"=>Value::Object(datas.clone()),"tracker"=>tracker_json(&t),"curstatus"=>Value::Object(st.clone()),"trail_id"=>json!({"id": trail_id})};
                match sc.eval(&s(&page, "content"), &vars) {
                    Ok(v) => {
                        if let Some(f) = v.get("flash").map(vs).filter(|x| !x.is_empty()) {
                            ctx.flash(&f);
                        }
                    }
                    Err(e) => web::log_error(ctx.db(), "portalTracker", "runonupdate", &e.to_string(), Some(module), Some(slug), ctx.uid()).await,
                }
            }
        }
        if let Some(eid) = i(st, "emailonupdate_id") {
            if let Ok(Some(email)) = ctx.db().get("portal_tracker_email", eid).await {
                let mut d = datas.clone();
                d.insert("update_desc".into(), json!(ctx.p("statusUpdateDesc")));
                send_tracker_email(&sc, &t, &email, &d).await;
            }
        }
    }
    ctx.redirect(&to).await
}

// ---------------- JSON lookups ----------------

async fn field_and_tracker(ctx: &Ctx) -> Option<(Tracker, Row)> {
    let fid = ctx.pi("id").or_else(|| ctx.pi("hx_field_id"))?;
    let f = ctx.db().get("portal_tracker_field", fid).await.ok().flatten()?;
    let t = Tracker::by_id(ctx.db(), i(&f, "tracker_id")?).await?;
    Some((t, f))
}

fn select2(items: Vec<(String, String)>) -> Value {
    json!({"results": items.into_iter().map(|(id, text)| json!({"id": id, "text": text})).collect::<Vec<_>>()})
}

pub async fn userlist(mut ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.json(json!({"results": []})).await;
    }
    let q = ctx.p("q");
    let like = format!("%{}%", q.trim().replace(' ', "%"));
    let mut out = vec![];
    let fo = field_and_tracker(&ctx).await;
    let opts = fo.as_ref().map(|(_, f)| s(f, "field_options")).unwrap_or_default();
    if !opts.trim().is_empty() {
        let sc = make_sc(&mut ctx);
        let ql = q.to_lowercase();
        if let Ok(Value::Array(a)) = sc.eval(&opts, &Row::new()) {
            for u in a {
                let (id, name) = match &u {
                    Value::Object(o) => (s(o, "id"), format!("{} ({})", s(o, "emp_name"), s(o, "staffid"))),
                    other => {
                        let id = vs(other);
                        let n = match id.parse::<i64>() {
                            Ok(x) => web::load_user(ctx.db(), x).await.map(|u| format!("{} ({})", s(&u, "emp_name"), s(&u, "staffid"))).unwrap_or_default(),
                            Err(_) => String::new(),
                        };
                        (id, n)
                    }
                };
                if !id.is_empty() && (ql.is_empty() || name.to_lowercase().contains(&ql)) {
                    out.push((id, name));
                }
            }
        }
    } else {
        let rows = ctx
            .db()
            .rows("select id, emp_name, staffid from iap_user where isactive is not false and (emp_name ilike :q or staffid ilike :q or new_staffid ilike :q) order by emp_name limit 20", &row! {"q"=>like})
            .await
            .unwrap_or_default();
        out = rows.iter().map(|r| (s(r, "id"), format!("{} ({})", s(r, "emp_name"), s(r, "staffid")))).collect();
    }
    ctx.json(select2(out)).await
}

pub async fn nodeslist(mut ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.json(json!({"results": []})).await;
    }
    let q = ctx.p("q");
    let fo = field_and_tracker(&ctx).await;
    let opts = fo.as_ref().map(|(_, f)| s(f, "field_options")).unwrap_or_default();
    let mut out = vec![];
    let ql = q.to_lowercase();
    // options: "tree:module:name" restricts to one tree; anything else is a script returning nodes
    let tok: Vec<&str> = opts.trim().split(':').collect();
    if tok.len() == 3 && tok[0] == "tree" {
        let rows = ctx
            .db()
            .rows("select n.id, n.name from tree_node n join tree t on t.id=n.tree_id where t.module=:m and t.name=:n and n.name ilike :q and n.disabled is not true order by n.lft limit 50", &row! {"m"=>tok[1],"n"=>tok[2],"q"=>format!("%{}%", q)})
            .await
            .unwrap_or_default();
        out = rows.iter().map(|r| (s(r, "id"), s(r, "name"))).collect();
    } else if !opts.trim().is_empty() {
        let sc = make_sc(&mut ctx);
        if let Ok(Value::Array(a)) = sc.eval(&opts, &Row::new()) {
            for n in a {
                if let Value::Object(o) = n {
                    if ql.is_empty() || s(&o, "name").to_lowercase().contains(&ql) {
                        out.push((s(&o, "id"), s(&o, "name")));
                    }
                }
            }
        }
    } else {
        let rows = ctx.db().rows("select id, name from tree_node where name ilike :q and disabled is not true order by name limit 50", &row! {"q"=>format!("%{}%", q)}).await.unwrap_or_default();
        out = rows.iter().map(|r| (s(r, "id"), s(r, "name"))).collect();
    }
    ctx.json(select2(out)).await
}

pub async fn objectlist(ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.json(json!({"results": []})).await;
    }
    let mut out = vec![];
    if let Some((t, f)) = field_and_tracker(&ctx).await {
        if let Some(ot) = t.other_tracker(ctx.db(), &f).await {
            let ff = other_format(&f);
            let fname = if ff.is_empty() { ot.default_field().unwrap_or_else(|| "id".into()) } else { ff };
            let q = ctx.p("q");
            let qp = if q.is_empty() { Row::new() } else { row! {fname.clone()=>format!("%{}%", q)} };
            for r in ot.rows(ctx.db(), Some(&qp), Some(&qcol(&fname)), None, Some(30)).await.unwrap_or_default() {
                out.push((s(&r, "id"), s(&r, &fname)));
            }
        }
    }
    ctx.json(select2(out)).await
}

/// /portalTracker/dropdowndata?id={field id}&data_id={record id}: the linked record.
pub async fn dropdowndata(ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.json(json!({"datas": null})).await;
    }
    let mut datas = Value::Null;
    if let (Some((t, f)), Some(did)) = (field_and_tracker(&ctx).await, ctx.pi("data_id")) {
        match s(&f, "field_type").as_str() {
            "User" => datas = web::load_user(ctx.db(), did).await.map(Value::Object).unwrap_or(Value::Null),
            "TreeNode" | "Branch" => datas = ctx.db().get("tree_node", did).await.ok().flatten().map(Value::Object).unwrap_or(Value::Null),
            _ => {
                if let Some(ot) = t.other_tracker(ctx.db(), &f).await {
                    datas = ot.get(ctx.db(), did).await.map(Value::Object).unwrap_or(Value::Null);
                }
            }
        }
    }
    ctx.json(json!({"datas": datas})).await
}

/// /portalTracker/dropdownlist?id={field id}: the field's options, evaluated with the posted values.
pub async fn dropdownlist(mut ctx: Ctx) -> Response {
    let Some((_, f)) = field_and_tracker(&ctx).await else { return ctx.json(json!([])).await };
    let sc = make_sc(&mut ctx);
    let opts = crate::script::option_pairs(&sc.eval_options(&s(&f, "field_options"), &row! {"datas"=>Value::Object(ctx.params.clone())}));
    ctx.json(json!(opts.into_iter().map(|(id, text)| json!({"id": id, "text": text})).collect::<Vec<_>>())).await
}

/// HTMX: re-check one field's error rules against the form being edited.
pub async fn onchange(mut ctx: Ctx) -> Response {
    let Some((t, f)) = field_and_tracker(&ctx).await else { return ctx.raw_html(String::new()).await };
    let sc = make_sc(&mut ctx);
    let mut datas = ctx.params.clone();
    if let Some(v) = datas.get("indata_slug").cloned() {
        datas.insert("slug".into(), v); // the slug field is posted as indata_slug
    }
    let errs = tags::field_errors(&sc, &t, &f, &datas).await;
    if db::g6() {
        // a g6 portal answers with the whole span, which replaces the old one
        let name = s(&f, "name");
        let n = if name == "slug" { "indata_slug".to_string() } else { name.clone() };
        let ft = s(&f, "field_type");
        let trig = if matches!(ft.as_str(), "Date" | "DateTime" | "Drop Down" | "File") { format!("change from:#{n}") } else { format!("change from:#{n},keyup from:#{n} changed delay:500ms") };
        let mut hx = format!("hx-trigger='{}' hx-post='/portalTrackerField/onchange?hx_field_id={}' hx-swap='outerHTML' hx-target='this' ", trig, i(&f, "id").unwrap_or(0));
        if !s(&f, "hyperscript").trim().is_empty() {
            hx.push_str(&tags::groovy_literal(&s(&f, "hyperscript")));
        }
        let fatal = errs.iter().any(|(_, x)| *x);
        let msgs = errs.iter().map(|(m, _)| m.clone()).collect::<Vec<_>>().join("; ");
        return ctx.raw_html(format!(" <span class='text-danger {}' {} _='on load update_submit()'>{}</span>", if fatal { "fatal_error" } else { "" }, hx, crate::web::esc(&msgs))).await;
    }
    ctx.raw_html(tags::errors_html(&errs)).await
}

/// /portalTracker/api_list?module=&slug= : the list as JSON, with the same visibility rules.
pub async fn api_list(mut ctx: Ctx) -> Response {
    let (m, sl) = (ctx.p("module"), ctx.p("slug"));
    let Some(t) = load(&mut ctx, &m, &sl).await else { return ctx.json(json!({"error": "Tracker not found"})).await };
    let sc = make_sc(&mut ctx);
    let mut p = ctx.params.clone();
    if !p.contains_key("max") {
        p.insert("max".into(), json!(100));
    }
    let total = ctx.db().count(&t.list_query(&sc, &p, "select count(*)", false).await, &db::no_params()).await.unwrap_or(0);
    let rows = ctx.db().rows(&t.list_query(&sc, &p, "select *", false).await, &db::no_params()).await.unwrap_or_default();
    ctx.json(json!({"total": total, "rows": rows})).await
}

// ---------------- admin actions on a tracker definition ----------------

pub async fn workflow_graph(ctx: Ctx, t: &Tracker) -> Response {
    let nodes: Vec<Value> = t.statuses.iter().map(|st| json!({"id": i(st, "id"), "label": s(st, "name"), "shape": "box", "color": if i(st, "id") == t.initial_status() { "#b6e3b6" } else { "#d2e5ff" }})).collect();
    let mut edges: Vec<Value> = vec![];
    let mut starts: Vec<Value> = vec![];
    for tr in &t.transitions {
        let label = if tr.roles.is_empty() { tr.label() } else { format!("{}\n[{}]", tr.label(), tr.roles.join(", ")) };
        let Some(next) = tr.next else { continue };
        if tr.prev.is_empty() {
            starts.push(json!({"from": "start", "to": next, "label": label, "arrows": "to"}));
        }
        for p in &tr.prev {
            edges.push(json!({"from": p, "to": next, "label": label, "arrows": "to", "font": {"align": "middle", "size": 11}}));
        }
    }
    let mut all_nodes = nodes;
    if !starts.is_empty() {
        all_nodes.push(json!({"id": "start", "label": "Start", "shape": "ellipse", "color": "#eeeeee"}));
        edges.extend(starts);
    }
    let h = format!(
        "<div class='container-fluid'><div class='nav'><ul><li><a class='list' href='/portalTracker/show/{}'>Back to {}</a></li></ul></div><h1>Workflow: {}</h1><div id='graph' style='height:75vh;border:1px solid #ccc;background:#fff'></div></div><script src='/static/libs/vis-network/vis-network.min.js'></script><script>new vis.Network(document.getElementById('graph'),{{nodes:new vis.DataSet({}),edges:new vis.DataSet({})}},{{layout:{{hierarchical:{{direction:'LR',sortMethod:'directed',levelSeparation:260,nodeSpacing:120}}}},physics:false,edges:{{smooth:{{type:'cubicBezier'}}}}}});</script>",
        t.id, esc(&t.name), esc(&t.name), Value::Array(all_nodes), Value::Array(edges)
    );
    ctx.html("Workflow", h).await
}

/// Give a new tracker a working workflow: Admin role, New/Delete statuses
/// and New/Edit/Delete transitions over all of its fields.
pub async fn create_defaults(db: &db::Db, t: &Tracker) -> anyhow::Result<()> {
    let tid = t.id;
    let names: Vec<String> = t.fields.iter().filter(|f| !matches!(s(f, "field_type").as_str(), "HasMany")).map(|f| s(f, "name")).collect();
    let all = names.join(",");
    let role = match t.roles.iter().find(|r| s(r, "name") == "Admin") {
        Some(r) => i(r, "id").unwrap_or(0),
        None => db.insert("portal_tracker_role", &row! {"tracker_id"=>tid,"name"=>"Admin","role_type"=>"User Role","role_desc"=>format!("Admin role for {}", t.name),"last_updated"=>web::now_str()}).await?,
    };
    if t.tracker_type == "DataStore" {
        // no statuses: transitions without status changes
        for (name, f) in [("New", &all), ("Edit", &all), ("Delete", &String::new())] {
            if t.transitions.iter().all(|x| x.name != name) {
                let id = db.insert("portal_tracker_transition", &row! {"tracker_id"=>tid,"name"=>name,"editfields"=>f.clone(),"cancelbutton"=>true}).await?;
                db.exec("insert into portal_tracker_transition_portal_tracker_role (portal_tracker_transition_roles_id,portal_tracker_role_id) values (:t,:r)", &row! {"t"=>id,"r"=>role}).await?;
            }
        }
        return Ok(());
    }
    let mut st_ids = std::collections::HashMap::new();
    for name in ["New", "Delete"] {
        let id = match t.status(name) {
            Some(r) => i(r, "id").unwrap_or(0),
            None => db.insert("portal_tracker_status", &row! {"tracker_id"=>tid,"name"=>name,"updateable"=>name == "New","attachable"=>name == "New","flow"=>if name == "New" { 1 } else { 99 }}).await?,
        };
        st_ids.insert(name, id);
    }
    if t.initial_status().is_none() {
        db.update("portal_tracker", tid, &row! {"initial_status_id"=>st_ids["New"]}).await?;
    }
    if has(&t.row, "listfields") == false {
        db.update("portal_tracker", tid, &row! {"listfields"=>names.iter().take(6).cloned().collect::<Vec<_>>().join(",")}).await?;
    }
    for (name, prev, next, f) in [("New", None, "New", all.clone()), ("Edit", Some("New"), "New", all.clone()), ("Delete", Some("New"), "Delete", String::new())] {
        if t.transitions.iter().any(|x| x.name == name) {
            continue;
        }
        let id = db
            .insert("portal_tracker_transition", &row! {"tracker_id"=>tid,"name"=>name,"editfields"=>f,"next_status_id"=>st_ids[next],"cancelbutton"=>true,"same_status"=>false,"gotoprevstatuslist"=>false})
            .await?;
        db.exec("insert into portal_tracker_transition_portal_tracker_role (portal_tracker_transition_roles_id,portal_tracker_role_id) values (:t,:r)", &row! {"t"=>id,"r"=>role}).await?;
        if let Some(p) = prev {
            db.exec("insert into portal_tracker_transition_portal_tracker_status (portal_tracker_transition_prev_status_id,portal_tracker_status_id) values (:t,:s)", &row! {"t"=>id,"s"=>st_ids[p]}).await?;
        }
    }
    Ok(())
}

/// Create editable copies of the default list/show/edit screens as pages.
pub async fn create_default_pages(db: &db::Db, t: &Tracker) -> anyhow::Result<usize> {
    let pages = [
        (format!("{}_list", t.slug), format!("List {}", t.name), "<div id=\"content\" role=\"main\">\n    <div class=\"container\">\n        <section class=\"row\">\n            <a href=\"#list-portalTracker\" class=\"skip\" tabindex=\"-1\">Skip to content&hellip;</a>\n            {{ tracker_nav() }}\n        </section>\n        <section class=\"row\">\n            <div id=\"list-portalTracker\" class=\"col-12 content scaffold-list\" role=\"main\">\n                <h1>List {{ tracker.name }}</h1>\n                {% if flash %}<div class=\"message\" role=\"status\">{{ flash }}</div>{% endif %}\n                {{ tracker_filter() }}\n                {{ tracker_list() }}\n            </div>\n        </section>\n    </div>\n</div>\n".to_string()),
        (format!("{}_show_default", t.slug), format!("Display {}", t.name), "<div id=\"content\" role=\"main\">\n    <div class=\"container\">\n        <section class=\"row\">\n            <a href=\"#list-portalTracker\" class=\"skip\" tabindex=\"-1\">Skip to content&hellip;</a>\n            {{ tracker_nav() }}\n        </section>\n        <section class=\"row\">\n            <div id=\"list-portalTracker\" class=\"col-12 content scaffold-list\" role=\"main\">\n                <h1>Display {{ tracker.name }}</h1>\n                {% if flash %}<div class=\"message\" role=\"status\">{{ flash }}</div>{% endif %}\n                {{ tracker_display() }}\n                {{ tracker_updates() }}\n            </div>\n        </section>\n    </div>\n</div>\n".to_string()),
        (format!("{}_edit", t.slug), format!("Edit {}", t.name), "<div id=\"content\" role=\"main\">\n    <div class=\"container\">\n        <section class=\"row\">\n            <a href=\"#list-portalTracker\" class=\"skip\" tabindex=\"-1\">Skip to content&hellip;</a>\n            {{ tracker_nav() }}\n        </section>\n        <section class=\"row\">\n            <div id=\"list-portalTracker\" class=\"col-12 content scaffold-list\" role=\"main\">\n                <h1>{{ transition.display_name or transition.name }} {{ tracker.name }}</h1>\n                {% if flash %}<div class=\"message\" role=\"status\">{{ flash }}</div>{% endif %}\n                {{ tracker_info() }}\n                {{ tracker_form() }}\n            </div>\n        </section>\n    </div>\n</div>\n".to_string()),
    ];
    let mut n = 0;
    for (slug, title, content) in pages {
        if find_page(db, &t.module, &slug).await.is_none() {
            db.insert(crate::db::page_t(), &row! {"title"=>title,"slug"=>slug,"module"=>t.module.clone(),"content"=>content,"published"=>true,"requirelogin"=>false,"runable"=>false,"fullpage"=>false,"last_updated"=>web::now_str()}).await?;
            n += 1;
        }
    }
    Ok(n)
}
