//! File links: stored uploads, downloads with role checks, and the file manager.

use crate::db::{self, i, lit, s, Db, Row};
use crate::row;
use crate::web::{self, enc, esc, now_str, Ctx, Upload};
use anyhow::{anyhow, Result};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const BLOCKED_EXT: &[&str] = &["exe", "bat", "cmd", "com", "sh", "ps1", "vbs", "js", "jar", "msi", "scr", "dll", "php", "jsp", "asp", "aspx", "cgi", "pl", "py", "html", "htm", "svg"];

pub fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let clean: String = base.chars().map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ' | '(' | ')') { c } else { '_' }).collect();
    let clean = clean.trim_matches('.').trim().to_string();
    if clean.is_empty() { "file".into() } else { clean }
}

pub fn validate_upload(up: &Upload) -> Result<String> {
    let name = sanitize_filename(&up.filename);
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    if name.contains('.') && BLOCKED_EXT.contains(&ext.as_str()) {
        return Err(anyhow!("File type .{} is not allowed", ext));
    }
    if up.head().starts_with(b"MZ") || up.head().starts_with(b"\x7fELF") {
        return Err(anyhow!("Executable files are not allowed"));
    }
    Ok(name)
}

/// Write an upload under `dir` (never overwriting) and register a FileLink.
pub async fn store_upload(db: &Db, up: &Upload, dir: &str, module: &str, slug: &str, tracker_slug: Option<&str>, data_id: Option<i64>) -> Result<i64> {
    let name = validate_upload(up)?;
    tokio::fs::create_dir_all(dir).await?;
    let mut target = PathBuf::from(dir).join(&name);
    let mut n = 1;
    while target.exists() {
        let (stem, ext) = match name.rsplit_once('.') {
            Some((a, b)) => (a.to_string(), format!(".{}", b)),
            None => (name.clone(), String::new()),
        };
        target = PathBuf::from(dir).join(format!("{}_{}{}", stem, n, ext));
        n += 1;
    }
    up.save_to(&target).await?;
    let mut uslug = slug.to_string();
    let mut k = 1;
    while db.count("select count(*) from file_link where slug=:s and module=:m", &row! {"s"=>uslug.clone(),"m"=>module}).await? > 0 && k < 1000 {
        uslug = format!("{}_{}", slug, k);
        k += 1;
    }
    db.insert(
        "file_link",
        &row! {"name"=>up.filename.clone(),"path"=>target.to_string_lossy().to_string(),"module"=>module,"slug"=>uslug,
        "tracker_slug"=>tracker_slug,"tracker_data_id"=>data_id,"size"=>up.size as i64,"last_updated"=>now_str()},
    )
    .await
}

pub async fn delete_file_link(db: &Db, id: i64) {
    if let Ok(Some(f)) = db.get("file_link", id).await {
        let _ = tokio::fs::remove_file(s(&f, "path")).await;
        let _ = db.delete("file_link", id).await;
    }
}

async fn allowed(ctx: &Ctx, f: &Row) -> bool {
    if db::g6() {
        return allowed_g6(ctx, f).await;
    }
    let roles = db::csv(&s(f, "allowedroles"));
    if roles.is_empty() || roles.iter().any(|r| r == "All") {
        return true;
    }
    let Some(u) = &ctx.user else { return false };
    if db::b(u, "is_admin") {
        return true;
    }
    let mine = web::module_roles(ctx.db(), u, &s(f, "module")).await;
    let cur = web::current_role(ctx.db(), u).await.map(|r| s(&r, "role")).unwrap_or_default();
    roles.iter().any(|r| mine.contains(r) || *r == cur || r == "Authenticated")
}

async fn serve(ctx: Ctx, f: Row) -> Response {
    if !allowed(&ctx, &f).await {
        if db::g6() {
            return ctx.text_status(axum::http::StatusCode::UNAUTHORIZED, "Authentication required").await;
        }
        if ctx.user.is_none() {
            return ctx.need_login().await;
        }
        return ctx.status(axum::http::StatusCode::FORBIDDEN, "You are not authorized to download this file").await;
    }
    let path = s(&f, "path");
    match tokio::fs::read(&path).await {
        Ok(data) => {
            if let Some(id) = i(&f, "id") {
                let _ = ctx.db().exec("update file_link set last_access=now() where id=:id", &row! {"id"=>id}).await;
            }
            let name = { let n = s(&f, "name"); if n.is_empty() { Path::new(&path).file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default() } else { n } };
            let mime = mime_guess::from_path(&name).first_or_octet_stream().to_string();
            let inline = ctx.params.contains_key("inline") && (mime.starts_with("image/") || mime == "application/pdf");
            ctx.bytes(&mime, Some(&name), data, inline).await
        }
        Err(_) => ctx.status(axum::http::StatusCode::NOT_FOUND, "File not found on the server").await,
    }
}

/// /download/{module}/{slug} and /download/{slug}
pub async fn download(ctx: Ctx, module: &str, slug: &str) -> Response {
    // a slug may carry a made-up extension (clip.mp4); without a module, the Grails
    // portal finds the slug in any module, and so does this one in g6 mode
    let slug = if db::g6() { slug.split('.').next().unwrap_or("") } else { slug };
    let found = if module.is_empty() && db::g6() {
        ctx.db().first("select * from file_link where slug=:s order by id desc limit 1", &row! {"s"=>slug}).await
    } else {
        let m = if module.is_empty() { "portal" } else { module };
        ctx.db().first("select * from file_link where module=:m and slug=:s order by id desc limit 1", &row! {"m"=>m,"s"=>slug}).await
    };
    match found {
        Ok(Some(f)) => serve(ctx, f).await,
        _ => ctx.status(axum::http::StatusCode::NOT_FOUND, "File not found").await,
    }
}

/// /fileLink/get/{id}
pub async fn get(ctx: Ctx, id: i64) -> Response {
    match ctx.db().get("file_link", id).await {
        Ok(Some(f)) => serve(ctx, f).await,
        _ => ctx.status(axum::http::StatusCode::NOT_FOUND, "File not found").await,
    }
}

