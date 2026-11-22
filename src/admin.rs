//! Generic admin screens (index / show / create / edit / delete / lookup)
//! for every entity in `entity.rs`, with per-entity hooks and extra actions.

use crate::db::{self, b, i, ident, lit, s, vs, Db, Row};
use crate::entity::{label_of, Entity, Scope, F, K};
use crate::row;
use crate::tracker::Tracker;
use crate::web::{self, enc, esc, now_str, Ctx};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use std::collections::HashMap;

/// Definitions that contain code: only developers of the module (or portal admins) may touch them.
const DEV_ONLY: &[&str] = &[
    "portalPage", "portalPageData", "portalTracker", "portalTrackerField", "portalTrackerError", "portalTrackerStatus", "portalTrackerTransition",
    "portalTrackerRole", "portalTrackerEmail", "portalTrackerIndex", "portalTrackerFlow", "portalEndpoint", "portalScheduler",
];

async fn my_modules(ctx: &Ctx, e: &Entity) -> Vec<String> {
    match &ctx.user {
        None => vec![],
        Some(u) => {
            if DEV_ONLY.contains(&e.name) { web::developer_list(ctx.db(), u).await } else { web::admin_list(ctx.db(), u).await }
        }
    }
}

/// SQL restricting an entity's rows to the given modules.
fn scope_filter(ctx: &Ctx, e: &Entity, mods: &[String]) -> Option<String> {
    let list = lit(&json!(mods));
    match e.scope {
        Scope::Global => None,
        Scope::Module(c) => Some(format!("\"{}\" in {}", c, list)),
        Scope::Via(fk, parent) => {
            let p = ctx.st.entity(parent)?;
            let inner = scope_filter(ctx, p, mods)?;
            Some(format!("\"{}\" in (select id from \"{}\" where {})", fk, p.table, inner))
        }
    }
}

async fn row_module(ctx: &Ctx, e: &Entity, r: &Row) -> Option<String> {
    let mut e = e;
    let mut cur = r.clone();
    for _ in 0..5 {
        match e.scope {
            Scope::Global => return None,
            Scope::Module(c) => return Some(s(&cur, c)),
            Scope::Via(fk, parent) => {
                let p = ctx.st.entity(parent)?;
                cur = ctx.db().get(p.table, i(&cur, fk)?).await.ok().flatten()?;
                e = p;
            }
        }
    }
    None
}

async fn may(ctx: &Ctx, e: &Entity, r: &Row) -> bool {
    if ctx.is_admin() {
        return true;
    }
    if ctx.user.is_none() || e.scope == Scope::Global {
        return false;
    }
    match row_module(ctx, e, r).await {
        Some(m) if !m.is_empty() => my_modules(ctx, e).await.contains(&m),
        _ => false,
    }
}

/// The class name the Grails portal shows in headings ("PortalSetting List").
fn ename(e: &Entity) -> String {
    let mut c = e.name.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Grails scaffold navigation: Home, the list (except on the list itself), New.
fn nav(e: &Entity, extra: &str) -> String {
    nav_for(e, extra, true, true)
}

fn nav_for(e: &Entity, extra: &str, list: bool, create: bool) -> String {
    let n = ename(e);
    let mut o = String::from("<div class=\"nav\" role=\"navigation\">\n                    <ul>\n                        <li><a class=\"home\" href=\"/\">Home</a></li>\n");
    if list {
        o.push_str(&format!("                        <li><a class=\"list\" href=\"/{}/index\">{} List</a></li>\n", e.name, n));
    }
    if create && !e.readonly {
        o.push_str(&format!("                        <li><a class=\"create\" href=\"/{}/create\">New {}</a></li>\n", e.name, n));
    }
    o.push_str(extra);
    o.push_str("                    </ul>\n                </div>");
    o
}

/// The Grails scaffold page frame; `kind` is list / show / edit / create.
fn frame(e: &Entity, kind: &str, nav: &str, title: &str, flash: &str, body: &str) -> String {
    format!(
        "\n    <div id=\"content\" role=\"main\">\n        <div class=\"container\">\n            <section class=\"row\">\n                <a href=\"#{k}-{n}\" class=\"skip\" tabindex=\"-1\">Skip to content&hellip;</a>\n                {nav}\n            </section>\n            <section class=\"row\">\n                <div id=\"{k}-{n}\" class=\"col-12 content scaffold-{k}\" role=\"main\">\n                    <h1>{title}</h1>\n                    {msg}\n{body}\n                </div>\n            </section>\n        </div>\n    </div>\n",
        k = kind, n = e.name, nav = nav, title = esc(title),
        msg = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(flash)) },
        body = body
    )
}

/// Display names for referenced rows, fetched in one query per column.
async fn ref_names(ctx: &Ctx, target: &str, ids: &[i64]) -> HashMap<i64, String> {
    let mut m = HashMap::new();
    let Some(te) = ctx.st.entity(target) else { return m };
    if ids.is_empty() {
        return m;
    }
    let extra = if te.name == "user" { ", userid" } else if te.field("module").is_some() && te.display != "module" { ", module" } else { "" };
    let q = format!("select id, \"{}\"::text as d{} from \"{}\" where id in {}", te.display, extra, te.table, lit(&json!(ids)));
    for r in ctx.db().rows(&q, &db::no_params()).await.unwrap_or_default() {
        let mut d = s(&r, "d");
        if te.name == "user" {
            d = format!("{} ({})", d, s(&r, "userid"));
        } else if r.contains_key("module") && matches!(te.name, "portalPage" | "portalTracker") {
            d = format!("{}: {}", s(&r, "module"), d);
        }
        m.insert(i(&r, "id").unwrap_or(0), d);
    }
    m
}

async fn multi_names(ctx: &Ctx, f: &F, id: i64) -> Vec<(i64, String)> {
    let K::Multi(target, join, own, other) = f.kind else { return vec![] };
    let Some(te) = ctx.st.entity(target) else { return vec![] };
    let q = format!("select t.id, t.\"{}\"::text as d from \"{}\" j join \"{}\" t on t.id=j.\"{}\" where j.\"{}\"={} order by t.id", te.display, join, te.table, other, own, id);
    ctx.db().rows(&q, &db::no_params()).await.unwrap_or_default().iter().map(|r| (i(r, "id").unwrap_or(0), s(r, "d"))).collect()
}

fn cell_text(f: &F, v: &Value, full: bool) -> String {
    match f.kind {
        K::Bool => match v {
            Value::Bool(true) => "<i class='bi bi-check-lg text-success'></i>".into(),
            Value::Bool(false) => "<span class='text-muted'>-</span>".into(),
            _ => String::new(),
        },
        K::Password => if v.is_null() { String::new() } else { "••••••".into() },
        K::Code if full => format!("<pre class='code-view'>{}</pre>", esc(&vs(v))),
        K::Text if full => esc(&vs(v)).replace('\n', "<br/>"),
        _ => {
            let t = vs(v);
            if !full && t.chars().count() > 90 { format!("{}&hellip;", esc(&t.chars().take(90).collect::<String>())) } else { esc(&t) }
        }
    }
}

