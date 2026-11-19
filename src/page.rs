//! Dynamic pages stored in the database: `/view/...` renders a template,
//! `/run/...` executes a script and acts on its result.

use crate::db::{self, b, i, s, vs, Db, Row};
use crate::row;
use crate::script::Sc;
use crate::web::{self, esc, Ctx};
use anyhow::{anyhow, Result};
use axum::response::Response;
use serde_json::{json, Value};

pub async fn find_page(db: &Db, module: &str, slug: &str) -> Option<Row> {
    crate::cache::PAGES
        .get_or(format!("{}\u{1}{}", module, slug), async {
            db.first(&format!("select * from {} where module=:m and slug=:s order by id limit 1", crate::db::page_t()), &row! {"m"=>module,"s"=>slug}).await.ok().flatten()
        })
        .await
}

/// Send a rendered page template the way the Grails portal's portalPage/display view
/// does: inside `<div id="content" role="main">` on the main layout, or on the empty
/// layout (no header or sidebar) for a full page.
pub async fn page_response(ctx: Ctx, page: &Row, title: &str, body: String) -> Response {
    let html = format!("\n    <div id=\"content\" role=\"main\">\n\t{}\n    </div>\n", body);
    if b(page, "fullpage") {
        ctx.empty_page(title, html).await
    } else {
        ctx.html(title, html).await
    }
}

pub fn make_sc(ctx: &mut Ctx) -> Sc {
    let csrf = ctx.csrf();
    let sc = Sc::new(ctx.st.clone(), ctx.user.clone(), ctx.params.clone(), csrf, ctx.path.clone());
    *sc.inner.files.lock().unwrap() = std::mem::take(&mut ctx.files);
    let mut sess = ctx.sess.clone();
    for k in crate::script::SESSION_RESERVED {
        sess.remove(*k);
    }
    *sc.inner.sess.lock().unwrap() = sess;
    *sc.inner.base.lock().unwrap() = request_base(ctx);
    sc
}

/// scheme://host[:port] as the browser sees it (X-Forwarded-* first, for a proxy).
pub fn request_base(ctx: &Ctx) -> String {
    let h = |k: &str| ctx.headers.get(k).and_then(|v| v.to_str().ok()).map(|v| v.split(',').next().unwrap_or("").trim().to_string()).filter(|v| !v.is_empty());
    let host = h("x-forwarded-host").or_else(|| h("host")).unwrap_or_else(|| format!("{}:{}", ctx.st.cfg.address, ctx.st.cfg.port));
    let scheme = h("x-forwarded-proto").unwrap_or_else(|| "http".to_string());
    format!("{}://{}", scheme, host)
}

/// Copy what a page's scripts did to the session back into the request's session.
pub fn keep_session(ctx: &mut Ctx, sc: &Sc) {
    let after = sc.inner.sess.lock().unwrap().clone();
    let reserved = crate::script::SESSION_RESERVED;
    ctx.sess.retain(|k, _| reserved.contains(&k.as_str()) || after.contains_key(k));
    for (k, v) in after {
        if !reserved.contains(&k.as_str()) {
            ctx.sess.insert(k, v);
        }
    }
}

/// Role check for a page (`allowedroles` is a comma separated list; `All` or empty = anyone).
pub async fn run_allowed(ctx: &Ctx, page: &Row) -> bool {
    let roles = db::csv(&s(page, "allowedroles"));
    if roles.is_empty() || roles.iter().any(|r| r == "All") {
        return true;
    }
    let Some(u) = &ctx.user else { return false };
    if b(u, "is_admin") {
        return true;
    }
    let module = s(page, "module");
    if web::developer_list(ctx.db(), u).await.contains(&module) {
        return true;
    }
    let mine = web::module_roles(ctx.db(), u, &module).await;
    if roles.iter().any(|r| mine.contains(r)) {
        return true;
    }
    match web::current_role(ctx.db(), u).await {
        Some(r) => roles.contains(&s(&r, "role")),
        None => false,
    }
}

