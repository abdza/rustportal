//! PortalTrackerData controller and views matching g6ikram's
//! `PortalTrackerDataController` + `views/portalTrackerData/*.gsp`.

use crate::db::{self, b, i, lit, s, Db, Row};
use crate::row;
use crate::tracker::Tracker;
use crate::upload;
use crate::web::{self, enc, esc, now_str, Ctx};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;

/// Main dispatcher for `/portalTrackerData/*`.
pub async fn controller(ctx: Ctx, act: &str, id: Option<i64>) -> Response {
    match act {
        "upload" => upload::upload(ctx).await,
        "moduledump" => upload::moduledump(ctx, id.unwrap_or(0)).await,
        "datadump" => upload::datadump(ctx, id.unwrap_or(0)).await,
        "rollback" => upload::rollback(ctx, id.unwrap_or(0)).await,
        "process" => process_action(ctx, id.unwrap_or(0)).await,
        "show" => match id {
            Some(did) => show(ctx, did).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "create" => create(ctx).await,
        "save" => save(ctx).await,
        "customfield" => customfield(ctx, id.unwrap_or(0)).await,
        "doupload" => doupload(ctx).await,
        "uploadsummary" => match id {
            Some(did) => uploadsummary(ctx, did).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "stuckuploads" => stuckuploads(ctx).await,
        "requeueupload" => match id {
            Some(did) => requeueupload(ctx, did).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "requeueall" => requeueall(ctx).await,
        "edit" => match id {
            Some(did) => edit(ctx, did).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "update" => match id {
            Some(did) => update(ctx, did).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "delete" => match id {
            Some(did) => delete_action(ctx, did).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "cleardb" => cleardb(ctx).await,
        "cleandb" => cleandb(ctx).await,
        "resetdb" => resetdb(ctx).await,
        "syncupload" => syncupload(ctx, id.unwrap_or(0)).await,
        _ => index(ctx).await,
    }
}

fn fmt_dt(raw: &str) -> String {
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

fn opt_int(v: String) -> Value {
    v.trim().parse::<i64>().map(|n| json!(n)).unwrap_or(Value::Null)
}

async fn process_action(mut ctx: Ctx, id: i64) -> Response {
    if !ctx.is_admin() || ctx.method != "POST" {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let st = ctx.st.clone();
    match upload::process(&st, id).await {
        Ok(m) => ctx.flash(&m),
        Err(e) => ctx.flash(&format!("Failed: {}", e)),
    }
    ctx.redirect(&format!("/portalTrackerData/show/{id}")).await
}

// ---------------- List (index.gsp) ----------------

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
        cond.push(format!("(d.\"module\"={m} or t.\"module\"={m})", m = lit(&json!(m))));
    } else if !is_admin {
        let ml = lit(&json!(my_mods));
        cond.push(format!("(d.\"module\" in {ml} or t.\"module\" in {ml})"));
    }
    if !q.trim().is_empty() {
        let like = lit(&json!(format!("%{}%", q.trim())));
        cond.push(format!(
            "(t.name ilike {l} or t.slug ilike {l} or d.\"module\" ilike {l} or d.path ilike {l})",
            l = like
        ));
    }
    let wh = if cond.is_empty() { String::new() } else { format!(" where {}", cond.join(" and ")) };
    let max = ctx.pi("max").unwrap_or(10).clamp(1, 500);
    let offset = ctx.pi("offset").unwrap_or(0).max(0);
    let sort_p = ctx.p("sort");
    let order_dir = if ctx.p("order") == "desc" { "desc" } else { "asc" };
    let order_sql = match sort_p.as_str() {
        "tracker" | "tracker_id" => format!("t.name {}, d.id desc", order_dir),
        "module" => format!("d.\"module\" {}, d.id desc", order_dir),
        "uploadStatus" | "upload_status" => format!("d.upload_status {}, d.id desc", order_dir),
        "path" => format!("d.path {}, d.id desc", order_dir),
        "header_start" => format!("d.header_start {}, d.id desc", order_dir),
        "header_end" => format!("d.header_end {}, d.id desc", order_dir),
        "data_row" => format!("d.data_row {}, d.id desc", order_dir),
        _ => "d.id desc".to_string(),
    };

    let from_sql = "from portal_tracker_data d left join portal_tracker t on t.id=d.tracker_id";
    let total = db.count(&format!("select count(*) {from_sql}{wh}"), &db::no_params()).await.unwrap_or(0);
    let rows = db
        .rows(
            &format!(
                "select d.*, t.name as tracker_name, t.slug as tracker_slug {from_sql}{wh} order by {order_sql} limit {max} offset {offset}"
            ),
            &db::no_params(),
        )
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

    let mut mod_opts = String::from("<option value=\"All\">All</option>");
    for x in &mods {
        let sel = if *x == m { " selected=\"selected\"" } else { "" };
        mod_opts.push_str(&format!("<option value=\"{}\"{sel}>{}</option>", esc(x), esc(x)));
    }

    const COLS: &[(&str, &str)] = &[
        ("tracker", "Tracker"),
        ("module", "Module"),
        ("uploadStatus", "Upload Status"),
        ("path", "Path"),
        ("header_start", "Header Start"),
        ("header_end", "Header End"),
        ("data_row", "Data Row"),
    ];
    let qs_keep = web::qs(&ctx.params, &["sort", "order", "offset", "max"]);
    let mut thead = String::new();
    for (col, lbl) in COLS {
        let sorted = sort_p == *col;
        let next_ord = if sorted && order_dir == "asc" { "desc" } else { "asc" };
        let cls = if sorted { format!("sortable sorted {}", order_dir) } else { "sortable".into() };
        let extra_q = if qs_keep.is_empty() { String::new() } else { format!("&amp;{}", esc(&qs_keep)) };
        thead.push_str(&format!(
            "<th class=\"{cls}\"><a href=\"/portalTrackerData/index?sort={col}&amp;max={max}&amp;order={next_ord}{extra_q}\">{}</a></th>",
            esc(lbl)
        ));
    }

    let mut tbody = String::new();
    for (rn, r) in rows.iter().enumerate() {
        let did = i(r, "id").unwrap_or(0);
        let row_cls = if rn % 2 == 0 { "even" } else { "odd" };
        let tname = {
            let tn = s(r, "tracker_name");
            if tn.is_empty() {
                i(r, "tracker_id").map(|x| format!("#{x}")).unwrap_or_else(|| format!("#{did}"))
            } else {
                tn
            }
        };
        let status_s = i(r, "upload_status").map(|v| v.to_string()).unwrap_or_default();
        let hs = i(r, "header_start").map(|v| v.to_string()).unwrap_or_default();
        let he = i(r, "header_end").map(|v| v.to_string()).unwrap_or_default();
        let dr = i(r, "data_row").map(|v| v.to_string()).unwrap_or_default();
        tbody.push_str(&format!(
            "<tr class=\"{row_cls}\">\
             <td><a href=\"/portalTrackerData/show/{did}\">{}</a>&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{status_s}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{hs}&nbsp;</td>\
             <td>{he}&nbsp;</td>\
             <td>{dr}&nbsp;</td>\
             </tr>",
            esc(&tname),
            esc(&s(r, "module")),
            esc(&s(r, "path"))
        ));
    }

    let pag = if total > max {
        format!("<div class=\"pagination\">{}</div>", crate::tags::paginate("/portalTrackerData/index", &ctx.params, total, max, offset))
    } else {
        String::new()
    };
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#list-portalTrackerData" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="create" href="/portalTrackerData/create">New Data Update</a></li>
  <li><a class="list" href="/portalTrackerData/stuckuploads">Stuck Uploads</a></li>
</ul></div></section>
<section class="row"><div id="list-portalTrackerData" class="col-12 content scaffold-list" role="main">
  <h1>Data Update</h1>
  {flash_html}
  <form action="/portalTrackerData/index" method="get" name="searchform" id="searchform">
    <fieldset class="form">
      <div class="fieldcontain" id="listsearch">
        <label>Module:</label>
        <select name="module" id="module" class="module-select " onchange="this.form.submit()">{mod_opts}</select>
      </div>
      <div class="fieldcontain" id="listsearch">
        <label>Search:</label><input type="text" name="q" id="q" value="{q_val}"/><input id="dosearch" name="dosearch" type="submit" value="Search"/>
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
    ctx.html("PortalTrackerData List", body).await
}

// ---------------- Show (show.gsp) ----------------

async fn show(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(d)) = db.get("portal_tracker_data", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let tracker_row = match i(&d, "tracker_id") {
        Some(tid) => db.get("portal_tracker", tid).await.ok().flatten(),
        None => None,
    };
    let module = {
        let m = s(&d, "module");
        if m.is_empty() { tracker_row.as_ref().map(|t| s(t, "module")).unwrap_or_default() } else { m }
    };
    if !ctx.can_admin(&module).await {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this").await;
    }

    let is_dev = ctx.is_admin() || web::developer_list(db, ctx.user.as_ref().unwrap()).await.contains(&module);
    let tracker_html = match (&tracker_row, i(&d, "tracker_id")) {
        (Some(tr), Some(tid)) => {
            let tn = s(tr, "name");
            if is_dev {
                format!("<a href=\"/portalTracker/show/{tid}\">{}</a>", esc(&tn))
            } else {
                esc(&tn)
            }
        }
        (None, Some(tid)) => tid.to_string(),
        _ => String::new(),
    };
    let file_link_html = match i(&d, "file_link_id") {
        Some(fid) => {
            let name = db.get("file_link", fid).await.ok().flatten().map(|f| s(&f, "name")).filter(|n| !n.is_empty()).unwrap_or_else(|| fid.to_string());
            format!("<a href=\"/fileLink/show/{fid}\">{}</a>", esc(&name))
        }
        None => String::new(),
    };
    let uploader_html = match i(&d, "uploader_id") {
        Some(uid) => match db.get("portal_user", uid).await.ok().flatten() {
            Some(u) => format!("<a href=\"/user/show/{uid}\">{} ({})</a>", esc(&s(&u, "name")), esc(&s(&u, "userid"))),
            None => uid.to_string(),
        },
        None => String::new(),
    };

    let bool_txt = |col: &str| -> String {
        match d.get(col) {
            Some(Value::Bool(true)) => "True".into(),
            Some(Value::Bool(false)) => "False".into(),
            _ => String::new(),
        }
    };
    let int_txt = |col: &str| -> String { i(&d, col).map(|v| v.to_string()).unwrap_or_default() };
    let msg_html = esc(&s(&d, "messages")).replace('\n', "<br/>");

    let mut items = String::new();
    let mut li = |col: &str, label: &str, val: String| {
        items.push_str(&format!(
            "<li class=\"fieldcontain\"><span id=\"{col}-label\" class=\"property-label\">{label}</span>\
             <div class=\"property-value\" aria-labelledby=\"{col}-label\">{val}</div></li>"
        ));
    };
    li("tracker", "Tracker", tracker_html);
    li("module", "Module", esc(&module));
    li("uploadStatus", "Upload Status", int_txt("upload_status"));
    li("path", "Path", esc(&s(&d, "path")));
    li("header_start", "Header Start", int_txt("header_start"));
    li("header_end", "Header End", int_txt("header_end"));
    li("data_row", "Data Row", int_txt("data_row"));
    li("data_end", "Data End", int_txt("data_end"));
    li("uploaded", "Uploaded", bool_txt("uploaded"));
    li("send_email", "Send Email", bool_txt("send_email"));
    li("sent_email_date", "Sent Email Date", esc(&fmt_dt(&s(&d, "sent_email_date"))));
    li("messages", "Messages", msg_html);
    li("savedparams", "Savedparams", esc(&s(&d, "savedparams")));
    li("file_link", "File Link", file_link_html);
    li("date_created", "Date Created", esc(&fmt_dt(&s(&d, "date_created"))));
    li("isTrackerDeleting", "Is Tracker Deleting", bool_txt("is_tracker_deleting"));
    li("uploader", "Uploader", uploader_html);
    li("excel_password", "Excel Password", esc(&s(&d, "excel_password")));

    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#show-portalTrackerData" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTrackerData/index">PortalTrackerData List</a></li>
  <li><a class="create" href="/portalTrackerData/create">New PortalTrackerData</a></li>
  <li><a class="list" href="/portalTrackerData/uploadsummary/{id}">Upload Summary</a></li>
</ul></div></section>
<section class="row"><div id="show-portalTrackerData" class="col-12 content scaffold-show" role="main">
  <h1>Data Update</h1>
  {flash_html}
  <ol class="property-list portalTrackerData">{items}</ol>
  <form action="/portalTrackerData/delete/{id}" method="post">
    {csrf}
    <fieldset class="buttons">
      <a class="edit" href="/portalTrackerData/edit/{id}">Edit</a>
      <input class="delete" type="submit" value="Delete" onclick="return confirm('Are you sure?');" />
    </fieldset>
  </form>
</div></section></div></div>"##
    );
    ctx.html("Show PortalTrackerData", body).await
}

// ---------------- Create / Save / Customfield / Doupload / UploadSummary ----------------

async fn create(mut ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let is_admin = ctx.is_admin();
    let u = ctx.user.as_ref().unwrap();
    let my_mods = if is_admin { vec![] } else { web::admin_list(db, u).await };
    if !is_admin && my_mods.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }

    let tracker_id = ctx.pi("tracker_id");
    let mut tracker_section = String::new();
    let mut hidden_custom = String::new();

    if let Some(tid) = tracker_id {
        if let Some(t) = Tracker::by_id(db, tid).await {
            if !ctx.can_admin(&t.module).await {
                return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
            }
            hidden_custom.push_str(&format!("<input type=\"hidden\" name=\"tracker\" id=\"tracker\" value=\"{tid}\"/>"));
            let key_list: Vec<String> = db::csv(&ctx.p("customkeyfields"));
            let internal = ["tracker_id", "module", "slug", "action", "controller", "format", "customkeyfields"];
            let mut field_names = vec![];
            for f in &t.fields {
                let fnm = s(f, "name");
                field_names.push(fnm.clone());
                let pv = ctx.p(&fnm);
                if !pv.is_empty() {
                    hidden_custom.push_str(&format!("<input type=\"hidden\" name=\"{}\" id=\"{}\" value=\"{}\"/>", esc(&fnm), esc(&fnm), esc(&pv)));
                } else if fnm == "created_by" {
                    if let Some(uid) = ctx.uid() {
                        hidden_custom.push_str(&format!("<input type=\"hidden\" name=\"created_by\" id=\"created_by\" value=\"{uid}\"/>"));
                    }
                } else if fnm == "created_date" {
                    let now_m = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
                    hidden_custom.push_str(&format!("<input type=\"hidden\" name=\"created_date\" id=\"created_date\" value=\"{now_m}\"/>"));
                }
            }
            for (k, v) in &ctx.params {
                if !internal.contains(&k.as_str()) && !field_names.contains(k) {
                    let vs_val = db::vs(v);
                    if !vs_val.is_empty() {
                        hidden_custom.push_str(&format!("<input type=\"hidden\" name=\"{}\" id=\"{}\" value=\"{}\"/>", esc(k), esc(k), esc(&vs_val)));
                    }
                }
            }
            if !key_list.is_empty() {
                hidden_custom.push_str(&format!("<input type=\"hidden\" name=\"customkeyfields\" value=\"{}\"/>", esc(&key_list.join(","))));
            }
        }
    } else {
        let trackers = if is_admin {
            db.rows("select id, module, name from portal_tracker order by module, name", &db::no_params()).await.unwrap_or_default()
        } else {
            db.rows(
                &format!("select id, module, name from portal_tracker where \"module\" in {} order by module, name", lit(&json!(my_mods))),
                &db::no_params(),
            )
            .await
            .unwrap_or_default()
        };
        let mut opts = String::new();
        for tr in &trackers {
            let tid = i(tr, "id").unwrap_or(0);
            opts.push_str(&format!("<option value=\"{tid}\">{} ({})</option>", esc(&s(tr, "name")), esc(&s(tr, "module"))));
        }
        tracker_section = format!("<div class=\"fieldcontain\"><label for=\"tracker\">Tracker</label><select name=\"tracker\" id=\"tracker\">{opts}</select></div>");
    }

    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let hs = ctx.pi("header_start").unwrap_or(1);
    let he = ctx.pi("header_end").unwrap_or(1);
    let dr = ctx.pi("data_row").unwrap_or(2);
    let de = ctx.p("data_end");

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#create-portalTrackerData" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTrackerData/index">Data Update List</a></li>
</ul></div></section>
<section class="row"><div id="create-portalTrackerData" class="col-12 content scaffold-create" role="main">
  <h1>Data Update</h1>
  {flash_html}
  <form action="/portalTrackerData/save" method="post" enctype="multipart/form-data">
    {csrf}
    <fieldset class="form">
      <div class="fieldcontain">
        <label for="fileupload">File</label>
        <input type="file" name="fileupload" value="" id="fileupload" required="required" />
      </div>
    </fieldset>
    <fieldset class="form">
      {tracker_section}
      <div class="fieldcontain"><label for="header_start">Header Start</label><input type="number" name="header_start" id="header_start" value="{hs}"/></div>
      <div class="fieldcontain"><label for="header_end">Header End</label><input type="number" name="header_end" id="header_end" value="{he}"/></div>
      <div class="fieldcontain"><label for="data_row">Data Row</label><input type="number" name="data_row" id="data_row" value="{dr}"/></div>
      <div class="fieldcontain"><label for="data_end">Data End</label><input type="number" name="data_end" id="data_end" value="{de}"/></div>
      <div class="fieldcontain"><label for="excel_password">Excel Password</label><input type="text" name="excel_password" id="excel_password" value=""/></div>
      {hidden_custom}
    </fieldset>
    <fieldset class="buttons">
      <input type="submit" name="create" class="save" value="Create" />
    </fieldset>
  </form>
</div></section></div></div>"##,
        de = esc(&de)
    );
    ctx.html("Create PortalTrackerData", body).await
}

async fn save(mut ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if ctx.method != "POST" {
        return ctx.redirect("/portalTrackerData/create").await;
    }
    let db = &ctx.st.db.clone();
    let tid = ctx.pi("tracker").or_else(|| ctx.pi("tracker_id")).unwrap_or(0);
    let Some(t) = Tracker::by_id(db, tid).await else {
        ctx.flash("Please choose a tracker");
        return ctx.redirect("/portalTrackerData/create").await;
    };
    if !ctx.can_admin(&t.module).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let Some(up) = ctx.files.remove("fileupload").or_else(|| ctx.files.remove("file")).filter(|u| !u.filename.is_empty() || u.size > 0) else {
        ctx.flash("file cannot be empty");
        return ctx.redirect("/portalTrackerData/create").await;
    };
    let clean = match crate::files::validate_upload(&up) {
        Ok(n) => n,
        Err(e) => {
            ctx.flash(&format!("File upload failed: {}", e));
            return ctx.redirect("/portalTrackerData/create").await;
        }
    };
    let base = web::setting_str(db, "uploadfolder")
        .await
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| ctx.st.cfg.upload_dir.clone());
    let dir = PathBuf::from(&base).join(&t.module).join(&t.slug);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        ctx.flash(&format!("File upload failed: {}", e));
        return ctx.redirect("/portalTrackerData/create").await;
    }
    let mut target = dir.join(&clean);
    let mut n = 1;
    while target.exists() {
        let (stem, ext) = match clean.rsplit_once('.') {
            Some((a, b)) => (a.to_string(), format!(".{}", b)),
            None => (clean.clone(), String::new()),
        };
        target = dir.join(format!("{}_{}{}", stem, n, ext));
        n += 1;
    }
    if let Err(e) = up.save_to(&target).await {
        ctx.flash(&format!("File upload failed: {}", e));
        return ctx.redirect("/portalTrackerData/create").await;
    }
    let path_str = target.to_string_lossy().to_string();
    let hs = ctx.pi("header_start").unwrap_or(1).max(1);
    let he = ctx.pi("header_end").unwrap_or(hs).max(hs);
    let dr = ctx.pi("data_row").unwrap_or(he + 1).max(1);
    let de = opt_int(ctx.p("data_end"));
    let pw = ctx.p("excel_password").trim().to_string();

    let rec = row! {
        "tracker_id"=>tid,
        "module"=>t.module.clone(),
        "path"=>path_str.clone(),
        "header_start"=>hs,
        "header_end"=>he,
        "data_row"=>dr,
        "data_end"=>de,
        "excel_password"=>if pw.is_empty() { Value::Null } else { json!(pw) },
        "date_created"=>now_str(),
        "uploader_id"=>ctx.uid(),
        "uploaded"=>false,
        "upload_status"=>0i64
    };
    let update_id = match db.insert("portal_tracker_data", &rec).await {
        Ok(id) => id,
        Err(e) => {
            ctx.flash(&format!("Could not save data update: {}", e));
            return ctx.redirect("/portalTrackerData/create").await;
        }
    };

    let path_for_read = path_str.clone();
    let headers = match tokio::task::spawn_blocking(move || upload::extract_headers(&path_for_read, hs as usize, he as usize)).await {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            ctx.flash(&format!("Could not read headers from file: {}", e));
            vec![]
        }
        Err(e) => {
            ctx.flash(&format!("Could not read headers: {}", e));
            vec![]
        }
    };

    // Match headers to tracker fields
    let mut setfields: HashMap<i64, String> = HashMap::new();
    let mut customdata: HashMap<i64, String> = HashMap::new();
    let key_names = db::csv(&ctx.p("customkeyfields"));
    let mut keydata: HashMap<i64, bool> = HashMap::new();

    for (col_idx, hslug, htext) in &headers {
        let norm_h: String = hslug.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let norm_t: String = htext.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        if let Some(f) = t.fields.iter().find(|f| {
            let fnm: String = s(f, "name").to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
            let flb: String = s(f, "label").to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
            fnm == norm_h || fnm == norm_t || flb == norm_h || flb == norm_t
        }) {
            let fid = i(f, "id").unwrap_or(0);
            setfields.entry(fid).or_insert_with(|| col_idx.to_string());
        }
    }
    for f in &t.fields {
        let fid = i(f, "id").unwrap_or(0);
        let fnm = s(f, "name");
        let pv = ctx.p(&fnm);
        if !pv.is_empty() && pv != "All" {
            customdata.insert(fid, pv);
            setfields.insert(fid, "custom".into());
        }
        if key_names.contains(&fnm) {
            keydata.insert(fid, true);
        }
    }

    let mut fields_sorted = t.fields.clone();
    fields_sorted.sort_by(|a, b| s(a, "name").to_lowercase().cmp(&s(b, "name").to_lowercase()));

    let mut hidden_pre = String::new();
    let mut rows_html = String::new();
    for f in &fields_sorted {
        let ft = s(f, "field_type");
        if matches!(ft.as_str(), "FieldGroup" | "HasMany") {
            continue;
        }
        let fid = i(f, "id").unwrap_or(0);
        let fnm = s(f, "name");
        let cur_sel = setfields.get(&fid).cloned().unwrap_or_else(|| "ignore".into());
        if cur_sel == "custom" {
            if let Some(cv) = customdata.get(&fid) {
                hidden_pre.push_str(&format!(
                    "<input type=\"hidden\" name=\"datasource_{fid}\" value=\"custom\"/>\
                     <input type=\"hidden\" name=\"custom_{fid}\" value=\"{}\"/>\
                     <input type=\"hidden\" name=\"update_{fid}\" value=\"on\"/>",
                    esc(cv)
                ));
                continue;
            }
        }
        let mut opts = String::new();
        let mut add_opt = |val: &str, label: &str| {
            let sel = if cur_sel == val { " selected=\"selected\"" } else { "" };
            opts.push_str(&format!("<option value=\"{}\"{sel}>{}</option>", esc(val), esc(label)));
        };
        add_opt("ignore", "Ignore");
        add_opt("custom", "Custom");
        let mut sorted_headers = headers.clone();
        sorted_headers.sort_by(|a, b| a.1.cmp(&b.1));
        for (cidx, _, htext) in &sorted_headers {
            add_opt(&cidx.to_string(), htext);
        }
        let custom_val = customdata.get(&fid).cloned().unwrap_or_default();
        let key_cell = if keydata.get(&fid).copied().unwrap_or(false) {
            format!("<input type=\"hidden\" name=\"update_{fid}\" value=\"on\"/>Yes")
        } else {
            format!("<input type=\"checkbox\" style=\"width:auto;\" name=\"update_{fid}\" id=\"update_{fid}\"/>")
        };
        rows_html.push_str(&format!(
            "<tr>\
             <td>{}</td>\
             <td><select data-field-id=\"{fid}\" class=\"fieldpos\" name=\"datasource_{fid}\" id=\"datasource_{fid}\">{opts}</select></td>\
             <td><input type=\"text\" id=\"custom_{fid}\" name=\"custom_{fid}\" value=\"{}\"/></td>\
             <td style=\"width:20px;\">{key_cell}</td>\
             </tr>",
            esc(&fnm),
            esc(&custom_val)
        ));
    }

    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTrackerData/index">Data Update List</a></li>
</ul></div></section>
<section class="row"><div class="col-12 content" role="main">
  <h1>Data Update Fields</h1>
  {flash_html}
  <form action="/portalTrackerData/doupload" method="post">
    {csrf}
    <input type="hidden" name="update_id" value="{update_id}"/>
    {hidden_pre}
    <table>
      <tr>
        <th>Database Field</th>
        <th>Excel Position</th>
        <th>Manual Entry</th>
        <th style="width:20px;">Key Update</th>
      </tr>
      {rows_html}
    </table>
    <fieldset class="buttons"><input type="submit" class="save" name="Upload" value="Upload"/></fieldset>
  </form>
</div></section></div></div>
<script>
$('.fieldpos').on('change', function(event) {{
  var field_id = $(event.currentTarget).data('field-id');
  if ($(event.currentTarget).val() === 'custom') {{
    htmx.ajax('GET', '/portalTrackerData/customfield/' + field_id, {{target: '#custom_' + field_id, swap: 'outerHTML'}});
  }}
}});
</script>"##
    );
    ctx.html("Save Data Update", body).await
}