async fn table(ctx: &Ctx, e: &Entity, rows: &[Row], skip: Option<&str>) -> String {
    let cols: Vec<&F> = e.list.iter().filter(|c| Some(**c) != skip).filter_map(|c| e.field(c)).collect();
    let mut names: HashMap<&str, HashMap<i64, String>> = HashMap::new();
    for f in &cols {
        if let K::Ref(target) = f.kind {
            let mut ids: Vec<i64> = rows.iter().filter_map(|r| i(r, f.name)).collect();
            ids.sort();
            ids.dedup();
            names.insert(f.name, ref_names(ctx, target, &ids).await);
        }
    }
    let q = web::qs(&ctx.params, &["sort", "order", "offset", "max"]);
    let max = ctx.pi("max").unwrap_or(10);
    let mut o = String::from("<table class=\"responsive\">\n    <thead>\n         <tr>\n");
    for f in &cols {
        let sortable = !matches!(f.kind, K::Multi(..));
        if sortable && skip.is_none() {
            let sorted = ctx.p("sort") == f.name;
            let cur = if ctx.p("order") == "desc" { "desc" } else { "asc" };
            let ord = if sorted && cur == "asc" { "desc" } else { "asc" };
            o.push_str(&format!(
                "                <th class=\"sortable{}\" ><a href=\"/{}/index?sort={}&amp;max={}&amp;order={}{}\">{}</a></th>\n",
                if sorted { format!(" sorted {}", cur) } else { String::new() }, e.name, f.name, max, ord, if q.is_empty() { String::new() } else { format!("&amp;{}", esc(&q)) }, esc(&label_of(f.name))
            ));
        } else {
            o.push_str(&format!("                <th>{}</th>\n", esc(&label_of(f.name))));
        }
    }
    o.push_str("        </tr>\n    </thead>\n    <tbody>\n");
    for (rn, r) in rows.iter().enumerate() {
        let id = i(r, "id").unwrap_or(0);
        o.push_str(&format!("            <tr class=\"{}\">\n", if rn % 2 == 0 { "even" } else { "odd" }));
        for (pos, f) in cols.iter().enumerate() {
            let mut cell = match f.kind {
                K::Ref(target) => match i(r, f.name) {
                    Some(rid) => {
                        let n = names.get(f.name).and_then(|m| m.get(&rid)).cloned().unwrap_or_else(|| rid.to_string());
                        if pos == 0 { esc(&n) } else { format!("<a href='/{}/show/{}'>{}</a>", target, rid, esc(&n)) }
                    }
                    None => String::new(),
                },
                K::Multi(..) => multi_names(ctx, f, id).await.iter().map(|(_, n)| esc(n)).collect::<Vec<_>>().join(", "),
                _ => cell_text(f, r.get(f.name).unwrap_or(&Value::Null), false),
            };
            if pos == 0 {
                if cell.trim().is_empty() {
                    cell = format!("#{}", id);
                }
                cell = format!("<a href=\"/{}/show/{}\">{}</a>", e.name, id, cell);
            }
            o.push_str(&format!("                        <td>{}&nbsp;</td>\n", cell));
        }
        o.push_str("            </tr>\n");
    }
    o.push_str("    </tbody>\n</table>\n");
    o
}

pub async fn index(ctx: Ctx, e: &Entity) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let mut cond: Vec<String> = vec![];
    if !ctx.is_admin() {
        let mods = my_modules(&ctx, e).await;
        match scope_filter(&ctx, e, &mods) {
            Some(f) if !mods.is_empty() => cond.push(f),
            _ => return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this list").await,
        }
    }
    let q = ctx.p("q");
    if !q.trim().is_empty() {
        let like = lit(&json!(format!("%{}%", q.trim())));
        let ors: Vec<String> = e.fields.iter().filter(|f| matches!(f.kind, K::Str | K::Text | K::Code | K::Choice(_))).map(|f| format!("\"{}\" ilike {}", f.name, like)).collect();
        if !ors.is_empty() {
            cond.push(format!("({})", ors.join(" or ")));
        }
    }
    for f in &e.fields {
        if matches!(f.kind, K::Multi(..) | K::Password) {
            continue;
        }
        let v = ctx.p(f.name);
        if !v.is_empty() && v != "All" && ctx.params.contains_key(f.name) {
            match f.kind {
                K::Ref(_) | K::Int => {
                    if let Ok(n) = v.parse::<i64>() {
                        cond.push(format!("\"{}\"={}", f.name, n));
                    }
                }
                K::Bool => cond.push(format!("\"{}\" is {}", f.name, if v == "true" || v == "1" { "true" } else { "not true" })),
                _ => cond.push(format!("\"{}\"={}", f.name, lit(&json!(v)))),
            }
        }
    }
    let wh = if cond.is_empty() { String::new() } else { format!(" where {}", cond.join(" and ")) };
    let max = ctx.pi("max").unwrap_or(10).clamp(1, 500);
    let offset = ctx.pi("offset").unwrap_or(0).max(0);
    let sort = ctx.p("sort");
    let order = match e.field(&sort) {
        Some(f) if !matches!(f.kind, K::Multi(..)) => format!("\"{}\" {}", f.name, if ctx.p("order") == "desc" { "desc" } else { "asc" }),
        _ => e.order.to_string(),
    };
    let total = ctx.db().count(&format!("select count(*) from \"{}\"{}", e.table, wh), &db::no_params()).await.unwrap_or(0);
    let rows = ctx.db().rows(&format!("select * from \"{}\"{} order by {} limit {} offset {}", e.table, wh, order, max, offset), &db::no_params()).await.unwrap_or_default();
    let extra = index_extras(e, ctx.is_admin());
    let m = ctx.p("module");
    let navh = if e.name != "portalModule" && e.field("module").is_some() && !m.is_empty() && m != "All" {
        let back = module_url(ctx.db(), &m).await.map(|u| format!("                        <li><a class=\"list\" href=\"{}\">Module {}</a></li>\n", u, esc(&m))).unwrap_or_default();
        nav_for(e, &format!("{}{}", extra, back), false, true).replace(&format!("href=\"/{}/create\"", e.name), &format!("href=\"/{}/create?module={}\"", e.name, enc(&m)))
    } else {
        nav_for(e, &extra, false, true)
    };
    let mut h = format!(
        "\t\t    <form action=\"/{n}/index\" method=\"get\" name=\"searchform\" id=\"searchform\" >\n\t\t\t<fieldset class='form'>\n                        <div class='fieldcontain' id='listsearch'>\n                        <label>Search:</label><input type='text' name='q' id='q' value='{q}'/><input id='dosearch' name='dosearch' type='submit' value='Search'/>\n                        </div>\n",
        n = e.name, q = esc(&q)
    );
    if e.field("module").is_some() && e.name != "portalModule" {
        let mut mods: Vec<String> = ctx.db().rows("select name from portal_module order by name", &db::no_params()).await.unwrap_or_default().iter().map(|x| s(x, "name")).collect();
        if !ctx.is_admin() {
            let mine = my_modules(&ctx, e).await;
            mods.retain(|x| mine.contains(x));
        }
        h.push_str("                        <div class='fieldcontain' id='listsearch'>\n                        <label>Module:</label>\n                          <select name='module' id='module' class='module-select ' onchange='this.form.submit()'><option value='All'>All</option>");
        for x in mods {
            h.push_str(&format!("<option value='{}'{}>{}</option>", esc(&x), if x == m { " selected" } else { "" }, esc(&x)));
        }
        h.push_str("</select>\n                        </div>\n");
    }
    h.push_str("\t\t\t</fieldset>\n\t\t    </form>\n");
    h.push_str(&table(&ctx, e, &rows, None).await);
    if total > max {
        h.push_str(&format!("\n                    <div class=\"pagination\">\n                        {}\n                    </div>\n", crate::tags::paginate(&format!("/{}/index", e.name), &ctx.params, total, max, offset)));
    }
    let flash = s(&ctx.sess, "flash");
    let page = frame(e, "list", &navh, &format!("{} List", ename(e)), &flash, &h);
    ctx.html(&format!("{} List", ename(e)), page).await
}

/// Link to the details page of the module called `name`.
pub async fn module_url(db: &Db, name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let r = db.first("select id from portal_module where name=:n", &row! {"n"=>name}).await.ok().flatten()?;
    Some(format!("/portalModule/show/{}", i(&r, "id")?))
}