/// Run the page's data sources: each query is a template producing SQL,
/// with `:name` placeholders bound from the request parameters.
pub async fn datasources(sc: &Sc, page: &Row, vars: &Row) -> Row {
    let mut datas = Row::new();
    let pid = i(page, "id").unwrap_or(0);
    let srcs = crate::cache::PAGE_DATA
        .get_or(pid.to_string(), async { sc.db().rows(&format!("select * from {} where page_id=:p order by id", crate::db::page_data_t()), &row! {"p"=>pid}).await.unwrap_or_default() })
        .await;
    for ds in srcs {
        let name = s(&ds, "name");
        let q = s(&ds, "query").replace("\r\n", " ");
        let sql = match sc.render(&q, vars, false) {
            Ok(x) => x,
            Err(e) => {
                web::log_error(sc.db(), "portalPage", "datasource", &format!("{}: {}", name, e), Some(&s(page, "module")), Some(&s(page, "slug")), None).await;
                continue;
            }
        };
        if !sql.trim().to_lowercase().starts_with("select") && !sql.trim().to_lowercase().starts_with("with") {
            continue;
        }
        let val = if b(&ds, "return_one") {
            sc.db().first(&sql, &sc.inner.params).await.map(|r| r.map(Value::Object).unwrap_or(Value::Null))
        } else {
            sc.db().rows(&sql, &sc.inner.params).await.map(|r| Value::Array(r.into_iter().map(Value::Object).collect()))
        };
        match val {
            Ok(v) => {
                datas.insert(name, v);
            }
            Err(e) => web::log_error(sc.db(), "portalPage", "datasource", &format!("{}: {}", name, e), Some(&s(page, "module")), Some(&s(page, "slug")), None).await,
        }
    }
    datas
}

fn error_box(ctx: &Ctx, what: &str, e: &anyhow::Error) -> String {
    if ctx.is_admin() {
        format!("<div class='alert alert-danger'><b>{}</b><pre class='mb-0' style='white-space:pre-wrap'>{}</pre></div>", esc(what), esc(&e.to_string()))
    } else {
        "<div class='alert alert-danger'>There was an error displaying this page. The administrators have been notified.</div>".to_string()
    }
}

async fn report_error(ctx: &Ctx, page: &Row, action: &str, e: &anyhow::Error) {
    web::log_error(ctx.db(), "portalPage", action, &e.to_string(), Some(&s(page, "module")), Some(&s(page, "slug")), ctx.uid()).await;
    if let Some(to) = web::setting_str(ctx.db(), "emailpagerror").await {
        let _ = crate::mail::send(&ctx.st, &to, "", "Page Error", &format!("Error with page {} : {}", esc(&s(page, "title")), esc(&e.to_string())), "portal").await;
    }
}

/// Render a page template with its data sources and preprocess script.
/// Returns (title, html).
pub async fn render_page(sc: &Sc, page: &Row, mut vars: Row) -> Result<(String, String)> {
    let mut datas = match vars.remove("datas") {
        Some(Value::Object(o)) => o,
        _ => Row::new(),
    };
    for (k, v) in datasources(sc, page, &vars).await {
        datas.insert(k, v);
    }
    vars.insert("datas".into(), Value::Object(datas));
    let pre = s(page, "preprocess");
    if !pre.trim().is_empty() {
        let pp = sc.eval(&pre, &vars).map_err(|e| anyhow!("preprocess: {}", e))?;
        vars.insert("pp".into(), pp);
    } else {
        vars.entry("pp".to_string()).or_insert(Value::Null);
    }
    let body = sc.render(&s(page, "content"), &vars, true)?;
    let title = s(page, "title");
    let title = if title.contains("{{") { sc.render(&title, &vars, false).unwrap_or(title) } else { title };
    Ok((title, body))
}

/// The home page when no `portal/home` page exists (the Grails portal's home.gsp).
fn home_default(ctx: &Ctx) -> String {
    format!(
        "\n    <div id=\"content\" role=\"main\">\n\t<h1>Welcome to {}</h1>\n\t<p>We have done a lot of upgrade to our systems. Please enjoy!</p>\n    </div>\n",
        esc(&ctx.st.cfg.app_name)
    )
}