/// POST /fileLink/upload : admin upload creating a FileLink.
pub async fn upload(mut ctx: Ctx) -> Response {
    let module = { let m = ctx.p("module"); if m.is_empty() { "portal".to_string() } else { m } };
    if !ctx.can_admin(&module).await {
        return ctx.status(axum::http::StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let Some(up) = ctx.files.remove("file") else {
        ctx.flash("No file was selected");
        return ctx.redirect("/fileLink/index").await;
    };
    let slug = { let x = ctx.p("slug"); if x.trim().is_empty() { sanitize_filename(&up.filename).replace(' ', "_").to_lowercase() } else { x.trim().to_string() } };
    let dir = format!("{}/{}", ctx.st.cfg.upload_dir, module);
    match store_upload(ctx.db(), &up, &dir, &module, &slug, None, None).await {
        Ok(id) => {
            let extra = row! {"allowedroles"=>ctx.p("allowedroles"),"filegroup"=>ctx.p("filegroup")};
            let _ = ctx.db().update("file_link", id, &extra).await;
            ctx.flash("File uploaded");
            ctx.redirect(&format!("/fileLink/show/{}", id)).await
        }
        Err(e) => {
            ctx.flash(&format!("Upload failed: {}", e));
            ctx.redirect("/fileLink/index").await
        }
    }
}

/// HTML for a file link (used by display tags).
pub async fn link_html(db: &Db, id: i64) -> String {
    match db.get("file_link", id).await {
        Ok(Some(f)) => {
            let name = s(&f, "name");
            let lower = name.to_lowercase();
            if lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg") || lower.ends_with(".gif") {
                format!("<a href='/fileLink/get/{id}'><img src='/fileLink/get/{id}?inline=1' style='max-height:120px;max-width:200px' alt='{n}'/></a>", id = id, n = esc(&name))
            } else {
                format!("<a href='/fileLink/get/{}' class='filelink'><i class='bi bi-paperclip'></i> {}</a>", id, esc(&name))
            }
        }
        _ => String::new(),
    }
}

/// A g6 portal's FileLink.readable: an unrestricted file is public only in the
/// download_module_whitelist modules (default: portal); otherwise it is for portal
/// admins, the module's admins and whoever holds a role on the file's tracker
/// record. Listed roles: All, Authenticated, a module role or the current role.
async fn allowed_g6(ctx: &Ctx, f: &Row) -> bool {
    let db = ctx.db();
    let module = s(f, "module");
    let roles = db::csv(&s(f, "allowedroles"));
    let user = ctx.user.clone();
    let legacy = matches!(web::setting_str(db, "portal.legacy_filegroup_access").await.as_deref(), Some("true") | Some("1") | Some("on"));
    if legacy && !s(f, "filegroup").is_empty() && s(f, "tracker_slug").is_empty() {
        if let Some(u) = &user {
            if roles.is_empty() || (!module.is_empty() && web::module_roles(db, u, &module).await.iter().any(|r| r == "Admin")) {
                return true;
            }
        }
    }
    if roles.is_empty() {
        let whitelist = match web::setting_str(db, "download_module_whitelist").await {
            Some(w) => db::csv(&w.replace(['[', ']', '\'', '"'], "")),
            None => vec!["portal".to_string()],
        };
        if whitelist.iter().any(|m| m == &module) {
            return true;
        }
        let Some(u) = &user else { return false };
        if db::b(u, "is_admin") || (!module.is_empty() && web::admin_list(db, u).await.iter().any(|m| m == &module)) {
            return true;
        }
        // a role on the record the file belongs to (or, when its tracker is unknown,
        // on any tracker of the module)
        let sc = crate::script::Sc::new(ctx.st.clone(), user.clone(), ctx.params.clone(), String::new(), ctx.path.clone());
        let data_id = db::i(f, "tracker_data_id");
        let tslug = s(f, "tracker_slug");
        if !tslug.is_empty() {
            if let Some(t) = crate::tracker::Tracker::load(db, &module, &tslug).await {
                let datas = match data_id { Some(id) => t.get(db, id).await, None => None };
                return !t.user_roles(&sc, datas.as_ref()).await.is_empty();
            }
        }
        for row in db.rows("select slug from portal_tracker where module=:m", &crate::row! {"m"=>module.clone()}).await.unwrap_or_default() {
            if let Some(t) = crate::tracker::Tracker::load(db, &module, &s(&row, "slug")).await {
                let datas = match data_id { Some(id) if tslug.is_empty() => t.get(db, id).await, _ => None };
                if !t.user_roles(&sc, datas.as_ref()).await.is_empty() {
                    return true;
                }
            }
        }
        return false;
    }
    if roles.iter().any(|r| r == "All") {
        return true;
    }
    let Some(u) = &user else { return false };
    if roles.iter().any(|r| r == "Authenticated") {
        return true;
    }
    let mine = web::module_roles(db, u, &module).await;
    if roles.iter().any(|r| mine.contains(r)) {
        return true;
    }
    let cur = web::current_role(db, u).await.map(|r| s(&r, "role")).unwrap_or_default();
    !cur.is_empty() && roles.iter().any(|r| *r == cur)
}

// ---------------- file size backfill jobs (g6ikram FileLinkController + FileLinkUpdateService) ----------------

async fn size_progress(db: &Db) -> (i64, i64, i64, f64) {
    let total = db.count("select count(*) from file_link", &db::no_params()).await.unwrap_or(0);
    let missing = db.count("select count(*) from file_link where size is null", &db::no_params()).await.unwrap_or(0);
    let updated = (total - missing).max(0);
    let pct = if total > 0 { ((updated as f64 / total as f64) * 10000.0).round() / 100.0 } else { 0.0 };
    (total, updated, missing, pct)
}

fn spawn_size_backfill(db: Db, job_id: i64, scope: Option<String>, batch_size: i64, delay_ms: u64) {
    tokio::spawn(async move {
        let mut sized: i64 = 0;
        let mut skipped: i64 = 0;
        let mut cancelled = false;
        let mut last_id: i64 = 0;
        let mut failed: Option<String> = None;
        let lim = batch_size.max(1);
        loop {
            if let Ok(Some(jr)) = db.first("select cancel_requested from job_status where id=:id", &row! {"id"=>job_id}).await {
                if db::b(&jr, "cancel_requested") {
                    cancelled = true;
                    break;
                }
            }
            let q = if scope.is_some() {
                format!("select id, path from file_link where size is null and id > :last_id and module=:m order by id limit {}", lim)
            } else {
                format!("select id, path from file_link where size is null and id > :last_id order by id limit {}", lim)
            };
            let params = match &scope {
                Some(m) => row! {"last_id"=>last_id,"m"=>m.clone()},
                None => row! {"last_id"=>last_id},
            };
            let rows = match db.rows(&q, &params).await {
                Ok(r) => r,
                Err(e) => {
                    failed = Some(e.to_string());
                    break;
                }
            };
            if rows.is_empty() {
                break;
            }
            for r in rows {
                let fid = i(&r, "id").unwrap_or(0);
                last_id = fid;
                let path = s(&r, "path");
                if !path.is_empty() {
                    match tokio::fs::metadata(&path).await {
                        Ok(md) => {
                            let sz = md.len() as i64;
                            if db.exec("update file_link set size=:sz where id=:id", &row! {"sz"=>sz,"id"=>fid}).await.is_ok() {
                                sized += 1;
                            } else {
                                skipped += 1;
                            }
                        }
                        Err(_) => skipped += 1,
                    }
                } else {
                    skipped += 1;
                }
            }
            let _ = db.update("job_status", job_id, &row! {"processed_records"=>sized,"skipped_records"=>skipped}).await;
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }
        let final_status = if failed.is_some() { "FAILED" } else if cancelled { "CANCELLED" } else { "COMPLETED" };
        let _ = db
            .update(
                "job_status",
                job_id,
                &row! {
                    "processed_records"=>sized,
                    "skipped_records"=>skipped,
                    "status"=>final_status,
                    "end_time"=>now_str(),
                    "error_message"=>failed.map(|e| e.chars().take(1000).collect::<String>())
                },
            )
            .await;
    });
}

pub async fn size_action(mut ctx: Ctx, act: &str, id: Option<i64>) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    match act {
        "sizeUpdateProgress" => {
            let (total, updated, missing, pct) = size_progress(ctx.db()).await;
            ctx.json(serde_json::json!({"total": total, "updated": updated, "missing": missing, "percentComplete": pct})).await
        }
        "updateMissingSizes" | "updateMissingSizesWithTracking" => {
            if !ctx.is_admin() {
                ctx.flash("Administrator rights are required to run a file size job.");
                return ctx.redirect("/fileLink/jobstatus").await;
            }
            if let Ok(Some(running)) = ctx.db().first("select id from job_status where job_type='FILE_SIZE_UPDATE' and status='RUNNING' limit 1", &db::no_params()).await {
                ctx.flash(&format!("A file size job is already running (Job #{}). Wait for it to finish, or cancel it first.", i(&running, "id").unwrap_or(0)));
                return ctx.redirect("/fileLink/jobstatus").await;
            }
            let scope = {
                let sc = ctx.p("scopemodule");
                if !sc.trim().is_empty() {
                    Some(sc.trim().to_string())
                } else {
                    let m = ctx.p("module");
                    if !m.trim().is_empty() { Some(m.trim().to_string()) } else { None }
                }
            };
            let remaining = match &scope {
                Some(m) => ctx.db().count("select count(*) from file_link where module=:m and size is null", &row! {"m"=>m.clone()}).await.unwrap_or(0),
                None => ctx.db().count("select count(*) from file_link where size is null", &db::no_params()).await.unwrap_or(0),
            };
            let batch_size = web::setting_str(ctx.db(), "filesize_job_batch_size").await.and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(500);
            let delay_ms = web::setting_str(ctx.db(), "filesize_job_batch_delay_ms").await.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(100);
            let job_id = match ctx
                .db()
                .insert(
                    "job_status",
                    &row! {
                        "job_type"=>"FILE_SIZE_UPDATE",
                        "module_name"=>scope.clone(),
                        "status"=>"RUNNING",
                        "start_time"=>now_str(),
                        "total_records"=>remaining,
                        "processed_records"=>0i64,
                        "skipped_records"=>0i64,
                        "cancel_requested"=>false
                    },
                )
                .await
            {
                Ok(jid) => jid,
                Err(e) => {
                    ctx.flash(&format!("Could not start file size job: {}", e));
                    return ctx.redirect("/fileLink/jobstatus").await;
                }
            };
            spawn_size_backfill(ctx.st.db.clone(), job_id, scope.clone(), batch_size, delay_ms);
            let scope_txt = match scope {
                Some(m) => format!(" for module '{}'", m),
                None => " for all modules".into(),
            };
            ctx.flash(&format!("File size job #{} started{} covering {} unsized records. Progress refreshes automatically.", job_id, scope_txt, remaining));
            ctx.redirect("/fileLink/jobstatus").await
        }
        "cancelSizeJob" => {
            if !ctx.is_admin() {
                ctx.flash("Administrator rights are required to cancel a file size job.");
                return ctx.redirect("/fileLink/jobstatus").await;
            }
            let Some(jid) = id.or_else(|| ctx.pi("id")) else {
                ctx.flash("Job not found.");
                return ctx.redirect("/fileLink/jobstatus").await;
            };
            match ctx.db().get("job_status", jid).await {
                Ok(Some(job)) => {
                    if s(&job, "status") != "RUNNING" {
                        ctx.flash(&format!("Job #{} is not running.", jid));
                    } else if db::b(&job, "cancel_requested") {
                        let _ = ctx.db().update("job_status", jid, &row! {"status"=>"CANCELLED","end_time"=>now_str()}).await;
                        ctx.flash(&format!("Job #{} force-stopped.", jid));
                    } else {
                        let _ = ctx.db().update("job_status", jid, &row! {"cancel_requested"=>true}).await;
                        ctx.flash(&format!("Cancellation requested for job #{}. It will stop after the current batch.", jid));
                    }
                }
                _ => ctx.flash("Job not found."),
            }
            ctx.redirect("/fileLink/jobstatus").await
        }
        _ => {
            // jobstatus
            let jobs = ctx
                .db()
                .rows("select * from job_status where job_type='FILE_SIZE_UPDATE' order by start_time desc, id desc limit 20", &db::no_params())
                .await
                .unwrap_or_default();
            let running = jobs.iter().any(|j| s(j, "status") == "RUNNING");
            let (total, updated, missing, pct) = size_progress(ctx.db()).await;
            let flash = s(&ctx.sess, "flash");
            let mut h = String::new();
            if running {
                h.push_str("<script>setTimeout(function(){ location.reload(); }, 5000);</script>");
            }
            h.push_str(
                "<div id=\"content\" role=\"main\"><div class=\"container\"><section class=\"row\"><div class=\"nav\" role=\"navigation\"><ul>\
                 <li><a class=\"home\" href=\"/\">Home</a></li>\
                 <li><a class=\"list\" href=\"/fileLink/index\">File List</a></li>\
                 <li><a class=\"create\" href=\"/fileLink/jobstatus\">Refresh</a></li>\
                 </ul></div></section><section class=\"row\"><div class=\"col-12 content scaffold-list\" role=\"main\">\
                 <h1>File Size Job Status</h1>",
            );
            if !flash.is_empty() {
                h.push_str(&format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)));
            }
            if running {
                h.push_str("<div class=\"message\" role=\"status\">A job is currently running. This page refreshes every 5 seconds.</div>");
            }
            h.push_str(&format!(
                "<h3>Overall Coverage</h3><table class='table'><tr><th>Total File Records</th><th>With Size Recorded</th><th>Still Missing</th><th>Complete</th></tr>\
                 <tr><td>{total}</td><td>{updated}</td><td>{missing}</td><td>{pct}%</td></tr></table><br/><h3>Recent Jobs</h3>"
            ));
            if jobs.is_empty() {
                h.push_str("<div class=\"message\" role=\"status\">No file size jobs have been run yet. Start one from any module's Files section, or from the file list.</div>");
            } else {
                h.push_str(
                    "<table class='table'><tr><th>#</th><th>Scope</th><th>Status</th><th>Started</th><th>Ended</th><th>Duration</th><th>Sized</th><th>Could Not Size</th><th>Missing at Start</th><th>Error</th><th>Action</th></tr>",
                );
                let now = chrono::Local::now().naive_local();
                for j in &jobs {
                    let jid = i(j, "id").unwrap_or(0);
                    let st = s(j, "status");
                    let cancel_req = db::b(j, "cancel_requested");
                    let st_html = match st.as_str() {
                        "RUNNING" => format!("<strong>RUNNING</strong>{}", if cancel_req { "<br/><small>cancelling&hellip;</small>" } else { "" }),
                        "FAILED" => "<span style=\"color:#b00;\"><strong>FAILED</strong></span>".into(),
                        other => esc(other),
                    };
                    let start_s = s(j, "start_time");
                    let end_s = s(j, "end_time");
                    let dur_s = match crate::script::parse_dt(&start_s) {
                        Some(st_dt) => {
                            let en_dt = crate::script::parse_dt(&end_s).unwrap_or(now);
                            format!("{}s", (en_dt - st_dt).num_seconds().max(0))
                        }
                        None => String::new(),
                    };
                    let act_html = if st == "RUNNING" && !cancel_req {
                        format!("<a href=\"/fileLink/cancelSizeJob/{jid}\" onclick=\"return confirm('Cancel job #{jid}? Work already done is kept.');\">Cancel</a>")
                    } else if st == "RUNNING" && cancel_req {
                        format!("<a href=\"/fileLink/cancelSizeJob/{jid}\" onclick=\"return confirm('Force-stop job #{jid}? Use this only if the worker is gone, e.g. after a restart.');\">Force stop</a>")
                    } else {
                        String::new()
                    };
                    let scope = { let m = s(j, "module_name"); if m.is_empty() { "All modules".into() } else { m } };
                    h.push_str(&format!(
                        "<tr><td>{jid}</td><td>{}</td><td>{st_html}</td><td>{}</td><td>{}</td><td>{dur_s}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{act_html}</td></tr>",
                        esc(&scope),
                        esc(&start_s.chars().take(19).collect::<String>()),
                        esc(&end_s.chars().take(19).collect::<String>()),
                        i(j, "processed_records").unwrap_or(0),
                        i(j, "skipped_records").unwrap_or(0),
                        i(j, "total_records").unwrap_or(0),
                        esc(&s(j, "error_message"))
                    ));
                }
                h.push_str("</table>");
            }
            h.push_str(
                "<p class=\"text-muted\"><em>Sized</em> counts records this run successfully measured on disk. \
                 <em>Could Not Size</em> counts records whose file is no longer present or which have no path recorded &mdash; rerunning will not change those. \
                 Batch size and the pause between batches are tunable via the <code>filesize_job_batch_size</code> and <code>filesize_job_batch_delay_ms</code> portal settings.</p>\
                 </div></section></div></div>",
            );
            ctx.html("File Size Job Status", h).await
        }
    }
}

