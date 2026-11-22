//! PortalTracker admin views & tab actions matching g6ikram's
//! `PortalTrackerController`, `PortalTrackerFieldController`, and `views/portalTracker/*.gsp`.

use crate::db::{self, i, lit, s, Db, Row};
use crate::entity::FIELD_TYPES;
use crate::row;
use crate::tracker::Tracker;
use crate::tracker_web;
use crate::upload;
use crate::web::{self, enc, esc, Ctx};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;

async fn dev_tracker(ctx: &Ctx, id: i64) -> Option<Tracker> {
    let t = Tracker::by_id(ctx.db(), id).await?;
    let u = ctx.user.as_ref()?;
    if ctx.is_admin() || web::developer_list(ctx.db(), u).await.contains(&t.module) {
        Some(t)
    } else {
        None
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

fn bool_label(r: &Row, col: &str) -> &'static str {
    match r.get(col) {
        Some(Value::Bool(true)) => "True",
        Some(Value::Bool(false)) => "False",
        _ => "",
    }
}

fn trunc(v: &str, max: usize) -> String {
    if v.chars().count() > max {
        format!("{}&hellip;", esc(&v.chars().take(max).collect::<String>()))
    } else {
        esc(v)
    }
}

// ---------------- /portalTracker/index ----------------

pub async fn index(ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let is_admin = ctx.is_admin();
    let u = ctx.user.as_ref().unwrap();
    let my_mods = if is_admin { vec![] } else { web::developer_list(db, u).await };
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
        cond.push(format!("(name ilike {l} or slug ilike {l} or \"module\" ilike {l})", l = like));
    }
    let wh = if cond.is_empty() { String::new() } else { format!(" where {}", cond.join(" and ")) };
    let max = ctx.pi("max").unwrap_or(10).clamp(1, 500);
    let offset = ctx.pi("offset").unwrap_or(0).max(0);
    let sort_p = ctx.p("sort");
    let order_dir = if ctx.p("order") == "desc" { "desc" } else { "asc" };
    const COLS: &[(&str, &str)] = &[("module", "Module"), ("slug", "Slug"), ("name", "Name"), ("tracker_type", "Tracker Type")];
    let order_sql = if COLS.iter().any(|(c, _)| *c == sort_p) {
        format!("\"{}\" {}, id asc", sort_p, order_dir)
    } else {
        "\"module\" asc, slug asc".to_string()
    };

    let total = db.count(&format!("select count(*) from portal_tracker{}", wh), &db::no_params()).await.unwrap_or(0);
    let rows = db
        .rows(&format!("select * from portal_tracker{} order by {} limit {} offset {}", wh, order_sql, max, offset), &db::no_params())
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

    let qs_keep = web::qs(&ctx.params, &["sort", "order", "offset", "max"]);
    let mut thead = String::new();
    for (col, lbl) in COLS {
        let sorted = sort_p == *col;
        let next_ord = if sorted && order_dir == "asc" { "desc" } else { "asc" };
        let cls = if sorted { format!("sortable sorted {}", order_dir) } else { "sortable".into() };
        let extra_q = if qs_keep.is_empty() { String::new() } else { format!("&amp;{}", esc(&qs_keep)) };
        thead.push_str(&format!(
            "<th class=\"{cls}\"><a href=\"/portalTracker/index?sort={col}&amp;max={max}&amp;order={next_ord}{extra_q}\">{}</a></th>",
            esc(lbl)
        ));
    }

    let mut tbody = String::new();
    for (rn, r) in rows.iter().enumerate() {
        let tid = i(r, "id").unwrap_or(0);
        let row_cls = if rn % 2 == 0 { "even" } else { "odd" };
        tbody.push_str(&format!(
            "<tr class=\"{row_cls}\">\
             <td><a href=\"/portalTracker/show/{tid}\">{}</a>&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             </tr>",
            esc(&s(r, "module")),
            esc(&s(r, "slug")),
            esc(&s(r, "name")),
            esc(&s(r, "tracker_type"))
        ));
    }

    let pag = if total > max {
        format!("<div class=\"pagination\">{}</div>", crate::tags::paginate("/portalTracker/index", &ctx.params, total, max, offset))
    } else {
        String::new()
    };
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let create_href = if !m.is_empty() && m != "All" {
        format!("/portalTracker/create?module={}", enc(&m))
    } else {
        "/portalTracker/create".into()
    };

    let body = format!(
        r##"<style>
@media only screen and (max-width: 760px), (min-device-width: 768px) and (max-device-width: 1024px) {{
  td:nth-of-type(1):before {{ content: "Module"; }}
  td:nth-of-type(2):before {{ content: "Slug"; }}
  td:nth-of-type(3):before {{ content: "Name"; }}
  td:nth-of-type(4):before {{ content: "Trackertype"; }}
}}
</style>
<div id="content" role="main"><div class="container">
<section class="row"><a href="#list-portalTracker" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="create" href="{create_href}">New PortalTracker</a></li>
  <li><a class="create" href="/portalTrackerData/index">Data Dump</a></li>
</ul></div></section>
<section class="row"><div id="list-portalTracker" class="col-12 content scaffold-list" role="main">
  <h1>PortalTracker List</h1>
  {flash_html}
  <form action="/portalTracker/index" method="get" name="searchform" id="searchform">
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
    ctx.html("PortalTracker List", body).await
}

// ---------------- /portalTracker/show/{id} (11 Bootstrap tabs) ----------------

async fn lookup_name(db: &Db, table: &str, col: &str, id: Option<i64>) -> String {
    let Some(rid) = id else { return String::new() };
    db.get(table, rid).await.ok().flatten().map(|r| s(&r, col)).unwrap_or_else(|| rid.to_string())
}

pub async fn show(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let Some(t) = Tracker::by_id(db, id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    if !ctx.is_admin() && !web::developer_list(db, ctx.user.as_ref().unwrap()).await.contains(&t.module) {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this").await;
    }

    let r = &t.row;
    let module = t.module.clone();
    let slug = t.slug.clone();

    // ---- Tab 1: Details ----
    let init_st_html = match i(r, "initial_status_id") {
        Some(sid) => format!("<a href=\"/portalTrackerStatus/show/{sid}\">{}</a>", esc(&lookup_name(db, "portal_tracker_status", "name", Some(sid)).await)),
        None => String::new(),
    };
    let postproc_html = match i(r, "postprocess_id") {
        Some(pid) => format!("<a href=\"/portalPage/show/{pid}\">{}</a>", esc(&lookup_name(db, db::page_t(), "slug", Some(pid)).await)),
        None => String::new(),
    };
    let def_field_html = match i(r, "defaultfield_id") {
        Some(fid) => format!("<a href=\"/portalTrackerField/show/{fid}\">{}</a>", esc(&lookup_name(db, "portal_tracker_field", "name", Some(fid)).await)),
        None => String::new(),
    };

    let mut details_li = String::new();
    let mut add_li = |col: &str, label: &str, val_html: String| {
        details_li.push_str(&format!(
            "<li class=\"fieldcontain\"><span id=\"{col}-label\" class=\"property-label\">{label}</span>\
             <div class=\"property-value\" aria-labelledby=\"{col}-label\">{val_html}</div></li>"
        ));
    };
    add_li("name", "Name", esc(&s(r, "name")));
    add_li("module", "Module", esc(&module));
    add_li("slug", "Slug", esc(&slug));
    add_li("tracker_type", "Tracker Type", esc(&s(r, "tracker_type")));
    add_li("initial_status", "Initial Status", init_st_html);
    add_li("listfields", "Listfields", esc(&s(r, "listfields")));
    add_li("allowedroles", "Allowedroles", esc(&s(r, "allowedroles")));
    add_li("hiddenlistfields", "Hiddenlistfields", esc(&s(r, "hiddenlistfields")));
    add_li("excelfields", "Excelfields", esc(&s(r, "excelfields")));
    add_li("searchfields", "Searchfields", esc(&s(r, "searchfields")));
    add_li("filterfields", "Filterfields", esc(&s(r, "filterfields")));
    add_li("row_validation_fields", "Row Validation Fields", esc(&s(r, "row_validation_fields")));
    add_li("allowadd", "Allowadd", bool_label(r, "allowadd").into());
    add_li("downloadexcel", "Downloadexcel", bool_label(r, "downloadexcel").into());
    add_li("excel_audit", "Excel Audit", bool_label(r, "excel_audit").into());
    add_li("defaultsort", "Defaultsort", esc(&s(r, "defaultsort")));
    add_li("defaultlimit", "Defaultlimit", i(r, "defaultlimit").map(|x| x.to_string()).unwrap_or_default());
    add_li("rolesort", "Rolesort", esc(&s(r, "rolesort")));
    add_li("tickactions", "Tickactions", esc(&s(r, "tickactions")));
    add_li("actionbuttons", "Actionbuttons", esc(&s(r, "actionbuttons")));
    add_li("anonymous_list", "Anonymous List", bool_label(r, "anonymous_list").into());
    add_li("anonymous_view", "Anonymous View", bool_label(r, "anonymous_view").into());
    add_li("require_login", "Require Login", bool_label(r, "require_login").into());
    add_li("condition_q", "Condition Q", esc(&s(r, "condition_q")));
    add_li("rowclassval", "Rowclassval", esc(&s(r, "rowclassval")));
    add_li("datatable", "Datatable", esc(&s(r, "datatable")));
    add_li("trailtable", "Trailtable", esc(&s(r, "trailtable")));
    add_li("postprocess", "Postprocess", postproc_html);
    add_li("defaultfield", "Defaultfield", def_field_html);
    add_li("side_menu", "Side Menu", esc(&s(r, "side_menu")));

    let csrf = ctx.csrf_field();
    let tab_details = format!(
        "<ol class=\"property-list portalTracker\">{details_li}</ol>\
         <form action=\"/portalTracker/delete/{id}\" method=\"post\">{csrf}\
           <fieldset class=\"buttons\">\
             <a class=\"edit\" href=\"/portalTracker/edit/{id}\">Edit</a>\
             <input class=\"delete\" type=\"submit\" value=\"Delete\" onclick=\"return confirm('Are you sure?');\" />\
           </fieldset>\
         </form>"
    );

    // ---- Tab 2: Fields (_fieldsList.gsp) ----
    let mut fields_sorted = t.fields.clone();
    fields_sorted.sort_by(|a, b| s(a, "name").to_lowercase().cmp(&s(b, "name").to_lowercase()));
    let mut field_rows = String::new();
    for (idx, f) in fields_sorted.iter().enumerate() {
        let fid = i(f, "id").unwrap_or(0);
        let cls = if idx % 2 == 0 { "even" } else { "odd" };
        field_rows.push_str(&format!(
            "<tr class=\"{cls}\">\
             <td><a href=\"/portalTrackerField/show/{fid}\">{}</a>&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             </tr>",
            esc(&s(f, "name")),
            esc(&s(f, "label")),
            esc(&s(f, "field_type")),
            trunc(&s(f, "field_options"), 80),
            trunc(&s(f, "field_default"), 80),
            esc(&s(f, "row_validation_regex")),
            i(f, "field_order").map(|x| x.to_string()).unwrap_or_default()
        ));
    }
    let tab_fields = format!(
        r##"<section class="row"><div class="nav" role="navigation"><ul>
  <li><a class="create" href="/portalTrackerField/create?tracker_id={id}">Create Field</a></li>
  <li><a class="create" href="/portalTrackerField/excelTemplate/{id}">Excel Template</a></li>
  <li><a class="create" href="/portalTrackerField/multifields/{id}">Multi Fields</a></li>
  <li><a class="create" href="/portalTrackerField/fromTable/{id}" onclick="return confirm('Import from table?');">From Table</a></li>
  <li><a class="create" href="/portalTrackerField/updateDb/{id}">Update DB</a></li>
  <li><a class="create" href="/portalTrackerField/createIndex/{id}">Create Index</a></li>
  <li><a class="create" href="/portalTracker/fix_file_links/{id}">Fix FileLinks</a></li>
  <li><a class="create" href="/portalTracker/fix_trail_file_links/{id}">Fix Trail FileLinks</a></li>
  <li><a class="create" href="/portalTracker/convert_text_columns/{id}" onclick="return confirm('Convert this tracker\'s text columns to varchar(max)?');">Text &rarr; varchar(max)</a></li>
</ul></div></section>
<table class="responsive"><thead><tr>
  <th>Name</th><th>Label</th><th>Field Type</th><th>Field Options</th><th>Field Default</th><th>Row Validation Regex</th><th>Field Order</th>
</tr></thead><tbody>{field_rows}</tbody></table>"##
    );

    // ---- Tab 3: Roles (_rolesList.gsp) ----
    let mut roles_sorted = t.roles.clone();
    roles_sorted.sort_by(|a, b| s(a, "name").to_lowercase().cmp(&s(b, "name").to_lowercase()));
    let mut role_rows = String::new();
    for (idx, rl) in roles_sorted.iter().enumerate() {
        let rid = i(rl, "id").unwrap_or(0);
        let cls = if idx % 2 == 0 { "even" } else { "odd" };
        role_rows.push_str(&format!(
            "<tr class=\"{cls}\">\
             <td><a href=\"/portalTrackerRole/show/{rid}\">{}</a>&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             </tr>",
            esc(&s(rl, "name")),
            esc(&s(rl, "role_type")),
            trunc(&s(rl, "role_desc"), 90),
            esc(&fmt_dt(&s(rl, "last_updated")))
        ));
    }
    let tab_roles = format!(
        "<section class=\"row\"><div class=\"nav\" role=\"navigation\"><ul>\
         <li><a class=\"create\" href=\"/portalTrackerRole/create?tracker_id={id}\">Create Role</a></li>\
         </ul></div></section>\
         <table class=\"responsive\"><thead><tr><th>Name</th><th>Role Type</th><th>Role Desc</th><th>Last Updated</th></tr></thead><tbody>{role_rows}</tbody></table>"
    );

    // ---- Tab 4: Status (_statusList.gsp) ----
    let mut statuses_sorted = t.statuses.clone();
    statuses_sorted.sort_by(|a, b| s(a, "name").to_lowercase().cmp(&s(b, "name").to_lowercase()));
    let mut status_names: HashMap<i64, String> = HashMap::new();
    let mut status_rows = String::new();
    for (idx, st_row) in statuses_sorted.iter().enumerate() {
        let sid = i(st_row, "id").unwrap_or(0);
        status_names.insert(sid, s(st_row, "name"));
        let cls = if idx % 2 == 0 { "even" } else { "odd" };
        let run_html = match i(st_row, "runonupdate_id") {
            Some(pid) => format!("<a href=\"/portalPage/show/{pid}\">{}</a>", esc(&lookup_name(db, db::page_t(), "slug", Some(pid)).await)),
            None => String::new(),
        };
        status_rows.push_str(&format!(
            "<tr class=\"{cls}\">\
             <td><a href=\"/portalTrackerStatus/show/{sid}\">{}</a>&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{run_html}&nbsp;</td>\
             </tr>",
            esc(&s(st_row, "name")),
            i(st_row, "flow").map(|x| x.to_string()).unwrap_or_default(),
            bool_label(st_row, "actiontransitions"),
            esc(&s(st_row, "composite_statuses"))
        ));
    }
    let tab_status = format!(
        "<section class=\"row\"><div class=\"nav\" role=\"navigation\"><ul>\
         <li><a class=\"create\" href=\"/portalTrackerStatus/create?tracker_id={id}\">Create Status</a></li>\
         <li><a class=\"create\" href=\"/portalTracker/create_default_pages/{id}?category=display\">Create Default Display Page</a></li>\
         </ul></div></section>\
         <table class=\"responsive\"><thead><tr><th>Name</th><th>Flow</th><th>Actiontransitions</th><th>Composite Statuses</th><th>Runonupdate</th></tr></thead><tbody>{status_rows}</tbody></table>"
    );

    // ---- Tab 5: Transitions (_transitionsList.gsp) ----
    let mut trans_sorted = t.transitions.clone();
    trans_sorted.sort_by(|a, b| s(&a.row, "name").to_lowercase().cmp(&s(&b.row, "name").to_lowercase()));
    let mut trans_names: HashMap<i64, String> = HashMap::new();
    let mut trans_rows = String::new();
    for (idx, tr) in trans_sorted.iter().enumerate() {
        let trid = tr.id;
        trans_names.insert(trid, s(&tr.row, "name"));
        let cls = if idx % 2 == 0 { "even" } else { "odd" };
        let rnames = &tr.roles;
        let pnames: Vec<String> = tr.prev.iter().filter_map(|sid| status_names.get(sid).cloned()).collect();
        let next_html = match i(&tr.row, "next_status_id") {
            Some(nsid) => {
                let nm = status_names.get(&nsid).cloned().unwrap_or_else(|| nsid.to_string());
                format!("<a href=\"/portalTrackerStatus/show/{nsid}\">{}</a>", esc(&nm))
            }
            None => String::new(),
        };
        let pp_html = match i(&tr.row, "postprocess_id") {
            Some(pid) => format!("<a href=\"/portalPage/show/{pid}\">{}</a>", esc(&lookup_name(db, db::page_t(), "slug", Some(pid)).await)),
            None => String::new(),
        };
        trans_rows.push_str(&format!(
            "<tr class=\"{cls}\">\
             <td><a href=\"/portalTrackerTransition/show/{trid}\">{trid}</a>&nbsp;</td>\
             <td><a href=\"/portalTrackerTransition/show/{trid}\">{}</a>&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{next_html}&nbsp;</td>\
             <td>{pp_html}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             </tr>",
            esc(&s(&tr.row, "name")),
            esc(&rnames.join(", ")),
            esc(&pnames.join(", ")),
            trunc(&s(&tr.row, "displayfields"), 70),
            trunc(&s(&tr.row, "editfields"), 70)
        ));
    }
    let tab_transitions = format!(
        "<section class=\"row\"><div class=\"nav\" role=\"navigation\"><ul>\
         <li><a class=\"create\" href=\"/portalTrackerTransition/create?tracker_id={id}\">Create Transition</a></li>\
         <li><a class=\"create\" href=\"/portalTracker/create_default_pages/{id}?category=form\">Create Default Transition Pages</a></li>\
         </ul></div></section>\
         <table class=\"responsive\"><thead><tr>\
         <th>Id</th><th>Name</th><th>Roles</th><th>Prev Status</th><th>Next Status</th><th>Postprocess</th><th>Displayfields</th><th>Editfields</th>\
         </tr></thead><tbody>{trans_rows}</tbody></table>"
    );

    // ---- Tab 6: Flows (_flowList.gsp) ----
    let flows = db
        .rows("select * from portal_tracker_flow where tracker_id=:tid order by lower(name)", &row! {"tid"=>id})
        .await
        .unwrap_or_default();
    let mut flow_rows = String::new();
    for (idx, fl) in flows.iter().enumerate() {
        let flid = i(fl, "id").unwrap_or(0);
        let cls = if idx % 2 == 0 { "even" } else { "odd" };
        flow_rows.push_str(&format!(
            "<tr class=\"{cls}\">\
             <td><a href=\"/portalTrackerFlow/show/{flid}\">{}</a>&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             </tr>",
            esc(&s(fl, "name")),
            trunc(&s(fl, "fields"), 80),
            trunc(&s(fl, "transitions"), 80)
        ));
    }
    let tab_flows = format!(
        "<section class=\"row\"><div class=\"nav\" role=\"navigation\"><ul>\
         <li><a class=\"create\" href=\"/portalTrackerFlow/create?tracker_id={id}\">Create Flow</a></li>\
         </ul></div></section>\
         <table class=\"responsive\"><thead><tr><th>Name</th><th>Fields</th><th>Transitions</th></tr></thead><tbody>{flow_rows}</tbody></table>"
    );

    // ---- Tab 7: Index (_indexList.gsp) ----
    let indexes = db
        .rows("select * from portal_tracker_index where tracker_id=:tid order by lower(name)", &row! {"tid"=>id})
        .await
        .unwrap_or_default();
    let mut idx_rows = String::new();
    for (idx, ix) in indexes.iter().enumerate() {
        let ixid = i(ix, "id").unwrap_or(0);
        let cls = if idx % 2 == 0 { "even" } else { "odd" };
        idx_rows.push_str(&format!(
            "<tr class=\"{cls}\">\
             <td><a href=\"/portalTrackerIndex/show/{ixid}\">{}</a>&nbsp;</td>\
             <td>{}&nbsp;</td>\
             </tr>",
            esc(&s(ix, "name")),
            esc(&s(ix, "fields"))
        ));
    }
    let tab_index = format!(
        "<section class=\"row\"><div class=\"nav\" role=\"navigation\"><ul>\
         <li><a class=\"create\" href=\"/portalTrackerIndex/create?tracker_id={id}\">Create Index</a></li>\
         </ul></div></section>\
         <table class=\"responsive\"><thead><tr><th>Name</th><th>Fields</th></tr></thead><tbody>{idx_rows}</tbody></table>"
    );

    // ---- Tab 8: Data Updates (_datasList.gsp) ----
    let datas = db
        .rows("select * from portal_tracker_data where tracker_id=:tid order by id desc limit 200", &row! {"tid"=>id})
        .await
        .unwrap_or_default();
    let mut data_rows = String::new();
    for (idx, dr) in datas.iter().enumerate() {
        let did = i(dr, "id").unwrap_or(0);
        let cls = if idx % 2 == 0 { "even" } else { "odd" };
        data_rows.push_str(&format!(
            "<tr class=\"{cls}\">\
             <td><a href=\"/portalTrackerData/show/{did}\">{}</a>&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             </tr>",
            esc(&{ let m = s(dr, "module"); if m.is_empty() { format!("#{did}") } else { m } }),
            i(dr, "upload_status").map(|x| x.to_string()).unwrap_or_default(),
            esc(&s(dr, "path")),
            i(dr, "header_start").map(|x| x.to_string()).unwrap_or_default(),
            i(dr, "header_end").map(|x| x.to_string()).unwrap_or_default(),
            i(dr, "data_row").map(|x| x.to_string()).unwrap_or_default()
        ));
    }
    let tab_datas = format!(
        r##"<section class="row"><div class="nav" role="navigation"><ul>
  <li><a class="create" href="/portalTrackerData/create?tracker_id={id}">Create Data Update</a></li>
  <li><a class="delete" href="/portalTrackerData/cleardb?tracker_id={id}" onclick="return confirm('Are you sure to clear DB?');">Clear DB</a></li>
  <li><a class="delete" href="/portalTrackerData/cleandb?tracker_id={id}" onclick="return confirm('Are you sure to clean DB?');">Clean DB</a></li>
  <li><a class="create" href="/portalTrackerData/datadump/{id}?forimport=1">Data Dump (for re-import)</a></li>
  <li><a class="create" href="/portalTrackerData/datadump/{id}">Data Dump (readable)</a></li>
  <li><a class="create" href="/portalTrackerData/syncupload/{id}">Sync Data</a></li>
</ul></div></section>
<section class="row"><div class="col-12">
  <p class="text-muted" style="font-size:12px;margin:4px 0 10px">
    <strong>For re-import</strong> is the file to feed back through Create Data Update &mdash;
    headers are field names, references carry ids, computed columns are left out.
    <strong>Readable</strong> is the one to send to a person: names instead of ids,
    and every computed column included, which on a large tracker makes it much slower.
    Both honour any filter you pass in the URL, e.g. <code>&amp;record_status=Routed</code>.
  </p>
</div></section>
<table class="responsive"><thead><tr>
  <th>Module</th><th>Upload Status</th><th>Path</th><th>Header Start</th><th>Header End</th><th>Data Row</th>
</tr></thead><tbody>{data_rows}</tbody></table>"##
    );

    // ---- Tab 9: Pages (_pagesList.gsp) ----
    let pages = db
        .rows(&format!("select * from \"{}\" where \"module\"=:m order by lower(slug)", db::page_t()), &row! {"m"=>module.clone()})
        .await
        .unwrap_or_default();
    let mut page_rows = String::new();
    for (idx, pg) in pages.iter().enumerate() {
        let pid = i(pg, "id").unwrap_or(0);
        let cls = if idx % 2 == 0 { "even" } else { "odd" };
        page_rows.push_str(&format!(
            "<tr class=\"{cls}\">\
             <td><a href=\"/portalPage/show/{pid}\">{}</a>&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             </tr>",
            esc(&s(pg, "title")),
            esc(&s(pg, "slug")),
            esc(&s(pg, "module")),
            bool_label(pg, "published"),
            bool_label(pg, "runable"),
            bool_label(pg, "requirelogin"),
            esc(&fmt_dt(&s(pg, "last_updated")))
        ));
    }
    let tab_pages = format!(
        "<section class=\"row\"><div class=\"nav\" role=\"navigation\"><ul>\
         <li><a class=\"create\" href=\"/portalPage/create?module={}&amp;tracker_id={id}\">Create Page</a></li>\
         <li><a class=\"create\" href=\"/portalTracker/create_default_pages/{id}\">Create Default Pages</a></li>\
         </ul></div>\
         <table class=\"responsive\"><thead><tr><th>Title</th><th>Slug</th><th>Module</th><th>Published</th><th>Runable</th><th>Requirelogin</th><th>Last Updated</th></tr></thead><tbody>{page_rows}</tbody></table>\
         </section>",
        enc(&module)
    );

    // ---- Tab 10: Emails (_emailList.gsp) ----
    let emails = db
        .rows("select * from portal_tracker_email where tracker_id=:tid order by lower(name)", &row! {"tid"=>id})
        .await
        .unwrap_or_default();
    let mut email_rows = String::new();
    for (idx, em) in emails.iter().enumerate() {
        let eid = i(em, "id").unwrap_or(0);
        let cls = if idx % 2 == 0 { "even" } else { "odd" };
        let tr_nm = i(em, "transition_id").and_then(|x| trans_names.get(&x).cloned()).unwrap_or_default();
        let st_nm = i(em, "status_id").and_then(|x| status_names.get(&x).cloned()).unwrap_or_default();
        email_rows.push_str(&format!(
            "<tr class=\"{cls}\">\
             <td>{}</td>\
             <td><a href=\"/portalTrackerEmail/show/{eid}\">{}</a></td>\
             <td>{}</td>\
             <td>{}</td>\
             <td>{}</td>\
             </tr>",
            idx + 1,
            esc(&s(em, "name")),
            esc(&s(em, "emailto")),
            esc(&tr_nm),
            esc(&st_nm)
        ));
    }
    let tab_emails = format!(
        "<table><thead><tr><th>#</th><th>Name</th><th>Email To</th><th>Transition</th><th>Status</th></tr></thead><tbody>{email_rows}</tbody></table>\
         <a href=\"/portalTrackerEmail/create?tracker_id={id}\">Create Email</a>"
    );

    // ---- Tab 11: Tools ----
    let tab_tools: String = fields_sorted.iter().map(|f| format!("{}<br/>", esc(&s(f, "name")))).collect();

    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#show-portalTracker" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTracker/index">PortalTracker List</a></li>
  <li><a class="create" href="/portalTracker/create">New PortalTracker</a></li>
  <li><a class="list" href="/{mod_enc}/{slug_enc}/list">View Tracker</a></li>
  <li><a class="create" href="/portalTracker/create_defaults/{id}">Create Defaults</a></li>
  <li><a class="create" href="/portalTracker/export_data/{id}">Export Data</a></li>
  <li><a class="create" href="/portalTracker/fix_status/{id}">Fix Status</a></li>
  <li><a class="create" href="/portalTracker/delete_fields/{id}" onclick="return confirm('Delete ALL fields of this tracker?');">Delete Fields</a></li>
  <li><a class="create" href="/portalTracker/workflow_graph/{id}">Workflow Graph</a></li>
</ul></div></section>
<section class="row"><div id="show-portalTracker" class="col-12 content scaffold-show" role="main">
  <h1>Show PortalTracker : {mod_esc} - {slug_esc}</h1>
  {flash_html}
  <ul class="nav nav-tabs d-flex" id="trackertabs" role="tablist">
    <li class="nav-item flex-fill" role="presentation"><button class="nav-link w-100 active" id="details-tab" data-bs-toggle="tab" data-bs-target="#details" type="button" role="tab" aria-controls="details" aria-selected="true">Details</button></li>
    <li class="nav-item flex-fill" role="presentation"><button class="nav-link w-100" id="fields-tab" data-bs-toggle="tab" data-bs-target="#fields" type="button" role="tab" aria-controls="fields" aria-selected="false">Fields</button></li>
    <li class="nav-item flex-fill" role="presentation"><button class="nav-link w-100" id="roles-tab" data-bs-toggle="tab" data-bs-target="#roles" type="button" role="tab" aria-controls="roles" aria-selected="false">Roles</button></li>
    <li class="nav-item flex-fill" role="presentation"><button class="nav-link w-100" id="status-tab" data-bs-toggle="tab" data-bs-target="#status" type="button" role="tab" aria-controls="status" aria-selected="false">Status</button></li>
    <li class="nav-item flex-fill" role="presentation"><button class="nav-link w-100" id="transitions-tab" data-bs-toggle="tab" data-bs-target="#transitions" type="button" role="tab" aria-controls="transitions" aria-selected="false">Transitions</button></li>
    <li class="nav-item flex-fill" role="presentation"><button class="nav-link w-100" id="flow-tab" data-bs-toggle="tab" data-bs-target="#flow" type="button" role="tab" aria-controls="flow" aria-selected="false">Flows</button></li>
    <li class="nav-item flex-fill" role="presentation"><button class="nav-link w-100" id="index-tab" data-bs-toggle="tab" data-bs-target="#index" type="button" role="tab" aria-controls="index" aria-selected="false">Index</button></li>
    <li class="nav-item flex-fill" role="presentation"><button class="nav-link w-100" id="dataupdates-tab" data-bs-toggle="tab" data-bs-target="#dataupdates" type="button" role="tab" aria-controls="dataupdates" aria-selected="false">Data Updates</button></li>
    <li class="nav-item flex-fill" role="presentation"><button class="nav-link w-100" id="pages-tab" data-bs-toggle="tab" data-bs-target="#pages" type="button" role="tab" aria-controls="pages" aria-selected="false">Pages</button></li>
    <li class="nav-item flex-fill" role="presentation"><button class="nav-link w-100" id="emails-tab" data-bs-toggle="tab" data-bs-target="#emails" type="button" role="tab" aria-controls="emails" aria-selected="false">Emails</button></li>
    <li class="nav-item flex-fill" role="presentation"><button class="nav-link w-100" id="tools-tab" data-bs-toggle="tab" data-bs-target="#tools" type="button" role="tab" aria-controls="tools" aria-selected="false">Tools</button></li>
  </ul>
  <div class="tab-content pt-2" id="trackercontents">
    <div class="tab-pane fade show active" id="details" role="tabpanel" aria-labelledby="details-tab">{tab_details}</div>
    <div class="tab-pane fade" id="fields" role="tabpanel" aria-labelledby="fields-tab">{tab_fields}</div>
    <div class="tab-pane fade" id="roles" role="tabpanel" aria-labelledby="roles-tab">{tab_roles}</div>
    <div class="tab-pane fade" id="status" role="tabpanel" aria-labelledby="status-tab">{tab_status}</div>
    <div class="tab-pane fade" id="transitions" role="tabpanel" aria-labelledby="transitions-tab">{tab_transitions}</div>
    <div class="tab-pane fade" id="flow" role="tabpanel" aria-labelledby="flow-tab">{tab_flows}</div>
    <div class="tab-pane fade" id="index" role="tabpanel" aria-labelledby="index-tab">{tab_index}</div>
    <div class="tab-pane fade" id="dataupdates" role="tabpanel" aria-labelledby="dataupdates-tab">{tab_datas}</div>
    <div class="tab-pane fade" id="pages" role="tabpanel" aria-labelledby="pages-tab">{tab_pages}</div>
    <div class="tab-pane fade" id="emails" role="tabpanel" aria-labelledby="emails-tab">{tab_emails}</div>
    <div class="tab-pane fade" id="tools" role="tabpanel" aria-labelledby="tools-tab">{tab_tools}</div>
  </div>
</div></section></div></div>
<script>
(function() {{
  var hash = window.location.hash;
  if (hash) {{
    var el = document.querySelector(hash) || document.querySelector('button[data-bs-target="' + hash + '"]');
    if (el && typeof bootstrap !== 'undefined') {{
      new bootstrap.Tab(el).show();
    }}
  }}
}})();
</script>"##,
        mod_enc = enc(&module),
        slug_enc = enc(&slug),
        mod_esc = esc(&module),
        slug_esc = esc(&slug)
    );
    ctx.html("Show PortalTracker", body).await
}

// ---------------- Extra PortalTracker actions ----------------

pub async fn tracker_tool_action(mut ctx: Ctx, act: &str, id: Option<i64>) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let tid = id.unwrap_or(0);
    let Some(t) = dev_tracker(&ctx, tid).await else {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    };
    let db = &ctx.st.db.clone();
    match act {
        "updatedb" => match t.updatedb(db).await {
            Ok(_) => {
                let _ = t.create_index(db).await;
                ctx.flash("Database tables updated");
            }
            Err(e) => ctx.flash(&format!("Failed: {}", e)),
        },
        "create_defaults" => match tracker_web::create_defaults(db, &t).await {
            Ok(_) => {
                if let Some(t2) = Tracker::by_id(db, t.id).await {
                    let _ = t2.updatedb(db).await;
                }
                ctx.flash("Default role, statuses and transitions created");
            }
            Err(e) => ctx.flash(&format!("Failed: {}", e)),
        },
        "create_default_pages" => match tracker_web::create_default_pages(db, &t).await {
            Ok(n) => ctx.flash(&format!("{} default pages created", n)),
            Err(e) => ctx.flash(&format!("Failed: {}", e)),
        },
        "from_table" => match t.from_table(db).await {
            Ok(n) => ctx.flash(&format!("{} fields created from table columns", n)),
            Err(e) => ctx.flash(&format!("Failed: {}", e)),
        },
        "cleardb" => match t.cleardb(db).await {
            Ok(_) => ctx.flash("All data cleared"),
            Err(e) => ctx.flash(&format!("Failed: {}", e)),
        },
        "delete_fields" => {
            let _ = db.exec("delete from portal_tracker_field where tracker_id=:tid", &row! {"tid"=>t.id}).await;
            ctx.flash("Fields deleted");
        }
        "fix_status" => {
            let init = t.status_name(t.initial_status());
            if init.is_empty() {
                ctx.flash("Tracker has no initial status configured");
            } else if let Ok(dt) = db::ident(&t.data_table()) {
                let n = db
                    .exec(&format!("update {dt} set record_status=:st where record_status is null"), &row! {"st"=>init})
                    .await
                    .unwrap_or(0);
                ctx.flash(&format!("Status for {} fixed ({} row(s) updated)", t.name, n));
            }
        }
        "export_data" => {
            let base = web::setting_str(db, "migrationfolder")
                .await
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| ctx.st.cfg.module_dir.clone());
            let dir = PathBuf::from(&base).join(&t.module);
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join("data.json"), "[]");
            ctx.flash(&format!("Data for {} exported", t.name));
        }
        "fix_file_links" => {
            let file_fields: Vec<String> = t.fields.iter().filter(|f| s(f, "field_type") == "File").map(|f| s(f, "name")).collect();
            let mut fixed = 0usize;
            if !file_fields.is_empty() {
                if let Ok(dt) = db::ident(&t.data_table()) {
                    let rows = db.rows(&format!("select * from {dt}"), &db::no_params()).await.unwrap_or_default();
                    for r in &rows {
                        let rid = i(r, "id").unwrap_or(0);
                        for ff in &file_fields {
                            if let Some(fid) = i(r, ff) {
                                if db
                                    .exec(
                                        "update file_link set \"module\"=:m, tracker_slug=:ts, tracker_data_id=:rd where id=:id",
                                        &row! {"m"=>t.module.clone(),"ts"=>t.slug.clone(),"rd"=>rid,"id"=>fid},
                                    )
                                    .await
                                    .unwrap_or(0)
                                    > 0
                                {
                                    fixed += 1;
                                }
                            }
                        }
                    }
                }
            }
            ctx.flash(&format!("File links fixed ({fixed} updated)"));
        }
        "fix_trail_file_links" => {
            let mut fixed = 0usize;
            if let Ok(tt) = db::ident(&t.trail_table()) {
                let rows = db
                    .rows(&format!("select attachment_id, record_id from {tt} where attachment_id is not null"), &db::no_params())
                    .await
                    .unwrap_or_default();
                for r in &rows {
                    if let Some(aid) = i(r, "attachment_id") {
                        let rid = i(r, "record_id");
                        fixed += db
                            .exec(
                                "update file_link set \"module\"=:m, tracker_slug=:ts, tracker_data_id=:rd where id=:id and (tracker_slug is null or tracker_data_id is null)",
                                &row! {"m"=>t.module.clone(),"ts"=>t.slug.clone(),"rd"=>rid,"id"=>aid},
                            )
                            .await
                            .unwrap_or(0) as usize;
                    }
                }
            }
            ctx.flash(&format!("Trail file links fixed: {fixed} updated"));
        }
        "convert_text_columns" => {
            ctx.flash("Nothing to do on Postgres/SQLite: text is already the idiomatic unlimited column type.");
        }
        _ => {}
    }
    ctx.redirect(&format!("/portalTracker/show/{}", t.id)).await
}