pub async fn display(mut ctx: Ctx, module: &str, slug: &str, args: Vec<String>) -> Response {
    ctx.module = Some(module.to_string());
    ctx.slug = Some(slug.to_string());
    let Some(page) = find_page(ctx.db(), module, slug).await else {
        if module == "portal" && slug == "home" {
            let h = home_default(&ctx);
            return ctx.html("Home", h).await;
        }
        ctx.flash("The page you are looking for does not exist");
        return ctx.redirect("/").await;
    };
    let admin = ctx.can_admin(module).await;
    if !b(&page, "published") && !admin {
        return ctx.status(axum::http::StatusCode::NOT_FOUND, "This page is not published").await;
    }
    let need_login = page.get("requirelogin").map(|v| v.is_null() || v == &json!(true)).unwrap_or(true);
    if need_login && ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if !run_allowed(&ctx, &page).await {
        ctx.flash("You do not have the rights to view that page");
        return ctx.redirect("/").await;
    }
    for (n, a) in args.iter().enumerate() {
        ctx.params.insert(format!("arg{}", n + 1), json!(a));
    }
    ctx.params.insert("module".into(), json!(module));
    ctx.params.insert("slug".into(), json!(slug));
    if has_side_menu(&page) {
        ctx.side_menu = Some(s(&page, "side_menu"));
    }
    let sc = make_sc(&mut ctx);
    let flash_peek = s(&ctx.sess, "flash");
    let vars = row! {"flash"=>flash_peek,"args"=>args.clone()};
    let rendered = render_page(&sc, &page, vars).await;
    keep_session(&mut ctx, &sc);
    match rendered {
        Ok((title, body)) => match s(&page, "render").as_str() {
            "XML" => ctx.text("text/xml", body).await,
            "JSON" => ctx.text("application/json", body).await,
            "HTML" => ctx.raw_html_with_token(body).await,
            _ => page_response(ctx, &page, &title, body).await,
        },
        Err(e) => {
            report_error(&ctx, &page, "display", &e).await;
            let h = error_box(&ctx, "Page error", &e);
            ctx.html(&s(&page, "title"), h).await
        }
    }
}

fn has_side_menu(page: &Row) -> bool {
    !s(page, "side_menu").trim().is_empty()
}

pub fn cell(ws: &mut rust_xlsxwriter::Worksheet, r: u32, c: u16, v: &Value) {
    match v {
        Value::Null => {}
        Value::Bool(x) => {
            let _ = ws.write_boolean(r, c, *x);
        }
        Value::Number(n) => {
            let _ = ws.write_number(r, c, n.as_f64().unwrap_or(0.0));
        }
        other => {
            let t: String = vs(other).chars().take(32000).collect();
            let _ = ws.write_string(r, c, t);
        }
    }
}