/// The logs share one sidebar entry ("Logs", opening the error log); each log
/// list links to the others. All of them are portal-admin only.
const LOGS: &[(&str, &str)] = &[("portalErrorLog", "Error Log"), ("portalAuditTrail", "Audit Trail"), ("userLog", "User Log"), ("portalBackgroundTrail", "Background Jobs")];

fn li(class: &str, href: &str, label: &str) -> String {
    format!("                        <li><a class=\"{}\" href=\"{}\">{}</a></li>\n", class, href, label)
}

fn index_extras(e: &Entity, is_admin: bool) -> String {
    let mut o = match e.name {
        "portalErrorLog" => li("list", "/portalErrorLog/index?resolved=false", "Unresolved only"),
        "portalSchedulerRun" => format!("{}{}", li("list", "/portalScheduler/status", "Scheduler Status"), li("list", "/portalScheduler/index", "Scheduler List")),
        "userRole" if is_admin => li("list", "/user/index", "User List"),
        _ => String::new(),
    };
    if LOGS.iter().any(|(n, _)| *n == e.name) {
        for (n, label) in LOGS.iter().filter(|(n, _)| *n != e.name) {
            o.push_str(&li("list", &format!("/{}/index", n), label));
        }
    }
    o
}

fn post_button(ctx: &mut Ctx, url: &str, label: &str, class: &str, confirm: Option<&str>) -> String {
    format!(
        "<form method='post' action='{}' style='display:inline'{}>{}<button class='btn btn-sm {}'>{}</button></form> ",
        url, confirm.map(|c| format!(" onsubmit=\"return confirm('{}')\"", c)).unwrap_or_default(), ctx.csrf_field(), class, label
    )
}

async fn show_extras(ctx: &mut Ctx, e: &Entity, r: &Row) -> String {
    let id = i(r, "id").unwrap_or(0);
    let mut o = String::new();
    match e.name {
        "portalErrorLog" => o.push_str(&post_button(ctx, &format!("/portalErrorLog/resolve/{}", id), "Mark resolved", "btn-outline-primary", None)),
        "portalEndpoint" => o.push_str(&format!("<code>/svc/{}/{}</code> ", esc(&s(r, "module")), esc(&s(r, "slug")))),
        "portalModuleAccessReview" => o.push_str(&post_button(ctx, &format!("/portalModuleAccessReview/approve/{}", id), "Approve review", "btn-outline-success", Some("Confirm that the access list of this module is correct?"))),
        _ => {}
    }
    o
}

fn fmt_bool_tf(v: Option<&Value>) -> &'static str {
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

