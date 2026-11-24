//! PortalModule web screens and actions matching g6ikram's PortalModuleController + views/portalModule/*.gsp.

use crate::access_review::{format_date_short, period_label};
use crate::db::{self, b, i, lit, s, vs, Db, Row};
use crate::module::{
    self, diff_module_folders, export_to, folder, import_from_opts, preview_schedulers, preview_settings, unzip_module, zip_dir, ExportOpts, ImportOpts,
};
use crate::row;
use crate::tracker::Tracker;
use crate::web::{self, enc, esc, now_str, Ctx};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

const STATUSES: &[&str] = &["Active", "Inactive", "Retired"];

fn human_size(n: i64) -> String {
    let mut v = n as f64;
    for u in ["B", "KB", "MB", "GB"] {
        if v < 1024.0 {
            return if u == "B" { format!("{} B", n) } else { format!("{:.1} {}", v, u) };
        }
        v /= 1024.0;
    }
    format!("{:.1} TB", v)
}

fn diff_view_html(diff: &str) -> String {
    let mut out = String::from(
        r#"<style>
    .module-diff {
        font-family: monospace;
        font-size: 13px;
        line-height: 1.4;
        background: #fff;
        border: 1px solid #ccc;
        border-radius: 4px;
        padding: 10px;
        max-height: 600px;
        overflow: auto;
        white-space: pre;
    }
    .module-diff .diff-add { background: #e6ffec; color: #1a7f37; display: block; }
    .module-diff .diff-del { background: #ffebe9; color: #cf222e; display: block; }
    .module-diff .diff-hunk { background: #ddf4ff; color: #0550ae; display: block; }
    .module-diff .diff-file { background: #f6f8fa; color: #57606a; font-weight: bold; display: block; }
    .module-diff .diff-ctx { display: block; }
</style>
<div class="module-diff">"#,
    );
    for line in diff.split('\n') {
        let cls = if line.starts_with("---") || line.starts_with("+++") || line.starts_with("Binary file") {
            "diff-file"
        } else if line.starts_with("@@") {
            "diff-hunk"
        } else if line.starts_with('+') {
            "diff-add"
        } else if line.starts_with('-') {
            "diff-del"
        } else {
            "diff-ctx"
        };
        out.push_str(&format!("<span class=\"{cls}\">{}</span>", esc(line)));
    }
    out.push_str("</div>");
    out
}

async fn contacts_by_role(db: &Db, module_id: i64, role: &str) -> Vec<Row> {
    db.rows(
        "select u.id, u.userid, u.name from portal_module_contact c join portal_user u on u.id=c.user_id where c.module_id=:m and c.role=:r order by c.id",
        &row! {"m"=>module_id,"r"=>role},
    )
    .await
    .unwrap_or_default()
}

async fn is_dev_of(ctx: &Ctx, module_name: &str) -> bool {
    if ctx.is_admin() {
        return true;
    }
    match &ctx.user {
        Some(u) => web::developer_list(ctx.db(), u).await.iter().any(|m| m == module_name),
        None => false,
    }
}

async fn is_adm_of(ctx: &Ctx, module_name: &str) -> bool {
    if is_dev_of(ctx, module_name).await {
        return true;
    }
    match &ctx.user {
        Some(u) => web::admin_list(ctx.db(), u).await.iter().any(|m| m == module_name),
        None => false,
    }
}

pub async fn controller(ctx: Ctx, action: &str, id: Option<i64>) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    match action {
        "" | "index" | "list" => index(ctx).await,
        "show" => show(ctx, id.unwrap_or(0)).await,
        "create" => create_or_edit(ctx, None).await,
        "edit" => create_or_edit(ctx, id).await,
        "save" => save_module(ctx, None).await,
        "update" => save_module(ctx, id).await,
        "delete" => delete_or_op(ctx, id.unwrap_or(0)).await,
        "updatedball" => updatedball(ctx, id.unwrap_or(0)).await,
        "updatelist" | "update_module_list" => module::action(ctx, "update_module_list", id).await,
        "importform" | "upload" => importform(ctx).await,
        "importpreview" => importpreview(ctx, id.unwrap_or(0)).await,
        "confirmimport" => confirmimport(ctx, id.unwrap_or(0)).await,
        "importlogs" => importlogs(ctx).await,
        "importlog" => importlog_show(ctx, id.unwrap_or(0)).await,
        "exportUserRoles" => export_user_roles(ctx, id.unwrap_or(0)).await,
        "importUserRoles" => import_user_roles(ctx, id.unwrap_or(0)).await,
        "deleteAllUserRoles" => delete_all_user_roles(ctx, id.unwrap_or(0)).await,
        "exportSettings" => export_settings(ctx, id.unwrap_or(0)).await,
        "importSettings" => import_settings(ctx, id.unwrap_or(0)).await,
        "deleteAllSettings" => delete_all_settings(ctx, id.unwrap_or(0)).await,
        "export" | "import" | "download" => module::action(ctx, action, id).await,
        _ => ctx.status(StatusCode::NOT_FOUND, "Page not found").await,
    }
}

async fn index(ctx: Ctx) -> Response {
    let db = &ctx.st.db.clone();
    let Some(u) = ctx.user.as_ref() else { return ctx.need_login().await };
    let is_admin = ctx.is_admin();
    let dev_mods = if is_admin { vec!["*".to_string()] } else { web::developer_list(db, u).await };
    let adm_mods = if is_admin { vec![] } else { web::admin_list(db, u).await };
    if !is_admin && adm_mods.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this list").await;
    }

    let q = ctx.p("q");
    let mut cond = vec![];
    if !is_admin {
        cond.push(format!("name in {}", lit(&json!(adm_mods))));
    }
    if !q.trim().is_empty() {
        let like = lit(&json!(format!("%{}%", q.trim())));
        cond.push(format!("(name ilike {l} or title ilike {l} or description ilike {l} or category ilike {l} or department ilike {l})", l = like));
    }
    let wh = if cond.is_empty() { String::new() } else { format!(" where {}", cond.join(" and ")) };
    let max = ctx.pi("max").unwrap_or(10).clamp(1, 500);
    let offset = ctx.pi("offset").unwrap_or(0).max(0);
    let total = db.count(&format!("select count(*) from portal_module{}", wh), &db::no_params()).await.unwrap_or(0);
    let rows = db
        .rows(&format!("select * from portal_module{} order by id limit {} offset {}", wh, max, offset), &db::no_params())
        .await
        .unwrap_or_default();

    let mut nav = String::from(
        "<div class=\"nav\" role=\"navigation\"><ul>\n\
         <li><a class=\"home\" href=\"/\">Home</a></li>\n",
    );
    if is_admin {
        nav.push_str(
            "<li><a class=\"create\" href=\"/portalModule/create\">New PortalModule</a></li>\n\
             <li><a class=\"create\" href=\"/portalModule/updatelist\">Update</a></li>\n\
             <li><a class=\"list\" href=\"/accessReview/index\">Access Reviews</a></li>\n",
        );
    }
    if !dev_mods.is_empty() {
        nav.push_str("<li><a class=\"create\" href=\"/portalModule/importform\">Import Zip</a></li>\n");
    }
    if is_admin || !dev_mods.is_empty() {
        nav.push_str("<li><a class=\"list\" href=\"/portalModule/importlogs\">Import Logs</a></li>\n");
    }
    if is_admin {
        let has_usage = db
            .count(
                &format!("select count(*) from {} where module='module_usage' and slug='usage_dashboard' and published is true", db::page_t()),
                &db::no_params(),
            )
            .await
            .unwrap_or(0)
            > 0;
        if has_usage {
            nav.push_str("<li><a class=\"list\" href=\"/view/module_usage/usage_dashboard\">Usage Dashboard</a></li>\n");
        }
    }
    nav.push_str("</ul></div>");

    let mut tbody = String::new();
    for m in &rows {
        let mid = i(m, "id").unwrap_or(0);
        let owners = contacts_by_role(db, mid, "Owner").await.iter().map(|x| esc(&s(x, "name"))).collect::<Vec<_>>().join(", ");
        tbody.push_str(&format!(
            "<tr><td><a href=\"/portalModule/show/{mid}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
            esc(&s(m, "name")),
            esc(&s(m, "title")),
            esc(&s(m, "status")),
            esc(&s(m, "category")),
            owners
        ));
    }

    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let pag = if total > max {
        format!("<div class=\"pagination\">{}</div>", crate::tags::paginate("/portalModule/index", &ctx.params, total, max, offset))
    } else {
        String::new()
    };

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#list-portalModule" class="skip" tabindex="-1">Skip to content&hellip;</a>{nav}</section>
<section class="row"><div id="list-portalModule" class="col-12 content scaffold-list" role="main">
  <h1>PortalModule List</h1>
  {flash_html}
  <form action="/portalModule/index" method="get" name="searchform" id="searchform">
    <fieldset class="form">
      <div class="fieldcontain" id="listsearch">
        <label>Search:</label><input type="text" name="q" id="q" value="{q}"/><input id="dosearch" name="dosearch" type="submit" value="Search"/>
      </div>
    </fieldset>
  </form>
  <table class="table">
    <thead><tr><th>Name</th><th>Title</th><th>Status</th><th>Category</th><th>Owners</th></tr></thead>
    <tbody>{tbody}</tbody>
  </table>
  {pag}
</div></section></div></div>"##,
        q = esc(&q)
    );
    ctx.html("PortalModule List", body).await
}

fn action_bar(id: i64, name: &str, csrf: &str, is_admin: bool, is_dev: bool, suffix: &str) -> String {
    let en = enc(name);
    let qn = esc(name).replace('\'', "\\'");
    let user_lbl = if suffix.is_empty() { "user:" } else { "User:" };
    let mut inner = String::new();
    if is_admin || is_dev {
        inner.push_str(&format!(
            "<label for='files{s}'>Files:</label><input type=\"checkbox\" name=\"files\" id=\"files{s}\" /> \
             <label for='trees{s}'>Trees:</label><input type=\"checkbox\" name=\"trees\" id=\"trees{s}\" title=\"Include this module's trees in the export. On import trees are taken whenever the package has them.\" /> \
             <label for='menus{s}'>Menus:</label><input type=\"checkbox\" name=\"menus\" id=\"menus{s}\" title=\"Include this module's menu entries in the export, so importing it elsewhere can wire up the megamenu and sidebar.\" /> \
             <label for='endpoints{s}'>Endpoints:</label><input type=\"checkbox\" name=\"endpoints\" id=\"endpoints{s}\" title=\"Include this module's HTTP endpoints in the export/import. Leave unticked to keep existing server endpoints untouched.\" /> \
             <label for='user{s}'>{user_lbl}</label><input type=\"checkbox\" name=\"user\" id=\"user{s}\" /> \
             <a class=\"edit\" href=\"/portalModule/edit/{id}\">Edit</a> \
             <input class=\"delete\" type=\"submit\" name=\"op\" value=\"Export\" onclick=\"return confirm('Export module {qn}?');\" /> ",
            s = suffix
        ));
    }
    if is_admin {
        inner.push_str(&format!(
            "<input class=\"delete\" type=\"submit\" name=\"op\" value=\"Delete\" onclick=\"return confirm('Delete module {qn}?');\" /> \
             <input class=\"delete\" type=\"submit\" name=\"op\" value=\"Import\" onclick=\"return confirm('Import module {qn}?');\" /> "
        ));
    }
    inner.push_str(&format!(
        "<a class=\"save\" href=\"/userRole/create?module={en}&amp;role=Admin\">Add Admin</a> \
         <a class=\"save\" href=\"/userRole/create?module={en}&amp;role=Developer\">Add Developer</a>"
    ));
    format!("<form action=\"/portalModule/delete/{id}\" method=\"post\">{csrf}<fieldset class=\"buttons\">{inner}</fieldset></form>")
}

async fn show(mut ctx: Ctx, id: i64) -> Response {
    let db = &ctx.st.db.clone();
    let Ok(Some(m)) = db.get("portal_module", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let name = s(&m, "name");
    let is_admin = ctx.is_admin();
    let is_dev = is_dev_of(&ctx, &name).await;
    let is_adm = is_adm_of(&ctx, &name).await;
    if !is_adm {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this").await;
    }

    let en = enc(&name);
    let p = row! {"m"=>name.clone()};
    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_errors: Vec<String> = ctx
        .sess
        .remove("flash_errors")
        .and_then(|v| v.as_array().cloned())
        .map(|a| a.iter().map(vs).collect())
        .unwrap_or_default();
    let updatedb_results: Option<Vec<Value>> = ctx.sess.remove("updatedb_results").and_then(|v| v.as_array().cloned());

    // Fetch module resources
    let fs = db
        .first("select count(*) as n, coalesce(sum(cast(size as bigint)),0) as total, coalesce(sum(case when size is null then 1 else 0 end),0) as unsized from file_link where module=:m", &p)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let trackers = db
        .rows(
            "select t.*, (select count(*) from portal_tracker_field f where f.tracker_id=t.id) as nfields, \
             (select count(*) from portal_tracker_transition x where x.tracker_id=t.id) as ntrans \
             from portal_tracker t where t.module=:m order by t.slug",
            &p,
        )
        .await
        .unwrap_or_default();
    let pages = db
        .rows(&format!("select id, title, slug, runable from {} where module=:m order by slug", db::page_t()), &p)
        .await
        .unwrap_or_default();
    let roles = db
        .rows(
            "select r.id, r.role, r.user_id, u.name, u.userid from user_role r left join portal_user u on u.id=r.user_id where r.module=:m order by lower(coalesce(u.name,'')), r.role",
            &p,
        )
        .await
        .unwrap_or_default();
    let role_user_ids: HashSet<i64> = roles.iter().filter_map(|r| i(r, "user_id")).collect();
    let settings = db.rows("select * from portal_setting where module=:m order by name", &p).await.unwrap_or_default();
    let schedulers = db.rows("select * from portal_scheduler where module=:m order by name", &p).await.unwrap_or_default();
    let trees = db
        .rows(
            "select t.*, (select name from portal_tree_node n where n.id=t.root_id) as root_name, \
             (select count(*) from portal_tree_node n where n.tree_id=t.id) as nodes \
             from portal_tree t where t.module=:m order by t.name",
            &p,
        )
        .await
        .unwrap_or_default();
    let endpoints = db.rows("select * from portal_endpoint where module=:m order by slug", &p).await.unwrap_or_default();
    let importlogs = db
        .rows("select * from portal_module_import_log where module=:m order by date_created desc, id desc limit 20", &p)
        .await
        .unwrap_or_default();

    let owners = contacts_by_role(db, id, "Owner").await;
    let maintainers = contacts_by_role(db, id, "Maintainer").await;
    let pmreview = db
        .first("select * from portal_module_access_review where module_id=:m order by period desc, id desc limit 1", &row! {"m"=>id})
        .await
        .ok()
        .flatten();

    let nav = format!(
        "<div class=\"nav\" role=\"navigation\"><ul>\
         <li><a class=\"home\" href=\"/\">Home</a></li>\
         <li><a class=\"list\" href=\"/portalModule/index\">PortalModule List</a></li>\
         {}\
         </ul></div>",
        if is_admin { "<li><a class=\"create\" href=\"/portalModule/create\">New PortalModule</a></li>" } else { "" }
    );

    let top_bar = action_bar(id, &name, &csrf, is_admin, is_dev, "");
    let bot_bar = action_bar(id, &name, &csrf, is_admin, is_dev, "_bottom");

    // Register card (.pm-register)
    let title = { let t = s(&m, "title"); if t.trim().is_empty() { name.clone() } else { t } };
    let status = s(&m, "status");
    let badge = if !status.is_empty() {
        let cls = match status.as_str() {
            "Active" => "bg-success",
            "Retired" => "bg-dark",
            _ => "bg-secondary",
        };
        format!("<span class=\"badge {cls}\">{}</span>", esc(&status))
    } else {
        "<span class=\"badge bg-light text-dark\">Status not recorded</span>".to_string()
    };
    let cat = s(&m, "category");
    let dept = s(&m, "department");
    let key_extra = format!(
        "{}{}",
        if cat.is_empty() { String::new() } else { format!(" &middot; {}", esc(&cat)) },
        if dept.is_empty() { String::new() } else { format!(" &middot; {}", esc(&dept)) }
    );
    let empty_note = if trackers.is_empty() && pages.is_empty() {
        "<div class=\"pm-note\">No trackers or pages in this portal yet &mdash; the module is registered here but has not been migrated or built.</div>"
    } else {
        ""
    };
    let fmt_contacts = |list: &[Row]| -> String {
        if list.is_empty() {
            return "<span class=\"text-muted\">Not recorded</span>".into();
        }
        list.iter()
            .map(|u| {
                let uid = i(u, "id").unwrap_or(0);
                let warn = if !role_user_ids.contains(&uid) {
                    " <span class=\"pm-warn\" title=\"Has no role in this module, so does not use it\">(no role in this module)</span>"
                } else {
                    ""
                };
                format!("{}{}", esc(&s(u, "name")), warn)
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let review_html = match &pmreview {
        Some(rv) => {
            let rvid = i(rv, "id").unwrap_or(0);
            let st = s(rv, "status");
            let due = if st == "Pending" {
                format!(" &middot; due {}", esc(&format_date_short(&s(rv, "due_date"))))
            } else {
                String::new()
            };
            format!(
                "<a href=\"/accessReview/show/{rvid}\">{}: {}</a>{due}",
                esc(&period_label(&s(rv, "period"))),
                esc(&st)
            )
        }
        None => "<span class=\"text-muted\">None yet</span>".into(),
    };
    let desc_html = {
        let d = s(&m, "description");
        if d.trim().is_empty() { "<span class=\"text-muted\">Not recorded</span>".into() } else { format!("<span class=\"pm-text\">{}</span>", esc(&d)) }
    };
    let ben_html = {
        let d = s(&m, "benefit");
        if d.trim().is_empty() { "<span class=\"text-muted\">Not recorded</span>".into() } else { format!("<span class=\"pm-text\">{}</span>", esc(&d)) }
    };

    let register_card = format!(
        r#"<div class="pm-register">
  <h2 class="pm-title">{title} {badge}</h2>
  <div class="pm-key"><code>{name}</code>{key_extra}</div>
  {empty_note}
  <dl class="pm-dl">
    <dt>Owners</dt><dd>{owners}</dd>
    <dt>Maintainers</dt><dd>{maintainers}</dd>
    <dt>Access review</dt><dd>{review_html}</dd>
    <dt>Description</dt><dd>{desc_html}</dd>
    <dt>Benefit</dt><dd>{ben_html}</dd>
  </dl>
</div>
<style>
  .pm-register {{ border: 1px solid #e4e6eb; border-radius: 12px; padding: 1rem 1.25rem; margin-bottom: 1.5rem; background: #fff; }}
  .pm-register .pm-title {{ font-size: 1.4rem; font-weight: 600; margin: 0 0 .2rem; }}
  .pm-register .pm-title .badge {{ font-size: .7rem; vertical-align: middle; }}
  .pm-register .pm-key {{ color: #6b7280; font-size: .9rem; margin-bottom: .75rem; }}
  .pm-register .pm-note {{ background: #fff8e6; border-radius: 8px; padding: .5rem .8rem; font-size: .85rem; margin-bottom: .75rem; }}
  .pm-register .pm-dl {{ display: grid; grid-template-columns: 140px 1fr; gap: .4rem 1rem; margin: 0; }}
  .pm-register .pm-dl dt {{ color: #6b7280; font-weight: 500; }}
  .pm-register .pm-dl dd {{ margin: 0; }}
  .pm-register .pm-text {{ white-space: pre-line; }}
  .pm-register .pm-warn {{ color: #b7791f; font-size: .8rem; }}
  @media (max-width: 600px) {{ .pm-register .pm-dl {{ grid-template-columns: 1fr; }} }}
</style>"#,
        title = esc(&title),
        name = esc(&name),
        owners = fmt_contacts(&owners),
        maintainers = fmt_contacts(&maintainers),
    );

    // Files section
    let unsized_cnt = i(&fs, "unsized").unwrap_or(0);
    let unsized_html = if unsized_cnt > 0 {
        let bf = if is_admin {
            format!(
                "&nbsp;<a href=\"/fileLink/updateMissingSizes?scopemodule={en}\" onclick=\"return confirm('Scan disk and backfill sizes for the {unsized_cnt} unsized file records in module {}?');\">Backfill</a>",
                esc(&name).replace('\'', "\\'")
            )
        } else {
            String::new()
        };
        format!("<span title=\"These file records have no size stored, so the total above understates actual disk usage.\">{unsized_cnt}</span>{bf}")
    } else {
        "0".to_string()
    };
    let mut files_nav = String::new();
    if is_adm {
        files_nav.push_str(&format!("<li><a class=\"list\" href=\"/fileLink/index?module={en}&amp;q=\">View Files for this Module</a></li>"));
    }
    if is_admin {
        files_nav.push_str("<li><a class=\"list\" href=\"/fileLink/jobstatus\">File Size Job Status</a></li>");
    }
    let files_sec = format!(
        "<h3>Files</h3><div class=\"nav\" role=\"navigation\"><ul>{files_nav}</ul></div>\
         <table class='table'><tr><th>Total Size on Disk</th><th>Files</th><th>Size Not Recorded</th></tr>\
         <tr><td><strong>{}</strong></td><td>{}</td><td>{unsized_html}</td></tr></table><br/>",
        human_size(i(&fs, "total").unwrap_or(0)),
        i(&fs, "n").unwrap_or(0)
    );

    // Trackers section
    let mut tr_nav = String::new();
    if is_dev {
        tr_nav.push_str(&format!("<li><a class=\"create\" href=\"/portalTracker/create?module={en}\">Add Tracker</a></li>"));
    }
    if !trackers.is_empty() && is_dev {
        let qn = esc(&name).replace('\'', "\\'");
        tr_nav.push_str(&format!(
            "<li><form action=\"/portalModule/updatedball/{id}\" method=\"post\" style=\"display:inline; margin:0;\">{csrf}\
             <input type=\"submit\" class=\"save\" value=\"Update DB (all {} trackers)\" \
             title=\"Create any missing tables and columns for every tracker in this module\" \
             onclick=\"return confirm('Run Update DB on all {} tracker(s) of {qn}? It only adds missing tables and columns.');\" /></form></li>",
            trackers.len(),
            trackers.len()
        ));
    }
    if !trackers.is_empty() && is_adm {
        tr_nav.push_str(&format!(
            "<li><a class=\"create\" href=\"/portalTrackerData/moduledump/{id}\" target=\"_blank\" \
             title=\"Download a zip with one re-importable Excel file per tracker in this module\">Data Dump (all {} trackers)</a></li>",
            trackers.len()
        ));
    }
    let mut udb_html = String::new();
    if let Some(res_list) = &updatedb_results {
        let mut rrows = String::new();
        for r in res_list {
            let ok = r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            let before = r.get("before").and_then(|v| v.as_i64()).unwrap_or(0);
            let after = r.get("after").and_then(|v| v.as_i64()).unwrap_or(0);
            let status_td = if !ok {
                format!("<span style=\"color:#b91c1c\"><b>Failed:</b> {}</span>", esc(r.get("error").and_then(|v| v.as_str()).unwrap_or("")))
            } else if after > before {
                format!("<span style=\"color:#1a7f37\">{} column(s) added</span>", after - before)
            } else {
                "already up to date".to_string()
            };
            rrows.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{before}</td><td>{after}</td><td>{status_td}</td></tr>",
                esc(r.get("name").and_then(|v| v.as_str()).unwrap_or("")),
                esc(r.get("slug").and_then(|v| v.as_str()).unwrap_or(""))
            ));
        }
        udb_html = format!(
            "<div class=\"message\" role=\"status\"><strong>Update DB results</strong>\
             <table class=\"table table-sm\" style=\"margin:6px 0 0\">\
             <tr><th>Tracker</th><th>Slug</th><th>Columns before</th><th>After</th><th>Result</th></tr>{rrows}</table></div>"
        );
    }
    let mut tr_rows = String::new();
    for (idx, t) in trackers.iter().enumerate() {
        let tid = i(t, "id").unwrap_or(0);
        let tslug = s(t, "slug");
        let edit_link = if is_dev { format!("&nbsp;&nbsp;<a href=\"/portalTracker/show/{tid}\">Edit</a>") } else { String::new() };
        tr_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><a href=\"/{en}/{}/list\">List</a>{edit_link}</td></tr>",
            idx + 1,
            esc(&s(t, "name")),
            esc(&tslug),
            esc(&s(t, "tracker_type")),
            s(t, "nfields"),
            s(t, "ntrans"),
            enc(&tslug)
        ));
    }
    let trackers_sec = format!(
        "<h3>Trackers</h3><div class=\"nav\" role=\"navigation\"><ul>{tr_nav}</ul></div>{udb_html}\
         <table class='table'><tr><th>#</th><th>Title</th><th>Slug</th><th>Type</th><th>Fields</th><th>Transitions</th><th>Action</th></tr>{tr_rows}</table><br/>"
    );

    // Pages section
    let pg_nav = if is_dev { format!("<li><a class=\"create\" href=\"/portalPage/create?module={en}\">Add Page</a></li>") } else { String::new() };
    let mut pg_rows = String::new();
    for (idx, g) in pages.iter().enumerate() {
        let pid = i(g, "id").unwrap_or(0);
        let pslug = s(g, "slug");
        let run = b(g, "runable");
        let view_link = if run {
            format!("<a class=\"create\" href=\"/run/{en}/{}\">Run</a>", enc(&pslug))
        } else {
            format!("<a class=\"create\" href=\"/view/{en}/{}\">View</a>", enc(&pslug))
        };
        let dev_links = if is_dev {
            format!("&nbsp;&nbsp;<a class=\"edit\" href=\"/portalPage/edit/{pid}\">Edit</a>&nbsp;&nbsp;<a class=\"display\" href=\"/portalPage/show/{pid}\">Details</a>")
        } else {
            String::new()
        };
        pg_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{view_link}{dev_links}</td></tr>",
            idx + 1,
            esc(&s(g, "title")),
            esc(&pslug)
        ));
    }
    let pages_sec = format!(
        "<h3>Pages</h3><div class=\"nav\" role=\"navigation\"><ul>{pg_nav}</ul></div>\
         <table class='table'><tr><th>#</th><th>Title</th><th>Slug</th><th>Action</th></tr>{pg_rows}</table><br/>"
    );

    // Roles section
    let roles_nav = if is_dev {
        format!(
            "<li><a class=\"create\" href=\"/userRole/create?module={en}\">Add User Role</a></li>\
             <li><a class=\"list\" href=\"/portalModule/exportUserRoles/{id}\">Export User Roles</a></li>"
        )
    } else {
        String::new()
    };
    let qn = esc(&name).replace('\'', "\\'");
    let roles_box = if is_dev {
        format!(
            "<div style=\"margin: 10px 0; padding: 10px; border: 1px solid #ccc; border-radius: 4px; display: flex; align-items: center; gap: 20px; flex-wrap: wrap;\">\
             <form action=\"/portalModule/importUserRoles/{id}\" method=\"post\" enctype=\"multipart/form-data\" style=\"display:inline-flex; align-items:center; gap:8px; margin:0;\">{csrf}\
             <label>Import User Roles (Excel): </label><input type=\"file\" name=\"userRoleFile\" accept=\".xlsx\" required /><input type=\"submit\" class=\"save\" value=\"Import\" /></form>\
             <form action=\"/portalModule/deleteAllUserRoles/{id}\" method=\"post\" style=\"display:inline; margin:0;\">{csrf}\
             <input type=\"submit\" class=\"delete\" value=\"Delete All Roles\" onclick=\"return confirm('Delete ALL user roles for module \\'{qn}\\'?') &amp;&amp; confirm('This cannot be undone. Are you absolutely sure?');\" /></form></div>"
        )
    } else {
        String::new()
    };
    let errors_box = if !flash_errors.is_empty() {
        let lis: String = flash_errors.iter().map(|e| format!("<li>{}</li>", esc(e))).collect();
        format!("<div class=\"errors\" role=\"status\"><ul>{lis}</ul></div>")
    } else {
        String::new()
    };
    let can_switch = is_admin && !ctx.sess.contains_key("adminlink");
    let mut role_rows = String::new();
    for (idx, r) in roles.iter().enumerate() {
        let rid = i(r, "id").unwrap_or(0);
        let nm = esc(&s(r, "name"));
        let uid_str = s(r, "userid");
        let name_cell = if can_switch && !uid_str.is_empty() {
            format!("<a href=\"/user/switchuser/{}\">{nm}</a>", enc(&uid_str))
        } else {
            nm
        };
        let edit_td = if is_dev { format!("<a href=\"/userRole/edit/{rid}\">Edit</a>") } else { String::new() };
        role_rows.push_str(&format!(
            "<tr><td>{}</td><td>{name_cell}</td><td>{}</td><td>{}</td><td>{edit_td}</td></tr>",
            idx + 1,
            esc(&uid_str),
            esc(&s(r, "role"))
        ));
    }
    let roles_sec = format!(
        "<h3>Roles</h3><div class=\"nav\" role=\"navigation\"><ul>{roles_nav}</ul></div>{roles_box}{errors_box}\
         <table class='table'><tr><th>#</th><th>Name</th><th>User ID</th><th>Role</th><th>Action</th></tr>{role_rows}</table><br/>"
    );

    // Settings section
    let set_nav = if is_dev {
        format!(
            "<li><a class=\"create\" href=\"/portalSetting/create?module={en}\">Add Setting</a></li>\
             <li><a class=\"list\" href=\"/portalModule/exportSettings/{id}\">Export Settings</a></li>"
        )
    } else {
        String::new()
    };
    let set_box = if is_dev {
        format!(
            "<div style=\"margin: 10px 0; padding: 10px; border: 1px solid #ccc; border-radius: 4px; display: flex; align-items: center; gap: 20px; flex-wrap: wrap;\">\
             <form action=\"/portalModule/importSettings/{id}\" method=\"post\" enctype=\"multipart/form-data\" style=\"display:inline-flex; align-items:center; gap:8px; margin:0;\">{csrf}\
             <label>Import Settings (Excel): </label><input type=\"file\" name=\"settingsFile\" accept=\".xlsx\" required /><input type=\"submit\" class=\"save\" value=\"Import\" /></form>\
             <form action=\"/portalModule/deleteAllSettings/{id}\" method=\"post\" style=\"display:inline; margin:0;\">{csrf}\
             <input type=\"submit\" class=\"delete\" value=\"Delete All Settings\" onclick=\"return confirm('Delete ALL settings for module \\'{qn}\\'?') &amp;&amp; confirm('This cannot be undone. Are you absolutely sure?');\" /></form></div>"
        )
    } else {
        String::new()
    };
    let mut set_rows = String::new();
    for (idx, r) in settings.iter().enumerate() {
        let sid = i(r, "id").unwrap_or(0);
        let v = web::setting_value(r);
        let txt = match &v {
            Value::String(x) => x.clone(),
            Value::Null => String::new(),
            x => x.to_string(),
        };
        let edit_td = if is_dev { format!("<a href=\"/portalSetting/edit/{sid}\">Edit</a>") } else { String::new() };
        set_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{edit_td}</td></tr>",
            idx + 1,
            esc(&s(r, "name")),
            esc(&s(r, "type")),
            esc(&txt)
        ));
    }
    let settings_sec = format!(
        "<h3>Settings</h3><div class=\"nav\" role=\"navigation\"><ul>{set_nav}</ul></div>{set_box}\
         <table class='table'><tr><th>#</th><th>Name</th><th>Type</th><th>Value</th><th>Action</th></tr>{set_rows}</table><br/>"
    );

    // Schedules section
    let sch_nav = if is_dev { format!("<li><a class=\"create\" href=\"/portalScheduler/create?module={en}\">Add Schedule</a></li>") } else { String::new() };
    let mut sch_rows = String::new();
    for (idx, r) in schedulers.iter().enumerate() {
        let sid = i(r, "id").unwrap_or(0);
        let hr = { let h = s(r, "hour"); if h.trim().is_empty() { s(r, "hour_of_day") } else { h } };
        let enb = if b(r, "enabled") { "<span class=\"badge bg-success\">Yes</span>" } else { "<span class=\"badge bg-secondary\">No</span>" };
        let edit_td = if is_dev { format!("&nbsp;&nbsp;<a href=\"/portalScheduler/edit/{sid}\">Edit</a>") } else { String::new() };
        sch_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{enb}</td><td>{}</td><td><a href=\"/portalScheduler/show/{sid}\">View</a>{edit_td}</td></tr>",
            idx + 1,
            esc(&s(r, "name")),
            esc(&s(r, "slugs")),
            esc(&hr),
            esc(&s(r, "day_of_week")),
            esc(&s(r, "day_of_month")),
            esc(&s(r, "lastrun").chars().take(16).collect::<String>())
        ));
    }
    if schedulers.is_empty() {
        sch_rows.push_str("<tr><td colspan=\"9\">No schedules in this module.</td></tr>");
    }
    let sched_sec = format!(
        "<h3>Schedules</h3><div class=\"nav\" role=\"navigation\"><ul>{sch_nav}</ul></div>\
         <table class='table js-collapse' data-collapse-rows='20' data-collapse-label='schedules'>\
         <tr><th>#</th><th>Name</th><th>Pages</th><th>Hour</th><th title=\"0 is Sunday\">Day Of Week</th><th>Day Of Month</th><th>Enabled</th><th>Last Run</th><th>Action</th></tr>{sch_rows}</table><br/>"
    );

    // Trees section
    let tree_nav = if is_adm {
        format!(
            "<li><a class=\"create\" href=\"/portalTree/create?module={en}\">Add Tree</a></li>\
             <li><a class=\"create\" href=\"/portalTree/importtreeform\">Import Tree</a></li>"
        )
    } else {
        String::new()
    };
    let mut tree_rows = String::new();
    for (idx, r) in trees.iter().enumerate() {
        let tid = i(r, "id").unwrap_or(0);
        tree_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><a href=\"/portalTree/show/{tid}\">View</a>&nbsp;&nbsp;<a href=\"/portalTree/exporttree/{tid}\">Export</a></td></tr>",
            idx + 1,
            esc(&s(r, "name")),
            esc(&s(r, "root_name")),
            s(r, "nodes"),
            esc(&s(r, "valid").chars().take(10).collect::<String>()),
            esc(&s(r, "expire").chars().take(10).collect::<String>())
        ));
    }
    if trees.is_empty() {
        tree_rows.push_str("<tr><td colspan=\"7\">No trees in this module.</td></tr>");
    }
    let trees_sec = format!(
        "<h3>Trees</h3><div class=\"nav\" role=\"navigation\"><ul>{tree_nav}</ul></div>\
         <table class='table js-collapse' data-collapse-rows='20' data-collapse-label='trees'>\
         <tr><th>#</th><th>Name</th><th>Root</th><th>Nodes</th><th>Valid From</th><th>Expires</th><th>Action</th></tr>{tree_rows}</table><br/>"
    );

    // Endpoints section (superusers only)
    let ep_sec = if is_admin {
        let mut ep_rows = String::new();
        for (idx, r) in endpoints.iter().enumerate() {
            let eid = i(r, "id").unwrap_or(0);
            let enb = if b(r, "enabled") { "<span class=\"badge bg-success\">Yes</span>" } else { "<span class=\"badge bg-secondary\">No</span>" };
            ep_rows.push_str(&format!(
                "<tr><td>{}</td><td><code>/svc/{}/{}</code></td><td>{}</td><td><code>{}</code></td><td>{}</td><td>{enb}</td><td><a href=\"/portalEndpoint/show/{eid}\">View</a></td></tr>",
                idx + 1,
                esc(&s(r, "module")),
                esc(&s(r, "slug")),
                esc(&s(r, "handler_type")),
                esc(&s(r, "target")),
                esc(&s(r, "auth_mode"))
            ));
        }
        if endpoints.is_empty() {
            ep_rows.push_str("<tr><td colspan=\"7\">No endpoints in this module.</td></tr>");
        }
        format!(
            "<h3>Endpoints</h3><div class=\"nav\" role=\"navigation\"><ul><li><a class=\"create\" href=\"/portalEndpoint/create?module={en}\">Add Endpoint</a></li></ul></div>\
             <table class='table js-collapse' data-collapse-rows='20' data-collapse-label='endpoints'>\
             <tr><th>#</th><th>URL</th><th>Handler</th><th>Target</th><th>Auth</th><th>Enabled</th><th>Action</th></tr>{ep_rows}</table><br/>"
        )
    } else {
        "<h3>Endpoints</h3><p class=\"small\">Endpoints are managed by system administrators.</p><br/>".to_string()
    };

    // Import History section
    let imp_sec = if is_dev {
        let mut irows = String::new();
        for (idx, r) in importlogs.iter().enumerate() {
            let iid = i(r, "id").unwrap_or(0);
            irows.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{} ({})</td><td>{}</td><td><a href=\"/portalModule/importlog/{iid}\">View Changes</a></td></tr>",
                idx + 1,
                esc(&s(r, "date_created").chars().take(19).collect::<String>()),
                esc(&s(r, "staffname")),
                esc(&s(r, "staffid")),
                esc(&s(r, "remarks"))
            ));
        }
        format!(
            "<h3>Import History</h3><div class=\"nav\" role=\"navigation\"><ul><li><a class=\"list\" href=\"/portalModule/importlogs?module={en}\">View All Import Logs</a></li></ul></div>\
             <table class='table'><tr><th>#</th><th>Date</th><th>Imported By</th><th>Remarks</th><th>Action</th></tr>{irows}</table><br/>"
        )
    } else {
        String::new()
    };

    // Admins & Developers lists
    let admins_html: String = roles
        .iter()
        .filter(|r| s(r, "role") == "Admin")
        .map(|r| format!("<li><a href=\"/userRole/show/{}\">{}</a></li>", i(r, "id").unwrap_or(0), esc(&s(r, "name"))))
        .collect();
    let devs_html: String = roles
        .iter()
        .filter(|r| s(r, "role") == "Developer")
        .map(|r| format!("<li><a href=\"/userRole/show/{}\">{}</a></li>", i(r, "id").unwrap_or(0), esc(&s(r, "name"))))
        .collect();

    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };

    let collapse_js = r#"<script type="text/javascript">
(function () {
    function collapsibleTables() {
        var tables = document.querySelectorAll('table.js-collapse');
        Array.prototype.forEach.call(tables, function (table) {
            var limit = parseInt(table.getAttribute('data-collapse-rows'), 10) || 20;
            var label = table.getAttribute('data-collapse-label') || 'rows';
            var rows = Array.prototype.filter.call(table.rows, function (r) {
                return r.getElementsByTagName('td').length > 0;
            });
            if (rows.length <= limit) return;
            var hidden = rows.slice(limit);
            var expanded = false;
            var toggle = document.createElement('button');
            toggle.type = 'button';
            toggle.className = 'btn btn-sm btn-secondary';
            toggle.style.marginTop = '4px';
            var holder = document.createElement('div');
            holder.appendChild(toggle);
            table.parentNode.insertBefore(holder, table.nextSibling);
            function render() {
                hidden.forEach(function (r) { r.style.display = expanded ? '' : 'none'; });
                toggle.textContent = expanded
                    ? 'Show first ' + limit + ' ' + label
                    : 'Show all ' + rows.length + ' ' + label + ' (' + hidden.length + ' hidden)';
            }
            toggle.addEventListener('click', function () { expanded = !expanded; render(); });
            render();
        });
    }
    if (document.readyState === 'loading') { document.addEventListener('DOMContentLoaded', collapsibleTables); }
    else { collapsibleTables(); }
})();
</script>"#;

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#show-portalModule" class="skip" tabindex="-1">Skip to content&hellip;</a>{nav}</section>
<section class="row"><div id="show-portalModule" class="col-12 content scaffold-show" role="main">
{top_bar}
<h1>Show PortalModule</h1>
{flash_html}
{register_card}
{files_sec}
{trackers_sec}
{pages_sec}
{roles_sec}
{settings_sec}
{sched_sec}
{trees_sec}
{ep_sec}
{imp_sec}
<h3>Admins</h3>
{admins_html}
<h3>Developers</h3>
{devs_html}
<br/>
{bot_bar}
</div></section></div></div>
{collapse_js}"##
    );
    ctx.html("Show PortalModule", body).await
}