/// Build a workbook from a script result:
/// `#{ filename: "x", sheets: [ #{ name: "S", header: [..], rows: [[..]] } ] }`
/// (or `header`/`rows` at the top level for a single sheet). Rows may be
/// arrays or maps (maps use the header as keys).
pub fn build_xlsx(result: &Value) -> Result<(String, Vec<u8>)> {
    let o = result.as_object().ok_or_else(|| anyhow!("XLSX page must return a map with filename and rows/sheets"))?;
    let filename = { let f = s(o, "filename"); if f.is_empty() { "report".to_string() } else { f } };
    let sheets: Vec<Value> = match o.get("sheets") {
        Some(Value::Array(a)) => a.clone(),
        _ => vec![Value::Object(o.clone())],
    };
    let mut wb = rust_xlsxwriter::Workbook::new();
    let bold = rust_xlsxwriter::Format::new().set_bold();
    for (n, sh) in sheets.iter().enumerate() {
        let so = sh.as_object().cloned().unwrap_or_default();
        let ws = wb.add_worksheet();
        let name: String = { let x = s(&so, "name"); if x.is_empty() { format!("Sheet{}", n + 1) } else { x } }.chars().filter(|c| !matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\')).take(31).collect();
        let _ = ws.set_name(&name);
        let header: Vec<Value> = so.get("header").and_then(|v| v.as_array().cloned()).unwrap_or_default();
        let keys: Vec<String> = so.get("keys").and_then(|v| v.as_array()).map(|a| a.iter().map(vs).collect()).unwrap_or_else(|| header.iter().map(vs).collect());
        let mut r = 0u32;
        if !header.is_empty() {
            let plain = so.get("bold_header") == Some(&Value::Bool(false));
            for (c, h) in header.iter().enumerate() {
                let _ = if plain { ws.write_string(0, c as u16, vs(h)) } else { ws.write_string_with_format(0, c as u16, vs(h), &bold) };
            }
            r = 1;
        }
        // optional layout: column widths (in characters), a frozen header row
        if let Some(Value::Array(w)) = so.get("widths") {
            for (c, v) in w.iter().enumerate() {
                if let Some(x) = v.as_f64() {
                    let _ = ws.set_column_width(c as u16, x);
                }
            }
        }
        if !header.is_empty() && so.get("freeze").map(|v| v != &Value::Bool(false)).unwrap_or(true) {
            let _ = ws.set_freeze_panes(1, 0);
        }
        if let Some(Value::Array(rows)) = so.get("rows") {
            for row in rows {
                match row {
                    Value::Array(cells) => {
                        for (c, v) in cells.iter().enumerate() {
                            match v {
                                // a cell with formatting: #{v: value, bold: true} or #{v: "2026-01-02 10:00:00", date: true}
                                Value::Object(m) if m.contains_key("v") => {
                                    let val = m.get("v").cloned().unwrap_or(Value::Null);
                                    if b(m, "date") {
                                        let fmt = rust_xlsxwriter::Format::new().set_num_format("yyyy-mm-dd hh:mm:ss");
                                        match rust_xlsxwriter::ExcelDateTime::parse_from_str(&vs(&val)) {
                                            Ok(d) => { let _ = ws.write_datetime_with_format(r, c as u16, &d, &fmt); }
                                            Err(_) => cell(ws, r, c as u16, &val),
                                        }
                                    } else if b(m, "bold") {
                                        match &val {
                                            Value::Number(n) => { let _ = ws.write_number_with_format(r, c as u16, n.as_f64().unwrap_or(0.0), &bold); }
                                            Value::Null => {}
                                            other => { let _ = ws.write_string_with_format(r, c as u16, vs(other), &bold); }
                                        }
                                    } else {
                                        cell(ws, r, c as u16, &val);
                                    }
                                }
                                _ => cell(ws, r, c as u16, v),
                            }
                        }
                    }
                    Value::Object(m) => {
                        for (c, k) in keys.iter().enumerate() {
                            cell(ws, r, c as u16, m.get(k).unwrap_or(&Value::Null));
                        }
                    }
                    _ => {}
                }
                r += 1;
            }
        }
        // native Excel bar charts over cells of this sheet, so they stay live:
        // #{title, series, first_row, last_row, cat_col, val_col, horizontal, row, col, height_rows}
        // (rows and columns 0-based, as written)
        if let Some(Value::Array(charts)) = so.get("charts") {
            for ch in charts {
                let Some(m) = ch.as_object() else { continue };
                let n = |k: &str, d: i64| m.get(k).and_then(|v| v.as_i64()).unwrap_or(d);
                let (r1, r2) = (n("first_row", 1) as u32, n("last_row", 1) as u32);
                let (cc, vc) = (n("cat_col", 0) as u16, n("val_col", 1) as u16);
                let mut chart = rust_xlsxwriter::Chart::new(if b(m, "horizontal") { rust_xlsxwriter::ChartType::Bar } else { rust_xlsxwriter::ChartType::Column });
                chart.title().set_name(s(m, "title").as_str());
                chart.add_series().set_categories((name.as_str(), r1, cc, r2, cc)).set_values((name.as_str(), r1, vc, r2, vc)).set_name(s(m, "series").as_str());
                chart.legend().set_hidden();
                chart.set_width(720).set_height((n("height_rows", 15).max(8) * 20) as u32);
                let _ = ws.insert_chart(n("row", 0) as u32, n("col", 4) as u16, &chart);
            }
        }
    }
    Ok((filename, wb.save_to_buffer().map_err(|e| anyhow!("{}", e))?))
}

/// Where a script result wants the browser to go next, if anywhere.
pub async fn result_redirect(db: &Db, default_module: &str, result: &Value) -> Option<String> {
    let o = result.as_object()?;
    if let Some(u) = o.get("redirect_url").map(vs).filter(|x| !x.is_empty()) {
        return Some(u);
    }
    let slug = o.get("redirect_slug").map(vs).filter(|x| !x.is_empty())?;
    let module = o.get("redirect_module").map(vs).filter(|x| !x.is_empty()).unwrap_or_else(|| default_module.to_string());
    let mut rp = o.get("redirect_params").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    // arg1, arg2, arg3 go in the path, as Grails' URL mappings put them
    let mut path_args = String::new();
    for k in ["arg1", "arg2", "arg3"] {
        match rp.get(k).map(vs).filter(|x| !x.is_empty()) {
            Some(a) => {
                path_args.push('/');
                path_args.push_str(&web::enc(&a));
                rp.remove(k);
            }
            None => break,
        }
    }
    let q = web::qs(&rp, &[]);
    let q = if q.is_empty() { String::new() } else { format!("?{}", q) };
    Some(match find_page(db, &module, &slug).await {
        Some(p) if b(&p, "runable") => format!("/run/{}/{}{}{}", module, slug, path_args, q),
        Some(_) => format!("/view/{}/{}{}{}", module, slug, path_args, q),
        None => {
            if crate::tracker::Tracker::load(db, &module, &slug).await.is_some() {
                format!("/{}/{}/list{}", module, slug, q)
            } else {
                format!("/view/{}/{}{}", module, slug, q)
            }
        }
    })
}

/// `module:slug`, `tracker:module:slug`, `page:module:slug` or a bare slug.
async fn redirect_after(db: &Db, spec: &str, default_module: &str) -> String {
    let t: Vec<&str> = spec.split(':').map(|x| x.trim()).collect();
    match t.as_slice() {
        ["tracker", m, sl] => format!("/{}/{}/list", m, sl),
        ["page", m, sl] => format!("/view/{}/{}", m, sl),
        ["run", m, sl] => format!("/run/{}/{}", m, sl),
        [m, sl] => match find_page(db, m, sl).await {
            Some(p) if b(&p, "runable") => format!("/run/{}/{}", m, sl),
            Some(_) => format!("/view/{}/{}", m, sl),
            None => format!("/{}/{}/list", m, sl),
        },
        [sl] if sl.starts_with('/') || sl.starts_with("http") => sl.to_string(),
        [sl] => format!("/view/{}/{}", default_module, sl),
        _ => "/".into(),
    }
}

pub async fn runpage(mut ctx: Ctx, module: &str, slug: &str, args: Vec<String>) -> Response {
    ctx.module = Some(module.to_string());
    ctx.slug = Some(slug.to_string());
    let Some(page) = find_page(ctx.db(), module, slug).await else {
        ctx.flash("The page you are looking for does not exist");
        return ctx.redirect("/").await;
    };
    if !run_allowed(&ctx, &page).await {
        ctx.flash("You do not have the rights to run that page");
        return ctx.redirect("/").await;
    }
    let need_login = page.get("requirelogin").map(|v| v.is_null() || v == &json!(true)).unwrap_or(true);
    if need_login && ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if !(b(&page, "published") && b(&page, "runable")) && !ctx.is_admin() {
        ctx.flash("That page cannot be run");
        return ctx.redirect("/").await;
    }
    for (n, a) in args.iter().enumerate() {
        ctx.params.insert(format!("arg{}", n + 1), json!(a));
    }
    ctx.params.insert("module".into(), json!(module));
    ctx.params.insert("slug".into(), json!(slug));
    if has_side_menu(&page) {
        ctx.side_menu = Some(s(&page, "side_menu"));
    }
    if s(&page, "render") == "XLSX" && crate::db::g6() {
        return xlsx_async(ctx, page, args).await;
    }
    let sc = make_sc(&mut ctx);
    let vars = row! {"args"=>args.clone(),"request"=>json!({"method": ctx.method, "path": ctx.path, "ip": ctx.ip})};
    let datas = datasources(&sc, &page, &vars).await;
    let mut vars = vars;
    vars.insert("datas".into(), Value::Object(datas));
    let evaluated = sc.eval(&s(&page, "content"), &vars);
    keep_session(&mut ctx, &sc);
    let result = match evaluated {
        Ok(v) => v,
        Err(e) => {
            report_error(&ctx, &page, "runpage", &e).await;
            let h = error_box(&ctx, "Script error", &e);
            return ctx.html(&s(&page, "title"), h).await;
        }
    };
    if let Some(f) = result.get("flash").map(vs).filter(|x| !x.is_empty()) {
        ctx.flash(&f);
    }
    match s(&page, "render").as_str() {
        // a g6 portal sends a JSON page's result as text/html (render(text: x as JSON, contentType: "text/html"))
        "JSON" if crate::db::g6() => return ctx.text("text/html;charset=utf-8", serde_json::to_string(&result).unwrap_or_default()).await,
        "JSON" => return ctx.json(result).await,
        "XML" => return ctx.text("text/xml", vs(&result)).await,
        "HTML" => return ctx.raw_html_with_token(result.get("content").map(vs).unwrap_or_else(|| vs(&result))).await,
        "XLSX" => {
            return match build_xlsx(&result) {
                Ok((name, bytes)) => ctx.bytes("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", Some(&format!("{}.xlsx", name)), bytes, false).await,
                Err(e) => {
                    report_error(&ctx, &page, "runpage xlsx", &e).await;
                    ctx.flash("Error generating excel file");
                    ctx.redirect("/").await
                }
            };
        }
        "File" => {
            // result: a file_link id, #{file_id: n}, or #{content: "...", filename: "x.csv", content_type: "text/csv"}
            if let Some(fid) = result.as_i64().or_else(|| result.get("file_id").and_then(|v| v.as_i64())) {
                return crate::files::get(ctx, fid).await;
            }
            if let Some(o) = result.as_object() {
                let name = { let n = s(o, "filename"); if n.is_empty() { "download.txt".to_string() } else { n } };
                let ct = { let c = s(o, "content_type"); if c.is_empty() { mime_guess::from_path(&name).first_or_octet_stream().to_string() } else { c } };
                return ctx.bytes(&ct, Some(&name), s(o, "content").into_bytes(), false).await;
            }
            ctx.flash("The file you requested does not exist");
            return ctx.redirect("/").await;
        }
        _ => {}
    }
    if let Some(to) = result_redirect(ctx.db(), module, &result).await {
        return ctx.redirect(&to).await;
    }
    let ra = s(&page, "redirectafter");
    if !ra.trim().is_empty() {
        let to = redirect_after(ctx.db(), &ra, module).await;
        return ctx.redirect(&to).await;
    }
    let body = match &result {
        Value::Object(o) if o.contains_key("content") => s(o, "content"),
        Value::String(x) => x.clone(),
        Value::Null => "<div class='alert alert-success'>Done</div>".to_string(),
        // a g6 portal prints what the script returned as Java would ([a, b], {k=v})
        other if crate::db::g6() => esc(&crate::groovy::to_groovy_string(other)),
        other => format!("<pre>{}</pre>", esc(&serde_json::to_string_pretty(other).unwrap_or_default())),
    };
    let title = s(&page, "title");
    page_response(ctx, &page, &title, body).await
}

/// A g6 portal builds a runable page's spreadsheet in the background: the first
/// request answers with a "Generating Excel" page that polls
/// ?async_token=..&async_status=1 and then downloads ?async_token=..
async fn xlsx_async(mut ctx: Ctx, page: Row, args: Vec<String>) -> Response {
    use crate::web::ExcelJob;
    let token = ctx.p("async_token");
    if !token.is_empty() {
        if ctx.params.contains_key("async_status") {
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
            Some(ExcelJob { done: true, file: Some(path), filename, .. }) => ctx.file_stream("application/octet-stream", Some(&format!("{}.xlsx", filename)), &path, true).await,
            Some(ExcelJob { error: Some(e), .. }) => ctx.status(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &format!("Export failed: {}", e)).await,
            _ => ctx.status(axum::http::StatusCode::NOT_FOUND, "Export not ready or expired").await,
        };
    }
    let token = uuid::Uuid::new_v4().to_string();
    ctx.st.excel_jobs.lock().unwrap().insert(token.clone(), ExcelJob { done: false, file: None, error: None, filename: s(&page, "slug") });
    let sc = make_sc(&mut ctx);
    let vars = row! {"args"=>args,"request"=>json!({"method": ctx.method, "path": ctx.path, "ip": ctx.ip})};
    let st = ctx.st.clone();
    let uid = ctx.uid();
    let tk = token.clone();
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let start = web::now_str();
        let dir = format!("{}/tmp", st.cfg.upload_dir);
        let _ = tokio::fs::create_dir_all(&dir).await;
        let path = format!("{}/{}.xlsx", dir, tk);
        let datas = datasources(&sc, &page, &vars).await;
        let mut vars = vars;
        vars.insert("datas".into(), Value::Object(datas));
        let built = sc.eval(&s(&page, "content"), &vars).and_then(|r| build_xlsx(&r));
        let (file, name, error) = match built {
            Ok((name, bytes)) => match tokio::fs::write(&path, bytes).await {
                Ok(_) => (Some(path), Some(name), None),
                Err(e) => (None, None, Some(e.to_string())),
            },
            Err(e) => (None, None, Some(e.to_string())),
        };
        let _ = st
            .db
            .insert(
                "portal_background_trail",
                &row! {"name"=>"portalPage.xlsx","details"=>format!("{}:{}", s(&page, "module"), s(&page, "slug")),"user_id"=>uid,"start_date"=>start,
                "duration_ms"=>started.elapsed().as_millis() as i64,"success"=>error.is_none(),"error"=>error.clone()},
            )
            .await;
        if let Some(j) = st.excel_jobs.lock().unwrap().get_mut(&tk) {
            j.done = true;
            j.file = file;
            j.error = error;
            if let Some(n) = name {
                j.filename = n;
            }
        }
    });
    ctx.raw_html(XLSX_WAIT_PAGE.replace("{token}", &token)).await
}