fn fmt_dt_show(raw: &str) -> String {
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

/// Match g6ikram's `views/portalPage/show.gsp` + `datasources/_displayWrapper.gsp`
/// + `content/_displayWrapper.gsp`.
async fn show_portal_page(mut ctx: Ctx, e: &Entity, r: &Row, id: i64) -> Response {
    let m = s(r, "module");
    let sl = s(r, "slug");
    let runable = b(r, "runable");
    let mod_cell = match module_url(ctx.db(), &m).await {
        Some(u) => format!("<a href=\"{u}\">{}</a>", esc(&m)),
        None => esc(&m),
    };
    let content_raw = s(r, "content");
    let content_cell = if content_raw.is_empty() {
        String::new()
    } else {
        format!("<pre class=\"code-view\">{}</pre>", esc(&content_raw))
    };
    let pre_raw = s(r, "preprocess");
    let pre_cell = if pre_raw.is_empty() {
        String::new()
    } else {
        format!("<pre class=\"code-view\">{}</pre>", esc(&pre_raw))
    };

    let dss = ctx
        .db()
        .rows(
            &format!("select * from \"{}\" where page_id=:id order by id", db::page_data_t()),
            &row! {"id"=>id},
        )
        .await
        .unwrap_or_default();
    let mut ds_items = String::new();
    for ds in &dss {
        let dsid = i(ds, "id").unwrap_or(0);
        ds_items.push_str(&format!(
            "<li><a class=\"create\" href=\"/portalPageData/edit/{dsid}?page_id={id}\">{}</a> : {}</li>\n",
            esc(&s(ds, "name")),
            esc(&s(ds, "query")),
        ));
    }
    ds_items.push_str(&format!("<li><a class=\"create\" href=\"/portalPageData/create?page_id={id}\">Add Datasource</a></li>\n"));

    let prop = |key: &str, label: &str, val: &str| {
        format!(
            "    <li class=\"fieldcontain\">\n        <span id=\"{key}-label\" class=\"property-label\">{label}</span>\n        <div class=\"property-value\" aria-labelledby=\"{key}-label\">{val}</div>\n    </li>\n"
        )
    };
    let mut h = String::from("<ol class=\"property-list portalPage\">\n");
    h.push_str(&prop("title", "Title", &esc(&s(r, "title"))));
    h.push_str(&prop("slug", "Slug", &esc(&sl)));
    h.push_str(&prop("module", "Module", &mod_cell));
    h.push_str(&prop("published", "Published", fmt_bool_tf(r.get("published"))));
    h.push_str(&prop("requirelogin", "Requirelogin", fmt_bool_tf(r.get("requirelogin"))));
    h.push_str(&prop("allowedroles", "Allowedroles", &esc(&s(r, "allowedroles"))));
    h.push_str(&prop("runable", "Runable", fmt_bool_tf(r.get("runable"))));
    h.push_str(&prop("redirectafter", "Redirectafter", &esc(&s(r, "redirectafter"))));
    h.push_str(&prop("fullpage", "Fullpage", fmt_bool_tf(r.get("fullpage"))));
    h.push_str(&prop("render", "Render", &esc(&s(r, "render"))));
    h.push_str(&prop("lastUpdated", "Last Updated", &esc(&fmt_dt_show(&s(r, "last_updated")))));
    h.push_str(&prop("side_menu", "Side Menu", &esc(&s(r, "side_menu"))));
    h.push_str(&prop("content", "Content", &content_cell));
    h.push_str(&prop("preprocess", "Preprocess", &pre_cell));
    h.push_str(&prop("datasources", "Datasources", &format!("<a id=\"dslist\"></a>\n<ul>\n{ds_items}</ul>")));
    h.push_str("</ol>\n");
    h.push_str(&format!(
        "                    <form action=\"/portalPage/delete/{id}\" method=\"post\">{}\n                        <fieldset class=\"buttons\">\n                            <a href=\"/portalPage/edit/{id}\" class=\"edit\">Edit</a>\n                            <input class=\"delete\" type=\"submit\" value=\"Delete\" onclick=\"return confirm('Are you sure?');\" />\n                        </fieldset>\n                    </form>",
        ctx.csrf_field()
    ));

    let view_or_run = if runable {
        li("create", &format!("/run/{}/{}", enc(&m), enc(&sl)), "Run")
    } else {
        li("create", &format!("/view/{}/{}", enc(&m), enc(&sl)), "View")
    };
    let extra_nav = format!(
        "{}{}{}",
        li("edit", &format!("/portalPage/edit/{id}"), "Edit"),
        view_or_run,
        li("list", "#dslist", "Datasources"),
    );
    let flash = s(&ctx.sess, "flash");
    let page = frame(e, "show", &nav(e, &extra_nav), "Show PortalPage", &flash, &h);
    ctx.html("Show PortalPage", page).await
}

pub async fn show(mut ctx: Ctx, e: &Entity, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let Ok(Some(r)) = ctx.db().get(e.table, id).await else { return ctx.status(StatusCode::NOT_FOUND, "Not found").await };
    if !may(&ctx, e, &r).await {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this").await;
    }
    if e.name == "portalPage" {
        return show_portal_page(ctx, e, &r, id).await;
    }
    let mut h = String::new();
    let extras = show_extras(&mut ctx, e, &r).await;
    if !extras.is_empty() {
        h.push_str(&format!("<fieldset class=\"buttons\">{}</fieldset>", extras));
    }
    h.push_str(&format!("<ol class=\"property-list {}\">\n", e.name));
    for f in &e.fields {
        let v = r.get(f.name).cloned().unwrap_or(Value::Null);
        let cell = match f.kind {
            K::Ref(target) => match i(&r, f.name) {
                Some(rid) => {
                    let n = ref_names(&ctx, target, &[rid]).await.get(&rid).cloned().unwrap_or_else(|| rid.to_string());
                    format!("<a href=\"/{}/show/{}\">{}</a>", target, rid, esc(&n))
                }
                None => String::new(),
            },
            K::Multi(target, ..) => multi_names(&ctx, f, id).await.iter().map(|(mid, n)| format!("<a href=\"/{}/show/{}\">{}</a>", target, mid, esc(n))).collect::<Vec<_>>().join(", "),
            K::Str if f.name == "module" && e.name != "portalModule" => match module_url(ctx.db(), &vs(&v)).await {
                Some(u) => format!("<a href=\"{}\">{}</a>", u, esc(&vs(&v))),
                None => esc(&vs(&v)),
            },
            _ => cell_text(f, &v, true),
        };
        if cell.is_empty() && f.hidden {
            continue;
        }
        h.push_str(&format!(
            "    <li class=\"fieldcontain\">\n        <span id=\"{n}-label\" class=\"property-label\">{}</span>\n        <div class=\"property-value\" aria-labelledby=\"{n}-label\">{}</div>\n    </li>\n",
            esc(&label_of(f.name)), cell, n = f.name
        ));
    }
    h.push_str("</ol>\n");
    if !e.readonly {
        h.push_str(&format!(
            "                    <form action=\"/{n}/delete/{id}\" method=\"post\" >{}\n                        <fieldset class=\"buttons\">\n                            <a href=\"/{n}/edit/{id}\" class=\"edit\">Edit</a>\n                            <input class=\"delete\" type=\"submit\" value=\"Delete\" onclick=\"return confirm('Are you sure?');\" />\n                        </fieldset>\n                    </form>",
            ctx.csrf_field(), n = e.name, id = id
        ));
    }
    for (child, fk) in e.children {
        let Some(ce) = ctx.st.entity(child) else { continue };
        let rows = ctx.db().rows(&format!("select * from \"{}\" where \"{}\"={} order by {} limit 500", ce.table, fk, id, ce.order), &db::no_params()).await.unwrap_or_default();
        h.push_str(&format!("<h3>{}s</h3><div class=\"nav\" role=\"navigation\"><ul><li><a class=\"create\" href=\"/{}/create?{}={}\">Add {}</a></li></ul></div>", ce.label, ce.name, fk, id, ce.label));
        h.push_str(&table(&ctx, ce, &rows, Some(fk)).await);
        h.push_str("<br/>");
    }
    let flash = s(&ctx.sess, "flash");
    let page = frame(e, "show", &nav(e, ""), &format!("Show {}", ename(e)), &flash, &h);
    ctx.html(&format!("Show {}", ename(e)), page).await
}

/// Which rows a reference select offers, given the row being edited.
async fn ref_options(ctx: &Ctx, e: &Entity, target: &Entity, r: &Row) -> (Vec<(i64, String)>, bool) {
    if target.name == "user" {
        return (vec![], true);
    }
    let mut cond = vec![];
    if target.field("tracker_id").is_some() {
        let tid = if e.name == "portalTracker" {
            i(r, "id")
        } else if let Some(t) = i(r, "tracker_id") {
            Some(t)
        } else if let Some(fid) = i(r, "field_id") {
            ctx.db().get("portal_tracker_field", fid).await.ok().flatten().and_then(|f| i(&f, "tracker_id"))
        } else {
            None
        };
        match tid {
            Some(t) => cond.push(format!("tracker_id={}", t)),
            None => {
                if e.name == "portalTracker" {
                    return (vec![], false);
                }
            }
        }
    }
    if target.name == "portalPage" {
        if let Some(m) = row_module(ctx, e, r).await.filter(|m| !m.is_empty()) {
            cond.push(format!("module={}", lit(&json!(m))));
        }
    }
    if target.name == "portalTreeNode" {
        if let Some(t) = i(r, "tree_id") {
            cond.push(format!("tree_id={}", t));
        }
    }
    if !ctx.is_admin() {
        let mods = my_modules(ctx, target).await;
        if let Some(f) = scope_filter(ctx, target, &mods) {
            cond.push(f);
        }
    }
    let wh = if cond.is_empty() { String::new() } else { format!(" where {}", cond.join(" and ")) };
    let total = ctx.db().count(&format!("select count(*) from \"{}\"{}", target.table, wh), &db::no_params()).await.unwrap_or(0);
    if total > 400 {
        return (vec![], true);
    }
    let extra = if target.field("module").is_some() && target.display != "module" { ", module" } else { "" };
    let rows = ctx.db().rows(&format!("select id, \"{}\"::text as d{} from \"{}\"{} order by 2", target.display, extra, target.table, wh), &db::no_params()).await.unwrap_or_default();
    (rows.iter().map(|x| (i(x, "id").unwrap_or(0), if x.contains_key("module") && !s(x, "module").is_empty() { format!("{}: {}", s(x, "module"), s(x, "d")) } else { s(x, "d") })).collect(), false)
}

fn is_vim_setting(val: Option<&str>) -> bool {
    matches!(val.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("vim" | "vi"))
}

/// Matches the Grails portal's `PortalSetting.namedefault(curuser.userID + '_editor', 'normal') == 'vim'`.
async fn use_vim_editor(ctx: &Ctx) -> bool {
    let staffid = ctx.user.as_ref().map(|u| s(u, "userid")).unwrap_or_default();
    if staffid.is_empty() {
        return false;
    }
    let v = web::setting_str(ctx.db(), &format!("{}_editor", staffid)).await;
    is_vim_setting(v.as_deref())
}

fn page_ace_js(vim_mode: bool) -> String {
    let vim_script = if vim_mode {
        "\n<script src=\"/static/libs/ace_editor/js/keybinding-vim.js\"></script>"
    } else {
        ""
    };
    format!(
        r#"<link href="/static/libs/ace_editor/css/theme/github.css" rel="stylesheet"/>
<script src="/static/libs/ace_editor/js/ace.js"></script>
<script src="/static/libs/ace_editor/js/theme-github.js"></script>
<script src="/static/libs/ace_editor/js/mode-html.js"></script>
<script src="/static/libs/ace_editor/js/mode-rust.js"></script>{vim_script}
<script>
$(function(){{
  if (typeof ace === 'undefined') {{
    $('textarea.ace-source').show();
    $('.ace-outer').hide();
    return;
  }}
  ace.config.set('basePath', '/static/libs/ace_editor/js');
  var vimMode = {vim};
  var editors = {{}};
  function syncAll() {{
    Object.keys(editors).forEach(function(k){{
      var ta = document.getElementById(k);
      if (ta) ta.value = editors[k].getValue();
    }});
  }}
  function triggerSubmit(preferId) {{
    syncAll();
    var btn = document.getElementById(preferId) || document.getElementById('savebutton');
    if (btn) btn.click();
  }}
  function updateContentMode() {{
    var ed = editors.content;
    if (!ed) return;
    var modeName = $('#runable').prop('checked') ? 'ace/mode/rust' : 'ace/mode/html';
    ed.session.setMode(modeName);
  }}
  $('textarea.ace-source').each(function(){{
    var ta = this;
    var field = ta.getAttribute('data-ace-field');
    var ed = ace.edit(field + 'div');
    ed.setTheme('ace/theme/github');
    ed.session.setValue(ta.value, -1);
    ed.session.on('change', function(){{ ta.value = ed.getValue(); }});
    if (vimMode) {{
      ed.setKeyboardHandler('ace/keyboard/vim');
    }}
    ed.commands.addCommand({{
      name: 'Toggle Fullscreen',
      bindKey: 'F11',
      exec: function(editor) {{
        $('body').toggleClass('fullScreen');
        $('#' + field + 'div.ace_editor').toggleClass('fullScreen');
        editor.setAutoScrollEditorIntoView(true);
        editor.resize();
      }}
    }});
    ed.commands.addCommand({{
      name: 'Save Page',
      bindKey: {{ win: 'Ctrl-S', mac: 'Command-S' }},
      exec: function() {{ triggerSubmit('updatebutton'); }}
    }});
    var outer = document.getElementById('outer' + field);
    if (outer && typeof ResizeObserver !== 'undefined') {{
      new ResizeObserver(function(){{ ed.resize(); }}).observe(outer);
    }}
    editors[field] = ed;
  }});
  if (editors.preprocess) {{
    editors.preprocess.session.setMode('ace/mode/rust');
  }}
  updateContentMode();
  $('#runable').on('change', updateContentMode);
  $('.content form').on('submit', syncAll);
  if (vimMode) {{
    ace.config.loadModule('ace/keyboard/vim', function(module) {{
      var VimApi = (module && module.CodeMirror && module.CodeMirror.Vim) || (module && module.Vim);
      if (!VimApi) return;
      VimApi.defineEx('write', 'w', function() {{
        triggerSubmit('updatebutton');
      }});
      VimApi.defineEx('wq', 'wq', function() {{
        triggerSubmit('savebutton');
      }});
    }});
  }}
}});
</script>"#,
        vim_script = vim_script,
        vim = if vim_mode { "true" } else { "false" }
    )
}

async fn form(ctx: &mut Ctx, e: &Entity, r: &Row, id: Option<i64>) -> String {
    let is_page = e.name == "portalPage";
    let action = match id {
        Some(x) => format!("/{}/update/{}", e.name, x),
        None => format!("/{}/save", e.name),
    };
    let mut h = format!("<form action=\"{}\" method=\"post\" >{}\n                        <fieldset class=\"form\">\n", action, ctx.csrf_field());
    for f in e.fields.iter().filter(|f| !f.hidden) {
        let n = f.name;
        let v = r.get(n).cloned().unwrap_or(Value::Null);
        let t = vs(&v);
        let label = esc(&label_of(n));
        let input = match f.kind {
            K::Str if n == "module" && e.name != "portalModule" => {
                let mut mods: Vec<String> = ctx.db().rows("select name from portal_module order by name", &db::no_params()).await.unwrap_or_default().iter().map(|m| s(m, "name")).collect();
                if !ctx.is_admin() {
                    let mine = my_modules(ctx, e).await;
                    mods.retain(|m| mine.contains(m));
                }
                if !t.is_empty() && !mods.contains(&t) {
                    mods.push(t.clone());
                }
                if mods.is_empty() {
                    mods.push("portal".into());
                }
                let mut o = format!("<select name='{n}' id='{n}' class='module-select '>");
                if t.is_empty() {
                    o.push_str("<option value=''>- choose a module -</option>");
                }
                for m in mods {
                    o.push_str(&format!("<option value='{}'{}>{}</option>", esc(&m), if m == t { " selected" } else { "" }, esc(&m)));
                }
                o.push_str("</select>");
                o
            }
            K::Str => format!("<input type=\"text\" name=\"{n}\" value=\"{}\" id=\"{n}\" />", esc(&t)),
            K::Password => format!("<input type=\"password\" name=\"{n}\" value=\"\" id=\"{n}\" autocomplete=\"new-password\" placeholder=\"{}\" />", if id.is_some() { "leave blank to keep the current password" } else { "" }),
            K::Text => format!("<textarea name=\"{n}\" id=\"{n}\" >{}</textarea>", esc(&t)),
            K::Code if is_page && matches!(n, "content" | "preprocess") => format!(
                "<div id=\"outer{n}\" class=\"ace-outer ace-outer-{n}\"><div id=\"{n}div\" class=\"ace-box\"></div></div><textarea name=\"{n}\" id=\"{n}\" class=\"code-edit ace-source\" data-ace-field=\"{n}\" style=\"display:none\">\n{}</textarea>",
                esc(&t)
            ),
            K::Code => format!("<textarea name=\"{n}\" id=\"{n}\" class=\"code-edit\" rows=\"{}\" spellcheck=\"false\" >{}</textarea>", if n == "content" { 24 } else { 6 }, esc(&t)),
            K::Bool => format!("<input type=\"hidden\" name=\"_{n}\" /><input type=\"checkbox\" name=\"{n}\"{} id=\"{n}\" />", if v == json!(true) { " checked=\"checked\"" } else { "" }),
            K::Int => format!("<input type=\"number\" name=\"{n}\" value=\"{}\" id=\"{n}\" />", esc(&t)),
            K::Date => format!("<input type=\"datetime-local\" name=\"{n}\" value=\"{}\" id=\"{n}\" />", esc(&t.replace(' ', "T").chars().take(16).collect::<String>())),
            K::Choice(opts) => {
                let mut o = format!("<select name=\"{n}\" id=\"{n}\" >");
                if !opts.contains(&"") && id.is_none() && t.is_empty() {
                    // keep the first option as the default
                }
                for op in opts {
                    o.push_str(&format!("<option value='{}'{}>{}</option>", esc(op), if *op == t { " selected" } else { "" }, esc(op)));
                }
                o.push_str("</select>");
                o
            }
            K::Ref(target) => {
                let Some(te) = ctx.st.entity(target) else { continue };
                let (opts, ajax) = ref_options(ctx, e, te, r).await;
                let cur = i(r, n);
                if ajax {
                    let mut opt = String::new();
                    if let Some(c) = cur {
                        let nm = ref_names(ctx, target, &[c]).await.get(&c).cloned().unwrap_or_default();
                        opt = format!("<option value='{}' selected>{}</option>", c, esc(&nm));
                    }
                    format!("<select class=\"ajax-select\" style=\"width: 40%;\" data-url=\"/{}/lookup\" name=\"{n}\" id=\"{n}\">{}</select>", target, opt)
                } else {
                    let mut o = format!("<select name=\"{n}\" id=\"{n}\" ><option value=\"null\"></option>");
                    for (oid, on) in opts {
                        o.push_str(&format!("<option value='{}'{}>{}</option>", oid, if Some(oid) == cur { " selected" } else { "" }, esc(&on)));
                    }
                    o.push_str("</select>");
                    o
                }
            }
            K::Multi(target, ..) => {
                let Some(te) = ctx.st.entity(target) else { continue };
                let (opts, _) = ref_options(ctx, e, te, r).await;
                let cur: Vec<i64> = match id {
                    Some(x) => multi_names(ctx, f, x).await.iter().map(|(a, _)| *a).collect(),
                    None => vec![],
                };
                let mut o = format!("<select multiple name=\"{n}\" id=\"{n}\" >");
                for (oid, on) in opts {
                    o.push_str(&format!("<option value='{}'{}>{}</option>", oid, if cur.contains(&oid) { " selected" } else { "" }, esc(&on)));
                }
                o.push_str("</select>");
                o
            }
        };
        h.push_str(&format!("<div class='fieldcontain'>\n  <label for='{n}'>{}</label>{}\n</div>", label, input));
    }
    if is_page && id.is_some() {
        h.push_str(
            "                        </fieldset>\n                        <fieldset class=\"buttons\">\n                            <input id=\"savebutton\" class=\"save\" type=\"submit\" name=\"submitpage\" value=\"Save\" />\n                            <input id=\"updatebutton\" class=\"save\" type=\"submit\" name=\"submitpage\" value=\"Update\" />\n                        </fieldset>\n                    </form>",
        );
    } else if is_page {
        h.push_str(
            "                        </fieldset>\n                        <fieldset class=\"buttons\">\n                            <input id=\"savebutton\" class=\"save\" type=\"submit\" value=\"Create\" />\n                        </fieldset>\n                    </form>",
        );
    } else {
        h.push_str(&format!(
            "                        </fieldset>\n                        <fieldset class=\"buttons\">\n                            <input class=\"save\" type=\"submit\" value=\"{}\" />\n                        </fieldset>\n                    </form>",
            if id.is_some() { "Update" } else { "Create" }
        ));
    }
    h.push_str(crate::tags::form_js());
    if is_page {
        h.push_str(&page_ace_js(use_vim_editor(ctx).await));
    }
    h
}

pub async fn create(mut ctx: Ctx, e: &Entity) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if e.readonly {
        return ctx.status(StatusCode::FORBIDDEN, "Read only").await;
    }
    // prefill from the query string (e.g. ?tracker_id=5)
    let mut r = Row::new();
    for f in &e.fields {
        if ctx.params.contains_key(f.name) {
            let v = ctx.p(f.name);
            r.insert(f.name.to_string(), match f.kind { K::Ref(_) | K::Int => v.parse::<i64>().map(|x| json!(x)).unwrap_or(Value::Null), _ => json!(v) });
        }
    }
    if e.name == "portalPage" {
        r.entry("requirelogin".to_string()).or_insert(json!(true));
    }
    if e.name == "portalPageData" && !r.contains_key("page_id") && ctx.params.contains_key("page") {
        if let Ok(pid) = ctx.p("page").parse::<i64>() {
            r.insert("page_id".into(), json!(pid));
        }
    }
    let body = form(&mut ctx, e, &r, None).await;
    let flash = s(&ctx.sess, "flash");
    let page = frame(e, "create", &nav_for(e, "", true, false), &format!("Create {}", ename(e)), &flash, &body);
    ctx.html(&format!("Create {}", ename(e)), page).await
}