// ---------------- g6ikram FileLink controller & views ----------------

/// Main dispatcher for `/fileLink/*` matching g6ikram's `FileLinkController` + `views/fileLink/*.gsp`.
pub async fn controller(ctx: Ctx, act: &str, id: Option<i64>) -> Response {
    match act {
        "jobstatus" | "updateMissingSizes" | "updateMissingSizesWithTracking" | "cancelSizeJob" | "sizeUpdateProgress" => {
            size_action(ctx, act, id).await
        }
        "get" => get(ctx, id.unwrap_or(0)).await,
        "download" => {
            if let Some(fid) = id.or_else(|| ctx.p("id").parse::<i64>().ok()) {
                return get(ctx, fid).await;
            }
            let (m, sl) = (ctx.p("module"), ctx.p("slug"));
            download(ctx, &m, &sl).await
        }
        "stream" => stream(ctx, id).await,
        "upload" => {
            if ctx.method == "POST" {
                return upload(ctx).await;
            }
            if ctx.user.is_none() {
                return ctx.need_login().await;
            }
            let m = ctx.p("module");
            let dest = if m.is_empty() { "/fileLink/create".into() } else { format!("/fileLink/create?module={}", enc(&m)) };
            ctx.redirect(&dest).await
        }
        "show" => match id {
            Some(fid) => show(ctx, fid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "create" => create_or_edit(ctx, None).await,
        "save" => save(ctx).await,
        "edit" => match id {
            Some(fid) => create_or_edit(ctx, Some(fid)).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "update" => match id {
            Some(fid) => update(ctx, fid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "delete" => match id {
            Some(fid) => delete_action(ctx, fid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        _ => index(ctx).await,
    }
}

fn file_link_label(field: &str) -> &'static str {
    match field {
        "name" => "Name",
        "module" => "Module",
        "slug" => "Slug",
        "allowedroles" => "Allowedroles",
        "filegroup" => "Filegroup",
        "sortnum" => "Sortnum",
        "path" => "Path",
        "tracker_slug" => "Tracker Slug",
        "tracker_data_id" => "Tracker Data Id",
        "size" => "Size",
        "last_updated" => "Last Updated",
        "last_access" => "Last Access",
        _ => "",
    }
}

fn download_url_for(r: &Row) -> String {
    let m = s(r, "module");
    let sl = s(r, "slug");
    if !sl.is_empty() {
        if m.is_empty() {
            format!("/fileLink/download?slug={}", enc(&sl))
        } else {
            format!("/fileLink/download?module={}&slug={}", enc(&m), enc(&sl))
        }
    } else {
        format!("/fileLink/get/{}", i(r, "id").unwrap_or(0))
    }
}

fn fmt_file_dt(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return String::new();
    }
    if let Some(dt) = crate::script::parse_dt(t) {
        return dt.format("%Y-%m-%d %H:%M:%S").to_string();
    }
    let norm = t.replace('T', " ");
    let base = norm.split('.').next().unwrap_or(&norm).trim_end_matches('Z');
    let base = if base.len() > 19 { &base[..19] } else { base };
    match crate::script::parse_dt(base) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => t.to_string(),
    }
}

async fn allowed_modules_for_files(ctx: &Ctx) -> Option<Vec<String>> {
    if ctx.is_admin() {
        return Some(vec![]); // empty vec with Some(_) means unrestricted (admin)
    }
    let u = ctx.user.as_ref()?;
    let mods = web::admin_list(ctx.db(), u).await;
    if mods.is_empty() { None } else { Some(mods) }
}

async fn index(ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let is_admin = ctx.is_admin();
    let Some(my_mods) = allowed_modules_for_files(&ctx).await else {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this list").await;
    };

    let q = ctx.p("q");
    let m = ctx.p("module");
    let mut cond: Vec<String> = vec![];
    if !m.is_empty() && m != "All" {
        if !is_admin && !my_mods.contains(&m) {
            return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this module").await;
        }
        cond.push(format!("\"module\"={}", lit(&json!(m))));
    } else if !is_admin {
        cond.push(format!("\"module\" in {}", lit(&json!(my_mods))));
    }
    if !q.trim().is_empty() {
        let like = lit(&json!(format!("%{}%", q.trim())));
        cond.push(format!("(name ilike {l} or \"module\" ilike {l} or slug ilike {l})", l = like));
    }
    let wh = if cond.is_empty() { String::new() } else { format!(" where {}", cond.join(" and ")) };
    let max = ctx.pi("max").unwrap_or(10).clamp(1, 500);
    let offset = ctx.pi("offset").unwrap_or(0).max(0);
    let sort_p = ctx.p("sort");
    let order_dir = if ctx.p("order") == "desc" { "desc" } else { "asc" };
    const COLS: &[&str] = &["name", "module", "slug", "size", "filegroup", "sortnum", "allowedroles", "path"];
    let order_sql = if COLS.contains(&sort_p.as_str()) {
        format!("\"{}\" {}, id asc", sort_p, order_dir)
    } else {
        "sortnum asc, id asc".to_string()
    };

    let total = db.count(&format!("select count(*) from file_link{}", wh), &db::no_params()).await.unwrap_or(0);
    let rows = db
        .rows(&format!("select * from file_link{} order by {} limit {} offset {}", wh, order_sql, max, offset), &db::no_params())
        .await
        .unwrap_or_default();

    let mut mods: Vec<String> = if is_admin {
        db.rows("select name from portal_module order by name", &db::no_params())
            .await
            .unwrap_or_default()
            .iter()
            .map(|r| s(r, "name"))
            .collect()
    } else {
        let mut ms = my_mods.clone();
        ms.sort();
        ms
    };
    if !m.is_empty() && m != "All" && !mods.contains(&m) {
        mods.push(m.clone());
    }

    let create_href = if !m.is_empty() && m != "All" {
        format!("/fileLink/create?module={}", enc(&m))
    } else {
        "/fileLink/create".into()
    };
    let mut nav_extra = String::new();
    if is_admin {
        nav_extra.push_str("<li><a class=\"list\" href=\"/fileLink/jobstatus\">File Size Job Status</a></li>");
    }

    let mut mod_opts = String::from("<option value=\"All\">All</option>");
    for x in &mods {
        let sel = if *x == m { " selected=\"selected\"" } else { "" };
        mod_opts.push_str(&format!("<option value=\"{}\"{sel}>{}</option>", esc(x), esc(x)));
    }

    let qs_keep = web::qs(&ctx.params, &["sort", "order", "offset", "max"]);
    let mut thead = String::new();
    for col in COLS {
        let sorted = sort_p == *col;
        let next_ord = if sorted && order_dir == "asc" { "desc" } else { "asc" };
        let cls = if sorted { format!("sortable sorted {}", order_dir) } else { "sortable".into() };
        let extra_q = if qs_keep.is_empty() { String::new() } else { format!("&amp;{}", esc(&qs_keep)) };
        thead.push_str(&format!(
            "<th class=\"{cls}\"><a href=\"/fileLink/index?sort={col}&amp;max={max}&amp;order={next_ord}{extra_q}\">{}</a></th>",
            esc(file_link_label(col))
        ));
    }

    let mut tbody = String::new();
    for (rn, r) in rows.iter().enumerate() {
        let fid = i(r, "id").unwrap_or(0);
        let row_cls = if rn % 2 == 0 { "even" } else { "odd" };
        let name_txt = { let n = s(r, "name"); if n.trim().is_empty() { format!("#{fid}") } else { n } };
        let path_txt = s(r, "path");
        let path_cell = if path_txt.is_empty() {
            String::new()
        } else {
            format!("<a href=\"{}\">{}</a>", esc(&download_url_for(r)), esc(&path_txt))
        };
        let size_cell = i(r, "size").map(|v| v.to_string()).unwrap_or_default();
        let sort_cell = i(r, "sortnum").map(|v| v.to_string()).unwrap_or_default();
        tbody.push_str(&format!(
            "<tr class=\"{row_cls}\">\
             <td><a href=\"/fileLink/show/{fid}\">{}</a>&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{size_cell}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{sort_cell}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{path_cell}&nbsp;</td>\
             </tr>",
            esc(&name_txt),
            esc(&s(r, "module")),
            esc(&s(r, "slug")),
            esc(&s(r, "filegroup")),
            esc(&s(r, "allowedroles"))
        ));
    }

    let pag = if total > max {
        format!("<div class=\"pagination\">{}</div>", crate::tags::paginate("/fileLink/index", &ctx.params, total, max, offset))
    } else {
        String::new()
    };
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };

    let body = format!(
        r##"<style>
@media only screen and (max-width: 760px), (min-device-width: 768px) and (max-device-width: 1024px) {{
  td:nth-of-type(1):before {{ content: "Name"; }}
  td:nth-of-type(2):before {{ content: "Module"; }}
  td:nth-of-type(3):before {{ content: "Slug"; }}
  td:nth-of-type(4):before {{ content: "Size"; }}
  td:nth-of-type(5):before {{ content: "Filegroup"; }}
  td:nth-of-type(6):before {{ content: "Sortnum"; }}
  td:nth-of-type(7):before {{ content: "Allowedroles"; }}
  td:nth-of-type(8):before {{ content: "Path"; }}
}}
</style>
<div id="content" role="main"><div class="container">
<section class="row"><a href="#list-fileLink" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="create" href="{create_href}">New FileLink</a></li>
  {nav_extra}
</ul></div></section>
<section class="row"><div id="list-fileLink" class="col-12 content scaffold-list" role="main">
  <h1>FileLink List</h1>
  {flash_html}
  <form action="/fileLink/index" method="get" name="searchform" id="searchform">
    <fieldset class="form">
      <div class="fieldcontain" id="filesearch">
        <label>Search:</label><input type="text" name="q" id="q" value="{q_val}"/><input id="dosearch" name="dosearch" type="submit" value="Search"/>
      </div>
      <div class="fieldcontain" id="listsearch">
        <label>Module:</label>
        <select name="module" id="module" class="module-select " onchange="this.form.submit()">{mod_opts}</select>
      </div>
    </fieldset>
  </form>
  <table class="responsive">
    <thead><tr>{thead}</tr></thead>
    <tbody>{tbody}</tbody>
  </table>
  {pag}
</div></section></div></div>"##,
        q_val = esc(&q)
    );
    ctx.html("FileLink List", body).await
}

async fn show(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(r)) = db.get("file_link", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let module = s(&r, "module");
    if !ctx.can_admin(&module).await {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this").await;
    }

    let mut items = String::new();
    let push_li = |buf: &mut String, field: &str, val_html: String| {
        buf.push_str(&format!(
            "<li class=\"fieldcontain\"><span id=\"{field}-label\" class=\"property-label\">{}</span>\
             <div class=\"property-value\" aria-labelledby=\"{field}-label\">{val_html}</div></li>",
            esc(file_link_label(field))
        ));
    };
    for &f in &["name", "module", "slug", "allowedroles", "filegroup", "sortnum", "path", "tracker_slug", "tracker_data_id", "size", "last_updated", "last_access"] {
        let v_html = match f {
            "path" => {
                let p = s(&r, "path");
                if p.is_empty() {
                    String::new()
                } else {
                    format!("<a href=\"{}\">{}</a>", esc(&download_url_for(&r)), esc(&p))
                }
            }
            "sortnum" | "tracker_data_id" | "size" => i(&r, f).map(|n| n.to_string()).unwrap_or_default(),
            "last_updated" | "last_access" => esc(&fmt_file_dt(&s(&r, f))),
            _ => esc(&s(&r, f)),
        };
        push_li(&mut items, f, v_html);
    }

    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#show-fileLink" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/fileLink/index">FileLink List</a></li>
  <li><a class="create" href="/fileLink/create">New FileLink</a></li>
</ul></div></section>
<section class="row"><div id="show-fileLink" class="col-12 content scaffold-show" role="main">
  <h1>Show FileLink</h1>
  {flash_html}
  <ol class="property-list fileLink">{items}</ol>
  <form action="/fileLink/delete/{id}" method="post">
    {csrf}
    <fieldset class="buttons">
      <a class="edit" href="/fileLink/edit/{id}">Edit</a>
      <input class="delete" type="submit" value="Delete" onclick="return confirm('Are you sure?');" />
    </fieldset>
  </form>
</div></section></div></div>"##
    );
    ctx.html("Show FileLink", body).await
}

async fn create_or_edit(mut ctx: Ctx, id: Option<i64>) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let is_admin = ctx.is_admin();
    let Some(my_mods) = allowed_modules_for_files(&ctx).await else {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    };
    let is_new = id.is_none();
    let r = match id {
        Some(fid) => match db.get("file_link", fid).await {
            Ok(Some(row)) => {
                if !is_admin && !my_mods.contains(&s(&row, "module")) {
                    return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to edit this").await;
                }
                row
            }
            _ => return ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        None => {
            let mut row = Row::new();
            for f in &["name", "module", "slug", "allowedroles", "filegroup", "sortnum", "path", "tracker_slug", "tracker_data_id", "size"] {
                let v = ctx.p(f);
                if !v.is_empty() {
                    row.insert((*f).to_string(), json!(v));
                }
            }
            row
        }
    };

    let mut mods: Vec<String> = if is_admin {
        db.rows("select name from portal_module order by name", &db::no_params())
            .await
            .unwrap_or_default()
            .iter()
            .map(|m| s(m, "name"))
            .collect()
    } else {
        let mut ms = my_mods.clone();
        ms.sort();
        ms
    };
    let cur_mod = s(&r, "module");
    if !cur_mod.is_empty() && !mods.contains(&cur_mod) {
        mods.push(cur_mod.clone());
    }
    if mods.is_empty() {
        mods.push("portal".into());
    }

    let txt = |name: &str| -> String {
        format!(
            "<div class=\"fieldcontain\"><label for=\"{name}\">{}</label><input type=\"text\" name=\"{name}\" id=\"{name}\" value=\"{}\"/></div>",
            esc(file_link_label(name)),
            esc(&s(&r, name))
        )
    };
    let num = |name: &str| -> String {
        let val = i(&r, name).map(|n| n.to_string()).unwrap_or_else(|| s(&r, name));
        format!(
            "<div class=\"fieldcontain\"><label for=\"{name}\">{}</label><input type=\"number\" name=\"{name}\" id=\"{name}\" value=\"{}\"/></div>",
            esc(file_link_label(name)),
            esc(&val)
        )
    };

    let mut mod_select = String::from("<div class=\"fieldcontain\"><label for=\"module\">Module</label><select name=\"module\" id=\"module\" class=\"module-select \">");
    if cur_mod.is_empty() {
        mod_select.push_str("<option value=\"\">- choose a module -</option>");
    }
    for m in &mods {
        let sel = if *m == cur_mod { " selected=\"selected\"" } else { "" };
        mod_select.push_str(&format!("<option value=\"{}\"{sel}>{}</option>", esc(m), esc(m)));
    }
    mod_select.push_str("</select></div>");

    let mut fields = String::new();
    fields.push_str(&txt("name"));
    fields.push_str(&mod_select);
    fields.push_str(&txt("slug"));
    fields.push_str(&txt("allowedroles"));
    fields.push_str(&txt("filegroup"));
    fields.push_str(&num("sortnum"));
    fields.push_str(&txt("path"));
    fields.push_str(&txt("tracker_slug"));
    fields.push_str(&num("tracker_data_id"));
    fields.push_str(&num("size"));

    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let action = match id {
        Some(fid) => format!("/fileLink/update/{fid}"),
        None => "/fileLink/save".into(),
    };
    let kind = if is_new { "create" } else { "edit" };
    let heading = if is_new { "Create FileLink" } else { "Edit FileLink" };
    let btn = if is_new { "Create" } else { "Update" };
    let new_li = if !is_new { "<li><a class=\"create\" href=\"/fileLink/create\">New FileLink</a></li>" } else { "" };

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#{kind}-fileLink" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/fileLink/index">FileLink List</a></li>
  {new_li}
</ul></div></section>
<section class="row"><div id="{kind}-fileLink" class="col-12 content scaffold-{kind}" role="main">
  <h1>{heading}</h1>
  {flash_html}
  <form action="{action}" method="post" enctype="multipart/form-data">
    {csrf}
    <fieldset class="form">{fields}</fieldset>
    <fieldset class="form">
      <div class="fieldcontain">
        <label for="fileupload">File</label>
        <input type="file" name="fileupload" value="" id="fileupload" />
      </div>
    </fieldset>
    <fieldset class="buttons"><input class="save" type="submit" value="{btn}" /></fieldset>
  </form>
  {form_js}
</div></section></div></div>"##,
        form_js = crate::tags::form_js()
    );
    ctx.html(heading, body).await
}

fn opt_str(v: String) -> Value {
    let t = v.trim();
    if t.is_empty() { Value::Null } else { json!(t) }
}

fn opt_int(v: String) -> Value {
    v.trim().parse::<i64>().map(|n| json!(n)).unwrap_or(Value::Null)
}

async fn save_uploaded_file_for_link(ctx: &Ctx, up: &Upload, module: &str) -> Result<(String, i64)> {
    let clean = validate_upload(up)?;
    let max_size: u64 = if let Some(u) = &ctx.user {
        web::setting_str(ctx.db(), &format!("filemanager_max_{}", s(u, "userid")))
            .await
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(5_242_880)
    } else {
        5_242_880
    };
    if max_size > 0 && up.size > max_size {
        return Err(anyhow!("File size {} exceeds maximum allowed ({} bytes)", up.size, max_size));
    }
    let base = web::setting_str(ctx.db(), "uploadfolder")
        .await
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| ctx.st.cfg.upload_dir.clone());
    let dir = PathBuf::from(&base).join(module);
    tokio::fs::create_dir_all(&dir).await?;
    let target = dir.join(&clean);
    up.save_to(&target).await?;
    Ok((target.to_string_lossy().to_string(), up.size as i64))
}