/// The page g6 shows while a runable page's spreadsheet is built.
const XLSX_WAIT_PAGE: &str = r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>Generating Excel...</title>
<style>
body{font-family:Arial,sans-serif;text-align:center;padding:60px 20px;background:#f5f5f5;}
.box{background:#fff;border-radius:8px;padding:40px;display:inline-block;box-shadow:0 2px 8px rgba(0,0,0,.1);min-width:320px;}
h2{color:#333;margin-bottom:10px;}
#status{color:#666;margin:16px 0;}
.spinner{display:inline-block;width:40px;height:40px;border:4px solid #ddd;border-top-color:#0078d4;border-radius:50%;animation:spin .8s linear infinite;margin-bottom:16px;}
@keyframes spin{to{transform:rotate(360deg);}}
.error{color:#c00;}
</style>
</head>
<body>
<div class="box">
  <div class="spinner" id="spinner"></div>
  <h2>Generating Excel Report</h2>
  <p id="status">Please wait while your report is being prepared&hellip;</p>
</div>
<script>
var token = '{token}';
var baseUrl = window.location.href.split('?')[0];
var checkUrl = baseUrl + '?async_token=' + token + '&async_status=1';
var downloadUrl = baseUrl + '?async_token=' + token;
function check() {
    fetch(checkUrl, {credentials:'same-origin'})
        .then(function(r){return r.json();})
        .then(function(data){
            if(data.ready){
                document.getElementById('status').textContent='Report ready! Downloading\u2026';
                document.getElementById('spinner').style.display='none';
                var a=document.createElement('a');
                a.href=downloadUrl;
                a.download='';
                document.body.appendChild(a);
                a.click();
                document.body.removeChild(a);
                document.getElementById('status').textContent='Download complete.';
                setTimeout(function(){
                    if(document.referrer && document.referrer !== window.location.href){
                        window.location.href=document.referrer;
                    } else {
                        window.history.back();
                    }
                },1500);
            } else if(data.error){
                document.getElementById('spinner').style.display='none';
                document.getElementById('status').innerHTML='<span class="error">Error: '+data.error+'</span>';
            } else {
                setTimeout(check,2000);
            }
        })
        .catch(function(){setTimeout(check,2000);});
}
setTimeout(check,2000);
</script>
</body>
</html>"#;