async fn customfield(ctx: Ctx, field_id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.text_status(StatusCode::UNAUTHORIZED, "Login required").await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(f)) = db.get("portal_tracker_field", field_id).await else {
        return ctx.raw_html("No field".into()).await;
    };
    if s(&f, "field_type") == "BelongsTo" {
        let fopts = s(&f, "field_options");
        let parts: Vec<&str> = fopts.split(':').filter(|x| !x.is_empty()).collect();
        let parent_t = match i(&f, "tracker_id") {
            Some(tid) => Tracker::by_id(db, tid).await,
            None => None,
        };
        let (omod, oslug, mut ofield) = if parts.len() == 1 {
            (parent_t.as_ref().map(|t| t.module.clone()).unwrap_or_default(), parts[0].to_string(), s(&f, "field_format"))
        } else if parts.len() >= 2 {
            (parts[0].to_string(), parts[1].to_string(), if parts.len() > 2 { parts[2].to_string() } else { s(&f, "field_format") })
        } else {
            (String::new(), String::new(), String::new())
        };
        if let Some(ot) = Tracker::load(db, &omod, &oslug).await {
            if ofield.is_empty() {
                ofield = ot.default_field().unwrap_or_else(|| "id".into());
            }
            if let (Ok(dt), Ok(col)) = (db::ident(&ot.data_table()), db::ident(&ofield)) {
                let rows = db.rows(&format!("select id, {}::text as lbl from {} order by 2 limit 500", col, dt), &db::no_params()).await.unwrap_or_default();
                let mut opts = String::new();
                for r in &rows {
                    opts.push_str(&format!("<option value=\"{}\">{}</option>", i(r, "id").unwrap_or(0), esc(&s(r, "lbl"))));
                }
                return ctx.raw_html(format!("<select id=\"custom_{field_id}\" name=\"custom_{field_id}\">{opts}</select>")).await;
            }
        }
    }
    ctx.raw_html(format!("<input type=\"text\" id=\"custom_{field_id}\" name=\"custom_{field_id}\"/>")).await
}