// ---------------- Extra PortalTrackerField actions (referenced from Fields tab) ----------------

pub async fn field_action(mut ctx: Ctx, act: &str, id: Option<i64>) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let tid = id.unwrap_or(0);
    let Some(t) = dev_tracker(&ctx, tid).await else {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    };
    let db = &ctx.st.db.clone();
    match act {
        "updateDb" => {
            match t.updatedb(db).await {
                Ok(_) => ctx.flash("DB updated"),
                Err(e) => ctx.flash(&format!("DB update failed: {}", e)),
            }
            ctx.redirect(&format!("/portalTracker/show/{tid}#fields-tab")).await
        }
        "createIndex" => {
            match t.create_index(db).await {
                Ok(_) => ctx.flash("DB index created"),
                Err(e) => ctx.flash(&format!("Create index failed: {}", e)),
            }
            ctx.redirect(&format!("/portalTracker/show/{tid}#fields-tab")).await
        }
        "fromTable" => {
            match t.from_table(db).await {
                Ok(n) => ctx.flash(&format!("Fields imported from DB ({n} added)")),
                Err(e) => ctx.flash(&format!("From table failed: {}", e)),
            }
            ctx.redirect(&format!("/portalTracker/show/{tid}#fields-tab")).await
        }
        "multifields" => {
            if ctx.method == "POST" {
                let names = param_list(&ctx, "field_name");
                let labels = param_list(&ctx, "field_label");
                let types = param_list(&ctx, "field_type");
                let mut added = 0usize;
                for (idx, raw_nm) in names.iter().enumerate() {
                    let nm = raw_nm.trim().to_string();
                    if nm.is_empty() {
                        continue;
                    }
                    if db.count("select count(*) from portal_tracker_field where tracker_id=:tid and name=:n", &row! {"tid"=>tid,"n"=>nm.clone()}).await.unwrap_or(1) > 0 {
                        continue;
                    }
                    let lbl = labels.get(idx).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).unwrap_or_else(|| nm.clone());
                    let ft = types.get(idx).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).unwrap_or_else(|| "Text".into());
                    if db
                        .insert("portal_tracker_field", &row! {"tracker_id"=>tid,"name"=>nm,"label"=>lbl,"field_type"=>ft})
                        .await
                        .is_ok()
                    {
                        added += 1;
                    }
                }
                if added > 0 {
                    ctx.flash(&format!("{added} field(s) created"));
                    return ctx.redirect(&format!("/portalTracker/show/{tid}#fields-tab")).await;
                }
            }
            let opts: String = FIELD_TYPES
                .iter()
                .map(|ft| format!("<option value=\"{}\"{}>{}</option>", esc(ft), if *ft == "Text" { " selected" } else { "" }, esc(ft)))
                .collect();
            let csrf = ctx.csrf_field();
            let flash = s(&ctx.sess, "flash");
            let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
            let body = format!(
                r##"<div id="content" role="main"><div class="container">
<section class="row"><div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTracker/show/{tid}#fields-tab">Back to {tname}</a></li>
</ul></div></section>
<section class="row"><div id="create-portalTrackerField" class="col-12 content scaffold-create" role="main">
  <h1>Create Multiple Fields</h1>
  {flash_html}
  <form method="post" action="/portalTrackerField/multifields/{tid}">
    {csrf}
    <fieldset class="form">
      <table>
        <tr><th>Name</th><th>Label</th><th>Field Type</th></tr>
        <tr>
          <td><input type="text" name="field_name" /></td>
          <td><input type="text" name="field_label" /></td>
          <td><select name="field_type">{opts}</select></td>
        </tr>
        <tr id="addbuttonrow"><td colspan="3"><button type="button" id="addfield">Add</button></td></tr>
      </table>
    </fieldset>
    <fieldset class="buttons"><input type="submit" name="create" class="save" value="Create" /></fieldset>
  </form>
</div></section></div></div>
<script>
$('#addfield').on('click', function() {{
  $('#addbuttonrow').before('<tr><td><input type="text" name="field_name"/></td><td><input type="text" name="field_label"/></td><td><select name="field_type">{opts_js}</select></td></tr>');
  return false;
}});
</script>"##,
                tname = esc(&t.name),
                opts_js = opts.replace('\'', "\\'")
            );
            ctx.html("Create Multiple Fields", body).await
        }
        "excelTemplate" => {
            let csrf = ctx.csrf_field();
            let flash = s(&ctx.sess, "flash");
            let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
            let body = format!(
                r##"<div id="content" role="main"><div class="container">
<section class="row"><div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTracker/show/{tid}#fields-tab">Back to {tname}</a></li>
</ul></div></section>
<section class="row"><div id="list-portalTrackerField" class="col-12 content scaffold-list" role="main">
  <h1>Excel Template - Upload File</h1>
  {flash_html}
  <form action="/portalTrackerField/excelTemplateFields/{tid}" method="post" enctype="multipart/form-data">
    {csrf}
    <fieldset class="form"><div class="fieldcontain"><label for="fileupload">Template File</label><input type="file" name="fileupload" id="fileupload" required/></div></fieldset>
    <fieldset class="form"><div class="fieldcontain"><label for="header_start">Header Start</label><input type="number" name="header_start" value="1" id="header_start"/></div></fieldset>
    <fieldset class="form"><div class="fieldcontain"><label for="header_end">Header End</label><input type="number" name="header_end" value="1" id="header_end"/></div></fieldset>
    <fieldset class="buttons"><input type="submit" name="upload" class="save" value="Upload" id="upload"/></fieldset>
  </form>
</div></section></div></div>"##,
                tname = esc(&t.name)
            );
            ctx.html("Excel Template - Upload File", body).await
        }
        "excelTemplateFields" => {
            let Some(up) = ctx.files.remove("fileupload").or_else(|| ctx.files.remove("file")).filter(|u| !u.filename.is_empty() || u.size > 0) else {
                ctx.flash("file cannot be empty");
                return ctx.redirect(&format!("/portalTrackerField/excelTemplate/{tid}")).await;
            };
            let ext = up.filename.rsplit('.').next().unwrap_or("").to_lowercase();
            let dir = PathBuf::from(&ctx.st.cfg.upload_dir).join("tmp");
            let _ = tokio::fs::create_dir_all(&dir).await;
            let tmp = dir.join(format!("tpl_{}_{}", tid, crate::files::sanitize_filename(&up.filename)));
            if let Err(e) = up.save_to(&tmp).await {
                ctx.flash(&format!("Upload failed: {}", e));
                return ctx.redirect(&format!("/portalTrackerField/excelTemplate/{tid}")).await;
            }
            let hs = ctx.pi("header_start").unwrap_or(1).max(1) as usize;
            let he = ctx.pi("header_end").unwrap_or(hs as i64).max(hs as i64) as usize;
            // Vec<(name, label, field_type, options)>
            let mut fields: Vec<(String, String, String, String)> = vec![];
            if matches!(ext.as_str(), "xls" | "xlsx" | "xlsm" | "csv") {
                let p_s = tmp.to_string_lossy().to_string();
                if let Ok(Ok(hdrs)) = tokio::task::spawn_blocking(move || upload::extract_headers(&p_s, hs, he)).await {
                    for (_, slug, text) in hdrs {
                        if !slug.is_empty() {
                            fields.push((slug, text, "Text".into(), String::new()));
                        }
                    }
                }
            } else if matches!(ext.as_str(), "json" | "txt") {
                if let Ok(raw) = tokio::fs::read_to_string(&tmp).await {
                    if let Ok(val) = serde_json::from_str::<Value>(&raw) {
                        let arr = val.as_array().or_else(|| val.get("fields").and_then(|x| x.as_array()));
                        if let Some(items) = arr {
                            for item in items {
                                let nm = item.get("name").or_else(|| item.get("field_name")).and_then(|x| x.as_str()).unwrap_or("").to_string();
                                if nm.is_empty() {
                                    continue;
                                }
                                let lbl = item.get("label").or_else(|| item.get("field_label")).and_then(|x| x.as_str()).unwrap_or(&nm).to_string();
                                let ft = item.get("type").or_else(|| item.get("field_type")).and_then(|x| x.as_str()).unwrap_or("Text").to_string();
                                let opts = item
                                    .get("options")
                                    .and_then(|x| x.as_array())
                                    .map(|a| format!("[{}]", a.iter().filter_map(|v| v.as_str()).map(|s| format!("'{}'", s)).collect::<Vec<_>>().join(",")))
                                    .unwrap_or_default();
                                fields.push((nm, lbl, ft, opts));
                            }
                        }
                    }
                }
            }
            let _ = tokio::fs::remove_file(&tmp).await;

            let mut ftypes = vec!["Ignore"];
            ftypes.extend_from_slice(FIELD_TYPES);
            let mut rows_html = String::new();
            for (nm, lbl, ft, opts_val) in &fields {
                let mut sel_opts = String::new();
                for t_opt in &ftypes {
                    let sel = if *t_opt == ft.as_str() { " selected=\"selected\"" } else { "" };
                    sel_opts.push_str(&format!("<option value=\"{}\"{sel}>{}</option>", esc(t_opt), esc(t_opt)));
                }
                rows_html.push_str(&format!(
                    "<tr>\
                     <td>{nm_e}</td>\
                     <td><input type=\"text\" name=\"fname_{nm_e}\" value=\"{nm_e}\"/></td>\
                     <td><input type=\"text\" name=\"flabel_{nm_e}\" value=\"{lbl_e}\"/></td>\
                     <td><select name=\"ftype_{nm_e}\">{sel_opts}</select></td>\
                     <td><textarea name=\"fdefault_{nm_e}\"></textarea></td>\
                     <td><textarea name=\"foptions_{nm_e}\">{opts_e}</textarea></td>\
                     </tr>",
                    nm_e = esc(nm),
                    lbl_e = esc(lbl),
                    opts_e = esc(opts_val)
                ));
            }
            let csrf = ctx.csrf_field();
            let body = format!(
                r##"<div id="content" role="main"><div class="container">
<section class="row"><div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTracker/show/{tid}#fields-tab">Back to {tname}</a></li>
</ul></div></section>
<section class="row"><div id="list-portalTrackerField" class="col-12 content scaffold-list" role="main">
  <h1>Excel Template - Select Fields</h1>
  <form action="/portalTrackerField/submitExcelTemplate/{tid}" method="post">
    {csrf}
    <table class="table">
      <tr><th>Field</th><th>Name</th><th>Label</th><th>Type</th><th>Default</th><th>Options</th></tr>
      {rows_html}
    </table>
    <fieldset class="buttons"><input type="submit" name="process" class="save" value="Process" id="process"/></fieldset>
  </form>
</div></section></div></div>"##,
                tname = esc(&t.name)
            );
            ctx.html("Excel Template - Select Fields", body).await
        }
        "submitExcelTemplate" => {
            let mut saved = 0usize;
            for (k, v) in ctx.params.clone() {
                if let Some(key_name) = k.strip_prefix("ftype_") {
                    let ftype = db::vs(&v);
                    if ftype == "Ignore" || ftype.is_empty() {
                        continue;
                    }
                    let fname = ctx.p(&format!("fname_{key_name}")).trim().to_string();
                    if fname.is_empty() {
                        continue;
                    }
                    let flabel = ctx.p(&format!("flabel_{key_name}")).trim().to_string();
                    let fdefault = ctx.p(&format!("fdefault_{key_name}"));
                    let foptions = ctx.p(&format!("foptions_{key_name}"));
                    let d = row! {
                        "tracker_id"=>tid,
                        "name"=>fname.clone(),
                        "label"=>if flabel.is_empty() { fname.clone() } else { flabel },
                        "field_type"=>ftype,
                        "field_default"=>if fdefault.trim().is_empty() { Value::Null } else { json!(fdefault) },
                        "field_options"=>if foptions.trim().is_empty() { Value::Null } else { json!(foptions) }
                    };
                    if let Ok(Some(ex)) = db.first("select id from portal_tracker_field where tracker_id=:tid and name=:n", &row! {"tid"=>tid,"n"=>fname}).await {
                        if let Some(fid) = i(&ex, "id") {
                            let _ = db.update("portal_tracker_field", fid, &d).await;
                            saved += 1;
                        }
                    } else if db.insert("portal_tracker_field", &d).await.is_ok() {
                        saved += 1;
                    }
                }
            }
            ctx.flash(&format!("{saved} field(s) saved from template"));
            ctx.redirect(&format!("/portalTracker/show/{tid}#fields-tab")).await
        }
        _ => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
    }
}