pub async fn edit(mut ctx: Ctx, e: &Entity, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let Ok(Some(r)) = ctx.db().get(e.table, id).await else { return ctx.status(StatusCode::NOT_FOUND, "Not found").await };
    if e.readonly || !may(&ctx, e, &r).await {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to edit this").await;
    }
    let extra_nav = if e.name == "portalPage" {
        let (m, sl) = (s(&r, "module"), s(&r, "slug"));
        if b(&r, "runable") {
            li("create", &format!("/run/{}/{}", enc(&m), enc(&sl)), "Run")
        } else {
            li("create", &format!("/view/{}/{}", enc(&m), enc(&sl)), "View")
        }
    } else {
        String::new()
    };
    let body = form(&mut ctx, e, &r, Some(id)).await;
    let flash = s(&ctx.sess, "flash");
    let page = frame(e, "edit", &nav(e, &extra_nav), &format!("Edit {}", ename(e)), &flash, &body);
    ctx.html(&format!("Edit {}", ename(e)), page).await
}

fn slug_ok(x: &str) -> bool {
    !x.is_empty() && x.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == ' ')
}

/// Collect and validate posted values. Returns the column map or an error message.
async fn gather(ctx: &Ctx, e: &Entity, existing: Option<&Row>) -> Result<Row, String> {
    let mut d = Row::new();
    for f in e.fields.iter().filter(|f| !f.hidden) {
        let n = f.name;
        let raw = ctx.p(n);
        let v = match f.kind {
            K::Multi(..) => continue,
            K::Bool => json!(ctx.params.contains_key(n) && raw != "false" && raw != "0"),
            K::Int | K::Ref(_) => raw.trim().parse::<i64>().map(|x| json!(x)).unwrap_or(Value::Null), // "" and "null" are no value
            K::Date => if raw.trim().is_empty() { Value::Null } else { json!(raw.trim().replace('T', " ")) },
            K::Password => {
                if raw.is_empty() {
                    continue;
                }
                if raw.len() < 8 {
                    return Err("The password must be at least 8 characters".into());
                }
                json!(bcrypt::hash(&raw, 12).map_err(|e| e.to_string())?)
            }
            K::Code | K::Text => if raw.is_empty() { Value::Null } else { json!(raw.replace("\r\n", "\n")) },
            _ => if raw.trim().is_empty() { Value::Null } else { json!(raw.trim()) },
        };
        d.insert(n.to_string(), v);
    }
    match e.name {
        "portalTrackerField" => {
            if s(&d, "name").is_empty() {
                let label = s(&d, "label");
                let mut name: String = label.trim().replace(' ', "_").chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').collect::<String>().to_lowercase();
                while name.contains("__") {
                    name = name.replace("__", "_");
                }
                d.insert("name".into(), json!(name));
            }
            if s(&d, "label").is_empty() {
                d.insert("label".into(), json!(label_of(&s(&d, "name"))));
            }
            if !db::ident_ok(&s(&d, "name")) {
                return Err("Field name must start with a letter and contain only letters, numbers and underscores".into());
            }
            if d.get("tracker_id").map(|v| v.is_null()).unwrap_or(true) {
                return Err("A field must belong to a tracker".into());
            }
        }
        "portalTracker" => {
            for k in ["module", "slug"] {
                if !db::ident_ok(&s(&d, k)) {
                    return Err(format!("{} must contain only letters, numbers and underscores", k));
                }
            }
            if s(&d, "name").is_empty() {
                return Err("Name is required".into());
            }
            for k in ["datatable", "trailtable"] {
                if !s(&d, k).is_empty() && !db::ident_ok(&s(&d, k)) {
                    return Err(format!("{} is not a valid table name", k));
                }
            }
            let dup = ctx.db().count("select count(*) from portal_tracker where module=:m and slug=:s and id!=:id", &row! {"m"=>s(&d, "module"),"s"=>s(&d, "slug"),"id"=>existing.and_then(|x| i(x, "id")).unwrap_or(0)}).await.unwrap_or(0);
            if dup > 0 {
                return Err("A tracker with that module and slug already exists".into());
            }
        }
        "portalPage" => {
            if !slug_ok(&s(&d, "slug")) || s(&d, "slug").contains(' ') {
                return Err("Slug must contain only letters, numbers, underscores and dashes".into());
            }
            if s(&d, "module").is_empty() {
                d.insert("module".into(), json!("portal"));
            }
            if s(&d, "title").is_empty() {
                d.insert("title".into(), json!(s(&d, "slug")));
            }
            let dup = ctx.db().count(&format!("select count(*) from {} where module=:m and slug=:s and id!=:id", db::page_t()), &row! {"m"=>s(&d, "module"),"s"=>s(&d, "slug"),"id"=>existing.and_then(|x| i(x, "id")).unwrap_or(0)}).await.unwrap_or(0);
            if dup > 0 {
                return Err("A page with that module and slug already exists".into());
            }
            d.insert("last_updated".into(), json!(now_str()));
        }
        "user" => {
            let sid = s(&d, "userid");
            if sid.len() < 3 || !sid.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
                return Err("User ID must be letters and numbers".into());
            }
            let dup = ctx.db().count("select count(*) from portal_user where userid=:s and id!=:id", &row! {"s"=>sid,"id"=>existing.and_then(|x| i(x, "id")).unwrap_or(0)}).await.unwrap_or(0);
            if dup > 0 {
                return Err("That user ID is already in use".into());
            }
            if existing.is_none() && !d.contains_key("password") {
                return Err("A password is required for a new user".into());
            }
            d.insert("last_updated".into(), json!(now_str()));
        }
        "portalTrackerRole" | "portalScheduler" | "portalEndpoint" | "portalModuleContact" | "portalModuleAccessReview" => {
            d.insert("last_updated".into(), json!(now_str()));
        }
        "portalSetting" | "userRole" | "portalTree" | "fileLink" | "portalFileManager" => {
            if e.field("module").is_some() && s(&d, "module").is_empty() {
                d.insert("module".into(), json!("portal"));
            }
        }
        _ => {}
    }
    if existing.is_none() {
        for k in ["date_created", "date"] {
            if e.field(k).map(|f| f.hidden).unwrap_or(false) {
                d.insert(k.into(), json!(now_str()));
            }
        }
    }
    Ok(d)
}