async fn save(mut ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if ctx.method != "POST" {
        return ctx.redirect("/fileLink/create").await;
    }
    let db = &ctx.st.db.clone();
    let module = { let m = ctx.p("module").trim().to_string(); if m.is_empty() { "portal".into() } else { m } };
    if !ctx.can_admin(&module).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }

    let uploaded = ctx.files.remove("fileupload").or_else(|| ctx.files.remove("file"));
    let mut name = ctx.p("name").trim().to_string();
    let mut slug = ctx.p("slug").trim().to_string();
    let mut path = ctx.p("path").trim().to_string();
    let size_val: Value;

    if let Some(up) = uploaded.filter(|u| !u.filename.is_empty() || u.size > 0) {
        match save_uploaded_file_for_link(&ctx, &up, &module).await {
            Ok((saved_path, sz)) => {
                path = saved_path;
                size_val = json!(sz);
                if name.is_empty() {
                    name = up.filename.clone();
                }
                if slug.is_empty() {
                    slug = sanitize_filename(&up.filename).replace(' ', "_").to_lowercase();
                }
            }
            Err(e) => {
                ctx.flash(&format!("File upload failed: {}", e));
                return ctx.redirect("/fileLink/create").await;
            }
        }
    } else if !path.is_empty() {
        if let Ok(md) = tokio::fs::metadata(&path).await {
            if md.is_file() {
                size_val = json!(md.len() as i64);
            } else {
                ctx.flash("file cannot be empty");
                return ctx.redirect("/fileLink/create").await;
            }
        } else {
            ctx.flash("file cannot be empty");
            return ctx.redirect("/fileLink/create").await;
        }
    } else {
        ctx.flash("file cannot be empty");
        return ctx.redirect("/fileLink/create").await;
    }

    if !slug.is_empty() && db.count("select count(*) from file_link where slug=:s", &row! {"s"=>slug.clone()}).await.unwrap_or(0) > 0 {
        ctx.flash(&format!("Slug '{}' already exists", slug));
        return ctx.redirect("/fileLink/create").await;
    }

    let d = row! {
        "name"=>opt_str(name),
        "module"=>json!(module),
        "slug"=>opt_str(slug),
        "allowedroles"=>opt_str(ctx.p("allowedroles")),
        "filegroup"=>opt_str(ctx.p("filegroup")),
        "sortnum"=>opt_int(ctx.p("sortnum")),
        "path"=>opt_str(path),
        "tracker_slug"=>opt_str(ctx.p("tracker_slug")),
        "tracker_data_id"=>opt_int(ctx.p("tracker_data_id")),
        "size"=>size_val,
        "last_updated"=>now_str()
    };
    match db.insert("file_link", &d).await {
        Ok(id) => {
            ctx.flash(&format!("FileLink {} created", id));
            ctx.redirect(&format!("/fileLink/show/{}", id)).await
        }
        Err(e) => {
            ctx.flash(&format!("Could not create FileLink: {}", e));
            ctx.redirect("/fileLink/create").await
        }
    }
}

