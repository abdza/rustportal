//! File links: stored uploads, downloads with role checks, and the file manager.

use crate::db::{self, i, s, Db, Row};
use crate::row;
use crate::web::{self, esc, now_str, Ctx, Upload};
use anyhow::{anyhow, Result};
use axum::response::Response;
use serde_json::Value;
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

// ---------------- file manager ----------------

fn safe_join(root: &str, rel: &str) -> Option<PathBuf> {
    let mut p = PathBuf::from(root);
    for part in rel.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains('\\') {
            return None;
        }
        p.push(part);
    }
    Some(p)
}

/// /portalFileManager/browse/{id}?path=sub/dir — list, download, upload, mkdir, delete.
pub async fn browse(mut ctx: Ctx, id: i64) -> Response {
    let Ok(Some(fm)) = ctx.db().get("portal_file_manager", id).await else {
        return ctx.status(axum::http::StatusCode::NOT_FOUND, "File manager not found").await;
    };
    if !ctx.can_admin(&s(&fm, "module")).await {
        return ctx.status(axum::http::StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let root = s(&fm, "path");
    let rel = ctx.p("path");
    let Some(dir) = safe_join(&root, &rel) else {
        return ctx.status(axum::http::StatusCode::BAD_REQUEST, "Invalid path").await;
    };
    let here = format!("/portalFileManager/browse/{}?path={}", id, web::enc(&rel));
    if ctx.method == "POST" {
        let act = ctx.p("do");
        if act == "mkdir" {
            let name = sanitize_filename(&ctx.p("name"));
            let _ = tokio::fs::create_dir_all(dir.join(name)).await;
        } else if act == "delete" {
            if let Some(t) = safe_join(&dir.to_string_lossy(), &sanitize_filename(&ctx.p("name"))) {
                if t.is_dir() {
                    let _ = tokio::fs::remove_dir(&t).await;
                } else {
                    let _ = tokio::fs::remove_file(&t).await;
                }
            }
        } else if let Some(up) = ctx.files.remove("file") {
            match validate_upload(&up) {
                Ok(name) => {
                    let _ = tokio::fs::create_dir_all(&dir).await;
                    let _ = up.save_to(&dir.join(name)).await;
                }
                Err(e) => ctx.flash(&e.to_string()),
            }
        }
        return ctx.redirect(&here).await;
    }
    if dir.is_file() {
        let name = dir.file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
        return match tokio::fs::read(&dir).await {
            Ok(d) => ctx.bytes(&mime_guess::from_path(&name).first_or_octet_stream().to_string(), Some(&name), d, false).await,
            Err(_) => ctx.status(axum::http::StatusCode::NOT_FOUND, "File not found").await,
        };
    }
    let mut entries: Vec<(bool, String, u64)> = vec![];
    if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let md = e.metadata().await.ok();
            entries.push((md.as_ref().map(|m| m.is_dir()).unwrap_or(false), e.file_name().to_string_lossy().to_string(), md.map(|m| m.len()).unwrap_or(0)));
        }
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.to_lowercase().cmp(&b.1.to_lowercase())));
    let csrf = ctx.csrf_field();
    let mut h = format!("<div class='container'><h1>{}</h1><nav class='breadcrumb'><a class='breadcrumb-item' href='/portalFileManager/browse/{}'>root</a>", esc(&s(&fm, "name")), id);
    let mut acc = String::new();
    for part in rel.split('/').filter(|x| !x.is_empty()) {
        acc = if acc.is_empty() { part.to_string() } else { format!("{}/{}", acc, part) };
        h.push_str(&format!("<a class='breadcrumb-item' href='/portalFileManager/browse/{}?path={}'>{}</a>", id, web::enc(&acc), esc(part)));
    }
    h.push_str("</nav><table class='table table-sm'><thead><tr><th>Name</th><th>Size</th><th></th></tr></thead><tbody>");
    for (is_dir, name, size) in entries {
        let sub = if rel.is_empty() { name.clone() } else { format!("{}/{}", rel.trim_matches('/'), name) };
        h.push_str(&format!(
            "<tr><td><i class='bi bi-{}'></i> <a href='/portalFileManager/browse/{}?path={}'>{}</a></td><td>{}</td><td><form method='post' action='{}' onsubmit=\"return confirm('Delete?')\">{}<input type='hidden' name='do' value='delete'/><input type='hidden' name='name' value='{}'/><button class='btn btn-sm btn-outline-danger'>Delete</button></form></td></tr>",
            if is_dir { "folder" } else { "file-earmark" }, id, web::enc(&sub), esc(&name), if is_dir { String::new() } else { size.to_string() }, esc(&here), csrf, esc(&name)
        ));
    }
    h.push_str(&format!(
        "</tbody></table><div class='row'><form class='col' method='post' enctype='multipart/form-data' action='{a}'>{c}<input type='file' name='file' required/> <button class='btn btn-primary btn-sm'>Upload</button></form><form class='col' method='post' action='{a}'>{c}<input type='hidden' name='do' value='mkdir'/><input name='name' placeholder='New folder' required/> <button class='btn btn-secondary btn-sm'>Create folder</button></form></div></div>",
        a = esc(&here), c = csrf
    ));
    let _ = Value::Null;
    ctx.html("File Manager", h).await
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