async fn save_multi(ctx: &Ctx, e: &Entity, id: i64) {
    for f in &e.fields {
        if let K::Multi(_, join, own, other) = f.kind {
            if f.hidden {
                continue;
            }
            let ids: Vec<i64> = match ctx.params.get(f.name) {
                Some(Value::Array(a)) => a.iter().filter_map(|v| vs(v).parse().ok()).collect(),
                Some(v) => vs(v).parse().ok().into_iter().collect(),
                None => vec![],
            };
            let _ = ctx.db().exec(&format!("delete from \"{}\" where \"{}\"={}", join, own, id), &db::no_params()).await;
            for x in ids {
                let _ = ctx.db().exec(&format!("insert into \"{}\" (\"{}\",\"{}\") values ({},{})", join, own, other, id, x), &db::no_params()).await;
            }
        }
    }
}

async fn after_save(ctx: &Ctx, e: &Entity, id: i64, created: bool) -> Option<String> {
    let db = ctx.db();
    let res: anyhow::Result<()> = async {
        match e.name {
            "portalTracker" => {
                if let Some(t) = Tracker::by_id(db, id).await {
                    t.updatedb(db).await?;
                    t.create_index(db).await?;
                }
                ensure_module(db, &db.get(e.table, id).await?.map(|r| s(&r, "module")).unwrap_or_default()).await;
            }
            "portalTrackerField" => {
                if let Some(f) = db.get(e.table, id).await? {
                    if let Some(t) = Tracker::by_id(db, i(&f, "tracker_id").unwrap_or(0)).await {
                        t.updatedb(db).await?;
                    }
                }
            }
            "portalTrackerIndex" => {
                if let Some(f) = db.get(e.table, id).await? {
                    if let Some(t) = Tracker::by_id(db, i(&f, "tracker_id").unwrap_or(0)).await {
                        t.create_index(db).await?;
                    }
                }
            }
            "portalTreeNode" => {
                if let Some(n) = db.get(e.table, id).await? {
                    if let Some(t) = i(&n, "tree_id") {
                        crate::tree::rebuild(db, t).await?;
                    }
                }
            }
            "portalTree" if created => {
                let name = db.get(e.table, id).await?.map(|r| s(&r, "name")).unwrap_or_default();
                let root = db.insert("portal_tree_node", &row! {"tree_id"=>id,"name"=>name,"lft"=>1,"rgt"=>2,"disabled"=>false}).await?;
                db.update("portal_tree", id, &row! {"root_id"=>root}).await?;
            }
            "portalPage" | "portalSetting" | "userRole" => {
                ensure_module(db, &db.get(e.table, id).await?.map(|r| s(&r, "module")).unwrap_or_default()).await;
            }
            _ => {}
        }
        Ok(())
    }
    .await;
    res.err().map(|e| e.to_string())
}