async fn choice_field(db: &Db, name: &str, value: &str, setting_name: &str, maxlength: usize) -> String {
    let opts: Vec<String> = web::setting_str(db, setting_name)
        .await
        .map(|raw| raw.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
        .unwrap_or_default();
    if opts.is_empty() {
        return format!("<input type=\"text\" name=\"{name}\" id=\"{name}\" value=\"{}\" maxlength=\"{maxlength}\"/>", esc(value));
    }
    let mut o = format!("<select name=\"{name}\" id=\"{name}\"><option value=\"\"></option>");
    if !value.is_empty() && !opts.iter().any(|x| x == value) {
        o.push_str(&format!("<option value=\"{}\" selected>{}</option>", esc(value), esc(value)));
    }
    for op in opts {
        o.push_str(&format!(
            "<option value=\"{}\"{}>{}</option>",
            esc(&op),
            if op == value { " selected" } else { "" },
            esc(&op)
        ));
    }
    o.push_str("</select>");
    o
}

async fn create_or_edit(mut ctx: Ctx, id: Option<i64>) -> Response {
    let db = &ctx.st.db.clone();
    let existing = match id {
        Some(mid) => match db.get("portal_module", mid).await {
            Ok(Some(m)) => {
                if !is_dev_of(&ctx, &s(&m, "name")).await {
                    return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to edit this").await;
                }
                Some(m)
            }
            _ => return ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        None => {
            if !ctx.is_admin() {
                return ctx.status(StatusCode::FORBIDDEN, "Administrator rights required").await;
            }
            None
        }
    };
    let is_new = existing.is_none();
    let m = existing.unwrap_or_default();
    let name_val = s(&m, "name");
    let title_val = s(&m, "title");
    let status_val = s(&m, "status");
    let cat_val = s(&m, "category");
    let dept_val = s(&m, "department");
    let desc_val = s(&m, "description");
    let ben_val = s(&m, "benefit");

    let owners = match id {
        Some(mid) => contacts_by_role(db, mid, "Owner").await,
        None => vec![],
    };
    let maintainers = match id {
        Some(mid) => contacts_by_role(db, mid, "Maintainer").await,
        None => vec![],
    };

    let name_input = if is_new {
        format!(
            "<input type=\"text\" name=\"name\" id=\"name\" value=\"{}\" required/>\
             <small class=\"pm-help\">Lowercase key used by every page, tracker, role and setting, e.g. <code>cbmy_srms</code>. It cannot be changed later.</small>",
            esc(&name_val)
        )
    } else {
        format!("<input type=\"text\" name=\"name\" id=\"name\" value=\"{}\" readonly/>", esc(&name_val))
    };

    let mut st_opts = String::from("<option value=\"\">Not recorded</option>");
    for st in STATUSES {
        st_opts.push_str(&format!("<option value=\"{st}\"{}>{st}</option>", if status_val == *st { " selected" } else { "" }));
    }
    let cat_input = choice_field(db, "category", &cat_val, "portal.module_categories", 100).await;
    let dept_input = choice_field(db, "department", &dept_val, "portal.module_departments", 255).await;

    let owner_opts: String = owners
        .iter()
        .map(|u| format!("<option value=\"{}\" selected>{}</option>", i(u, "id").unwrap_or(0), esc(&s(u, "name"))))
        .collect();
    let maint_opts: String = maintainers
        .iter()
        .map(|u| format!("<option value=\"{}\" selected>{}</option>", i(u, "id").unwrap_or(0), esc(&s(u, "name"))))
        .collect();

    let action = match id {
        Some(mid) => format!("/portalModule/update/{mid}"),
        None => "/portalModule/save".to_string(),
    };
    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let heading = if is_new { "Create PortalModule" } else { "Edit PortalModule" };
    let kind = if is_new { "create" } else { "edit" };
    let btn_label = if is_new { "Create" } else { "Update" };

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#{kind}-portalModule" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalModule/index">PortalModule List</a></li>
  {new_li}
</ul></div></section>
<section class="row"><div id="{kind}-portalModule" class="col-12 content scaffold-{kind}" role="main">
  <h1>{heading}</h1>
  {flash_html}
  <form action="{action}" method="post">
    {csrf}
    <fieldset class="form pm-form">
      <div class="fieldcontain required">
        <label for="name">Name (key) <span class="required-indicator">*</span></label>
        {name_input}
      </div>
      <div class="fieldcontain">
        <label for="title">Title</label>
        <input type="text" name="title" id="title" value="{title_val}" maxlength="255" placeholder="Readable name shown to people"/>
      </div>
      <div class="fieldcontain">
        <label for="status">Status</label>
        <select name="status" id="status">{st_opts}</select>
      </div>
      <div class="fieldcontain">
        <label for="category">Category</label>
        {cat_input}
      </div>
      <div class="fieldcontain">
        <label for="department">Department</label>
        {dept_input}
      </div>
      <input type="hidden" name="contactsSubmitted" value="1"/>
      <div class="fieldcontain" id="ownerIds_div">
        <label for="ownerIds">Owners</label>
        <select class="ajax-select" data-url="/user/lookup" name="ownerIds" id="ownerIds" multiple style="width: 60%;">{owner_opts}</select>
        <small class="pm-help">Accountable for the module; access reviews go to them.</small>
      </div>
      <div class="fieldcontain" id="maintainerIds_div">
        <label for="maintainerIds">Maintainers</label>
        <select class="ajax-select" data-url="/user/lookup" name="maintainerIds" id="maintainerIds" multiple style="width: 60%;">{maint_opts}</select>
        <small class="pm-help">Build and support it.</small>
      </div>
      <div class="fieldcontain">
        <label for="description">Description</label>
        <textarea name="description" id="description" rows="4" maxlength="4000" placeholder="What the module does">{desc_val}</textarea>
      </div>
      <div class="fieldcontain">
        <label for="benefit">Benefit</label>
        <textarea name="benefit" id="benefit" rows="4" maxlength="4000" placeholder="What it gives the business">{ben_val}</textarea>
      </div>
    </fieldset>
    <fieldset class="buttons">
      <input class="save" type="submit" value="{btn_label}" />
    </fieldset>
  </form>
  <style>
    .pm-form textarea {{ width: 60%; }}
    .pm-form .pm-help {{ display: block; margin-left: 25%; color: #6b7280; font-size: .8rem; }}
    @media (max-width: 760px) {{ .pm-form .pm-help {{ margin-left: 0; }} .pm-form textarea {{ width: 100%; }} }}
  </style>
  {form_js}
</div></section></div></div>"##,
        new_li = if !is_new && ctx.is_admin() { "<li><a class=\"create\" href=\"/portalModule/create\">New PortalModule</a></li>" } else { "" },
        title_val = esc(&title_val),
        desc_val = esc(&desc_val),
        ben_val = esc(&ben_val),
        form_js = crate::tags::form_js()
    );
    ctx.html(heading, body).await
}

fn param_ids(ctx: &Ctx, key: &str) -> Vec<i64> {
    let mut out = match ctx.params.get(key) {
        Some(Value::Array(a)) => a.iter().filter_map(|v| vs(v).trim().parse::<i64>().ok()).collect::<Vec<_>>(),
        Some(v) => vs(v).trim().parse::<i64>().ok().into_iter().collect(),
        None => vec![],
    };
    let mut seen = HashSet::new();
    out.retain(|x| seen.insert(*x));
    out
}

async fn replace_contacts(db: &Db, module_id: i64, role: &str, wanted: &[i64]) {
    let current = db
        .rows("select id, user_id from portal_module_contact where module_id=:m and role=:r", &row! {"m"=>module_id,"r"=>role})
        .await
        .unwrap_or_default();
    for c in &current {
        let uid = i(c, "user_id").unwrap_or(0);
        if !wanted.contains(&uid) {
            if let Some(cid) = i(c, "id") {
                let _ = db.delete("portal_module_contact", cid).await;
            }
        }
    }
    let cur_uids: HashSet<i64> = current.iter().filter_map(|c| i(c, "user_id")).collect();
    let now = now_str();
    for uid in wanted {
        if !cur_uids.contains(uid) {
            let _ = db
                .insert(
                    "portal_module_contact",
                    &row! {"module_id"=>module_id,"user_id"=>*uid,"role"=>role,"date_created"=>now.clone(),"last_updated"=>now.clone()},
                )
                .await;
        }
    }
}

async fn save_module(mut ctx: Ctx, id: Option<i64>) -> Response {
    if ctx.method != "POST" {
        return ctx.status(StatusCode::FORBIDDEN, "POST required").await;
    }
    let db = &ctx.st.db.clone();
    let existing = match id {
        Some(mid) => match db.get("portal_module", mid).await {
            Ok(Some(m)) => {
                if !is_dev_of(&ctx, &s(&m, "name")).await {
                    return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
                }
                Some(m)
            }
            _ => return ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        None => {
            if !ctx.is_admin() {
                return ctx.status(StatusCode::FORBIDDEN, "Administrator rights required").await;
            }
            None
        }
    };
    let name = match &existing {
        Some(m) => s(m, "name"), // read-only once created
        None => ctx.p("name").trim().to_string(),
    };
    if name.is_empty() {
        ctx.flash("Name is required");
        return ctx.redirect("/portalModule/create").await;
    }
    let opt = |k: &str| -> Value {
        let v = ctx.p(k);
        let t = v.trim();
        if t.is_empty() { Value::Null } else { json!(t) }
    };
    let data = row! {
        "name"=>name,
        "title"=>opt("title"),
        "status"=>opt("status"),
        "category"=>opt("category"),
        "department"=>opt("department"),
        "description"=>opt("description"),
        "benefit"=>opt("benefit")
    };
    let res = match id {
        Some(mid) => db.update("portal_module", mid, &data).await.map(|_| mid),
        None => db.insert("portal_module", &data).await,
    };
    let mid = match res {
        Ok(x) => x,
        Err(e) => {
            ctx.flash(&format!("Could not save: {}", e));
            return ctx.redirect("/portalModule/index").await;
        }
    };
    if !ctx.p("contactsSubmitted").is_empty() {
        let owner_ids = param_ids(&ctx, "ownerIds");
        let maint_ids = param_ids(&ctx, "maintainerIds");
        replace_contacts(db, mid, "Owner", &owner_ids).await;
        replace_contacts(db, mid, "Maintainer", &maint_ids).await;
    }
    ctx.flash(if id.is_some() { "Module updated" } else { "Module created" });
    ctx.redirect(&format!("/portalModule/show/{mid}")).await
}

async fn delete_or_op(mut ctx: Ctx, id: i64) -> Response {
    if ctx.method != "POST" {
        return ctx.status(StatusCode::FORBIDDEN, "POST required").await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(m)) = db.get("portal_module", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let name = s(&m, "name");
    let op = ctx.p("op");
    let file_on = ctx.params.contains_key("files");
    let user_on = ctx.params.contains_key("user");
    let tree_on = ctx.params.contains_key("trees");
    let menu_on = ctx.params.contains_key("menus");
    let ep_on = ctx.params.contains_key("endpoints");

    if op == "Export" {
        if !is_dev_of(&ctx, &name).await {
            return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
        }
        let dir = folder(&ctx.st, &name);
        let opts = ExportOpts { files: file_on, roles: user_on, trees: tree_on, menus: menu_on, endpoints: ep_on };
        return match export_to(&ctx.st, &name, &dir, opts).await {
            Ok(()) => {
                let d2 = dir.clone();
                match tokio::task::spawn_blocking(move || zip_dir(&d2)).await {
                    Ok(Ok(bytes)) => ctx.bytes("application/zip", Some(&format!("{}.zip", name.replace(' ', "_"))), bytes, false).await,
                    _ => ctx.status(StatusCode::INTERNAL_SERVER_ERROR, "Could not build the zip").await,
                }
            }
            Err(e) => {
                ctx.flash(&format!("Export failed: {}", e));
                ctx.redirect(&format!("/portalModule/show/{id}")).await
            }
        };
    }
    if op == "Import" {
        if !ctx.is_admin() {
            return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
        }
        let mut qs = vec![];
        if file_on {
            qs.push("files=on");
        }
        if user_on {
            qs.push("staff=on");
        }
        if ep_on {
            qs.push("endpoints=on");
        }
        let suffix = if qs.is_empty() { String::new() } else { format!("?{}", qs.join("&")) };
        return ctx.redirect(&format!("/portalModule/importpreview/{id}{suffix}")).await;
    }
    // Delete
    if !ctx.is_admin() {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let _ = db.exec("delete from portal_module_contact where module_id=:m", &row! {"m"=>id}).await;
    let _ = db.exec("delete from portal_module_access_review_log where review_id in (select id from portal_module_access_review where module_id=:m)", &row! {"m"=>id}).await;
    let _ = db.exec("delete from portal_module_access_review where module_id=:m", &row! {"m"=>id}).await;
    let _ = db.delete("portal_module", id).await;
    ctx.flash(&format!("PortalModule {} deleted", name));
    ctx.redirect("/portalModule/index").await
}

async fn updatedball(mut ctx: Ctx, id: i64) -> Response {
    let db = &ctx.st.db.clone();
    let Ok(Some(m)) = db.get("portal_module", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let name = s(&m, "name");
    if !is_dev_of(&ctx, &name).await || ctx.method != "POST" {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let count_cols = |table: String, db: Db| async move {
        if table.is_empty() {
            return 0i64;
        }
        db.table_columns(&table).await.map(|c| c.len() as i64).unwrap_or(0)
    };
    let trackers = db
        .rows("select id, slug, name from portal_tracker where module=:m order by slug", &row! {"m"=>name.clone()})
        .await
        .unwrap_or_default();
    let mut results = vec![];
    let mut total_added: i64 = 0;
    let mut failed_slugs = vec![];
    for r in &trackers {
        let Some(t) = Tracker::by_id(db, i(r, "id").unwrap_or(0)).await else { continue };
        let mut tables = vec![t.data_table()];
        if t.tracker_type == "Tracker" {
            tables.push(t.trail_table());
        }
        let mut before = 0i64;
        for tb in &tables {
            before += count_cols(tb.clone(), db.clone()).await;
        }
        let res: anyhow::Result<()> = async {
            t.updatedb(db).await?;
            t.create_index(db).await
        }
        .await;
        let (ok, after, err) = match res {
            Ok(()) => {
                let mut a = 0i64;
                for tb in &tables {
                    a += count_cols(tb.clone(), db.clone()).await;
                }
                if a > before {
                    total_added += a - before;
                }
                (true, a, Value::Null)
            }
            Err(e) => {
                failed_slugs.push(s(r, "slug"));
                web::log_error(db, "portalModule", "updatedball", &e.to_string(), Some(&name), Some(&s(r, "slug")), ctx.uid()).await;
                (false, before, json!(e.to_string()))
            }
        };
        results.push(json!({"name": s(r, "name"), "slug": s(r, "slug"), "before": before, "after": after, "ok": ok, "error": err}));
    }
    ctx.sess.insert("updatedb_results".into(), Value::Array(results));
    let mut msg = format!(
        "Update DB ran on {} tracker(s) of {}: {}",
        trackers.len(),
        name,
        if total_added > 0 { format!("{} column(s) added", total_added) } else { "nothing was missing".into() }
    );
    if !failed_slugs.is_empty() {
        msg.push_str(&format!("; {} FAILED ({}) - see the Trackers section", failed_slugs.len(), failed_slugs.join(", ")));
    }
    msg.push('.');
    ctx.flash(&msg);
    ctx.redirect(&format!("/portalModule/show/{id}")).await
}

async fn importform(mut ctx: Ctx) -> Response {
    let Some(u) = ctx.user.clone() else { return ctx.need_login().await };
    if ctx.method == "POST" {
        let up = ctx.files.remove("fileupload").or_else(|| ctx.files.remove("file"));
        let Some(up) = up else {
            ctx.flash("No file selected");
            return ctx.redirect("/portalModule/importform").await;
        };
        if !up.filename.to_ascii_lowercase().ends_with(".zip") {
            ctx.flash("Only ZIP files are allowed for module imports");
            return ctx.redirect("/portalModule/importform").await;
        }
        if up.size > 50 * 1024 * 1024 {
            ctx.flash("File too large. Maximum size is 50MB.");
            return ctx.redirect("/portalModule/importform").await;
        }
        let module_dir = PathBuf::from(&ctx.st.cfg.module_dir);
        let filename = up.filename.clone();
        let bytes = up.read_all().await.unwrap_or_default();
        let extracted = tokio::task::spawn_blocking(move || unzip_module(&module_dir, &filename, &bytes)).await;
        let mod_name = match extracted {
            Ok(Ok((m, _))) => m,
            Ok(Err(e)) => {
                ctx.flash(&format!("Error importing module: {}", e));
                return ctx.redirect("/portalModule/importform").await;
            }
            Err(e) => {
                ctx.flash(&format!("Error importing module: {}", e));
                return ctx.redirect("/portalModule/importform").await;
            }
        };
        // Check authorization if module already exists
        let existing = ctx.db().first("select id from portal_module where name=:n", &row! {"n"=>mod_name.clone()}).await.ok().flatten();
        let mod_id = if let Some(ex) = existing {
            if !is_dev_of(&ctx, &mod_name).await {
                ctx.flash("You are not authorized to update that module");
                return ctx.redirect("/portalModule/importform").await;
            }
            i(&ex, "id").unwrap_or(0)
        } else {
            let mid = ctx.db().insert("portal_module", &row! {"name"=>mod_name.clone()}).await.unwrap_or(0);
            if let Some(uid) = i(&u, "id") {
                let has_dev = ctx
                    .db()
                    .count("select count(*) from user_role where user_id=:u and module=:m and role='Developer'", &row! {"u"=>uid,"m"=>mod_name.clone()})
                    .await
                    .unwrap_or(0)
                    > 0;
                if !has_dev {
                    let _ = ctx.db().insert("user_role", &row! {"user_id"=>uid,"module"=>mod_name.clone(),"role"=>"Developer"}).await;
                }
            }
            mid
        };
        return ctx.redirect(&format!("/portalModule/importpreview/{mod_id}?files=on&staff=on&endpoints=on")).await;
    }

    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#create-importmodule" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul><li><a class="home" href="/portalModule/index">Home</a></li></ul></div></section>
<section class="row"><div id="create-importmodule" class="col-12 content scaffold-create" role="main">
  <h1>Import Module</h1>
  {flash_html}
  <form method="POST" action="/portalModule/importform" enctype="multipart/form-data">
    {csrf}
    <fieldset class="form">
      <div class="fieldcontain">
        <label for="fileupload">Module File</label>
        <input type="file" name="fileupload" id="fileupload" accept=".zip" required />
      </div>
    </fieldset>
    <fieldset class="buttons">
      <input type="submit" name="create" class="save" value="Create" />
    </fieldset>
  </form>
</div></section></div></div>"##
    );
    ctx.html("Create Import Module", body).await
}

async fn generate_import_diff(ctx: &Ctx, module_name: &str, opts: ExportOpts) -> anyhow::Result<String> {
    let incoming = folder(&ctx.st, module_name);
    let tmp = std::env::temp_dir().join(format!("rp_diff_{}_{}", module_name, uuid::Uuid::new_v4()));
    let res = async {
        export_to(&ctx.st, module_name, &tmp, opts).await?;
        let d1 = tmp.clone();
        let d2 = incoming.clone();
        Ok(tokio::task::spawn_blocking(move || diff_module_folders(&d1, &d2)).await.unwrap_or_default())
    }
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
    res
}

async fn importpreview(mut ctx: Ctx, id: i64) -> Response {
    let db = &ctx.st.db.clone();
    let Ok(Some(m)) = db.get("portal_module", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let name = s(&m, "name");
    if !is_dev_of(&ctx, &name).await {
        ctx.flash("You are not authorized to update that module");
        return ctx.redirect("/").await;
    }
    let dir = folder(&ctx.st, &name);
    if !dir.is_dir() {
        ctx.flash(&format!("No migration files found for module {}", name));
        return ctx.redirect(&format!("/portalModule/show/{id}")).await;
    }

    let file_on = !ctx.p("files").is_empty() && dir.join("filelinklist.json").is_file();
    let staff_on = !ctx.p("staff").is_empty() && (dir.join("userrolelist.json").is_file() || dir.join("userroles.json").is_file());
    let tree_on = dir.join("treelist.json").is_file();
    let menu_on = dir.join("menulist.json").is_file();
    let endpoints_present = dir.join("endpointlist.json").is_file();
    let endpoints_on = !ctx.p("endpoints").is_empty() && endpoints_present && ctx.is_admin();

    let diff = match generate_import_diff(&ctx, &name, ExportOpts { files: file_on, roles: staff_on, trees: tree_on, menus: menu_on, endpoints: endpoints_present }).await {
        Ok(d) => d,
        Err(e) => {
            ctx.flash(&format!("Error generating import diff: {}", e));
            return ctx.redirect(&format!("/portalModule/show/{id}")).await;
        }
    };
    let settings = preview_settings(db, &name, &dir).await;
    let schedulers = preview_schedulers(db, &name, &dir).await;
    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };

    let diff_html = if diff.trim().is_empty() {
        "<div class=\"message\" role=\"status\">No changes detected between the current module and the migration files.</div>".to_string()
    } else {
        diff_view_html(&diff)
    };

    let menu_box = if menu_on {
        "<div class=\"fieldcontain\" style=\"margin-bottom:1em;\">\
         <label for=\"menus\"><input type=\"checkbox\" name=\"menus\" id=\"menus\" value=\"on\" checked=\"checked\"/> Add this module's menu entries</label>\
         <span class=\"property-value\">Puts the module in the header megamenu and gives its pages a sidebar. Entries this module added before are updated or removed to match; anything you added by hand is left alone.</span></div>"
    } else {
        ""
    };
    let endpoints_box = if endpoints_present && ctx.is_admin() {
        let chk = if endpoints_on { " checked=\"checked\"" } else { "" };
        format!(
            "<div class=\"fieldcontain\" style=\"margin-bottom:1em;\">\
             <label for=\"endpoints\"><input type=\"checkbox\" name=\"endpoints\" id=\"endpoints\" value=\"on\"{chk}/> Import this module's HTTP endpoints</label>\
             <span class=\"property-value\">Updates this module's endpoints from <code>endpointlist.json</code>. Untick to keep existing endpoints configured on this server.</span></div>"
        )
    } else {
        String::new()
    };

    let settings_html = if !settings.is_empty() {
        let ch = settings.iter().filter(|x| x.status == "changed").count();
        let nw = settings.iter().filter(|x| x.status == "new").count();
        let sm = settings.iter().filter(|x| x.status == "same").count();
        let mut srows = String::new();
        for (idx, st) in settings.iter().enumerate() {
            let cur = if st.status == "new" { "<em style=\"color:#666;\">not set</em>".to_string() } else { esc(&st.current) };
            let act = match st.status {
                "changed" => format!(
                    "<input type=\"hidden\" name=\"settingkey_{idx}\" value=\"{}\"/>\
                     <label style=\"display:block;\"><input type=\"radio\" name=\"settingchoice_{idx}\" value=\"keep\" checked=\"checked\"/> Keep current value</label>\
                     <label style=\"display:block;\"><input type=\"radio\" name=\"settingchoice_{idx}\" value=\"import\"/> Use value from migration files</label>",
                    esc(&st.key)
                ),
                "new" => "<span style=\"color:#1a7f37;\">Will be created</span>".into(),
                _ => "<span style=\"color:#666;\">No change</span>".into(),
            };
            srows.push_str(&format!(
                "<tr><td><strong>{}</strong><br/><span style=\"color:#666;font-size:0.85em;\">{}</span></td>\
                 <td style=\"word-break:break-word;\">{cur}</td><td style=\"word-break:break-word;\">{}</td><td>{act}</td></tr>",
                esc(&st.name),
                esc(&st.module),
                esc(&st.incoming)
            ));
        }
        format!(
            "<h2>Settings</h2><p>{} setting(s) in the migration files: <strong>{ch}</strong> would change, <strong>{nw}</strong> new, <strong>{sm}</strong> already identical.</p>\
             <table class=\"table\" style=\"width:100%;\"><thead><tr><th style=\"width:22%;\">Setting</th><th style=\"width:29%;\">Current value on this server</th><th style=\"width:29%;\">Value in migration files</th><th style=\"width:20%;\">Action</th></tr></thead><tbody>{srows}</tbody></table>",
            settings.len()
        )
    } else {
        String::new()
    };

    let sched_html = if !schedulers.is_empty() {
        let ch = schedulers.iter().filter(|x| x.status == "changed").count();
        let nw = schedulers.iter().filter(|x| x.status == "new").count();
        let sm = schedulers.iter().filter(|x| x.status == "same").count();
        let mut srows = String::new();
        for (idx, sc) in schedulers.iter().enumerate() {
            let cur = if sc.status == "new" { "<em style=\"color:#666;\">not scheduled here</em>".to_string() } else { esc(&sc.current) };
            let act = match sc.status {
                "changed" => format!(
                    "<input type=\"hidden\" name=\"schedulerkey_{idx}\" value=\"{}\"/>\
                     <label style=\"display:block;\"><input type=\"radio\" name=\"schedulerchoice_{idx}\" value=\"keep\" checked=\"checked\"/> Keep current schedule</label>\
                     <label style=\"display:block;\"><input type=\"radio\" name=\"schedulerchoice_{idx}\" value=\"import\"/> Use schedule from migration files</label>",
                    esc(&sc.key)
                ),
                "new" => "<span style=\"color:#1a7f37;\">Will be created</span>".into(),
                _ => "<span style=\"color:#666;\">No change</span>".into(),
            };
            let lr = if sc.lastrun.is_empty() { String::new() } else { format!("<br/><span style=\"color:#666;font-size:0.85em;\">last run {}</span>", esc(&sc.lastrun)) };
            srows.push_str(&format!(
                "<tr><td><strong>{}</strong><br/><span style=\"color:#666;font-size:0.85em;\">{}</span>{lr}</td>\
                 <td style=\"word-break:break-word;\">{cur}</td><td style=\"word-break:break-word;\">{}</td><td>{act}</td></tr>",
                esc(&sc.name),
                esc(&sc.module),
                esc(&sc.incoming)
            ));
        }
        format!(
            "<h2>Scheduled Jobs</h2><p>{} scheduled job(s) in the migration files: <strong>{ch}</strong> would change, <strong>{nw}</strong> new, <strong>{sm}</strong> already identical.</p>\
             <table class=\"table\" style=\"width:100%;\"><thead><tr><th style=\"width:22%;\">Job</th><th style=\"width:29%;\">Current schedule on this server</th><th style=\"width:29%;\">Schedule in migration files</th><th style=\"width:20%;\">Action</th></tr></thead><tbody>{srows}</tbody></table>",
            schedulers.len()
        )
    } else {
        String::new()
    };

    let qn = esc(&name).replace('\'', "\\'");
    let body = format!(
        r#"<div id="content" role="main"><div class="container">
<section class="row"><div class="nav" role="navigation"><ul>
  <li><a class="home" href="/portalModule/index">Home</a></li>
  <li><a class="list" href="/portalModule/show/{id}">Back to Module</a></li>
</ul></div></section>
<section class="row"><div class="col-12 content" role="main">
  <h1>Import Preview: {ename}</h1>
  {flash_html}
  <p>Review the changes below before importing. Lines starting with <span style="color:#cf222e;">-</span> show the current state that will be replaced, lines starting with <span style="color:#1a7f37;">+</span> show the incoming changes.
  <br/>Importing files: <strong>{f_yn}</strong>, staff roles: <strong>{s_yn}</strong>, trees: <strong>{t_yn}</strong>{tree_note}, endpoints: <strong>{e_yn}</strong></p>
  {diff_html}
  <form action="/portalModule/confirmimport/{id}" method="POST">
    {csrf}
    {f_hid}{s_hid}
    {menu_box}
    {endpoints_box}
    {settings_html}
    {sched_html}
    <fieldset class="form">
      <div class="fieldcontain required">
        <label for="remarks">Import Remarks<span class="required-indicator">*</span></label>
        <textarea name="remarks" id="remarks" rows="4" cols="80" required="required" style="width:100%;" placeholder="Describe the changes being imported"></textarea>
      </div>
    </fieldset>
    <fieldset class="buttons">
      <input class="delete" type="submit" value="Confirm Import" onclick="return confirm('Import module {qn}? The changes shown above will be applied.');" />
      <a class="list" href="/portalModule/show/{id}">Cancel</a>
    </fieldset>
  </form>
</div></section></div></div>"#,
        ename = esc(&name),
        f_yn = if file_on { "Yes" } else { "No" },
        s_yn = if staff_on { "Yes" } else { "No" },
        t_yn = if tree_on { "Yes" } else { "No" },
        e_yn = if endpoints_on { "Yes" } else { "No" },
        tree_note = if tree_on && !staff_on { " (structure only, no role holders)" } else { "" },
        f_hid = if file_on { "<input type=\"hidden\" name=\"files\" value=\"on\"/>" } else { "" },
        s_hid = if staff_on { "<input type=\"hidden\" name=\"staff\" value=\"on\"/>" } else { "" },
    );
    ctx.html(&format!("Import Preview - {}", name), body).await
}

async fn confirmimport(mut ctx: Ctx, id: i64) -> Response {
    if ctx.method != "POST" {
        return ctx.redirect(&format!("/portalModule/show/{id}")).await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(m)) = db.get("portal_module", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let name = s(&m, "name");
    if !is_dev_of(&ctx, &name).await {
        ctx.flash("You are not authorized to update that module");
        return ctx.redirect("/").await;
    }
    if ctx.sess.contains_key("adminlink") {
        ctx.flash("Restore your own account before importing a module");
        return ctx.redirect(&format!("/portalModule/show/{id}")).await;
    }

    let dir = folder(&ctx.st, &name);
    let file_on = !ctx.p("files").is_empty() && dir.join("filelinklist.json").is_file();
    let staff_on = !ctx.p("staff").is_empty() && (dir.join("userrolelist.json").is_file() || dir.join("userroles.json").is_file());
    let tree_on = dir.join("treelist.json").is_file();
    let menu_present = dir.join("menulist.json").is_file();
    let menu_on = !ctx.p("menus").is_empty() && menu_present;
    let endpoints_present = dir.join("endpointlist.json").is_file();
    let endpoints_wanted = !ctx.p("endpoints").is_empty() && endpoints_present;
    let endpoints_on = ctx.is_admin() && endpoints_wanted;
    let endpoints_skipped_perm = endpoints_present && !ctx.is_admin();
    let endpoints_skipped_unticked = endpoints_present && ctx.is_admin() && !endpoints_wanted;

    let mut setting_choices = HashMap::new();
    let mut scheduler_choices = HashMap::new();
    for (k, v) in &ctx.params {
        if let Some(idx) = k.strip_prefix("settingkey_") {
            let choice = ctx.p(&format!("settingchoice_{idx}"));
            if !choice.is_empty() {
                setting_choices.insert(vs(v), choice);
            }
        } else if let Some(idx) = k.strip_prefix("schedulerkey_") {
            let choice = ctx.p(&format!("schedulerchoice_{idx}"));
            if !choice.is_empty() {
                scheduler_choices.insert(vs(v), choice);
            }
        }
    }
    let mut kept_set: Vec<String> = setting_choices.iter().filter(|(_, v)| v.as_str() == "keep").map(|(k, _)| k.clone()).collect();
    kept_set.sort();
    let mut kept_sch: Vec<String> = scheduler_choices.iter().filter(|(_, v)| v.as_str() == "keep").map(|(k, _)| k.clone()).collect();
    kept_sch.sort();

    let mut difftext = generate_import_diff(&ctx, &name, ExportOpts { files: file_on, roles: staff_on, trees: tree_on, menus: menu_present, endpoints: endpoints_on })
        .await
        .unwrap_or_default();
    if !kept_set.is_empty() {
        difftext = format!("Settings kept at their existing values (not imported):\n  {}\n\n{}", kept_set.join("\n  "), difftext);
    }
    if !kept_sch.is_empty() {
        difftext = format!("Schedules kept as they are on this server (not imported):\n  {}\n\n{}", kept_sch.join("\n  "), difftext);
    }
    if endpoints_skipped_perm {
        difftext = format!("Endpoints in this package were NOT imported: only a system administrator can bring them in.\n\n{}", difftext);
    }
    if endpoints_skipped_unticked {
        difftext = format!("Endpoints in this package were NOT imported, because the endpoints box was unticked.\n\n{}", difftext);
    }
    if menu_present && !menu_on {
        difftext = format!("Menu entries in this package were NOT imported, because the menu box was unticked.\n\n{}", difftext);
    }

    let opts = ImportOpts {
        files: file_on,
        roles: staff_on,
        trees: tree_on,
        menus: menu_on,
        endpoints: endpoints_on,
        setting_choices,
        scheduler_choices,
    };
    match import_from_opts(&ctx.st, &name, &dir, &opts).await {
        Ok((msg, detail)) => {
            if !detail.is_empty() {
                difftext = format!("{}\n\nNotes:\n{}", difftext, detail);
            }
            let remarks = { let r = ctx.p("remarks"); if r.trim().is_empty() { msg.clone() } else { r.trim().to_string() } };
            let _ = db
                .insert(
                    "portal_module_import_log",
                    &row! {
                        "module"=>name,
                        "staffid"=>ctx.user.as_ref().map(|u| s(u, "userid")),
                        "staffname"=>ctx.user.as_ref().map(|u| s(u, "name")),
                        "remarks"=>remarks,
                        "diff"=>difftext,
                        "date_created"=>now_str()
                    },
                )
                .await;
            crate::cache::SETTINGS.clear();
            let mut notes = vec![];
            if !kept_set.is_empty() {
                notes.push(format!("{} setting(s) kept at existing value", kept_set.len()));
            }
            if !kept_sch.is_empty() {
                notes.push(format!("{} schedule(s) kept as already set here", kept_sch.len()));
            }
            if endpoints_skipped_perm {
                notes.push("endpoints not imported - a system administrator has to do that".into());
            }
            if endpoints_skipped_unticked {
                notes.push("endpoints kept as already set here".into());
            }
            if menu_present && !menu_on {
                notes.push("menu entries not added".into());
            }
            ctx.flash(&if notes.is_empty() { msg } else { format!("{} ({})", msg, notes.join("; ")) });
        }
        Err(e) => {
            web::log_error(db, "portalModule", "confirmimport", &e.to_string(), Some(&name), None, ctx.uid()).await;
            ctx.flash(&format!("Error importing module: {}", e));
        }
    }
    ctx.redirect(&format!("/portalModule/show/{id}")).await
}

async fn importlogs(ctx: Ctx) -> Response {
    let db = &ctx.st.db.clone();
    let mfilter = ctx.p("module");
    if !mfilter.is_empty() && !is_dev_of(&ctx, &mfilter).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let max = ctx.pi("max").unwrap_or(25).clamp(1, 100);
    let offset = ctx.pi("offset").unwrap_or(0).max(0);
    let (total, logs, mod_row) = if !mfilter.is_empty() {
        let p = row! {"m"=>mfilter.clone()};
        let tot = db.count("select count(*) from portal_module_import_log where module=:m", &p).await.unwrap_or(0);
        let ls = db
            .rows(&format!("select * from portal_module_import_log where module=:m order by date_created desc, id desc limit {} offset {}", max, offset), &p)
            .await
            .unwrap_or_default();
        let mr = db.first("select * from portal_module where name=:m", &p).await.ok().flatten();
        (tot, ls, mr)
    } else {
        let tot = db.count("select count(*) from portal_module_import_log", &db::no_params()).await.unwrap_or(0);
        let ls = db
            .rows(&format!("select * from portal_module_import_log order by date_created desc, id desc limit {} offset {}", max, offset), &db::no_params())
            .await
            .unwrap_or_default();
        (tot, ls, None)
    };

    let mut nav = String::from(
        "<div class=\"nav\" role=\"navigation\"><ul>\
         <li><a class=\"home\" href=\"/\">Home</a></li>\
         <li><a class=\"list\" href=\"/portalModule/index\">Module List</a></li>",
    );
    if let Some(ref mr) = mod_row {
        nav.push_str(&format!(
            "<li><a class=\"list\" href=\"/portalModule/show/{}\">Back to Module</a></li>\
             <li><a class=\"list\" href=\"/portalModule/importlogs\">All Import Logs</a></li>",
            i(mr, "id").unwrap_or(0)
        ));
    }
    nav.push_str("</ul></div>");

    let mut rows_html = String::new();
    for (idx, l) in logs.iter().enumerate() {
        let lid = i(l, "id").unwrap_or(0);
        let lm = s(l, "module");
        rows_html.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td><a href=\"/portalModule/importlogs?module={}\">{}</a></td><td>{} ({})</td><td>{}</td><td><a href=\"/portalModule/importlog/{lid}\">View Changes</a></td></tr>",
            offset + idx as i64 + 1,
            esc(&s(l, "date_created").chars().take(19).collect::<String>()),
            enc(&lm),
            esc(&lm),
            esc(&s(l, "staffname")),
            esc(&s(l, "staffid")),
            esc(&s(l, "remarks"))
        ));
    }
    let empty_msg = if logs.is_empty() {
        format!(
            "<div class=\"message\" role=\"status\">No import logs found{}.</div>",
            if mfilter.is_empty() { String::new() } else { format!(" for module {}", esc(&mfilter)) }
        )
    } else {
        String::new()
    };
    let pag = if total > max {
        format!("<div class=\"pagination\">{}</div>", crate::tags::paginate("/portalModule/importlogs", &ctx.params, total, max, offset))
    } else {
        String::new()
    };
    let heading = if mfilter.is_empty() { "Import Logs".into() } else { format!("Import Logs: {}", mfilter) };
    let body = format!(
        "<div id=\"content\" role=\"main\"><div class=\"container\"><section class=\"row\">{nav}</section>\
         <section class=\"row\"><div class=\"col-12 content scaffold-list\" role=\"main\"><h1>{}</h1>\
         <table class='table'><tr><th>#</th><th>Date</th><th>Module</th><th>Imported By</th><th>Remarks</th><th>Action</th></tr>{rows_html}</table>\
         {empty_msg}{pag}</div></section></div></div>",
        esc(&heading)
    );
    ctx.html(&heading, body).await
}

async fn importlog_show(ctx: Ctx, id: i64) -> Response {
    let db = &ctx.st.db.clone();
    let Ok(Some(log)) = db.get("portal_module_import_log", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let mname = s(&log, "module");
    if !is_dev_of(&ctx, &mname).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let mr = db.first("select id from portal_module where name=:m", &row! {"m"=>mname.clone()}).await.ok().flatten();
    let back_li = mr.and_then(|r| i(&r, "id")).map(|mid| format!("<li><a class=\"list\" href=\"/portalModule/show/{mid}\">Back to Module</a></li>")).unwrap_or_default();
    let diff = s(&log, "diff");
    let diff_html = if diff.trim().is_empty() {
        "<div class=\"message\" role=\"status\">No changes were detected for this import.</div>".into()
    } else {
        diff_view_html(&diff)
    };
    let body = format!(
        "<div id=\"content\" role=\"main\"><div class=\"container\"><section class=\"row\">\
         <div class=\"nav\" role=\"navigation\"><ul><li><a class=\"home\" href=\"/\">Home</a></li>{back_li}\
         <li><a class=\"list\" href=\"/portalModule/importlogs?module={}\">Module Import Logs</a></li></ul></div></section>\
         <section class=\"row\"><div class=\"col-12 content\" role=\"main\">\
         <h1>Import Log: {}</h1>\
         <table class='table'>\
         <tr><th>Date</th><td>{}</td></tr>\
         <tr><th>Imported By</th><td>{} ({})</td></tr>\
         <tr><th>Remarks</th><td>{}</td></tr>\
         </table><h3>Changes</h3>{diff_html}</div></section></div></div>",
        enc(&mname),
        esc(&mname),
        esc(&s(&log, "date_created").chars().take(19).collect::<String>()),
        esc(&s(&log, "staffname")),
        esc(&s(&log, "staffid")),
        esc(&s(&log, "remarks"))
    );
    ctx.html(&format!("Import Log - {}", mname), body).await
}

// ---------------- User Roles & Settings Excel Export / Import / Bulk Delete ----------------

async fn export_user_roles(ctx: Ctx, id: i64) -> Response {
    let db = &ctx.st.db.clone();
    let Ok(Some(m)) = db.get("portal_module", id).await else { return ctx.status(StatusCode::NOT_FOUND, "Not found").await };
    let name = s(&m, "name");
    if !is_dev_of(&ctx, &name).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let roles = db
        .rows(
            "select u.userid, u.name, r.role from user_role r join portal_user u on u.id=r.user_id where r.module=:m order by lower(u.name), r.role",
            &row! {"m"=>name.clone()},
        )
        .await
        .unwrap_or_default();
    let tmp = std::env::temp_dir().join(format!("rp_roles_{}.xlsx", uuid::Uuid::new_v4()));
    let built: anyhow::Result<Vec<u8>> = (|| {
        let mut x = crate::xlsxstream::XlsxStream::create(tmp.to_str().unwrap_or(""), "User Roles", vec!["User ID".into(), "Name".into(), "Role".into()])?;
        for r in &roles {
            x.write_row(&[json!(s(r, "userid")), json!(s(r, "name")), json!(s(r, "role"))])?;
        }
        x.finish()?;
        let data = std::fs::read(&tmp)?;
        let _ = std::fs::remove_file(&tmp);
        Ok(data)
    })();
    match built {
        Ok(bytes) => {
            ctx.bytes("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", Some(&format!("user_roles_{}.xlsx", name)), bytes, false)
                .await
        }
        Err(e) => ctx.status(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).await,
    }
}

async fn import_user_roles(mut ctx: Ctx, id: i64) -> Response {
    let db = &ctx.st.db.clone();
    let Ok(Some(m)) = db.get("portal_module", id).await else { return ctx.status(StatusCode::NOT_FOUND, "Not found").await };
    let name = s(&m, "name");
    if !is_dev_of(&ctx, &name).await || ctx.method != "POST" {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let Some(up) = ctx.files.remove("userRoleFile") else {
        ctx.flash("Please select a file to import");
        return ctx.redirect(&format!("/portalModule/show/{id}")).await;
    };
    let tmp = std::env::temp_dir().join(format!("rp_imp_roles_{}.xlsx", uuid::Uuid::new_v4()));
    if let Err(e) = up.save_to(&tmp).await {
        ctx.flash(&format!("Error saving upload: {}", e));
        return ctx.redirect(&format!("/portalModule/show/{id}")).await;
    }
    let sheet_res = crate::upload::read_sheet(tmp.to_str().unwrap_or(""), 0);
    let _ = std::fs::remove_file(&tmp);
    let rows = match sheet_res {
        Ok(r) => r,
        Err(e) => {
            ctx.flash(&format!("Error parsing Excel file: {}", e));
            return ctx.redirect(&format!("/portalModule/show/{id}")).await;
        }
    };
    let (mut ok, mut errs_cnt, mut errors) = (0usize, 0usize, vec![]);
    for (idx, line) in rows.iter().enumerate().skip(1) {
        let uid_str = line.first().map(vs).unwrap_or_default().trim().to_string();
        let role_name = line.get(2).map(vs).unwrap_or_default().trim().to_string();
        if uid_str.is_empty() || role_name.is_empty() {
            continue;
        }
        match web::find_user_by_staffid(db, &uid_str).await {
            Some(u) => {
                let uid = i(&u, "id").unwrap_or(0);
                let exists = db
                    .count("select count(*) from user_role where user_id=:u and module=:m and role=:r", &row! {"u"=>uid,"m"=>name.clone(),"r"=>role_name.clone()})
                    .await
                    .unwrap_or(0)
                    > 0;
                if exists {
                    ok += 1;
                } else {
                    match db.insert("user_role", &row! {"user_id"=>uid,"module"=>name.clone(),"role"=>role_name.clone()}).await {
                        Ok(_) => ok += 1,
                        Err(_) => {
                            errs_cnt += 1;
                            errors.push(format!("Row {}: Failed to save role {} for {}", idx + 1, role_name, uid_str));
                        }
                    }
                }
            }
            None => {
                errs_cnt += 1;
                errors.push(format!("Row {}: User with User ID {} not found", idx + 1, uid_str));
            }
        }
    }
    ctx.flash(&format!("Import completed. Success: {}, Errors: {}", ok, errs_cnt));
    if !errors.is_empty() {
        ctx.sess.insert("flash_errors".into(), json!(errors));
    }
    ctx.redirect(&format!("/portalModule/show/{id}")).await
}

async fn delete_all_user_roles(mut ctx: Ctx, id: i64) -> Response {
    let db = &ctx.st.db.clone();
    let Ok(Some(m)) = db.get("portal_module", id).await else { return ctx.status(StatusCode::NOT_FOUND, "Not found").await };
    let name = s(&m, "name");
    if !is_dev_of(&ctx, &name).await || ctx.method != "POST" {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let deleted = db.exec("delete from user_role where module=:m and role != 'Developer'", &row! {"m"=>name.clone()}).await.unwrap_or(0);
    ctx.flash(&format!("Deleted {} user role(s) for module '{}'", deleted, name));
    ctx.redirect(&format!("/portalModule/show/{id}")).await
}

async fn export_settings(ctx: Ctx, id: i64) -> Response {
    let db = &ctx.st.db.clone();
    let Ok(Some(m)) = db.get("portal_module", id).await else { return ctx.status(StatusCode::NOT_FOUND, "Not found").await };
    let name = s(&m, "name");
    if !is_dev_of(&ctx, &name).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let settings = db.rows("select * from portal_setting where module=:m order by name", &row! {"m"=>name.clone()}).await.unwrap_or_default();
    let tmp = std::env::temp_dir().join(format!("rp_settings_{}.xlsx", uuid::Uuid::new_v4()));
    let built: anyhow::Result<Vec<u8>> = (|| {
        let mut x = crate::xlsxstream::XlsxStream::create(
            tmp.to_str().unwrap_or(""),
            "Settings",
            vec!["Name".into(), "Type".into(), "Datum Type".into(), "Text".into(), "Date Value".into(), "Number".into()],
        )?;
        for r in &settings {
            let dv: String = s(r, "date_value").chars().take(10).collect();
            let num = i(r, "number").map(|n| n.to_string()).unwrap_or_default();
            x.write_row(&[json!(s(r, "name")), json!(s(r, "type")), json!(s(r, "datum_type")), json!(s(r, "text")), json!(dv), json!(num)])?;
        }
        x.finish()?;
        let data = std::fs::read(&tmp)?;
        let _ = std::fs::remove_file(&tmp);
        Ok(data)
    })();
    match built {
        Ok(bytes) => {
            ctx.bytes("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", Some(&format!("settings_{}.xlsx", name)), bytes, false)
                .await
        }
        Err(e) => ctx.status(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).await,
    }
}

async fn import_settings(mut ctx: Ctx, id: i64) -> Response {
    let db = &ctx.st.db.clone();
    let Ok(Some(m)) = db.get("portal_module", id).await else { return ctx.status(StatusCode::NOT_FOUND, "Not found").await };
    let name = s(&m, "name");
    if !is_dev_of(&ctx, &name).await || ctx.method != "POST" {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let Some(up) = ctx.files.remove("settingsFile") else {
        ctx.flash("Please select a file to import");
        return ctx.redirect(&format!("/portalModule/show/{id}")).await;
    };
    let tmp = std::env::temp_dir().join(format!("rp_imp_settings_{}.xlsx", uuid::Uuid::new_v4()));
    if let Err(e) = up.save_to(&tmp).await {
        ctx.flash(&format!("Error saving upload: {}", e));
        return ctx.redirect(&format!("/portalModule/show/{id}")).await;
    }
    let sheet_res = crate::upload::read_sheet(tmp.to_str().unwrap_or(""), 0);
    let _ = std::fs::remove_file(&tmp);
    let rows = match sheet_res {
        Ok(r) => r,
        Err(e) => {
            ctx.flash(&format!("Error parsing Excel file: {}", e));
            return ctx.redirect(&format!("/portalModule/show/{id}")).await;
        }
    };
    let (mut ok, mut errs_cnt, mut errors) = (0usize, 0usize, vec![]);
    for (idx, line) in rows.iter().enumerate().skip(1) {
        let cell = |c: usize| -> String { line.get(c).map(vs).unwrap_or_default().trim().to_string() };
        let sname = cell(0);
        if sname.is_empty() {
            continue;
        }
        let stype = cell(1);
        let dtype = cell(2);
        let text = cell(3);
        let dstr = cell(4);
        let nstr = cell(5);
        let data = row! {
            "module"=>name.clone(),
            "name"=>sname.clone(),
            "type"=>if stype.is_empty() { Value::Null } else { json!(stype) },
            "datum_type"=>if dtype.is_empty() { Value::Null } else { json!(dtype) },
            "text"=>if text.is_empty() { Value::Null } else { json!(text) },
            "date_value"=>if dstr.is_empty() { Value::Null } else { json!(dstr) },
            "number"=>nstr.parse::<i64>().map(|n| json!(n)).unwrap_or(Value::Null)
        };
        let existing = db.first("select id from portal_setting where module=:m and name=:n", &row! {"m"=>name.clone(),"n"=>sname.clone()}).await.ok().flatten();
        let res = match existing.and_then(|r| i(&r, "id")) {
            Some(sid) => db.update("portal_setting", sid, &data).await.map(|_| sid),
            None => db.insert("portal_setting", &data).await,
        };
        match res {
            Ok(_) => ok += 1,
            Err(e) => {
                errs_cnt += 1;
                errors.push(format!("Row {}: Failed to save setting '{}': {}", idx + 1, sname, e));
            }
        }
    }
    crate::cache::SETTINGS.clear();
    ctx.flash(&format!("Settings import completed. Success: {}, Errors: {}", ok, errs_cnt));
    if !errors.is_empty() {
        ctx.sess.insert("flash_errors".into(), json!(errors));
    }
    ctx.redirect(&format!("/portalModule/show/{id}")).await
}

async fn delete_all_settings(mut ctx: Ctx, id: i64) -> Response {
    let db = &ctx.st.db.clone();
    let Ok(Some(m)) = db.get("portal_module", id).await else { return ctx.status(StatusCode::NOT_FOUND, "Not found").await };
    let name = s(&m, "name");
    if !is_dev_of(&ctx, &name).await || ctx.method != "POST" {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let deleted = db.exec("delete from portal_setting where module=:m", &row! {"m"=>name.clone()}).await.unwrap_or(0);
    crate::cache::SETTINGS.clear();
    ctx.flash(&format!("Deleted {} setting(s) for module '{}'", deleted, name));
    ctx.redirect(&format!("/portalModule/show/{id}")).await
}
