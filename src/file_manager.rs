//! PortalFileManager controller and views matching g6ikram's
//! `PortalFileManagerController` + `views/portalFileManager/*.gsp`.

use crate::db::{self, i, lit, s, Db, Row};
use crate::files::sanitize_filename;
use crate::row;
use crate::web::{self, enc, esc, now_str, Ctx, Upload};
use anyhow::{anyhow, Result};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;
use std::io::Read;
use std::path::{Path, PathBuf};

const FM_BLOCKED_EXT: &[&str] = &[
    "exe", "bat", "cmd", "com", "sh", "ps1", "vbs", "jar", "msi", "scr", "dll", "php", "jsp", "asp", "aspx", "cgi", "pl", "py",
];

/// Main dispatcher for `/portalFileManager/*`.
pub async fn controller(ctx: Ctx, act: &str, id: Option<i64>) -> Response {
    match act {
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
            Some(fid) => delete_def(ctx, fid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "explore" | "browse" => match id {
            Some(fid) => explore(ctx, fid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "explorepage" => match id {
            Some(fid) => explorepage(ctx, fid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "download" => match id {
            Some(fid) => download_file(ctx, fid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "createfolder" => match id {
            Some(fid) => createfolder(ctx, fid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "rename" => match id {
            Some(fid) => rename(ctx, fid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "deletefm" => match id {
            Some(fid) => deletefm(ctx, fid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "upload" => match id {
            Some(fid) => upload(ctx, fid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        _ => index(ctx).await,
    }
}

/// Normalizes a relative explorer path (rejecting `..`). Returns `""` for root.
fn norm_rel(raw: &str) -> Option<String> {
    let cleaned = raw.replace('\\', "/");
    let mut parts: Vec<&str> = vec![];
    for p in cleaned.split('/') {
        if p.is_empty() || p == "." {
            continue;
        }
        if p == ".." {
            return None;
        }
        parts.push(p);
    }
    Some(parts.join("/"))
}

fn safe_resolve(root: &str, rel: &str) -> Option<PathBuf> {
    let n = norm_rel(rel)?;
    let mut p = PathBuf::from(root);
    if !n.is_empty() {
        for seg in n.split('/') {
            p.push(seg);
        }
    }
    Some(p)
}

fn join_rel(rel: &str, child: &str) -> String {
    if rel.is_empty() {
        format!("/{}", child)
    } else {
        format!("/{}/{}", rel.trim_matches('/'), child)
    }
}

fn parent_rel(rel: &str) -> String {
    let trimmed = rel.trim_matches('/');
    match trimmed.rsplit_once('/') {
        Some((p, _)) if !p.is_empty() => format!("/{}", p),
        _ => "/".to_string(),
    }
}

fn filename_to_slug(filename: &str) -> String {
    filename.replace('.', "_")
}

fn validate_fm_upload(up: &Upload) -> Result<String> {
    let name = sanitize_filename(&up.filename);
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    if name.contains('.') && FM_BLOCKED_EXT.contains(&ext.as_str()) {
        return Err(anyhow!("File type .{} is not allowed", ext));
    }
    if up.head().starts_with(b"MZ") || up.head().starts_with(b"\x7fELF") {
        return Err(anyhow!("Executable files are not allowed"));
    }
    Ok(name)
}

fn fmt_mtime(md: &std::fs::Metadata) -> String {
    match md.modified() {
        Ok(st) => {
            let dt: chrono::DateTime<chrono::Local> = st.into();
            dt.format("%d/%m/%Y %H:%M:%S").to_string()
        }
        Err(_) => String::new(),
    }
}

async fn is_dev_for_any(ctx: &Ctx) -> bool {
    if ctx.is_admin() {
        return true;
    }
    match &ctx.user {
        Some(u) => !web::developer_list(ctx.db(), u).await.is_empty(),
        None => false,
    }
}

async fn is_dev_for_module(ctx: &Ctx, module: &str) -> bool {
    if ctx.is_admin() {
        return true;
    }
    match &ctx.user {
        Some(u) => web::developer_list(ctx.db(), u).await.iter().any(|m| m == module),
        None => false,
    }
}

async fn can_access_fm(ctx: &Ctx, fm: &Row) -> bool {
    if ctx.is_admin() {
        return true;
    }
    let m = s(fm, "module");
    match &ctx.user {
        Some(u) => web::admin_list(ctx.db(), u).await.contains(&m) || web::developer_list(ctx.db(), u).await.contains(&m),
        None => false,
    }
}

// ---------------- CRUD screens ----------------

async fn index(ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let is_admin = ctx.is_admin();
    let u = ctx.user.as_ref().unwrap();
    let my_mods = if is_admin { vec![] } else { web::admin_list(db, u).await };
    if !is_admin && my_mods.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this list").await;
    }

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
        cond.push(format!("(name ilike {l} or path ilike {l} or \"module\" ilike {l})", l = like));
    }
    let wh = if cond.is_empty() { String::new() } else { format!(" where {}", cond.join(" and ")) };
    let max = ctx.pi("max").unwrap_or(10).clamp(1, 500);
    let offset = ctx.pi("offset").unwrap_or(0).max(0);
    let sort_p = ctx.p("sort");
    let order_dir = if ctx.p("order") == "desc" { "desc" } else { "asc" };
    const COLS: &[(&str, &str)] = &[("name", "Name"), ("module", "Module"), ("path", "Path")];
    let order_sql = if COLS.iter().any(|(c, _)| *c == sort_p) {
        format!("\"{}\" {}, id asc", sort_p, order_dir)
    } else {
        "id asc".to_string()
    };

    let total = db.count(&format!("select count(*) from portal_file_manager{}", wh), &db::no_params()).await.unwrap_or(0);
    let rows = db
        .rows(&format!("select * from portal_file_manager{} order by {} limit {} offset {}", wh, order_sql, max, offset), &db::no_params())
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

    let can_create = is_dev_for_any(&ctx).await;
    let create_href = if !m.is_empty() && m != "All" {
        format!("/portalFileManager/create?module={}", enc(&m))
    } else {
        "/portalFileManager/create".into()
    };
    let create_li = if can_create {
        format!("<li><a class=\"create\" href=\"{create_href}\">New PortalFileManager</a></li>")
    } else {
        String::new()
    };

    let mut mod_opts = String::from("<option value=\"All\">All</option>");
    for x in &mods {
        let sel = if *x == m { " selected=\"selected\"" } else { "" };
        mod_opts.push_str(&format!("<option value=\"{}\"{sel}>{}</option>", esc(x), esc(x)));
    }

    let qs_keep = web::qs(&ctx.params, &["sort", "order", "offset", "max"]);
    let mut thead = String::new();
    for (col, lbl) in COLS {
        let sorted = sort_p == *col;
        let next_ord = if sorted && order_dir == "asc" { "desc" } else { "asc" };
        let cls = if sorted { format!("sortable sorted {}", order_dir) } else { "sortable".into() };
        let extra_q = if qs_keep.is_empty() { String::new() } else { format!("&amp;{}", esc(&qs_keep)) };
        thead.push_str(&format!(
            "<th class=\"{cls}\"><a href=\"/portalFileManager/index?sort={col}&amp;max={max}&amp;order={next_ord}{extra_q}\">{}</a></th>",
            esc(lbl)
        ));
    }

    let mut tbody = String::new();
    for (rn, r) in rows.iter().enumerate() {
        let fid = i(r, "id").unwrap_or(0);
        let row_cls = if rn % 2 == 0 { "even" } else { "odd" };
        let name_txt = { let n = s(r, "name"); if n.trim().is_empty() { format!("#{fid}") } else { n } };
        let path_txt = s(r, "path");
        tbody.push_str(&format!(
            "<tr class=\"{row_cls}\">\
             <td><a href=\"/portalFileManager/show/{fid}\">{}</a>&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td><a href=\"/portalFileManager/explore/{fid}\">{}</a>&nbsp;</td>\
             </tr>",
            esc(&name_txt),
            esc(&s(r, "module")),
            esc(&path_txt)
        ));
    }

    let pag = if total > max {
        format!("<div class=\"pagination\">{}</div>", crate::tags::paginate("/portalFileManager/index", &ctx.params, total, max, offset))
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
  td:nth-of-type(3):before {{ content: "Path"; }}
}}
</style>
<div id="content" role="main"><div class="container">
<section class="row"><a href="#list-portalFileManager" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  {create_li}
</ul></div></section>
<section class="row"><div id="list-portalFileManager" class="col-12 content scaffold-list" role="main">
  <h1>PortalFileManager List</h1>
  {flash_html}
  <form action="/portalFileManager/index" method="get" name="searchform" id="searchform">
    <fieldset class="form">
      <div class="fieldcontain" id="listsearch">
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
    ctx.html("PortalFileManager List", body).await
}

async fn show(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(fm)) = db.get("portal_file_manager", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    if !can_access_fm(&ctx, &fm).await {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this").await;
    }
    let module = s(&fm, "module");
    let is_dev = is_dev_for_module(&ctx, &module).await;
    let new_li = if is_dev { "<li><a class=\"create\" href=\"/portalFileManager/create\">New PortalFileManager</a></li>" } else { "" };
    let csrf = ctx.csrf_field();
    let buttons = if is_dev {
        format!(
            "<form action=\"/portalFileManager/delete/{id}\" method=\"post\">{csrf}\
             <fieldset class=\"buttons\">\
             <a class=\"edit\" href=\"/portalFileManager/edit/{id}\">Edit</a>\
             <input class=\"delete\" type=\"submit\" value=\"Delete\" onclick=\"return confirm('Are you sure?');\" />\
             </fieldset></form>"
        )
    } else {
        String::new()
    };
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#show-portalFileManager" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalFileManager/index">PortalFileManager List</a></li>
  <li><a class="create" href="/portalFileManager/explore/{id}">Explore</a></li>
  {new_li}
</ul></div></section>
<section class="row"><div id="show-portalFileManager" class="col-12 content scaffold-show" role="main">
  <h1>Show PortalFileManager</h1>
  {flash_html}
  <ol class="property-list portalFileManager">
    <li class="fieldcontain"><span id="name-label" class="property-label">Name</span><div class="property-value" aria-labelledby="name-label">{name}</div></li>
    <li class="fieldcontain"><span id="module-label" class="property-label">Module</span><div class="property-value" aria-labelledby="module-label">{module}</div></li>
    <li class="fieldcontain"><span id="path-label" class="property-label">Path</span><div class="property-value" aria-labelledby="path-label"><a href="/portalFileManager/explore/{id}">{path}</a></div></li>
  </ol>
  {buttons}
</div></section></div></div>"##,
        name = esc(&s(&fm, "name")),
        module = esc(&module),
        path = esc(&s(&fm, "path"))
    );
    ctx.html("Show PortalFileManager", body).await
}

async fn create_or_edit(mut ctx: Ctx, id: Option<i64>) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let is_admin = ctx.is_admin();
    let dev_mods = if is_admin { vec![] } else { web::developer_list(db, ctx.user.as_ref().unwrap()).await };
    if !is_admin && dev_mods.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let is_new = id.is_none();
    let fm = match id {
        Some(fid) => match db.get("portal_file_manager", fid).await {
            Ok(Some(r)) => {
                if !is_admin && !dev_mods.contains(&s(&r, "module")) {
                    return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
                }
                r
            }
            _ => return ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        None => {
            let mut r = Row::new();
            for k in &["name", "module", "path"] {
                let v = ctx.p(k);
                if !v.is_empty() {
                    r.insert((*k).to_string(), json!(v));
                }
            }
            r
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
        let mut ms = dev_mods.clone();
        ms.sort();
        ms
    };
    let cur_mod = s(&fm, "module");
    if !cur_mod.is_empty() && !mods.contains(&cur_mod) {
        mods.push(cur_mod.clone());
    }
    if mods.is_empty() {
        mods.push("portal".into());
    }
    let mut mod_opts = String::new();
    if cur_mod.is_empty() {
        mod_opts.push_str("<option value=\"\">- choose a module -</option>");
    }
    for m in &mods {
        let sel = if *m == cur_mod { " selected=\"selected\"" } else { "" };
        mod_opts.push_str(&format!("<option value=\"{}\"{sel}>{}</option>", esc(m), esc(m)));
    }

    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let action = match id {
        Some(fid) => format!("/portalFileManager/update/{fid}"),
        None => "/portalFileManager/save".into(),
    };
    let kind = if is_new { "create" } else { "edit" };
    let heading = if is_new { "Create PortalFileManager" } else { "Edit PortalFileManager" };
    let btn = if is_new { "Create" } else { "Update" };
    let new_li = if !is_new { "<li><a class=\"create\" href=\"/portalFileManager/create\">New PortalFileManager</a></li>" } else { "" };

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#{kind}-portalFileManager" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalFileManager/index">PortalFileManager List</a></li>
  {new_li}
</ul></div></section>
<section class="row"><div id="{kind}-portalFileManager" class="col-12 content scaffold-{kind}" role="main">
  <h1>{heading}</h1>
  {flash_html}
  <form action="{action}" method="post">
    {csrf}
    <fieldset class="form">
      <div class="fieldcontain required"><label for="name">Name<span class="required-indicator">*</span></label><input type="text" name="name" id="name" value="{name}" required="required"/></div>
      <div class="fieldcontain required"><label for="module">Module<span class="required-indicator">*</span></label><select name="module" id="module" class="module-select ">{mod_opts}</select></div>
      <div class="fieldcontain required"><label for="path">Path<span class="required-indicator">*</span></label><input type="text" name="path" id="path" value="{path}" required="required"/></div>
    </fieldset>
    <fieldset class="buttons"><input class="save" type="submit" value="{btn}" /></fieldset>
  </form>
  {form_js}
</div></section></div></div>"##,
        name = esc(&s(&fm, "name")),
        path = esc(&s(&fm, "path")),
        form_js = crate::tags::form_js()
    );
    ctx.html(heading, body).await
}

async fn save(mut ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if ctx.method != "POST" {
        return ctx.redirect("/portalFileManager/create").await;
    }
    let db = &ctx.st.db.clone();
    let name = ctx.p("name").trim().to_string();
    let module = ctx.p("module").trim().to_string();
    let path = ctx.p("path").trim().to_string();
    if !is_dev_for_module(&ctx, &module).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    if name.is_empty() || module.is_empty() || path.is_empty() {
        ctx.flash("Name, module and path are required");
        return ctx.redirect("/portalFileManager/create").await;
    }
    match db.insert("portal_file_manager", &row! {"name"=>name,"module"=>module,"path"=>path}).await {
        Ok(id) => {
            ctx.flash(&format!("PortalFileManager {} created", id));
            ctx.redirect(&format!("/portalFileManager/show/{id}")).await
        }
        Err(e) => {
            ctx.flash(&format!("Could not create PortalFileManager: {}", e));
            ctx.redirect("/portalFileManager/create").await
        }
    }
}

async fn update(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if ctx.method != "POST" {
        return ctx.redirect(&format!("/portalFileManager/edit/{id}")).await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(existing)) = db.get("portal_file_manager", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let name = ctx.p("name").trim().to_string();
    let module = ctx.p("module").trim().to_string();
    let path = ctx.p("path").trim().to_string();
    if !is_dev_for_module(&ctx, &s(&existing, "module")).await || !is_dev_for_module(&ctx, &module).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    if name.is_empty() || module.is_empty() || path.is_empty() {
        ctx.flash("Name, module and path are required");
        return ctx.redirect(&format!("/portalFileManager/edit/{id}")).await;
    }
    match db.update("portal_file_manager", id, &row! {"name"=>name,"module"=>module,"path"=>path}).await {
        Ok(_) => {
            ctx.flash(&format!("PortalFileManager {} updated", id));
            ctx.redirect(&format!("/portalFileManager/show/{id}")).await
        }
        Err(e) => {
            ctx.flash(&format!("Could not update PortalFileManager: {}", e));
            ctx.redirect(&format!("/portalFileManager/edit/{id}")).await
        }
    }
}

async fn delete_def(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if ctx.method != "POST" {
        return ctx.status(StatusCode::METHOD_NOT_ALLOWED, "POST required").await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(existing)) = db.get("portal_file_manager", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    if !is_dev_for_module(&ctx, &s(&existing, "module")).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let _ = db.delete("portal_file_manager", id).await;
    ctx.flash(&format!("PortalFileManager {} deleted", id));
    ctx.redirect("/portalFileManager/index").await
}

// ---------------- Explorer (HTMX) ----------------

fn render_breadcrumbs(id: i64, fm_name: &str, rel: &str) -> String {
    let mut h = format!(
        "Path : <span style=\"cursor:pointer;text-decoration:underline;\" hx-get=\"/portalFileManager/explorepage/{id}\" hx-target=\"#explorepage\">{}</span>",
        esc(fm_name)
    );
    let mut acc: Vec<&str> = vec![];
    for part in rel.split('/').filter(|x| !x.is_empty()) {
        acc.push(part);
        let p = format!("/{}", acc.join("/"));
        h.push_str(&format!(
            " / <span style=\"cursor:pointer;text-decoration:underline;\" hx-get=\"/portalFileManager/explorepage/{id}?fname={}\" hx-target=\"#explorepage\">{}</span>",
            enc(&p),
            esc(part)
        ));
    }
    h
}

async fn render_explore_inner(fm: &Row, id: i64, rel: &str, sort_order: &str) -> String {
    let root = s(fm, "path");
    let Some(dir) = safe_resolve(&root, rel) else {
        return "<div class=\"message\">Invalid path</div>".into();
    };
    let mut entries: Vec<(bool, String, String)> = vec![];
    if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(ent)) = rd.next_entry().await {
            let name = ent.file_name().to_string_lossy().to_string();
            let md = ent.metadata().await.ok();
            let is_file = md.as_ref().map(|m| m.is_file()).unwrap_or(false);
            let mtime = md.as_ref().map(fmt_mtime).unwrap_or_default();
            entries.push((is_file, name, mtime));
        }
    }
    if sort_order == "desc" {
        entries.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.to_lowercase().cmp(&a.1.to_lowercase())));
    } else {
        entries.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.to_lowercase().cmp(&b.1.to_lowercase())));
    }

    let next_ord = if sort_order == "desc" { "asc" } else { "desc" };
    let fname_q = if rel.is_empty() { "/".to_string() } else { format!("/{}", rel) };
    let breadcrumbs = render_breadcrumbs(id, &s(fm, "name"), rel);

    let mut rows_html = String::new();
    for (idx, (is_file, name, mtime)) in entries.iter().enumerate() {
        let num = idx + 1;
        let row_cls = if idx % 2 == 0 { "even" } else { "odd" };
        let child_fname = join_rel(rel, name);
        let child_enc = enc(&child_fname);
        let td_open = if *is_file {
            format!(
                "<td id=\"td_{num}\" style=\"cursor:pointer;\" data-url=\"/portalFileManager/download/{id}?fname={child_enc}\" onclick=\"filedownload(this)\">"
            )
        } else {
            format!(
                "<td id=\"td_{num}\" style=\"cursor:pointer;\" hx-target=\"#explorepage\" hx-get=\"/portalFileManager/explorepage/{id}?fname={child_enc}\">"
            )
        };
        let icon = if *is_file { "bi-file-richtext" } else { "bi-folder" };
        rows_html.push_str(&format!(
            "<tr class=\"{row_cls}\">\
             <td>{num}</td>\
             {td_open}<span class=\"bi {icon}\"></span>&nbsp; {}</td>\
             <td>{}</td>\
             <td>\
               <button hx-target=\"#td_{num}\" hx-swap=\"outerHTML\" hx-get=\"/portalFileManager/rename/{id}?fname={child_enc}\" class=\"btn btn-secondary\">Rename</button> \
               <button hx-target=\"#explorepage\" hx-get=\"/portalFileManager/deletefm/{id}?fname={child_enc}\" class=\"btn btn-danger\" hx-confirm=\"Confirm delete this file?\">Delete</button>\
             </td>\
             </tr>",
            esc(name),
            esc(mtime)
        ));
    }

    format!(
        "<input type=\"hidden\" id=\"fmCurrentFname\" value=\"{fname_attr}\"/>\
         {breadcrumbs}\
         <div id=\"filedownload\"></div>\
         <table id=\"filetable\">\
           <thead><tr>\
             <th>#</th>\
             <th class=\"sortable\"><a href=\"#\" hx-target=\"#explorepage\" hx-get=\"/portalFileManager/explorepage/{id}?fname={fname_enc}&amp;order={next_ord}\">File Name</a></th>\
             <th>Last Update</th>\
             <th>Action</th>\
           </tr></thead>\
           <tbody>{rows_html}</tbody>\
         </table>",
        fname_attr = esc(&fname_q),
        fname_enc = enc(&fname_q)
    )
}

async fn explore(ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(fm)) = db.get("portal_file_manager", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "File manager not found").await;
    };
    if !can_access_fm(&ctx, &fm).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let raw_fname = { let f = ctx.p("fname"); if f.is_empty() { ctx.p("path") } else { f } };
    let Some(rel) = norm_rel(&raw_fname) else {
        return ctx.status(StatusCode::BAD_REQUEST, "Invalid path").await;
    };
    if let Some(p) = safe_resolve(&s(&fm, "path"), &rel) {
        if p.is_file() {
            return ctx.redirect(&format!("/portalFileManager/download/{id}?fname={}", enc(&format!("/{}", rel)))).await;
        }
    }
    let fname_param = if rel.is_empty() { "/".to_string() } else { format!("/{}", rel) };
    let inner = render_explore_inner(&fm, id, &rel, &ctx.p("order")).await;
    let can_create = is_dev_for_any(&ctx).await;
    let new_li = if can_create { "<li><a class=\"create\" href=\"/portalFileManager/create\">New PortalFileManager</a></li>" } else { "" };
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let fname_enc = enc(&fname_param);

    let body = format!(
        r##"<style>
#uploadform {{
    width: 100%;
    height: 50px;
    border: 1px dashed green;
    display: flex;
    align-items: center;
    justify-content: center;
    color: #555;
    cursor: pointer;
    margin: 8px 0;
}}
</style>
<div id="content" role="main"><div class="container">
<section class="row"><a href="#show-portalFileManager" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalFileManager/index">PortalFileManager List</a></li>
  {new_li}
</ul></div></section>
<section class="row"><div id="show-portalFileManager" class="col-12 content scaffold-show" role="main">
  <h1>Explore {fm_name}</h1>
  {flash_html}

  <button type="button" class="btn btn-primary" data-bs-toggle="modal" data-bs-target="#createfolder">
    Create Folder
  </button>

  <label for="unzip">Unzip zip files:</label>
  <input type="checkbox" name="unzip" id="unzip"/>
  &nbsp;
  <label for="createfilelink">Create FileLink:</label>
  <input type="checkbox" name="createfilelink" id="createfilelink"/>

  <div class="modal fade" id="createfolder" tabindex="-1" aria-labelledby="createfolderLabel" aria-hidden="true">
    <div class="modal-dialog modal-dialog-centered">
      <div class="modal-content">
        <form onsubmit="return false;">
          <div class="modal-header">
            <h1 class="modal-title fs-5" id="createfolderLabel">Create Folder</h1>
            <button type="button" class="btn-close" data-bs-dismiss="modal" aria-label="Close"></button>
          </div>
          <div class="modal-body">
            <label for="foldername">New Folder Name:</label>
            <input type="text" name="foldername" id="foldername"/>
            <input type="hidden" name="mdfname" id="mdfname" value="{fname_attr}"/>
          </div>
          <div class="modal-footer">
            <button type="button" class="btn btn-secondary" data-bs-dismiss="modal">Close</button>
            <button type="button" class="btn btn-primary" data-bs-dismiss="modal" hx-target="#explorepage" hx-include="#foldername,#mdfname" hx-post="/portalFileManager/createfolder/{id}">Create Folder</button>
          </div>
        </form>
      </div>
    </div>
  </div>

  <input type="file" id="fmFilePicker" multiple style="display:none"/>
  <div id="uploadform" title="Drop files here or click to upload">Drop files here or click to upload</div>
  <div id="explorepage">{inner}</div>
</div></section></div></div>
<script>
function filedownload(target) {{
    var link = document.createElement("a");
    link.setAttribute('download', '');
    link.href = $(target).data("url");
    document.body.appendChild(link);
    link.click();
    link.remove();
}}

window.curpath = "/portalFileManager/upload/{id}?fname={fname_enc}";
window.fname = "{fname_js}";

function uploadFmFiles(fileList) {{
    if (!fileList || !fileList.length) return;
    var formData = new FormData();
    for (var i = 0; i < fileList.length; i++) {{
        formData.append("file_" + i, fileList[i]);
    }}
    if ($('#unzip').prop('checked')) {{
        formData.append("unzip", "1");
    }}
    if ($('#createfilelink').prop('checked')) {{
        formData.append("createfilelink", "1");
    }}
    var xhr = new XMLHttpRequest();
    xhr.open('POST', window.curpath, true);
    $('#uploadform').css('border-color', 'orange').text('Uploading...');
    xhr.onload = function() {{
        $('#uploadform').css('border-color', 'green').text('Drop files here or click to upload');
        var parser = new DOMParser();
        var responseDoc = parser.parseFromString(xhr.responseText, 'text/html');
        var flashMsg = responseDoc.querySelector('.message');
        var msgDiv = document.getElementById('upload-message');
        if (!msgDiv) {{
            msgDiv = document.createElement('div');
            msgDiv.id = 'upload-message';
            document.getElementById('uploadform').after(msgDiv);
        }}
        var msgText = flashMsg ? flashMsg.textContent.toLowerCase() : '';
        var isError = msgText.indexOf('fail') !== -1 || msgText.indexOf('cannot') !== -1 || msgText.indexOf('error') !== -1;
        if (flashMsg && isError) {{
            msgDiv.className = 'message errors';
            msgDiv.textContent = flashMsg.textContent;
            msgDiv.style.display = 'block';
            $('#uploadform').css('border-color', 'red');
        }} else if (xhr.status >= 400) {{
            msgDiv.className = 'message errors';
            msgDiv.textContent = 'Upload failed: ' + (xhr.responseText || xhr.statusText);
            msgDiv.style.display = 'block';
            $('#uploadform').css('border-color', 'red');
        }} else {{
            msgDiv.style.display = 'none';
        }}
        var refreshUrl = window.curpath.replace("/upload/", "/explorepage/");
        htmx.ajax('GET', refreshUrl, {{target: '#explorepage', swap: 'innerHTML'}});
    }};
    xhr.onerror = function() {{
        $('#uploadform').css('border-color', 'red').text('Drop files here or click to upload');
    }};
    xhr.send(formData);
}}

var dropZone = document.getElementById("uploadform");
dropZone.addEventListener("dragover", function(event) {{
    event.preventDefault();
    event.dataTransfer.dropEffect = "copy";
    dropZone.style.borderColor = "#0d6efd";
}});
dropZone.addEventListener("dragleave", function() {{
    dropZone.style.borderColor = "green";
}});
dropZone.addEventListener("drop", function(event) {{
    event.preventDefault();
    uploadFmFiles(event.dataTransfer.files);
}});
dropZone.addEventListener("click", function() {{
    document.getElementById("fmFilePicker").click();
}});
document.getElementById("fmFilePicker").addEventListener("change", function(event) {{
    uploadFmFiles(event.target.files);
    event.target.value = "";
}});

htmx.on("htmx:afterSettle", function(event) {{
    var curEl = document.getElementById("fmCurrentFname");
    if (curEl) {{
        var cur = curEl.value || "/";
        window.fname = cur;
        window.curpath = "/portalFileManager/upload/{id}?fname=" + encodeURIComponent(cur);
        $('#mdfname').val(cur);
        $('#foldername').val('');
    }}
}});
</script>"##,
        fm_name = esc(&s(&fm, "name")),
        fname_attr = esc(&fname_param),
        fname_js = fname_param.replace('\\', "\\\\").replace('"', "\\\"")
    );
    ctx.html(&format!("Explore {}", s(&fm, "name")), body).await
}

async fn explorepage(ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.text_status(StatusCode::UNAUTHORIZED, "Login required").await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(fm)) = db.get("portal_file_manager", id).await else {
        return ctx.text_status(StatusCode::NOT_FOUND, "Not found").await;
    };
    if !can_access_fm(&ctx, &fm).await {
        return ctx.text_status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let Some(rel) = norm_rel(&ctx.p("fname")) else {
        return ctx.text_status(StatusCode::BAD_REQUEST, "Invalid path").await;
    };
    if !ctx.is_htmx() {
        let dest = if rel.is_empty() {
            format!("/portalFileManager/explore/{id}")
        } else {
            format!("/portalFileManager/explore/{id}?fname={}", enc(&format!("/{}", rel)))
        };
        return ctx.redirect(&dest).await;
    }
    if let Some(p) = safe_resolve(&s(&fm, "path"), &rel) {
        if p.is_file() {
            return ctx.redirect(&format!("/portalFileManager/download/{id}?fname={}", enc(&format!("/{}", rel)))).await;
        }
    }
    let inner = render_explore_inner(&fm, id, &rel, &ctx.p("order")).await;
    ctx.raw_html(inner).await
}

async fn download_file(ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(fm)) = db.get("portal_file_manager", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    if !can_access_fm(&ctx, &fm).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let Some(target) = safe_resolve(&s(&fm, "path"), &ctx.p("fname")) else {
        return ctx.status(StatusCode::BAD_REQUEST, "Invalid path").await;
    };
    if !target.is_file() {
        return ctx.status(StatusCode::NOT_FOUND, "Sorry but the file no longer exists on the server").await;
    }
    let name = target.file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_else(|| "download".into());
    ctx.file_stream("application/octet-stream", Some(&name), &target.to_string_lossy(), false).await
}

async fn createfolder(ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.text_status(StatusCode::UNAUTHORIZED, "Login required").await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(fm)) = db.get("portal_file_manager", id).await else {
        return ctx.text_status(StatusCode::NOT_FOUND, "Not found").await;
    };
    if !can_access_fm(&ctx, &fm).await {
        return ctx.text_status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let base_raw = { let m = ctx.p("mdfname"); if m.is_empty() { ctx.p("fname") } else { m } };
    let Some(rel) = norm_rel(&base_raw) else {
        return ctx.text_status(StatusCode::BAD_REQUEST, "Invalid path").await;
    };
    let folder = sanitize_filename(ctx.p("foldername").trim());
    if !folder.is_empty() && folder != "file" {
        if let Some(dir) = safe_resolve(&s(&fm, "path"), &rel) {
            let _ = tokio::fs::create_dir_all(dir.join(&folder)).await;
        }
    } else if !ctx.p("foldername").trim().is_empty() {
        if let Some(dir) = safe_resolve(&s(&fm, "path"), &rel) {
            let _ = tokio::fs::create_dir_all(dir.join(&folder)).await;
        }
    }
    let inner = render_explore_inner(&fm, id, &rel, &ctx.p("order")).await;
    ctx.raw_html(inner).await
}

async fn rename(ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.text_status(StatusCode::UNAUTHORIZED, "Login required").await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(fm)) = db.get("portal_file_manager", id).await else {
        return ctx.text_status(StatusCode::NOT_FOUND, "Not found").await;
    };
    if !can_access_fm(&ctx, &fm).await {
        return ctx.text_status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let raw_fname = ctx.p("fname");
    let Some(rel) = norm_rel(&raw_fname) else {
        return ctx.text_status(StatusCode::BAD_REQUEST, "Invalid path").await;
    };
    let newname = ctx.p("newname").trim().to_string();
    if ctx.method == "POST" && !newname.is_empty() {
        let clean_new = sanitize_filename(&newname);
        if let Some(old_path) = safe_resolve(&s(&fm, "path"), &rel) {
            if old_path.exists() {
                if let Some(parent) = old_path.parent() {
                    let target = parent.join(&clean_new);
                    let _ = tokio::fs::rename(&old_path, &target).await;
                }
            }
        }
        let parent_n = norm_rel(&parent_rel(&rel)).unwrap_or_default();
        let inner = render_explore_inner(&fm, id, &parent_n, &ctx.p("order")).await;
        return ctx.raw_html(inner).await;
    }
    let current_name = rel.rsplit('/').next().unwrap_or("");
    let html = format!(
        "<td id=\"renametd\"><form hx-target=\"#explorepage\" hx-post=\"/portalFileManager/rename/{id}?fname={}\">\
         <input type=\"text\" name=\"newname\" value=\"{}\"/> <button class=\"btn btn-sm btn-primary\">Rename</button>\
         </form></td>",
        enc(&format!("/{}", rel)),
        esc(current_name)
    );
    ctx.raw_html(html).await
}

async fn deletefm(ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.text_status(StatusCode::UNAUTHORIZED, "Login required").await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(fm)) = db.get("portal_file_manager", id).await else {
        return ctx.text_status(StatusCode::NOT_FOUND, "Not found").await;
    };
    if !can_access_fm(&ctx, &fm).await {
        return ctx.text_status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let Some(rel) = norm_rel(&ctx.p("fname")) else {
        return ctx.text_status(StatusCode::BAD_REQUEST, "Invalid path").await;
    };
    if !rel.is_empty() {
        if let Some(target) = safe_resolve(&s(&fm, "path"), &rel) {
            if target.exists() {
                let raw_s = target.to_string_lossy().to_string();
                let canon = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
                let canon_s = canon.to_string_lossy().to_string();
                if target.is_file() {
                    let _ = tokio::fs::remove_file(&target).await;
                    let _ = db.exec("delete from file_link where path=:p or path=:r", &row! {"p"=>canon_s,"r"=>raw_s}).await;
                } else if target.is_dir() {
                    let _ = tokio::fs::remove_dir_all(&target).await;
                    let prefix_c = format!("{}/%", canon_s.trim_end_matches('/'));
                    let prefix_r = format!("{}/%", raw_s.trim_end_matches('/'));
                    let _ = db.exec("delete from file_link where path like :p or path like :r", &row! {"p"=>prefix_c,"r"=>prefix_r}).await;
                }
            }
        }
    }
    let parent_n = norm_rel(&parent_rel(&rel)).unwrap_or_default();
    let inner = render_explore_inner(&fm, id, &parent_n, &ctx.p("order")).await;
    ctx.raw_html(inner).await
}

async fn create_file_link_for_file(db: &Db, file_path: &Path, module: &str) {
    let fname = match file_path.file_name().map(|x| x.to_string_lossy().to_string()) {
        Some(n) if !n.is_empty() => n,
        _ => return,
    };
    let slug = filename_to_slug(&fname);
    if db.count("select count(*) from file_link where slug=:s", &row! {"s"=>slug.clone()}).await.unwrap_or(1) > 0 {
        return;
    }
    let canon = std::fs::canonicalize(file_path).unwrap_or_else(|_| file_path.to_path_buf());
    let md = tokio::fs::metadata(file_path).await.ok();
    let sz = md.as_ref().map(|m| m.len() as i64);
    let last_upd = md
        .as_ref()
        .and_then(|m| m.modified().ok())
        .map(|st| {
            let dt: chrono::DateTime<chrono::Local> = st.into();
            dt.format("%Y-%m-%d %H:%M:%S").to_string()
        })
        .unwrap_or_else(now_str);
    let _ = db
        .insert(
            "file_link",
            &row! {
                "name"=>fname,
                "module"=>module,
                "slug"=>slug,
                "path"=>canon.to_string_lossy().to_string(),
                "size"=>sz,
                "last_updated"=>last_upd
            },
        )
        .await;
}

fn extract_zip_safe(zip_path: &Path, dest_dir: &Path) -> Result<Vec<PathBuf>> {
    let f = std::fs::File::open(zip_path)?;
    let mut arc = zip::ZipArchive::new(f)?;
    let mut created = vec![];
    for idx in 0..arc.len() {
        let mut entry = arc.by_index(idx)?;
        let Some(rel) = norm_rel(entry.name()) else { continue };
        if rel.is_empty() {
            continue;
        }
        let out_path = dest_dir.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
        } else {
            if let Some(p) = out_path.parent() {
                std::fs::create_dir_all(p)?;
            }
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            std::fs::write(&out_path, &buf)?;
            created.push(out_path);
        }
    }
    Ok(created)
}

async fn upload(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.text_status(StatusCode::UNAUTHORIZED, "Login required").await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(fm)) = db.get("portal_file_manager", id).await else {
        return ctx.text_status(StatusCode::NOT_FOUND, "Not found").await;
    };
    if !can_access_fm(&ctx, &fm).await {
        return ctx.text_status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let Some(rel) = norm_rel(&ctx.p("fname")) else {
        return ctx.text_status(StatusCode::BAD_REQUEST, "Invalid path").await;
    };
    let Some(dest_dir) = safe_resolve(&s(&fm, "path"), &rel) else {
        return ctx.text_status(StatusCode::BAD_REQUEST, "Invalid path").await;
    };
    let do_unzip = !ctx.p("unzip").is_empty() && ctx.p("unzip") != "0" && ctx.p("unzip") != "false";
    let do_filelink = !ctx.p("createfilelink").is_empty() && ctx.p("createfilelink") != "0" && ctx.p("createfilelink") != "false";
    let max_size: u64 = if let Some(u) = &ctx.user {
        web::setting_str(db, &format!("filemanager_max_{}", s(u, "userid")))
            .await
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(5_242_880)
    } else {
        5_242_880
    };
    let module = s(&fm, "module");
    let mut keys: Vec<String> = ctx.files.keys().filter(|k| k.starts_with("file_") || k.as_str() == "file").cloned().collect();
    keys.sort();

    let mut err_msg: Option<String> = None;
    for k in keys {
        let Some(up) = ctx.files.remove(&k) else { continue };
        if up.size == 0 && up.filename.is_empty() {
            err_msg = Some("file cannot be empty".into());
            continue;
        }
        if max_size > 0 && up.size > max_size {
            err_msg = Some(format!("File upload failed: {} exceeds maximum size ({} bytes)", up.filename, max_size));
            continue;
        }
        let clean = match validate_fm_upload(&up) {
            Ok(n) => n,
            Err(e) => {
                err_msg = Some(format!("File upload failed: {}", e));
                continue;
            }
        };
        if let Err(e) = tokio::fs::create_dir_all(&dest_dir).await {
            err_msg = Some(format!("File upload failed: {}", e));
            continue;
        }
        let target = dest_dir.join(&clean);
        if let Err(e) = up.save_to(&target).await {
            err_msg = Some(format!("File upload failed: {}", e));
            continue;
        }
        let is_zip = clean.to_lowercase().ends_with(".zip");
        if is_zip && do_unzip {
            let t_clone = target.clone();
            let d_clone = dest_dir.clone();
            match tokio::task::spawn_blocking(move || extract_zip_safe(&t_clone, &d_clone)).await {
                Ok(Ok(files)) => {
                    if do_filelink {
                        for fp in files {
                            create_file_link_for_file(db, &fp, &module).await;
                        }
                    }
                }
                Ok(Err(e)) => err_msg = Some(format!("Unzip failed: {}", e)),
                Err(e) => err_msg = Some(format!("Unzip failed: {}", e)),
            }
        } else if do_filelink {
            create_file_link_for_file(db, &target, &module).await;
        }
    }

    if let Some(msg) = err_msg {
        return ctx.raw_html(format!("<div class=\"message\" role=\"status\">{}</div>", esc(&msg))).await;
    }
    let inner = render_explore_inner(&fm, id, &rel, &ctx.p("order")).await;
    ctx.raw_html(inner).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_rel_rejects_traversal_and_cleans_slashes() {
        assert_eq!(norm_rel(""), Some("".into()));
        assert_eq!(norm_rel("/"), Some("".into()));
        assert_eq!(norm_rel("/a//b\\c/"), Some("a/b/c".into()));
        assert_eq!(norm_rel("../secret"), None);
        assert_eq!(norm_rel("a/../../b"), None);
    }

    #[test]
    fn parent_and_slug_helpers() {
        assert_eq!(parent_rel("a/b/c.txt"), "/a/b");
        assert_eq!(parent_rel("top.txt"), "/");
        assert_eq!(filename_to_slug("style.min.css"), "style_min_css");
    }
}