pub async fn ensure_module(db: &Db, name: &str) {
    if name.trim().is_empty() {
        return;
    }
    if db.count("select count(*) from portal_module where name=:n", &row! {"n"=>name}).await.unwrap_or(1) == 0 {
        let _ = db.insert("portal_module", &row! {"name"=>name}).await;
    }
}

fn after_url(e: &Entity, d: &Row, id: i64, keep_editing: bool) -> String {
    if keep_editing {
        return format!("/{}/edit/{}", e.name, id);
    }
    if let Scope::Via(fk, parent) = e.scope {
        if let Some(pid) = i(d, fk) {
            return format!("/{}/show/{}", parent, pid);
        }
    }
    format!("/{}/show/{}", e.name, id)
}

pub async fn save(mut ctx: Ctx, e: &Entity, id: Option<i64>) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if e.readonly || ctx.method != "POST" {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let existing = match id {
        Some(x) => match ctx.db().get(e.table, x).await {
            Ok(Some(r)) => Some(r),
            _ => return ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        None => None,
    };
    if let Some(r) = &existing {
        if !may(&ctx, e, r).await {
            return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to change this").await;
        }
    }
    let d = match gather(&ctx, e, existing.as_ref()).await {
        Ok(d) => d,
        Err(msg) => {
            ctx.flash(&msg);
            let mut r = existing.clone().unwrap_or_default();
            for (k, v) in &ctx.params {
                r.insert(k.clone(), v.clone());
            }
            let body = form(&mut ctx, e, &r, id).await;
            let msg = s(&ctx.sess, "flash");
            let (kind, title) = if id.is_some() { ("edit", format!("Edit {}", ename(e))) } else { ("create", format!("Create {}", ename(e))) };
            let h = frame(e, kind, &nav_for(e, "", true, id.is_some()), &title, &msg, &body);
            return ctx.html(&title, h).await;
        }
    };
    // the row as it will be must also be within the user's modules
    let mut merged = existing.clone().unwrap_or_default();
    for (k, v) in &d {
        merged.insert(k.clone(), v.clone());
    }
    if !may(&ctx, e, &merged).await {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to save this in that module").await;
    }
    let res = match id {
        Some(x) => ctx.db().update(e.table, x, &d).await.map(|_| x),
        None => ctx.db().insert(e.table, &d).await,
    };
    let rid = match res {
        Ok(x) => x,
        Err(err) => {
            ctx.flash(&format!("Could not save: {}", err));
            return ctx.redirect(&format!("/{}/index", e.name)).await;
        }
    };
    save_multi(&ctx, e, rid).await;
    if e.name == "user" {
        web::user_log(ctx.db(), "user", if id.is_some() { "User updated" } else { "User created" }, ctx.uid(), Some(rid), &ctx.ip).await;
    }
    match after_save(&ctx, e, rid, id.is_none()).await {
        Some(err) => ctx.flash(&format!("{} saved, but: {}", e.label, err)),
        None => ctx.flash(&format!("{} {}", e.label, if id.is_some() { "updated" } else { "created" })),
    }
    let mut to = after_url(e, &merged, rid, ctx.p("submitpage") == "Update");
    if id.is_none() && matches!(e.name, "userRole" | "portalSetting") {
        if let Some(u) = module_url(ctx.db(), &s(&merged, "module")).await {
            to = u;
        }
    }
    ctx.redirect(&to).await
}

pub async fn delete(mut ctx: Ctx, e: &Entity, id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let Ok(Some(r)) = ctx.db().get(e.table, id).await else { return ctx.status(StatusCode::NOT_FOUND, "Not found").await };
    if e.readonly || ctx.method != "POST" || !may(&ctx, e, &r).await {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to delete this").await;
    }
    delete_row(&ctx, e, id, 0).await;
    if e.name == "portalTreeNode" {
        if let Some(t) = i(&r, "tree_id") {
            let _ = crate::tree::rebuild(ctx.db(), t).await;
        }
    }
    ctx.flash(&format!("{} deleted", e.label));
    let to = match e.scope {
        Scope::Via(fk, parent) => match i(&r, fk) {
            Some(pid) => format!("/{}/show/{}", parent, pid),
            None => format!("/{}/index", e.name),
        },
        _ => format!("/{}/index", e.name),
    };
    ctx.redirect(&to).await
}

/// Delete a row with its declared children and join rows (data tables are never dropped).
fn delete_row<'a>(ctx: &'a Ctx, e: &'a Entity, id: i64, depth: usize) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let db = ctx.db();
        if depth < 4 {
            for (child, fk) in e.children {
                if let Some(ce) = ctx.st.entity(child) {
                    for c in db.rows(&format!("select id from \"{}\" where \"{}\"={}", ce.table, fk, id), &db::no_params()).await.unwrap_or_default() {
                        if let Some(cid) = i(&c, "id") {
                            delete_row(ctx, ce, cid, depth + 1).await;
                        }
                    }
                }
            }
        }
        if e.name == "portalTreeNode" && depth < 30 {
            for c in db.rows(&format!("select id from portal_tree_node where parent_id={}", id), &db::no_params()).await.unwrap_or_default() {
                if let Some(cid) = i(&c, "id") {
                    delete_row(ctx, e, cid, depth + 1).await;
                }
            }
            let _ = db.exec(&format!("update portal_tree set root_id=null where root_id={}", id), &db::no_params()).await;
        }
        for f in &e.fields {
            if let K::Multi(_, join, own, _) = f.kind {
                let _ = db.exec(&format!("delete from \"{}\" where \"{}\"={}", join, own, id), &db::no_params()).await;
            }
        }
        match e.name {
            "portalTrackerStatus" => {
                let _ = db.exec(&format!("delete from portal_tracker_transition_portal_tracker_status where portal_tracker_status_id={}", id), &db::no_params()).await;
                let _ = db.exec(&format!("update portal_tracker set initial_status_id=null where initial_status_id={}", id), &db::no_params()).await;
                let _ = db.exec(&format!("update portal_tracker_transition set next_status_id=null where next_status_id={}", id), &db::no_params()).await;
            }
            "portalTrackerRole" => {
                let _ = db.exec(&format!("delete from portal_tracker_transition_portal_tracker_role where portal_tracker_role_id={}", id), &db::no_params()).await;
            }
            "portalTrackerField" => {
                let _ = db.exec(&format!("update portal_tracker set defaultfield_id=null where defaultfield_id={}", id), &db::no_params()).await;
            }
            "fileLink" => {
                crate::files::delete_file_link(db, id).await;
                return;
            }
            "user" => {
                let _ = db.exec(&format!("delete from user_role where user_id={}", id), &db::no_params()).await;
            }
            _ => {}
        }
        if let Ok(t) = ident(e.table) {
            let _ = db.exec(&format!("delete from {} where id={}", t, id), &db::no_params()).await;
        }
    })
}