async fn doupload(ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if ctx.method != "POST" {
        return ctx.redirect("/portalTrackerData/index").await;
    }
    let db = &ctx.st.db.clone();
    let uid_rec = ctx.pi("update_id").unwrap_or(0);
    let Ok(Some(d)) = db.get("portal_tracker_data", uid_rec).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let module = s(&d, "module");
    if !ctx.can_admin(&module).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let mut saveparams = serde_json::Map::new();
    for (k, v) in &ctx.params {
        if !k.starts_with('_') && k != "csrf_token" {
            saveparams.insert(k.clone(), v.clone());
        }
    }
    let _ = db
        .update(
            "portal_tracker_data",
            uid_rec,
            &row! {
                "messages"=>"Upload is currently in queue",
                "savedparams"=>Value::Object(saveparams).to_string(),
                "uploaded"=>false,
                "upload_status"=>0i64
            },
        )
        .await;
    upload::spawn_process(ctx.st.clone(), uid_rec, ctx.uid());
    ctx.redirect(&format!("/portalTrackerData/uploadsummary/{uid_rec}")).await
}

async fn uploadsummary(ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(d)) = db.get("portal_tracker_data", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let tracker_row = match i(&d, "tracker_id") {
        Some(tid) => db.get("portal_tracker", tid).await.ok().flatten(),
        None => None,
    };
    let module = {
        let m = s(&d, "module");
        if m.is_empty() { tracker_row.as_ref().map(|t| s(t, "module")).unwrap_or_default() } else { m }
    };
    if !ctx.can_admin(&module).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }

    let back_li = match &tracker_row {
        Some(tr) => format!(
            "<li><a class=\"list\" href=\"/{}/{}/list\">Back to {} List</a></li>",
            enc(&s(tr, "module")),
            enc(&s(tr, "slug")),
            esc(&s(tr, "name"))
        ),
        None => String::new(),
    };
    let msgs = s(&d, "messages");
    let still_processing = !b(&d, "uploaded") && !msgs.starts_with("Upload error") && !msgs.starts_with("Upload failed");

    let content_html = if still_processing {
        r##"<div style="text-align:center;padding:40px 20px;">
  <div style="display:inline-block;width:40px;height:40px;border:4px solid #ddd;border-top-color:#0078d4;border-radius:50%;animation:uploadspin .8s linear infinite;"></div>
  <style>@keyframes uploadspin{to{transform:rotate(360deg);}}</style>
  <h3 style="margin-top:16px;">Processing upload&hellip;</h3>
  <p style="color:#666;">Your file is being processed in the background. This page refreshes automatically &mdash; large files may take a few minutes.</p>
</div>
<script>setTimeout(function(){ window.location.reload(); }, 3000);</script>"##.to_string()
    } else {
        let mut rows = String::new();
        let mut extra = vec![];
        for line in msgs.lines() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            if let Some((k, v)) = t.split_once(':') {
                if !k.starts_with("===") {
                    rows.push_str(&format!("<tr><th style=\"width:220px;\">{}</th><td>{}</td></tr>", esc(k.trim()), esc(v.trim())));
                    continue;
                }
            }
            extra.push(esc(t));
        }
        let mut out = format!("<table class=\"table\">{rows}</table>");
        if !extra.is_empty() {
            out.push_str(&format!("<pre>{}</pre>", extra.join("\n")));
        }
        out
    };

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTrackerData/index">Data Update List</a></li>
  {back_li}