async fn update(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if ctx.method != "POST" {
        return ctx.redirect(&format!("/fileLink/edit/{id}")).await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(existing)) = db.get("file_link", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let old_mod = s(&existing, "module");
    let module = { let m = ctx.p("module").trim().to_string(); if m.is_empty() { old_mod.clone() } else { m } };
    if !ctx.can_admin(&old_mod).await || !ctx.can_admin(&module).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }

    let old_path = s(&existing, "path");
    let old_name = s(&existing, "name");
    let uploaded = ctx.files.remove("fileupload").or_else(|| ctx.files.remove("file"));
    let mut name = ctx.p("name").trim().to_string();
    let slug = ctx.p("slug").trim().to_string();
    let mut path = ctx.p("path").trim().to_string();
    let mut size_val = opt_int(ctx.p("size"));
    let mut touch_updated = false;

    if let Some(up) = uploaded.filter(|u| !u.filename.is_empty() || u.size > 0) {
        match save_uploaded_file_for_link(&ctx, &up, &module).await {
            Ok((saved_path, sz)) => {
                path = saved_path;
                size_val = json!(sz);
                touch_updated = true;
                if name.is_empty() {
                    name = up.filename.clone();
                }
            }
            Err(e) => {
                ctx.flash(&format!("File upload failed: {}", e));
                return ctx.redirect(&format!("/fileLink/edit/{id}")).await;
            }
        }
    } else if path != old_path || name != old_name {
        touch_updated = true;
        if path != old_path && !path.is_empty() {
            if let Ok(md) = tokio::fs::metadata(&path).await {
                if md.is_file() {
                    size_val = json!(md.len() as i64);
                }
            }
        }
    }

    if !slug.is_empty() && db.count("select count(*) from file_link where slug=:s and id<>:id", &row! {"s"=>slug.clone(),"id"=>id}).await.unwrap_or(0) > 0 {
        ctx.flash(&format!("Slug '{}' already exists", slug));
        return ctx.redirect(&format!("/fileLink/edit/{id}")).await;
    }

    let mut d = row! {
        "name"=>opt_str(name),
        "module"=>json!(module),
        "slug"=>opt_str(slug),
        "allowedroles"=>opt_str(ctx.p("allowedroles")),
        "filegroup"=>opt_str(ctx.p("filegroup")),
        "sortnum"=>opt_int(ctx.p("sortnum")),
        "path"=>opt_str(path),
        "tracker_slug"=>opt_str(ctx.p("tracker_slug")),
        "tracker_data_id"=>opt_int(ctx.p("tracker_data_id")),
        "size"=>size_val
    };
    if touch_updated {
        d.insert("last_updated".into(), json!(now_str()));
    }
    match db.update("file_link", id, &d).await {
        Ok(_) => {
            ctx.flash(&format!("FileLink {} updated", id));
            ctx.redirect(&format!("/fileLink/show/{}", id)).await
        }
        Err(e) => {
            ctx.flash(&format!("Could not update FileLink: {}", e));
            ctx.redirect(&format!("/fileLink/edit/{id}")).await
        }
    }
}