/// select2 lookup: /{controller}/lookup?q=
pub async fn lookup(ctx: Ctx, e: &Entity) -> Response {
    if ctx.user.is_none() {
        return ctx.json(json!({"results": []})).await;
    }
    let q = ctx.p("q");
    let like = lit(&json!(format!("%{}%", q.trim().replace(' ', "%"))));
    let mut cond = vec![if e.name == "user" { format!("(name ilike {l} or userid ilike {l} or email ilike {l})", l = like) } else { format!("\"{}\"::text ilike {}", e.display, like) }];
    if !ctx.is_admin() && e.name != "user" {
        let mods = my_modules(&ctx, e).await;
        match scope_filter(&ctx, e, &mods) {
            Some(f) if !mods.is_empty() => cond.push(f),
            _ => return ctx.json(json!({"results": []})).await,
        }
    }
    let rows = ctx.db().rows(&format!("select * from \"{}\" where {} order by \"{}\" limit 25", e.table, cond.join(" and "), e.display), &db::no_params()).await.unwrap_or_default();
    let res: Vec<Value> = rows
        .iter()
        .map(|r| {
            let text = if e.name == "user" { format!("{} ({})", s(r, "name"), s(r, "userid")) } else if e.field("module").is_some() && e.display != "module" { format!("{}: {}", s(r, "module"), s(r, e.display)) } else { s(r, e.display) };
            json!({"id": i(r, "id"), "text": text})
        })
        .collect();
    ctx.json(json!({"results": res})).await
}

/// Search pages and trackers the user can open.
pub async fn search(ctx: Ctx) -> Response {
    let q = { let a = ctx.p("q"); if a.is_empty() { ctx.p("query") } else { a } };
    let like = format!("%{}%", q.trim());
    let mut h = format!("<div class='container'><h1>Search</h1><form method='get' class='row g-2 mb-3'><div class='col'><input class='form-control' name='q' value='{}'/></div><div class='col-auto'><button class='btn btn-primary'>Search</button></div></form>", esc(&q));
    if !q.trim().is_empty() {
        let pages = ctx
            .db()
            .rows(&format!("select module, slug, title, runable, requirelogin from {} where published is true and runable is not true and (title ilike :q or slug ilike :q) and (allowedroles is null or allowedroles='' or allowedroles='All') order by module, title limit 50", db::page_t()), &row! {"q"=>like.clone()})
            .await
            .unwrap_or_default();
        h.push_str("<h4>Pages</h4><ul>");
        for p in pages.iter().filter(|p| ctx.user.is_some() || p.get("requirelogin") == Some(&json!(false))) {
            h.push_str(&format!("<li><a href='/view/{}/{}'>{}</a> <small class='text-muted'>{}</small></li>", esc(&s(p, "module")), esc(&s(p, "slug")), esc(&s(p, "title")), esc(&s(p, "module"))));
        }
        h.push_str("</ul>");
        if ctx.user.is_some() {
            let trackers = ctx.db().rows("select module, slug, name from portal_tracker where name ilike :q or slug ilike :q order by module, name limit 50", &row! {"q"=>like}).await.unwrap_or_default();
            h.push_str("<h4>Trackers</h4><ul>");
            for t in trackers {
                h.push_str(&format!("<li><a href='/{}/{}/list'>{}</a> <small class='text-muted'>{}</small></li>", esc(&s(&t, "module")), esc(&s(&t, "slug")), esc(&s(&t, "name")), esc(&s(&t, "module"))));
            }
            h.push_str("</ul>");
        }
    }
    h.push_str("</div>");
    ctx.html("Search", h).await
}

#[cfg(test)]
mod tests {
    use super::{is_vim_setting, page_ace_js};

    #[test]
    fn vim_setting_values() {
        assert!(is_vim_setting(Some("vim")));
        assert!(is_vim_setting(Some(" VIM ")));
        assert!(is_vim_setting(Some("vi")));
        assert!(!is_vim_setting(Some("normal")));
        assert!(!is_vim_setting(Some("")));
        assert!(!is_vim_setting(None));
    }

    #[test]
    fn ace_js_toggles_vim_mode() {
        let normal = page_ace_js(false);
        assert!(normal.contains("/static/libs/ace_editor/js/ace.js"));
        assert!(normal.contains("var vimMode = false;"));
        assert!(!normal.contains("keybinding-vim.js"));

        let vim = page_ace_js(true);
        assert!(vim.contains("/static/libs/ace_editor/js/keybinding-vim.js"));
        assert!(vim.contains("var vimMode = true;"));
        assert!(vim.contains("ace/keyboard/vim"));
        assert!(vim.contains("VimApi.defineEx('write', 'w'"));
    }
}