</ul></div></section>
<section class="row"><div class="col-12 content" role="main">
  <h1>Upload Summary</h1>
  {content_html}
</div></section></div></div>"##
    );
    ctx.html("Upload Summary", body).await
}

// ---------------- Stuck Uploads (stuckuploads.gsp) ----------------

async fn find_stuck_uploads(db: &Db, is_admin: bool, my_mods: &[String]) -> Vec<Row> {
    let scope = if is_admin {
        String::new()
    } else if my_mods.is_empty() {
        return vec![];
    } else {
        format!(" and d.\"module\" in {}", lit(&json!(my_mods)))
    };
    db.rows(
        &format!(
            "select d.*, t.slug as tracker_slug, u.userid as uploader_userid \
             from portal_tracker_data d \
             left join portal_tracker t on t.id=d.tracker_id \
             left join portal_user u on u.id=d.uploader_id \
             where (d.uploaded is null or d.uploaded is false) \
               and d.messages = 'Upload is currently in queue'{scope} \
             order by d.id desc"
        ),
        &db::no_params(),
    )
    .await
    .unwrap_or_default()
}

async fn stuckuploads(ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let is_admin = ctx.is_admin();
    let my_mods = if is_admin { vec![] } else { web::admin_list(db, ctx.user.as_ref().unwrap()).await };
    if !is_admin && my_mods.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let stuck = find_stuck_uploads(db, is_admin, &my_mods).await;
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };

    let list_html = if stuck.is_empty() {
        "<p><strong>No stuck uploads &mdash; everything has been processed.</strong></p>".to_string()
    } else {
        let mut trs = String::new();
        for u in &stuck {
            let uid = i(u, "id").unwrap_or(0);
            let fname = s(u, "path").rsplit('/').next().filter(|x| !x.is_empty()).unwrap_or("-").to_string();
            let uploader = { let x = s(u, "uploader_userid"); if x.is_empty() { "-".into() } else { x } };
            let state = if i(u, "upload_status") == Some(-1) { "Interrupted mid-processing" } else { "Never started" };
            trs.push_str(&format!(
                "<tr>\
                 <td><a href=\"/portalTrackerData/uploadsummary/{uid}\">{uid}</a></td>\
                 <td>{}</td>\
                 <td>{}</td>\
                 <td>{}</td>\
                 <td>{}</td>\
                 <td>{state}</td>\
                 <td><a href=\"/portalTrackerData/requeueupload/{uid}\" onclick=\"return confirm('Restart upload {uid}? Partial rows from the interrupted run will be removed first.');\">Restart</a></td>\
                 </tr>",
                esc(&s(u, "module")),
                esc(&s(u, "tracker_slug")),
                esc(&fname),
                esc(&uploader)
            ));
        }
        format!(
            "<table class=\"table\"><thead><tr>\
             <th>ID</th><th>Module</th><th>Tracker</th><th>File</th><th>Uploader</th><th>State</th><th></th>\
             </tr></thead><tbody>{trs}</tbody></table>\
             <a href=\"/portalTrackerData/requeueall\" class=\"btn btn-primary\" onclick=\"return confirm('Restart ALL {} stuck upload(s)? Partial rows from the interrupted runs will be removed first.');\">Restart All</a>",
            stuck.len()
        )
    };

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTrackerData/index">Data Update List</a></li>
</ul></div></section>
<section class="row"><div class="col-12 content" role="main">
  <h1>Stuck Uploads</h1>
  {flash_html}
  <p style="color:#666;">
    These uploads are still marked <strong>&ldquo;Upload is currently in queue&rdquo;</strong> &mdash; their background
    processing never finished, usually because the server restarted mid-upload.
    Restarting removes any partial rows the interrupted run loaded, then re-runs the upload from the saved file.
  </p>
  <p style="color:#a60;">
    <strong>Note:</strong> an upload started only moments ago may still be processing normally &mdash;
    give recent uploads a few minutes before restarting them.
  </p>
  {list_html}