fn param_list(ctx: &Ctx, name: &str) -> Vec<String> {
    match ctx.params.get(name) {
        Some(Value::Array(arr)) => arr.iter().map(db::vs).collect(),
        Some(Value::Null) | None => vec![],
        Some(v) => vec![db::vs(v)],
    }
}

// ---------------- PortalTrackerFlow controller & views (g6ikram parity) ----------------

fn script_json(v: &Value) -> String {
    v.to_string().replace("</", "<\\/").replace('\u{2028}', "\\u2028").replace('\u{2029}', "\\u2029")
}

pub async fn flow_controller(ctx: Ctx, act: &str, id: Option<i64>) -> Response {
    match act {
        "show" => match id {
            Some(fid) => flow_show(ctx, fid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "details" => match id {
            Some(fid) => flow_details(ctx, fid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "create" => flow_create_or_edit(ctx, None).await,
        "save" => flow_save(ctx).await,
        "edit" => match id {
            Some(fid) => flow_create_or_edit(ctx, Some(fid)).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "update" => match id {
            Some(fid) => flow_update(ctx, fid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "delete" => match id {
            Some(fid) => flow_delete(ctx, fid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        _ => flow_index(ctx).await,
    }
}

async fn flow_index(ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let is_admin = ctx.is_admin();
    let my_mods = if is_admin { vec![] } else { web::developer_list(db, ctx.user.as_ref().unwrap()).await };
    if !is_admin && my_mods.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this list").await;
    }
    let wh = if is_admin {
        String::new()
    } else {
        format!(" where t.\"module\" in {}", lit(&json!(my_mods)))
    };
    let max = ctx.pi("max").unwrap_or(10).clamp(1, 500);
    let offset = ctx.pi("offset").unwrap_or(0).max(0);
    let sort_p = ctx.p("sort");
    let order_dir = if ctx.p("order") == "desc" { "desc" } else { "asc" };
    let order_sql = match sort_p.as_str() {
        "name" => format!("f.name {}, f.id desc", order_dir),
        "tracker" | "tracker_id" => format!("t.name {}, f.id desc", order_dir),
        _ => "f.id desc".to_string(),
    };

    let from_sql = "from portal_tracker_flow f left join portal_tracker t on t.id=f.tracker_id";
    let total = db.count(&format!("select count(*) {from_sql}{wh}"), &db::no_params()).await.unwrap_or(0);
    let rows = db
        .rows(
            &format!("select f.*, t.name as tracker_name, t.module as tracker_module {from_sql}{wh} order by {order_sql} limit {max} offset {offset}"),
            &db::no_params(),
        )
        .await
        .unwrap_or_default();

    const COLS: &[(&str, &str)] = &[("name", "Name"), ("tracker", "Tracker"), ("fields", "Fields"), ("transitions", "Transitions")];
    let mut thead = String::new();
    for (col, lbl) in COLS {
        if matches!(*col, "name" | "tracker") {
            let sorted = sort_p == *col;
            let next_ord = if sorted && order_dir == "asc" { "desc" } else { "asc" };
            let cls = if sorted { format!("sortable sorted {}", order_dir) } else { "sortable".into() };
            thead.push_str(&format!("<th class=\"{cls}\"><a href=\"/portalTrackerFlow/index?sort={col}&amp;max={max}&amp;order={next_ord}\">{lbl}</a></th>"));
        } else {
            thead.push_str(&format!("<th>{lbl}</th>"));
        }
    }
    let mut tbody = String::new();
    for (rn, r) in rows.iter().enumerate() {
        let fid = i(r, "id").unwrap_or(0);
        let tid = i(r, "tracker_id").unwrap_or(0);
        let cls = if rn % 2 == 0 { "even" } else { "odd" };
        let tname = { let x = s(r, "tracker_name"); if x.is_empty() { format!("#{tid}") } else { x } };
        tbody.push_str(&format!(
            "<tr class=\"{cls}\">\
             <td><a href=\"/portalTrackerFlow/show/{fid}\">{}</a>&nbsp;</td>\
             <td><a href=\"/portalTracker/show/{tid}\">{}</a>&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             </tr>",
            esc(&s(r, "name")),
            esc(&tname),
            trunc(&s(r, "fields"), 80),
            trunc(&s(r, "transitions"), 80)
        ));
    }
    let pag = if total > max {
        format!("<div class=\"pagination\">{}</div>", crate::tags::paginate("/portalTrackerFlow/index", &ctx.params, total, max, offset))
    } else {
        String::new()
    };
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#list-portalTrackerFlow" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="create" href="/portalTrackerFlow/create">New PortalTrackerFlow</a></li>
</ul></div></section>
<section class="row"><div id="list-portalTrackerFlow" class="col-12 content scaffold-list" role="main">
  <h1>PortalTrackerFlow List</h1>
  {flash_html}
  <table class="responsive"><thead><tr>{thead}</tr></thead><tbody>{tbody}</tbody></table>
  {pag}
</div></section></div></div>"##
    );
    ctx.html("PortalTrackerFlow List", body).await
}

async fn flow_show(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(fl)) = db.get("portal_tracker_flow", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let tid = i(&fl, "tracker_id").unwrap_or(0);
    let Some(t) = dev_tracker(&ctx, tid).await else {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    };
    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#show-portalTrackerFlow" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTrackerFlow/index">PortalTrackerFlow List</a></li>
  <li><a class="create" href="/portalTrackerFlow/create?tracker_id={tid}">New PortalTrackerFlow</a></li>
</ul></div></section>
<section class="row"><div id="show-portalTrackerFlow" class="col-12 content scaffold-show" role="main">
  <h1>Show PortalTrackerFlow</h1>
  {flash_html}
  <ol class="property-list portalTrackerFlow">
    <li class="fieldcontain"><span id="name-label" class="property-label">Name</span><div class="property-value" aria-labelledby="name-label">{name}</div></li>
    <li class="fieldcontain"><span id="tracker-label" class="property-label">Tracker</span><div class="property-value" aria-labelledby="tracker-label"><a href="/portalTracker/show/{tid}">{tname}</a></div></li>
    <li class="fieldcontain"><span id="fields-label" class="property-label">Fields</span><div class="property-value" aria-labelledby="fields-label">{fields}</div></li>
    <li class="fieldcontain"><span id="transitions-label" class="property-label">Transitions</span><div class="property-value" aria-labelledby="transitions-label">{transitions}</div></li>
  </ol>
  <form action="/portalTrackerFlow/delete/{id}" method="post">
    {csrf}
    <fieldset class="buttons">
      <a class="edit" href="/portalTrackerFlow/edit/{id}">Edit</a>
      <a class="edit" href="/portalTrackerFlow/details/{id}">Details</a>
      <input class="delete" type="submit" value="Delete" onclick="return confirm('Are you sure?');" />
    </fieldset>
  </form>
</div></section></div></div>"##,
        name = esc(&s(&fl, "name")),
        tname = esc(&t.name),
        fields = esc(&s(&fl, "fields")),
        transitions = esc(&s(&fl, "transitions"))
    );
    ctx.html("Show PortalTrackerFlow", body).await
}

/// Interactive Field × (Transition / Next Status) matrix matching `views/portalTrackerFlow/details.gsp`.
async fn flow_details(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(fl)) = db.get("portal_tracker_flow", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let tid = i(&fl, "tracker_id").unwrap_or(0);
    let Some(mut t) = dev_tracker(&ctx, tid).await else {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    };

    let resolve_flow = |t: &Tracker| -> (Vec<crate::tracker::Trans>, Vec<Row>) {
        let mut transitions = vec![];
        for tn in db::csv(&s(&fl, "transitions")) {
            if let Some(tr) = t.transitions.iter().find(|x| x.name == tn) {
                transitions.push(tr.clone());
            }
        }
        let mut fields = vec![];
        for fnm in db::csv(&s(&fl, "fields")) {
            if let Some(f) = t.fields.iter().find(|x| s(x, "name") == fnm) {
                fields.push(f.clone());
            }
        }
        (transitions, fields)
    };

    if ctx.method == "POST" && !ctx.p("update").is_empty() {
        let (transitions, fields) = resolve_flow(&t);
        for tr in &transitions {
            let mut edit_arr: Vec<String> = vec![];
            let mut disp_arr: Vec<String> = vec![];
            let mut stat_arr: Vec<String> = vec![];
            for cf in &fields {
                let fid = i(cf, "id").unwrap_or(0);
                let fnm = s(cf, "name");
                if ctx.params.contains_key(&format!("td_{}_{}", tr.id, fid)) {
                    disp_arr.push(fnm.clone());
                }
                if ctx.params.contains_key(&format!("te_{}_{}", tr.id, fid)) {
                    edit_arr.push(fnm.clone());
                }
                if let Some(nsid) = tr.next {
                    if ctx.params.contains_key(&format!("sd_{}_{}", nsid, fid)) {
                        stat_arr.push(fnm.clone());
                    }
                }
            }
            let _ = db
                .update(
                    "portal_tracker_transition",
                    tr.id,
                    &row! {"editfields"=>edit_arr.join(","),"displayfields"=>disp_arr.join(",")},
                )
                .await;
            if let Some(nsid) = tr.next {
                let _ = db
                    .update("portal_tracker_status", nsid, &row! {"displayfields"=>stat_arr.join(",")})
                    .await;
            }
        }
        ctx.flash("Flow fields updated");
        if let Some(t2) = Tracker::by_id(db, tid).await {
            t = t2;
        }
    }

    let (transitions, fields) = resolve_flow(&t);
    let field_id_by_name: HashMap<String, i64> = t.fields.iter().filter_map(|f| Some((s(f, "name"), i(f, "id")?))).collect();
    let status_by_id: HashMap<i64, &Row> = t.statuses.iter().filter_map(|st| Some((i(st, "id")?, st))).collect();

    let mut checked: std::collections::HashSet<String> = std::collections::HashSet::new();
    for tr in &transitions {
        for fnm in db::csv(&s(&tr.row, "displayfields")) {
            if let Some(&fid) = field_id_by_name.get(&fnm) {
                checked.insert(format!("td_{}_{}", tr.id, fid));
            }
        }
        for fnm in db::csv(&s(&tr.row, "editfields")) {
            if let Some(&fid) = field_id_by_name.get(&fnm) {
                checked.insert(format!("te_{}_{}", tr.id, fid));
            }
        }
        if let Some(nsid) = tr.next {
            if let Some(st_row) = status_by_id.get(&nsid) {
                for fnm in db::csv(&s(st_row, "displayfields")) {
                    if let Some(&fid) = field_id_by_name.get(&fnm) {
                        checked.insert(format!("sd_{}_{}", nsid, fid));
                    }
                }
            }
        }
    }

    let mut hdr_cols = String::new();
    for tr in &transitions {
        let nsid_s = tr.next.map(|x| x.to_string()).unwrap_or_default();
        let ns_name = tr.next.and_then(|x| status_by_id.get(&x)).map(|r| s(r, "name")).unwrap_or_default();
        hdr_cols.push_str(&format!(
            "<th>{} \
             <button type=\"button\" id=\"t_{tid}\" class=\"all_c\" data-cid=\"t_{tid}\"><i class=\"bi bi-check\"></i></button>\
             <button type=\"button\" id=\"ut_{tid}\" class=\"undo_c\" data-cid=\"t_{tid}\"><i class=\"bi bi-x\"></i></button>\
             </th>\
             <th>{} \
             <button type=\"button\" id=\"s_{nsid_s}\" class=\"all_c\" data-cid=\"s_{nsid_s}\"><i class=\"bi bi-check\"></i></button>\
             <button type=\"button\" id=\"us_{nsid_s}\" class=\"undo_c\" data-cid=\"s_{nsid_s}\"><i class=\"bi bi-x\"></i></button>\
             </th>",
            esc(&tr.name),
            esc(&ns_name),
            tid = tr.id
        ));
    }

    let mut body_rows = String::new();
    for cf in &fields {
        let fid = i(cf, "id").unwrap_or(0);
        let fnm = s(cf, "name");
        let mut edit_cells = String::new();
        let mut disp_cells = String::new();
        for tr in &transitions {
            let te_key = format!("te_{}_{}", tr.id, fid);
            let td_key = format!("td_{}_{}", tr.id, fid);
            let te_chk = if checked.contains(&te_key) { " checked=\"checked\"" } else { "" };
            let td_chk = if checked.contains(&td_key) { " checked=\"checked\"" } else { "" };
            edit_cells.push_str(&format!(
                "<td><input type=\"checkbox\"{te_chk} id=\"{te_key}\" name=\"{te_key}\" title=\"Edit '{}' for '{}'\" class=\"fieldbox t_{} r_{fid}\"/></td><td>&nbsp;</td>",
                esc(&fnm),
                esc(&tr.name),
                tr.id
            ));
            let sd_cell = match tr.next {
                Some(nsid) => {
                    let sd_key = format!("sd_{}_{}", nsid, fid);
                    let sd_chk = if checked.contains(&sd_key) { " checked=\"checked\"" } else { "" };
                    let ns_name = status_by_id.get(&nsid).map(|r| s(r, "name")).unwrap_or_default();
                    format!(
                        "<input type=\"checkbox\"{sd_chk} id=\"{sd_key}\" name=\"{sd_key}\" title=\"Display '{}' for '{}'\" class=\"fieldbox s_{nsid} r_{fid}\"/>",
                        esc(&fnm),
                        esc(&ns_name)
                    )
                }
                None => "&nbsp;".into(),
            };
            disp_cells.push_str(&format!(
                "<td><input type=\"checkbox\"{td_chk} id=\"{td_key}\" name=\"{td_key}\" title=\"Display '{}' for '{}'\" class=\"fieldbox t_{} r_{fid}\"/></td><td>{sd_cell}</td>",
                esc(&fnm),
                esc(&tr.name),
                tr.id
            ));
        }
        body_rows.push_str(&format!(
            "<tr>\
               <th rowspan=\"2\">{} \
                 <button type=\"button\" id=\"r_{fid}\" class=\"all_r\" data-rid=\"r_{fid}\"><i class=\"bi bi-check\"></i></button>\
                 <button type=\"button\" id=\"ur_{fid}\" class=\"undo_r\" data-rid=\"r_{fid}\"><i class=\"bi bi-x\"></i></button>\
               </th>\
               <td>Edit</td>{edit_cells}\
             </tr>\
             <tr>\
               <td>Display</td>{disp_cells}\
             </tr>",
            esc(&fnm)
        ));
    }

    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#show-portalTrackerFlow" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTrackerFlow/index">PortalTrackerFlow List</a></li>
  <li><a class="show" href="/portalTrackerFlow/show/{id}">Back to Flow</a></li>
  <li><a class="create" href="/portalTrackerFlow/create?tracker_id={tid}">New PortalTrackerFlow</a></li>
</ul></div></section>
<section class="row"><div id="show-portalTrackerFlow" class="col-12 content scaffold-show" role="main">
  <h1>Show PortalTrackerFlow</h1>
  {flash_html}
  <ol class="property-list portalTrackerFlow">
    <li class="fieldcontain"><span id="name-label" class="property-label">Name</span><div class="property-value" aria-labelledby="name-label">{name}</div></li>
    <li class="fieldcontain"><span id="tracker-label" class="property-label">Tracker</span><div class="property-value" aria-labelledby="tracker-label"><a href="/portalTracker/show/{tid}">{tname}</a></div></li>
  </ol>
  <h2>All
    <button type="button" id="all"><i class="bi bi-check"></i></button>
    <button type="button" id="undoall"><i class="bi bi-x"></i></button>
  </h2>
  <form action="/portalTrackerFlow/details/{id}" method="post">
    {csrf}
    <input type="hidden" id="update" name="update" value="1"/>
    <div style="overflow-x:auto;">
      <table>
        <tr><th colspan="2">&nbsp;</th>{hdr_cols}</tr>
        {body_rows}
      </table>
    </div>
    <fieldset class="buttons"><input type="submit" name="create" class="save" value="Update" /></fieldset>
  </form>
</div></section></div></div>
<script>
$('#all').on('click', function() {{ $('.fieldbox').prop('checked', true); }});
$('#undoall').on('click', function() {{ $('.fieldbox').prop('checked', false); }});
$('.all_c').on('click', function(e) {{ $('.' + $(e.currentTarget).data('cid')).prop('checked', true); return false; }});
$('.all_r').on('click', function(e) {{ $('.' + $(e.currentTarget).data('rid')).prop('checked', true); return false; }});
$('.undo_c').on('click', function(e) {{ $('.' + $(e.currentTarget).data('cid')).prop('checked', false); return false; }});
$('.undo_r').on('click', function(e) {{ $('.' + $(e.currentTarget).data('rid')).prop('checked', false); return false; }});
</script>"##,
        name = esc(&s(&fl, "name")),
        tname = esc(&t.name)
    );
    ctx.html("Show PortalTrackerFlow", body).await
}

async fn flow_create_or_edit(mut ctx: Ctx, id: Option<i64>) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let is_admin = ctx.is_admin();
    let my_mods = if is_admin { vec![] } else { web::developer_list(db, ctx.user.as_ref().unwrap()).await };
    if !is_admin && my_mods.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }

    let is_new = id.is_none();
    let (fl, tracker_id) = match id {
        Some(fid) => {
            let Ok(Some(row)) = db.get("portal_tracker_flow", fid).await else {
                return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
            };
            let tid = i(&row, "tracker_id").unwrap_or(0);
            if dev_tracker(&ctx, tid).await.is_none() {
                return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
            }
            (row, Some(tid))
        }
        None => {
            let tid = ctx.pi("tracker_id").or_else(|| ctx.pi("tracker"));
            let mut row = Row::new();
            if let Some(t_id) = tid {
                if let Some(t) = dev_tracker(&ctx, t_id).await {
                    let mut fnames: Vec<String> = t.fields.iter().map(|f| s(f, "name")).collect();
                    fnames.sort_by_key(|a| a.to_lowercase());
                    let mut tnames: Vec<String> = t.transitions.iter().map(|x| x.name.clone()).collect();
                    tnames.sort_by_key(|a| a.to_lowercase());
                    row.insert("fields".into(), json!(fnames.join(",")));
                    row.insert("transitions".into(), json!(tnames.join(",")));
                }
            }
            (row, tid)
        }
    };

    let tracker_obj = match tracker_id {
        Some(tid) => Tracker::by_id(db, tid).await,
        None => None,
    };

    let tracker_input = match (is_new, tracker_id) {
        (true, Some(tid)) | (false, Some(tid)) => format!("<input type=\"hidden\" name=\"tracker_id\" id=\"tracker_id\" value=\"{tid}\"/>"),
        _ => {
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
                opts.push_str(&format!("<option value=\"{tid}\">{}: {}</option>", esc(&s(tr, "module")), esc(&s(tr, "name"))));
            }
            format!("<div class=\"fieldcontain required\"><label for=\"tracker_id\">Tracker<span class=\"required-indicator\">*</span></label><select name=\"tracker_id\" id=\"tracker_id\">{opts}</select></div>")
        }
    };

    let tagify_js = match &tracker_obj {
        Some(t) => {
            let mut fnames: Vec<String> = t.fields.iter().map(|f| s(f, "name")).collect();
            fnames.sort_by_key(|a| a.to_lowercase());
            let mut tnames: Vec<String> = t.transitions.iter().map(|x| x.name.clone()).collect();
            tnames.sort_by_key(|a| a.to_lowercase());
            format!(
                r##"<script>
(function() {{
  if (typeof Tagify === 'undefined') return;
  function initTagify(name, whitelist) {{
    var input = document.querySelector('textarea[name="' + name + '"]');
    if (!input) return;
    var tagify = new Tagify(input, {{
      enforceWhitelist: true,
      originalInputValueFormat: function(arr) {{ return arr.map(function(item) {{ return item.value; }}).join(','); }},
      delimiters: ',',
      whitelist: whitelist,
      dropdown: {{ maxItems: 50, enabled: 0 }}
    }});
    if (typeof DragSort !== 'undefined') {{
      new DragSort(tagify.DOM.scope, {{
        selector: '.' + tagify.settings.classNames.tag,
        callbacks: {{ dragEnd: function() {{ tagify.updateValueByDOMTags(); }} }}
      }});
    }}
  }}
  initTagify('fields', {f_wl});
  initTagify('transitions', {t_wl});
}})();
</script>"##,
                f_wl = script_json(&json!(fnames)),
                t_wl = script_json(&json!(tnames))
            )
        }
        None => String::new(),
    };

    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let action = match id {
        Some(fid) => format!("/portalTrackerFlow/update/{fid}"),
        None => "/portalTrackerFlow/save".into(),
    };
    let kind = if is_new { "create" } else { "edit" };
    let heading = if is_new { "Create PortalTrackerFlow" } else { "Edit PortalTrackerFlow" };
    let btn = if is_new { "Create" } else { "Update" };
    let new_li = if !is_new { "<li><a class=\"create\" href=\"/portalTrackerFlow/create\">New PortalTrackerFlow</a></li>" } else { "" };

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#{kind}-portalTrackerFlow" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTrackerFlow/index">PortalTrackerFlow List</a></li>
  {new_li}
</ul></div></section>
<section class="row"><div id="{kind}-portalTrackerFlow" class="col-12 content scaffold-{kind}" role="main">
  <h1>{heading}</h1>
  {flash_html}
  <form action="{action}" method="post">
    {csrf}
    <fieldset class="form">
      <div class="fieldcontain required"><label for="name">Name<span class="required-indicator">*</span></label><input type="text" name="name" id="name" value="{name}" required="required"/></div>
      {tracker_input}
      <div class="fieldcontain"><label for="fields">Fields</label><textarea id="fields" name="fields" style="width:40%;">{fields}</textarea></div>
      <div class="fieldcontain"><label for="transitions">Transitions</label><textarea id="transitions" name="transitions" style="width:40%;">{transitions}</textarea></div>
    </fieldset>
    <fieldset class="buttons"><input class="save" type="submit" value="{btn}" /></fieldset>
  </form>
</div></section></div></div>
{tagify_js}"##,
        name = esc(&s(&fl, "name")),
        fields = esc(&s(&fl, "fields")),
        transitions = esc(&s(&fl, "transitions"))
    );
    ctx.html(heading, body).await
}

fn parse_tagify_csv(raw: &str) -> String {
    let t = raw.trim();
    if t.starts_with('[') {
        if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(t) {
            return arr
                .iter()
                .filter_map(|v| v.get("value").and_then(|x| x.as_str()).or_else(|| v.as_str()))
                .map(|x| x.trim())
                .filter(|x| !x.is_empty())
                .collect::<Vec<_>>()
                .join(",");
        }
    }
    db::csv(t).join(",")
}

async fn flow_save(mut ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if ctx.method != "POST" {
        return ctx.redirect("/portalTrackerFlow/create").await;
    }
    let db = &ctx.st.db.clone();
    let tid = ctx.pi("tracker_id").or_else(|| ctx.pi("tracker")).unwrap_or(0);
    if dev_tracker(&ctx, tid).await.is_none() {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let name = ctx.p("name").trim().to_string();
    if name.is_empty() {
        ctx.flash("Name is required");
        return ctx.redirect(&format!("/portalTrackerFlow/create?tracker_id={tid}")).await;
    }
    let fields = parse_tagify_csv(&ctx.p("fields"));
    let transitions = parse_tagify_csv(&ctx.p("transitions"));
    match db
        .insert("portal_tracker_flow", &row! {"tracker_id"=>tid,"name"=>name,"fields"=>fields,"transitions"=>transitions})
        .await
    {
        Ok(fid) => {
            ctx.flash(&format!("PortalTrackerFlow {fid} created"));
            ctx.redirect(&format!("/portalTrackerFlow/show/{fid}")).await
        }
        Err(e) => {
            ctx.flash(&format!("Could not create PortalTrackerFlow: {e}"));
            ctx.redirect(&format!("/portalTrackerFlow/create?tracker_id={tid}")).await
        }
    }
}

async fn flow_update(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if ctx.method != "POST" {
        return ctx.redirect(&format!("/portalTrackerFlow/edit/{id}")).await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(ex)) = db.get("portal_tracker_flow", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let tid = i(&ex, "tracker_id").unwrap_or(0);
    if dev_tracker(&ctx, tid).await.is_none() {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let name = ctx.p("name").trim().to_string();
    if name.is_empty() {
        ctx.flash("Name is required");
        return ctx.redirect(&format!("/portalTrackerFlow/edit/{id}")).await;
    }
    let fields = parse_tagify_csv(&ctx.p("fields"));
    let transitions = parse_tagify_csv(&ctx.p("transitions"));
    match db
        .update("portal_tracker_flow", id, &row! {"name"=>name,"fields"=>fields,"transitions"=>transitions})
        .await
    {
        Ok(_) => {
            ctx.flash(&format!("PortalTrackerFlow {id} updated"));
            ctx.redirect(&format!("/portalTrackerFlow/show/{id}")).await
        }
        Err(e) => {
            ctx.flash(&format!("Could not update PortalTrackerFlow: {e}"));
            ctx.redirect(&format!("/portalTrackerFlow/edit/{id}")).await
        }
    }
}

async fn flow_delete(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if ctx.method != "POST" {
        return ctx.status(StatusCode::METHOD_NOT_ALLOWED, "POST required").await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(ex)) = db.get("portal_tracker_flow", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let tid = i(&ex, "tracker_id").unwrap_or(0);
    if dev_tracker(&ctx, tid).await.is_none() {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let _ = db.delete("portal_tracker_flow", id).await;
    ctx.flash(&format!("PortalTrackerFlow {id} deleted"));
    ctx.redirect("/portalTrackerFlow/index").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagify_csv_parses_both_json_and_csv() {
        assert_eq!(parse_tagify_csv(r#"[{"value":"a"},{"value":"b"}]"#), "a,b");
        assert_eq!(parse_tagify_csv("a, b , c"), "a,b,c");
    }

    #[test]
    fn safe_index_name_sanitizes_spaces_and_symbols() {
        assert_eq!(safe_index_name("trak_hr_emp_data", "By Dept & Date"), "ix_trak_hr_emp_data_By_Dept___Date");
    }
}

// ---------------- PortalTrackerIndex controller & views (g6ikram parity) ----------------

fn safe_index_name(table: &str, name: &str) -> String {
    let clean: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    format!("ix_{}_{}", table, clean)
}

fn wrap_csv_html(raw: &str) -> String {
    let mut out = String::new();
    let mut line_len = 0usize;
    for (idx, part) in raw.split(',').enumerate() {
        if idx > 0 {
            out.push(',');
            if line_len >= 80 {
                out.push_str("<br/>");
                line_len = 0;
            }
        }
        let e = esc(part);
        line_len += part.len() + 1;
        out.push_str(&e);
    }
    out
}

async fn apply_single_index(db: &Db, t: &Tracker, ix: &Row) -> anyhow::Result<String> {
    let tablename = db::ident(&t.data_table())?;
    let mut cols: Vec<String> = vec![];
    for fname in db::csv(&s(ix, "fields")) {
        if let Some(cf) = t.field(&fname) {
            let ft = s(cf, "field_type");
            if !matches!(ft.as_str(), "Text Area" | "HasMany" | "FieldGroup") {
                if let Ok(c) = db::ident(&fname) {
                    cols.push(c);
                }
            }
        }
    }
    let idx_name = db::ident(&safe_index_name(&t.data_table(), &s(ix, "name")))?;
    if cols.is_empty() {
        anyhow::bail!("No indexable columns in '{}'", s(ix, "fields"));
    }
    let _ = db.exec(&format!("drop index if exists {idx_name}"), &db::no_params()).await;
    db.exec(&format!("create index if not exists {idx_name} on {tablename} ({})", cols.join(",")), &db::no_params()).await?;
    Ok(t.data_table())
}

async fn drop_single_index(db: &Db, t: &Tracker, ix: &Row) {
    if let Ok(idx_name) = db::ident(&safe_index_name(&t.data_table(), &s(ix, "name"))) {
        let _ = db.exec(&format!("drop index if exists {idx_name}"), &db::no_params()).await;
    }
}

pub async fn index_controller(ctx: Ctx, act: &str, id: Option<i64>) -> Response {
    match act {
        "show" => match id {
            Some(iid) => idx_show(ctx, iid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "createIndex" => match id {
            Some(iid) => idx_create_db(ctx, iid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "create" => idx_create_or_edit(ctx, None).await,
        "save" => idx_save(ctx).await,
        "edit" => match id {
            Some(iid) => idx_create_or_edit(ctx, Some(iid)).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "update" => match id {
            Some(iid) => idx_update(ctx, iid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        "delete" => match id {
            Some(iid) => idx_delete(ctx, iid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        _ => idx_index(ctx).await,
    }
}

async fn idx_index(ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let is_admin = ctx.is_admin();
    let my_mods = if is_admin { vec![] } else { web::developer_list(db, ctx.user.as_ref().unwrap()).await };
    if !is_admin && my_mods.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this list").await;
    }
    let wh = if is_admin {
        String::new()
    } else {
        format!(" where t.\"module\" in {}", lit(&json!(my_mods)))
    };
    let max = ctx.pi("max").unwrap_or(10).clamp(1, 500);
    let offset = ctx.pi("offset").unwrap_or(0).max(0);
    let sort_p = ctx.p("sort");
    let order_dir = if ctx.p("order") == "desc" { "desc" } else { "asc" };
    let order_sql = match sort_p.as_str() {
        "name" => format!("x.name {}, x.id desc", order_dir),
        "tracker" | "tracker_id" => format!("t.name {}, x.id desc", order_dir),
        _ => "x.id desc".to_string(),
    };

    let from_sql = "from portal_tracker_index x left join portal_tracker t on t.id=x.tracker_id";
    let total = db.count(&format!("select count(*) {from_sql}{wh}"), &db::no_params()).await.unwrap_or(0);
    let rows = db
        .rows(
            &format!("select x.*, t.name as tracker_name {from_sql}{wh} order by {order_sql} limit {max} offset {offset}"),
            &db::no_params(),
        )
        .await
        .unwrap_or_default();

    const COLS: &[(&str, &str)] = &[("name", "Name"), ("tracker", "Tracker"), ("fields", "Fields")];
    let mut thead = String::new();
    for (col, lbl) in COLS {
        if matches!(*col, "name" | "tracker") {
            let sorted = sort_p == *col;
            let next_ord = if sorted && order_dir == "asc" { "desc" } else { "asc" };
            let cls = if sorted { format!("sortable sorted {}", order_dir) } else { "sortable".into() };
            thead.push_str(&format!("<th class=\"{cls}\"><a href=\"/portalTrackerIndex/index?sort={col}&amp;max={max}&amp;order={next_ord}\">{lbl}</a></th>"));
        } else {
            thead.push_str(&format!("<th>{lbl}</th>"));
        }
    }
    let mut tbody = String::new();
    for (rn, r) in rows.iter().enumerate() {
        let iid = i(r, "id").unwrap_or(0);
        let tid = i(r, "tracker_id").unwrap_or(0);
        let cls = if rn % 2 == 0 { "even" } else { "odd" };
        let tname = { let x = s(r, "tracker_name"); if x.is_empty() { format!("#{tid}") } else { x } };
        tbody.push_str(&format!(
            "<tr class=\"{cls}\">\
             <td><a href=\"/portalTrackerIndex/show/{iid}\">{}</a>&nbsp;</td>\
             <td><a href=\"/portalTracker/show/{tid}\">{}</a>&nbsp;</td>\
             <td>{}&nbsp;</td>\
             </tr>",
            esc(&s(r, "name")),
            esc(&tname),
            wrap_csv_html(&s(r, "fields"))
        ));
    }
    let pag = if total > max {
        format!("<div class=\"pagination\">{}</div>", crate::tags::paginate("/portalTrackerIndex/index", &ctx.params, total, max, offset))
    } else {
        String::new()
    };
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#list-portalTrackerIndex" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="create" href="/portalTrackerIndex/create">New PortalTrackerIndex</a></li>
</ul></div></section>
<section class="row"><div id="list-portalTrackerIndex" class="col-12 content scaffold-list" role="main">
  <h1>PortalTrackerIndex List</h1>
  {flash_html}
  <table class="responsive"><thead><tr>{thead}</tr></thead><tbody>{tbody}</tbody></table>
  {pag}
</div></section></div></div>"##
    );
    ctx.html("PortalTrackerIndex List", body).await
}

async fn idx_show(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(ix)) = db.get("portal_tracker_index", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let tid = i(&ix, "tracker_id").unwrap_or(0);
    let Some(t) = dev_tracker(&ctx, tid).await else {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    };
    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#show-portalTrackerIndex" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTrackerIndex/index">PortalTrackerIndex List</a></li>
  <li><a class="create" href="/portalTrackerIndex/create?tracker_id={tid}">New PortalTrackerIndex</a></li>
</ul></div></section>
<section class="row"><div id="show-portalTrackerIndex" class="col-12 content scaffold-show" role="main">
  <h1>Show PortalTrackerIndex</h1>
  {flash_html}
  <ol class="property-list portalTrackerIndex">
    <li class="fieldcontain"><span id="name-label" class="property-label">Name</span><div class="property-value" aria-labelledby="name-label">{name}</div></li>
    <li class="fieldcontain"><span id="tracker-label" class="property-label">Tracker</span><div class="property-value" aria-labelledby="tracker-label"><a href="/portalTracker/show/{tid}">{tname}</a></div></li>
    <li class="fieldcontain"><span id="fields-label" class="property-label">Fields</span><div class="property-value" aria-labelledby="fields-label">{fields}</div></li>
  </ol>
  <form action="/portalTrackerIndex/delete/{id}" method="post">
    {csrf}
    <fieldset class="buttons">
      <a class="edit" href="/portalTrackerIndex/edit/{id}">Edit</a>
      <a class="edit" href="/portalTrackerIndex/createIndex/{id}">Create Index</a>
      <input class="delete" type="submit" value="Delete" onclick="return confirm('Are you sure?');" />
    </fieldset>
  </form>
</div></section></div></div>"##,
        name = esc(&s(&ix, "name")),
        tname = esc(&t.name),
        fields = wrap_csv_html(&s(&ix, "fields"))
    );
    ctx.html("Show PortalTrackerIndex", body).await
}

async fn idx_create_db(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(ix)) = db.get("portal_tracker_index", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let tid = i(&ix, "tracker_id").unwrap_or(0);
    let Some(t) = dev_tracker(&ctx, tid).await else {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    };
    match apply_single_index(db, &t, &ix).await {
        Ok(tbl) => ctx.flash(&format!("Index created for table {tbl}")),
        Err(e) => ctx.flash(&format!("Create index failed: {e}")),
    }
    ctx.redirect(&format!("/portalTrackerIndex/show/{id}")).await
}

async fn idx_create_or_edit(mut ctx: Ctx, id: Option<i64>) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let db = &ctx.st.db.clone();
    let is_admin = ctx.is_admin();
    let my_mods = if is_admin { vec![] } else { web::developer_list(db, ctx.user.as_ref().unwrap()).await };
    if !is_admin && my_mods.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let is_new = id.is_none();
    let (ix, tracker_id) = match id {
        Some(iid) => {
            let Ok(Some(row)) = db.get("portal_tracker_index", iid).await else {
                return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
            };
            let tid = i(&row, "tracker_id").unwrap_or(0);
            if dev_tracker(&ctx, tid).await.is_none() {
                return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
            }
            (row, Some(tid))
        }
        None => {
            let tid = ctx.pi("tracker_id").or_else(|| ctx.pi("tracker"));
            let mut row = Row::new();
            if let Some(t_id) = tid {
                if let Some(t) = dev_tracker(&ctx, t_id).await {
                    let mut fnames: Vec<String> = t.fields.iter().map(|f| s(f, "name")).collect();
                    fnames.sort_by_key(|a| a.to_lowercase());
                    row.insert("fields".into(), json!(fnames.join(",")));
                }
            }
            (row, tid)
        }
    };

    let tracker_obj = match tracker_id {
        Some(tid) => Tracker::by_id(db, tid).await,
        None => None,
    };

    let tracker_input = match tracker_id {
        Some(tid) => format!("<input type=\"hidden\" name=\"tracker_id\" id=\"tracker_id\" value=\"{tid}\"/>"),
        None => {
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
                opts.push_str(&format!("<option value=\"{tid}\">{}: {}</option>", esc(&s(tr, "module")), esc(&s(tr, "name"))));
            }
            format!("<div class=\"fieldcontain required\"><label for=\"tracker_id\">Tracker<span class=\"required-indicator\">*</span></label><select name=\"tracker_id\" id=\"tracker_id\">{opts}</select></div>")
        }
    };

    let tagify_js = match &tracker_obj {
        Some(t) => {
            let mut fnames: Vec<String> = t.fields.iter().map(|f| s(f, "name")).collect();
            fnames.sort_by_key(|a| a.to_lowercase());
            format!(
                r##"<script>
(function() {{
  if (typeof Tagify === 'undefined') return;
  var input = document.querySelector('textarea[name="fields"]');
  if (!input) return;
  var tagify = new Tagify(input, {{
    enforceWhitelist: true,
    originalInputValueFormat: function(arr) {{ return arr.map(function(item) {{ return item.value; }}).join(','); }},
    delimiters: ',',
    whitelist: {f_wl},
    dropdown: {{ maxItems: 50, enabled: 0 }}
  }});
  if (typeof DragSort !== 'undefined') {{
    new DragSort(tagify.DOM.scope, {{
      selector: '.' + tagify.settings.classNames.tag,
      callbacks: {{ dragEnd: function() {{ tagify.updateValueByDOMTags(); }} }}
    }});
  }}
}})();
</script>"##,
                f_wl = script_json(&json!(fnames))
            )
        }
        None => String::new(),
    };

    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let action = match id {
        Some(iid) => format!("/portalTrackerIndex/update/{iid}"),
        None => "/portalTrackerIndex/save".into(),
    };
    let kind = if is_new { "create" } else { "edit" };
    let heading = if is_new { "Create PortalTrackerIndex" } else { "Edit PortalTrackerIndex" };
    let btn = if is_new { "Create" } else { "Update" };
    let new_li = if !is_new { "<li><a class=\"create\" href=\"/portalTrackerIndex/create\">New PortalTrackerIndex</a></li>" } else { "" };

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#{kind}-portalTrackerIndex" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTrackerIndex/index">PortalTrackerIndex List</a></li>
  {new_li}
</ul></div></section>
<section class="row"><div id="{kind}-portalTrackerIndex" class="col-12 content scaffold-{kind}" role="main">
  <h1>{heading}</h1>
  {flash_html}
  <form action="{action}" method="post">
    {csrf}
    <fieldset class="form">
      <div class="fieldcontain required"><label for="name">Name<span class="required-indicator">*</span></label><input type="text" name="name" id="name" value="{name}" required="required"/></div>
      {tracker_input}
      <div class="fieldcontain"><label for="fields">Fields</label><textarea id="fields" name="fields" style="width:40%;">{fields}</textarea></div>
    </fieldset>
    <fieldset class="buttons"><input class="save" type="submit" value="{btn}" /></fieldset>
  </form>
</div></section></div></div>
{tagify_js}"##,
        name = esc(&s(&ix, "name")),
        fields = esc(&s(&ix, "fields"))
    );
    ctx.html(heading, body).await
}

async fn idx_save(mut ctx: Ctx) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if ctx.method != "POST" {
        return ctx.redirect("/portalTrackerIndex/create").await;
    }
    let db = &ctx.st.db.clone();
    let tid = ctx.pi("tracker_id").or_else(|| ctx.pi("tracker")).unwrap_or(0);
    if dev_tracker(&ctx, tid).await.is_none() {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let name = ctx.p("name").trim().to_string();
    if name.is_empty() {
        ctx.flash("Name is required");
        return ctx.redirect(&format!("/portalTrackerIndex/create?tracker_id={tid}")).await;
    }
    let fields = parse_tagify_csv(&ctx.p("fields"));
    match db.insert("portal_tracker_index", &row! {"tracker_id"=>tid,"name"=>name,"fields"=>fields}).await {
        Ok(iid) => {
            ctx.flash(&format!("PortalTrackerIndex {iid} created"));
            ctx.redirect(&format!("/portalTracker/show/{tid}#index-tab")).await
        }
        Err(e) => {
            ctx.flash(&format!("Could not create PortalTrackerIndex: {e}"));
            ctx.redirect(&format!("/portalTrackerIndex/create?tracker_id={tid}")).await
        }
    }
}

async fn idx_update(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if ctx.method != "POST" {
        return ctx.redirect(&format!("/portalTrackerIndex/edit/{id}")).await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(ex)) = db.get("portal_tracker_index", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let tid = i(&ex, "tracker_id").unwrap_or(0);
    if dev_tracker(&ctx, tid).await.is_none() {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let name = ctx.p("name").trim().to_string();
    if name.is_empty() {
        ctx.flash("Name is required");
        return ctx.redirect(&format!("/portalTrackerIndex/edit/{id}")).await;
    }
    let fields = parse_tagify_csv(&ctx.p("fields"));
    match db.update("portal_tracker_index", id, &row! {"name"=>name,"fields"=>fields}).await {
        Ok(_) => {
            ctx.flash(&format!("PortalTrackerIndex {id} updated"));
            ctx.redirect(&format!("/portalTracker/show/{tid}#index-tab")).await
        }
        Err(e) => {
            ctx.flash(&format!("Could not update PortalTrackerIndex: {e}"));
            ctx.redirect(&format!("/portalTrackerIndex/edit/{id}")).await
        }
    }
}

async fn idx_delete(mut ctx: Ctx, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if ctx.method != "POST" {
        return ctx.status(StatusCode::METHOD_NOT_ALLOWED, "POST required").await;
    }
    let db = &ctx.st.db.clone();
    let Ok(Some(ex)) = db.get("portal_tracker_index", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let tid = i(&ex, "tracker_id").unwrap_or(0);
    let Some(t) = dev_tracker(&ctx, tid).await else {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    };
    drop_single_index(db, &t, &ex).await;
    let _ = db.delete("portal_tracker_index", id).await;
    ctx.flash(&format!("PortalTrackerIndex {id} deleted"));
    ctx.redirect(&format!("/portalTracker/show/{tid}#index-tab")).await
}