async fn delete_action(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if ctx.method != "POST" {
        return ctx.status(StatusCode::METHOD_NOT_ALLOWED, "POST required").await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(existing)) = db.get("file_link", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    if !ctx.can_admin(&s(&existing, "module")).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    delete_file_link(db, id).await;
    ctx.flash(&format!("FileLink {} deleted", id));
    ctx.redirect("/fileLink/index").await
}

fn parse_byte_range(header_val: &str, file_len: u64) -> Option<Result<(u64, u64), ()>> {
    let rest = header_val.strip_prefix("bytes=")?;
    let spec = rest.split(',').next()?.trim();
    if spec.is_empty() || file_len == 0 {
        return Some(Err(()));
    }
    let (start, end) = if let Some(suf) = spec.strip_prefix('-') {
        let suffix: u64 = suf.parse().ok()?;
        if suffix == 0 {
            return Some(Err(()));
        }
        (file_len.saturating_sub(suffix), file_len - 1)
    } else {
        let (a, b) = spec.split_once('-')?;
        let st: u64 = a.parse().ok()?;
        let en: u64 = if b.trim().is_empty() {
            file_len - 1
        } else {
            b.trim().parse::<u64>().ok()?.min(file_len - 1)
        };
        (st, en)
    };
    if start > end || start >= file_len {
        Some(Err(()))
    } else {
        Some(Ok((start, end)))
    }
}

/// `/fileLink/stream` with HTTP Range (206 Partial Content) support matching `FileLinkController.stream`.
pub async fn stream(ctx: Ctx, id: Option<i64>) -> Response {
    let found = if let Some(fid) = id.or_else(|| ctx.p("id").parse::<i64>().ok()) {
        ctx.db().get("file_link", fid).await
    } else {
        let slug = ctx.p("slug");
        let slug_root = slug.split('.').next().unwrap_or("");
        let m = ctx.p("module");
        if m.is_empty() {
            ctx.db().first("select * from file_link where slug=:s order by id desc limit 1", &row! {"s"=>slug_root}).await
        } else {
            ctx.db().first("select * from file_link where module=:m and slug=:s order by id desc limit 1", &row! {"m"=>m,"s"=>slug_root}).await
        }
    };
    let Ok(Some(f)) = found else {
        return ctx.text_status(StatusCode::NOT_FOUND, "File not found").await;
    };
    if !allowed(&ctx, &f).await {
        return ctx.text_status(StatusCode::UNAUTHORIZED, "Authentication required").await;
    }
    let path = s(&f, "path");
    if path.is_empty() {
        return ctx.text_status(StatusCode::NOT_FOUND, "File not found").await;
    }
    let Ok(md) = tokio::fs::metadata(&path).await else {
        return ctx.text_status(StatusCode::NOT_FOUND, "File not found").await;
    };
    if !md.is_file() {
        return ctx.text_status(StatusCode::NOT_FOUND, "File not found").await;
    }
    if let Some(fid) = i(&f, "id") {
        let _ = ctx.db().exec("update file_link set last_access=now() where id=:id", &row! {"id"=>fid}).await;
    }
    let file_len = md.len();
    let range_hdr = ctx.headers.get(header::RANGE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let (start, end, partial) = if !range_hdr.is_empty() {
        match parse_byte_range(range_hdr, file_len) {
            Some(Ok((s, e))) => (s, e, true),
            Some(Err(())) => {
                let mut resp = (StatusCode::RANGE_NOT_SATISFIABLE, "Requested range not satisfiable").into_response();
                if let Ok(v) = HeaderValue::from_str(&format!("bytes */{}", file_len)) {
                    resp.headers_mut().insert(header::CONTENT_RANGE, v);
                }
                return resp;
            }
            None => (0, file_len.saturating_sub(1), false),
        }
    } else {
        (0, file_len.saturating_sub(1), false)
    };

    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let Ok(mut file) = tokio::fs::File::open(&path).await else {
        return ctx.text_status(StatusCode::NOT_FOUND, "File not found").await;
    };
    let to_read = if file_len == 0 { 0 } else { (end - start + 1) as usize };
    let mut buf = vec![0u8; to_read];
    if to_read > 0 {
        if file.seek(std::io::SeekFrom::Start(start)).await.is_err() || file.read_exact(&mut buf).await.is_err() {
            return ctx.text_status(StatusCode::INTERNAL_SERVER_ERROR, "Error reading file").await;
        }
    }
    let fname = Path::new(&path).file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_else(|| s(&f, "name"));
    let mime = mime_guess::from_path(&fname).first_or_octet_stream().to_string();
    let status = if partial { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK };
    let mut resp = (status, buf).into_response();
    let hdrs = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&mime) {
        hdrs.insert(header::CONTENT_TYPE, v);
    }
    hdrs.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(v) = HeaderValue::from_str(&format!("inline; filename=\"{}\"", fname.replace('"', ""))) {
        hdrs.insert(header::CONTENT_DISPOSITION, v);
    }
    if partial {
        if let Ok(v) = HeaderValue::from_str(&format!("bytes {}-{}/{}", start, end, file_len)) {
            hdrs.insert(header::CONTENT_RANGE, v);
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_range_parsing() {
        assert_eq!(parse_byte_range("bytes=0-99", 1000), Some(Ok((0, 99))));
        assert_eq!(parse_byte_range("bytes=500-", 1000), Some(Ok((500, 999))));
        assert_eq!(parse_byte_range("bytes=-200", 1000), Some(Ok((800, 999))));
        assert_eq!(parse_byte_range("bytes=0-5000", 1000), Some(Ok((0, 999))));
        assert_eq!(parse_byte_range("bytes=1000-1005", 1000), Some(Err(())));
        assert_eq!(parse_byte_range("bytes=500-400", 1000), Some(Err(())));
    }

    #[test]
    fn download_url_prefers_module_and_slug() {
        let r = row! {"id"=>42i64,"module"=>"hr","slug"=>"policy_pdf"};
        assert_eq!(download_url_for(&r), "/fileLink/download?module=hr&slug=policy_pdf");
        let r2 = row! {"id"=>42i64,"module"=>"hr"};
        assert_eq!(download_url_for(&r2), "/fileLink/get/42");
    }
}