</div></section></div></div>"##
    );
    ctx.html("Stuck Uploads", body).await
}

async fn requeue_one(st: &web::St, d: &Row, uid: Option<i64>) -> Result<(), String> {
    let did = i(d, "id").unwrap_or(0);
    if s(d, "savedparams").trim().is_empty() {
        return Err(format!("Upload {did} has no saved column mapping - delete it and upload the file again"));
    }
    let path = s(d, "path");
    if path.is_empty() || !std::path::Path::new(&path).exists() {
        return Err(format!("Upload {did} - the uploaded file is no longer on disk, upload it again"));
    }
    if let Some(t) = Tracker::by_id(&st.db, i(d, "tracker_id").unwrap_or(0)).await {
        if let Ok(dt) = db::ident(&t.data_table()) {
            let _ = st.db.exec(&format!("delete from {dt} where dataupdate_id={did}"), &db::no_params()).await;
        }
    }
    upload::spawn_process(st.clone(), did, uid);
    Ok(())
}

async fn requeueupload(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(d)) = db.get("portal_tracker_data", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    if !ctx.can_admin(&s(&d, "module")).await {
        ctx.flash(&format!("You do not have the clearance to restart upload {id}"));
        return ctx.redirect("/portalTrackerData/stuckuploads").await;
    }
    if b(&d, "uploaded") {
        ctx.flash(&format!("Upload {id} has already completed - nothing to restart"));
        return ctx.redirect("/portalTrackerData/stuckuploads").await;
    }
    match requeue_one(&ctx.st, &d, ctx.uid()).await {
        Ok(()) => ctx.redirect(&format!("/portalTrackerData/uploadsummary/{id}")).await,
        Err(msg) => {
            ctx.flash(&msg);
            ctx.redirect("/portalTrackerData/stuckuploads").await
        }
    }
}

async fn requeueall(mut ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let is_admin = ctx.is_admin();
    let my_mods = if is_admin { vec![] } else { web::admin_list(db, ctx.user.as_ref().unwrap()).await };
    let stuck = find_stuck_uploads(db, is_admin, &my_mods).await;
    let mut restarted = 0usize;
    let mut problems = vec![];
    for d in &stuck {
        match requeue_one(&ctx.st, d, ctx.uid()).await {
            Ok(()) => restarted += 1,
            Err(e) => problems.push(e),
        }
    }
    let msg = if problems.is_empty() {
        format!("Restarted {restarted} upload(s).")
    } else {
        format!("Restarted {restarted} upload(s). Skipped: {}", problems.join("; "))
    };
    ctx.flash(&msg);
    ctx.redirect("/portalTrackerData/stuckuploads").await
}

// ---------------- Edit / Update / Delete ----------------

async fn edit(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(d)) = db.get("portal_tracker_data", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    if !ctx.can_admin(&s(&d, "module")).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let int_val = |col: &str| i(&d, col).map(|v| v.to_string()).unwrap_or_default();
    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#edit-portalTrackerData" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTrackerData/index">PortalTrackerData List</a></li>
  <li><a class="create" href="/portalTrackerData/create">New PortalTrackerData</a></li>
</ul></div></section>
<section class="row"><div id="edit-portalTrackerData" class="col-12 content scaffold-edit" role="main">
  <h1>Edit PortalTrackerData</h1>
  {flash_html}
  <form action="/portalTrackerData/update/{id}" method="post" enctype="multipart/form-data">
    {csrf}
    <fieldset class="form">
      <div class="fieldcontain"><label for="fileupload">File</label><input type="file" name="fileupload" value="" id="fileupload"/></div>
    </fieldset>
    <fieldset class="form">
      <div class="fieldcontain"><label for="module">Module</label><input type="text" name="module" id="module" value="{module}"/></div>
      <div class="fieldcontain"><label for="upload_status">Upload Status</label><input type="number" name="upload_status" id="upload_status" value="{us}"/></div>
      <div class="fieldcontain"><label for="header_start">Header Start</label><input type="number" name="header_start" id="header_start" value="{hs}"/></div>
      <div class="fieldcontain"><label for="header_end">Header End</label><input type="number" name="header_end" id="header_end" value="{he}"/></div>
      <div class="fieldcontain"><label for="data_row">Data Row</label><input type="number" name="data_row" id="data_row" value="{dr}"/></div>
      <div class="fieldcontain"><label for="data_end">Data End</label><input type="number" name="data_end" id="data_end" value="{de}"/></div>
      <div class="fieldcontain"><label for="excel_password">Excel Password</label><input type="text" name="excel_password" id="excel_password" value="{pw}"/></div>
      <input type="hidden" name="tracker" id="tracker" value="{tid}"/>
    </fieldset>
    <fieldset class="buttons"><input class="save" type="submit" value="Update"/></fieldset>
  </form>
</div></section></div></div>"##,
        module = esc(&s(&d, "module")),
        us = int_val("upload_status"),
        hs = int_val("header_start"),
        he = int_val("header_end"),
        dr = int_val("data_row"),
        de = int_val("data_end"),
        pw = esc(&s(&d, "excel_password")),
        tid = int_val("tracker_id")
    );
    ctx.html("Edit PortalTrackerData", body).await
}

async fn update(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if ctx.method != "POST" {
        return ctx.redirect(&format!("/portalTrackerData/edit/{id}")).await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(existing)) = db.get("portal_tracker_data", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let module = s(&existing, "module");
    if !ctx.can_admin(&module).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let mut path = s(&existing, "path");
    if let Some(up) = ctx.files.remove("fileupload").or_else(|| ctx.files.remove("file")).filter(|u| !u.filename.is_empty() || u.size > 0) {
        let clean = match crate::files::validate_upload(&up) {
            Ok(n) => n,
            Err(e) => {
                ctx.flash(&format!("File upload failed: {}", e));
                return ctx.redirect(&format!("/portalTrackerData/edit/{id}")).await;
            }
        };
        let tslug = match i(&existing, "tracker_id").and_then(|_| None::<String>) {
            Some(x) => x,
            None => match i(&existing, "tracker_id") {
                Some(tid) => db.get("portal_tracker", tid).await.ok().flatten().map(|t| s(&t, "slug")).unwrap_or_else(|| "data".into()),
                None => "data".into(),
            },
        };
        let base = web::setting_str(db, "uploadfolder").await.filter(|v| !v.trim().is_empty()).unwrap_or_else(|| ctx.st.cfg.upload_dir.clone());
        let dir = PathBuf::from(&base).join(&module).join(&tslug);
        let _ = tokio::fs::create_dir_all(&dir).await;
        let target = dir.join(&clean);
        if let Err(e) = up.save_to(&target).await {
            ctx.flash(&format!("File upload failed: {}", e));
            return ctx.redirect(&format!("/portalTrackerData/edit/{id}")).await;
        }
        path = target.to_string_lossy().to_string();
    }
    let pw = ctx.p("excel_password").trim().to_string();
    let d = row! {
        "module"=>ctx.p("module").trim().to_string(),
        "path"=>path,
        "upload_status"=>opt_int(ctx.p("upload_status")),
        "header_start"=>opt_int(ctx.p("header_start")),
        "header_end"=>opt_int(ctx.p("header_end")),
        "data_row"=>opt_int(ctx.p("data_row")),
        "data_end"=>opt_int(ctx.p("data_end")),
        "excel_password"=>if pw.is_empty() { Value::Null } else { json!(pw) }
    };
    let _ = db.update("portal_tracker_data", id, &d).await;
    ctx.flash(&format!("PortalTrackerData {id} updated"));
    ctx.redirect(&format!("/portalTrackerData/show/{id}")).await
}

async fn delete_action(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if ctx.method != "POST" {
        return ctx.status(StatusCode::METHOD_NOT_ALLOWED, "POST required").await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(existing)) = db.get("portal_tracker_data", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    if !ctx.can_admin(&s(&existing, "module")).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    // Like PortalTrackerData.beforeDelete: remove rows created by this upload and delete file on disk
    if !b(&existing, "is_tracker_deleting") {
        if let Some(t) = Tracker::by_id(db, i(&existing, "tracker_id").unwrap_or(0)).await {
            if let Ok(dt) = db::ident(&t.data_table()) {
                let _ = db.exec(&format!("delete from {dt} where dataupdate_id={id}"), &db::no_params()).await;
            }
        }
    }
    let path = s(&existing, "path");
    if !path.is_empty() {
        let _ = tokio::fs::remove_file(&path).await;
    }
    let _ = db.delete("portal_tracker_data", id).await;
    ctx.flash(&format!("PortalTrackerData {id} deleted"));
    ctx.redirect("/portalTrackerData/index").await
}

// ---------------- Tracker DB maintenance actions ----------------

async fn cleardb(mut ctx: Ctx) -> Response {
    let tid = ctx.pi("tracker_id").unwrap_or(0);
    let db = &ctx.st.db.clone();
    let Some(t) = Tracker::by_id(db, tid).await else { return ctx.redirect("/portalTracker/index").await };
    if ctx.can_admin(&t.module).await {
        let _ = db.exec("delete from portal_tracker_data where tracker_id=:tid", &row! {"tid"=>tid}).await;
        let _ = t.cleardb(db).await;
        ctx.flash(&format!("Clearing database {} has been done", t.name));
    } else {
        ctx.flash(&format!("You do not have the clearance to clear database for {}", t.name));
    }
    ctx.redirect(&format!("/portalTracker/show/{tid}")).await
}

async fn cleandb(mut ctx: Ctx) -> Response {
    let tid = ctx.pi("tracker_id").unwrap_or(0);
    let db = &ctx.st.db.clone();
    let Some(t) = Tracker::by_id(db, tid).await else { return ctx.redirect("/portalTracker/index").await };
    if ctx.can_admin(&t.module).await {
        if t.tracker_type != "DataStore" {
            if let Ok(dt) = db::ident(&t.data_table()) {
                let _ = db.exec(&format!("delete from {dt} where record_status='sys_draft'"), &db::no_params()).await;
            }
            ctx.flash(&format!("Cleaning database {} has been done", t.name));
        } else {
            ctx.flash(&format!("Cleaning database {} not done on a datastore", t.name));
        }
    } else {
        ctx.flash(&format!("You do not have the clearance to clean database for {}", t.name));
    }
    ctx.redirect(&format!("/portalTracker/show/{tid}")).await
}

async fn resetdb(mut ctx: Ctx) -> Response {
    let tid = ctx.pi("tracker_id").unwrap_or(0);
    let db = &ctx.st.db.clone();
    let Some(t) = Tracker::by_id(db, tid).await else { return ctx.redirect("/portalTracker/index").await };
    if ctx.can_admin(&t.module).await {
        if let Ok(dt) = db::ident(&t.data_table()) {
            let _ = db.exec(&format!("drop table if exists {dt}"), &db::no_params()).await;
        }
        if t.tracker_type != "DataStore" {
            if let Ok(tt) = db::ident(&t.trail_table()) {
                let _ = db.exec(&format!("drop table if exists {tt}"), &db::no_params()).await;
            }
        }
        ctx.flash(&format!("Reset database {} has been done", t.name));
    } else {
        ctx.flash(&format!("You do not have the clearance to reset database for {}", t.name));
    }
    ctx.redirect(&format!("/portalTracker/show/{tid}")).await
}

async fn syncupload(mut ctx: Ctx, tid: i64) -> Response {
    let db = &ctx.st.db.clone();
    let Some(t) = Tracker::by_id(db, tid).await else { return ctx.redirect("/portalTracker/index").await };
    if !ctx.can_admin(&t.module).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    if let Ok(dt) = db::ident(&t.data_table()) {
        let rows = db.rows(&format!("select distinct dataupdate_id from {dt} where dataupdate_id is not null"), &db::no_params()).await.unwrap_or_default();
        let mut created = 0usize;
        for r in rows {
            if let Some(duid) = i(&r, "dataupdate_id") {
                if db.get("portal_tracker_data", duid).await.ok().flatten().is_none() {
                    if let Ok(new_id) = db
                        .insert(
                            "portal_tracker_data",
                            &row! {"tracker_id"=>tid,"module"=>t.module.clone(),"messages"=>"Created dataupdate using syncupload","date_created"=>now_str(),"uploaded"=>true},
                        )
                        .await
                    {
                        let _ = db.exec(&format!("update {dt} set dataupdate_id={new_id} where dataupdate_id={duid}"), &db::no_params()).await;
                        created += 1;
                    }
                }
            }
        }
        ctx.flash(&format!("Synced {created} missing data update record(s)"));
    }
    ctx.redirect(&format!("/portalTracker/show/{tid}")).await
}
