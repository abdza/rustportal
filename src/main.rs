//! rustportal: a Rust re-implementation of the G5 portal — dynamic pages
//! stored in the database, tracker modules with workflows, and admin tools.

#[macro_use]
mod db;
mod audit;
mod cache;
mod admin;
mod endpoint;
mod entity;
mod files;
mod groovy;
mod gsp;
mod xlsxstream;
mod mail;
mod module;
mod page;
mod scheduler;
mod script;
mod tags;
mod tracker;
mod tracker_web;
mod tree;
mod upload;
mod user;
mod web;

use axum::extract::{DefaultBodyLimit, State};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use db::{i, s};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tracker::Tracker;
use web::{Ctx, St};

fn segs(ctx: &Ctx) -> Vec<String> {
    ctx.path.split('/').filter(|x| !x.is_empty()).map(|x| urlencoding::decode(x).map(|d| d.into_owned()).unwrap_or_else(|_| x.to_string())).collect()
}

async fn tracker_by_id_admin(ctx: &Ctx, id: Option<i64>) -> Option<Tracker> {
    let t = Tracker::by_id(ctx.db(), id?).await?;
    let ok = match &ctx.user {
        Some(u) => ctx.is_admin() || web::developer_list(ctx.db(), u).await.contains(&t.module),
        None => false,
    };
    if ok { Some(t) } else { None }
}

/// Actions of a named controller (`/controller/action/id`).
async fn controller(mut ctx: Ctx, c: &str, action: &str, idtxt: Option<String>) -> Response {
    let id = idtxt.as_deref().and_then(|x| x.parse::<i64>().ok()).or_else(|| ctx.pi("id"));
    let st = ctx.st.clone();
    match (c, action) {
        ("user", "login" | "authenticate" | "logout" | "register" | "my_profile" | "my_profile_save" | "change_password" | "update_password" | "switchuser" | "restoreadmin") => {
            return user::action(ctx, action, idtxt).await
        }
        ("portalPage", "home") => return page::display(ctx, "portal", "home", vec![]).await,
        ("portalPage", "display") | ("portalPage", "runpage") => {
            let m = { let m = ctx.p("module"); if m.is_empty() { "portal".to_string() } else { m } };
            let sl = ctx.p("slug");
            return if action == "display" { page::display(ctx, &m, &sl, vec![]).await } else { page::runpage(ctx, &m, &sl, vec![]).await };
        }
        ("portalSearch", _) => return admin::search(ctx).await,
        ("portalTracker", "list" | "create_data" | "display_data") => {
            let (m, sl) = (ctx.p("module"), ctx.p("slug"));
            return match action {
                "list" => tracker_web::list(ctx, &m, &sl).await,
                "create_data" => tracker_web::create(ctx, &m, &sl).await,
                _ => tracker_web::display(ctx, &m, &sl, id.unwrap_or(0)).await,
            };
        }
        ("portalTracker", "userlist") => return tracker_web::userlist(ctx).await,
        ("portalTracker", "nodeslist") => return tracker_web::nodeslist(ctx).await,
        ("portalTracker", "objectlist") => return tracker_web::objectlist(ctx).await,
        ("portalTracker", "dropdowndata") => return tracker_web::dropdowndata(ctx).await,
        ("portalTracker", "dropdownlist") => return tracker_web::dropdownlist(ctx).await,
        ("portalTracker", "api_list") => return tracker_web::api_list(ctx).await,
        ("portalTrackerField", "onchange") => return tracker_web::onchange(ctx).await,
        ("portalTracker", "workflow_graph") => {
            return match tracker_by_id_admin(&ctx, id).await {
                Some(t) => tracker_web::workflow_graph(ctx, &t).await,
                None => ctx.status(axum::http::StatusCode::FORBIDDEN, "Not allowed").await,
            }
        }
        ("portalTracker", "updatedb" | "create_defaults" | "create_default_pages" | "from_table" | "cleardb") => {
            let Some(t) = tracker_by_id_admin(&ctx, id).await else { return ctx.status(axum::http::StatusCode::FORBIDDEN, "Not allowed").await };
            if ctx.method != "POST" {
                return ctx.status(axum::http::StatusCode::METHOD_NOT_ALLOWED, "POST required").await;
            }
            let res: anyhow::Result<String> = async {
                Ok(match action {
                    "updatedb" => {
                        t.updatedb(&st.db).await?;
                        t.create_index(&st.db).await?;
                        "Database tables updated".to_string()
                    }
                    "create_defaults" => {
                        tracker_web::create_defaults(&st.db, &t).await?;
                        if let Some(t2) = Tracker::by_id(&st.db, t.id).await {
                            t2.updatedb(&st.db).await?;
                        }
                        "Default role, statuses and transitions created".to_string()
                    }
                    "create_default_pages" => format!("{} default pages created", tracker_web::create_default_pages(&st.db, &t).await?),
                    "from_table" => format!("{} fields created from table columns", t.from_table(&st.db).await?),
                    _ => {
                        t.cleardb(&st.db).await?;
                        "All data cleared".to_string()
                    }
                })
            }
            .await;
            match res {
                Ok(m) => ctx.flash(&m),
                Err(e) => ctx.flash(&format!("Failed: {}", e)),
            }
            return ctx.redirect(&format!("/portalTracker/show/{}", t.id)).await;
        }
        ("portalTrackerData", "upload") => return upload::upload(ctx).await,
        ("portalTrackerData", "rollback") => return upload::rollback(ctx, id.unwrap_or(0)).await,
        ("portalTrackerData", "process") => {
            if !ctx.is_admin() || ctx.method != "POST" {
                return ctx.status(axum::http::StatusCode::FORBIDDEN, "Not allowed").await;
            }
            match upload::process(&st, id.unwrap_or(0)).await {
                Ok(m) => ctx.flash(&m),
                Err(e) => ctx.flash(&format!("Failed: {}", e)),
            }
            return ctx.redirect(&format!("/portalTrackerData/show/{}", id.unwrap_or(0))).await;
        }
        ("portalModule", "updatedball") => {
            let Ok(Some(m)) = st.db.get("portal_module", id.unwrap_or(0)).await else { return ctx.status(axum::http::StatusCode::NOT_FOUND, "Not found").await };
            let name = s(&m, "name");
            let ok = match &ctx.user {
                Some(u) => ctx.is_admin() || web::developer_list(ctx.db(), u).await.contains(&name),
                None => false,
            };
            if !ok || ctx.method != "POST" {
                return ctx.status(axum::http::StatusCode::FORBIDDEN, "Not allowed").await;
            }
            let (mut done, mut errs) = (0, vec![]);
            for r in st.db.rows("select id, slug from portal_tracker where module=:m order by slug", &row! {"m"=>name.clone()}).await.unwrap_or_default() {
                let Some(t) = Tracker::by_id(&st.db, i(&r, "id").unwrap_or(0)).await else { continue };
                let res: anyhow::Result<()> = async {
                    t.updatedb(&st.db).await?;
                    t.create_index(&st.db).await
                }
                .await;
                match res {
                    Ok(_) => done += 1,
                    Err(e) => errs.push(format!("{}: {}", s(&r, "slug"), e)),
                }
            }
            ctx.flash(&if errs.is_empty() { format!("Database updated for {} tracker(s)", done) } else { format!("Database updated for {} tracker(s); failed: {}", done, errs.join("; ")) });
            return ctx.redirect(&format!("/portalModule/show/{}", id.unwrap_or(0))).await;
        }
        ("portalModule", "update_module_list" | "g5list" | "g5import" | "export" | "import" | "download" | "upload") => return module::action(ctx, action, id).await,
        ("fileLink", "get") => return files::get(ctx, id.unwrap_or(0)).await,
        ("fileLink", "download") => {
            let (m, sl) = ({ let m = ctx.p("module"); if m.is_empty() { "portal".to_string() } else { m } }, ctx.p("slug"));
            return files::download(ctx, &m, &sl).await;
        }
        ("fileLink", "upload") => {
            if ctx.method == "POST" {
                return files::upload(ctx).await;
            }
            if ctx.user.is_none() {
                return ctx.need_login().await;
            }
            let h = format!("<div class='container'><h1>Upload file</h1><form method='post' enctype='multipart/form-data' action='/fileLink/upload'>{}<label class='form-label'>File</label><input type='file' name='file' class='form-control mb-2' required/><label class='form-label'>Module</label><input name='module' class='form-control mb-2' value='{}'/><label class='form-label'>Slug (blank = from file name)</label><input name='slug' class='form-control mb-2'/><label class='form-label'>Allowed roles (blank = everyone)</label><input name='allowedroles' class='form-control mb-3'/><button class='btn btn-primary'>Upload</button></form></div>", ctx.csrf_field(), web::esc(&{ let m = ctx.p("module"); if m.is_empty() { "portal".to_string() } else { m } }));
            return ctx.html("Upload file", h).await;
        }
        ("portalFileManager", "browse") => return files::browse(ctx, id.unwrap_or(0)).await,
        ("portalEmail", "resend") | ("portalScheduler", "run") | ("portalTree", "rebuild") | ("portalErrorLog", "resolve") | ("portalModuleAccessReview" | "accessReview", "approve") => {
            let ename = if c == "accessReview" { "portalModuleAccessReview" } else { c };
            let rid = id.unwrap_or(0);
            let table = st.entity(ename).map(|e| e.table).unwrap_or("");
            let Ok(Some(r)) = st.db.get(table, rid).await else { return ctx.status(axum::http::StatusCode::NOT_FOUND, "Not found").await };
            let module = match ename {
                "portalModuleAccessReview" => st.db.get("portal_module", i(&r, "module_id").unwrap_or(0)).await.ok().flatten().map(|m| s(&m, "name")).unwrap_or_default(),
                "portalErrorLog" => String::new(),
                _ => s(&r, "module"),
            };
            if ctx.method != "POST" || !ctx.can_admin(&module).await {
                return ctx.status(axum::http::StatusCode::FORBIDDEN, "Not allowed").await;
            }
            match ename {
                "portalEmail" => match mail::send_queued(&st, rid).await {
                    Ok(()) => ctx.flash("Email sent"),
                    Err(e) => ctx.flash(&format!("Sending failed: {}", e)),
                },
                "portalScheduler" => {
                    scheduler::run_scheduler(&st, &r, &ctx.user.as_ref().map(|u| s(u, "staffid")).unwrap_or_default()).await;
                    ctx.flash("Scheduler run finished; see Scheduler Runs for the result");
                }
                "portalTree" => {
                    let _ = tree::rebuild(&st.db, rid).await;
                    ctx.flash("Tree rebuilt");
                }
                "portalErrorLog" => {
                    let _ = st.db.update("portal_error_log", rid, &row! {"resolved"=>true,"dateresolved"=>web::now_str()}).await;
                    ctx.flash("Marked as resolved");
                }
                _ => {
                    let snapshot = st.db.rows("select u.staffid, u.emp_name, r.role from user_role r join iap_user u on u.id=r.user_id where r.module=:m order by 1,3", &row! {"m"=>module}).await.unwrap_or_default();
                    let _ = st.db.update(table, rid, &row! {"status"=>"Approved","approved_by_id"=>ctx.uid(),"approved_date"=>web::now_str(),"approved_snapshot"=>serde_json::to_string(&snapshot).unwrap_or_default(),"last_updated"=>web::now_str()}).await;
                    let _ = st.db.insert("portal_module_access_review_log", &row! {"review_id"=>rid,"action"=>"Approved","actor_id"=>ctx.uid(),"date_created"=>web::now_str(),"detail"=>format!("{} role assignments confirmed", snapshot.len())}).await;
                    ctx.flash("Access review approved");
                }
            }
            return ctx.redirect(&format!("/{}/show/{}", ename, rid)).await;
        }
        _ => {}
    }
    let ename = if c == "accessReview" { "portalModuleAccessReview" } else { c };
    let Some(e) = st.entity(ename) else { return ctx.status(axum::http::StatusCode::NOT_FOUND, "Page not found").await };
    match action {
        "" | "index" | "list" => admin::index(ctx, e).await,
        "show" => admin::show(ctx, e, id.unwrap_or(0)).await,
        "create" => admin::create(ctx, e).await,
        "edit" => admin::edit(ctx, e, id.unwrap_or(0)).await,
        "save" => admin::save(ctx, e, None).await,
        "update" => admin::save(ctx, e, id).await,
        "delete" => admin::delete(ctx, e, id.unwrap_or(0)).await,
        "lookup" => admin::lookup(ctx, e).await,
        _ => ctx.status(axum::http::StatusCode::NOT_FOUND, "Page not found").await,
    }
}

const SPECIAL: &[&str] = &["portalSearch", "accessReview"];

async fn dispatch(State(_st): State<St>, ctx: Ctx) -> Response {
    let sg = segs(&ctx);
    let p: Vec<&str> = sg.iter().map(|x| x.as_str()).collect();
    let post = ctx.method == "POST";
    match p.as_slice() {
        [] => page::display(ctx, "portal", "home", vec![]).await,
        ["login"] => user::login(ctx).await,
        ["setup"] => user::setup(ctx).await,
        ["favicon.ico"] => ctx.redirect("/static/images/favicon.ico").await,
        ["view", m, sl, rest @ ..] => page::display(ctx, m, sl, rest.iter().map(|x| x.to_string()).collect()).await,
        ["run", sl] => page::runpage(ctx, "portal", sl, vec![]).await,
        ["run", m, sl, rest @ ..] => page::runpage(ctx, m, sl, rest.iter().map(|x| x.to_string()).collect()).await,
        ["download", sl] => files::download(ctx, "portal", sl).await,
        ["download", m, sl] => files::download(ctx, m, sl).await,
        ["svc", m, sl, rest @ ..] => {
            let r = if rest.is_empty() { String::new() } else { format!("/{}", rest.join("/")) };
            endpoint::serve(ctx, m.to_string(), sl.to_string(), r).await
        }
        [c, rest @ ..] if ctx.st.entity(c).is_some() || SPECIAL.contains(c) => {
            let action = rest.first().copied().unwrap_or("index").to_string();
            let id = rest.get(1).map(|x| x.to_string());
            let c = c.to_string();
            controller(ctx, &c, &action, id).await
        }
        [m, sl, "list"] => tracker_web::list(ctx, m, sl).await,
        [m, sl, "create"] => tracker_web::create(ctx, m, sl).await,
        [m, sl, "display", id] => match id.parse() {
            Ok(id) => tracker_web::display(ctx, m, sl, id).await,
            Err(_) => ctx.status(axum::http::StatusCode::NOT_FOUND, "Page not found").await,
        },
        [m, sl, "updaterecord", id] => match id.parse() {
            Ok(id) => tracker_web::update_record(ctx, m, sl, id).await,
            Err(_) => ctx.status(axum::http::StatusCode::NOT_FOUND, "Page not found").await,
        },
        [m, sl, tr] | [m, sl, tr, ..] if p.len() <= 5 => {
            let id = match p.get(3) {
                Some(x) => match x.parse::<i64>() {
                    Ok(v) => Some(v),
                    Err(_) => return ctx.status(axum::http::StatusCode::NOT_FOUND, "Page not found").await,
                },
                None => None,
            };
            if post { tracker_web::transition_post(ctx, m, sl, tr, id).await } else { tracker_web::transition_get(ctx, m, sl, tr, id).await }
        }
        _ => ctx.status(axum::http::StatusCode::NOT_FOUND, "Page not found").await,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(|x| x.as_str()) == Some("gsp-check") {
        return gsp_check(&argv[2..]);
    }
    let translate_modules: Option<Vec<String>> = if argv.get(1).map(|x| x.as_str()) == Some("translate-pages") { Some(argv[2..].to_vec()) } else { None };
    // `module-export <module>` / `module-import <module> [dir]`: the Module screen's
    // export and import, from the command line (config.toml, or CONFIG=path)
    let module_cmd: Option<Vec<String>> = match argv.get(1).map(|x| x.as_str()) {
        Some("module-export") | Some("module-import") => Some(argv[1..].to_vec()),
        _ => None,
    };
    let cfg_path = if translate_modules.is_some() || module_cmd.is_some() {
        std::env::var("CONFIG").unwrap_or_else(|_| "config.toml".to_string())
    } else {
        std::env::args().nth(1).unwrap_or_else(|| "config.toml".to_string())
    };
    let mut cfg: web::Config = match std::fs::read_to_string(&cfg_path) {
        Ok(t) => toml::from_str(&t).map_err(|e| anyhow::anyhow!("{}: {}", cfg_path, e))?,
        Err(_) => web::Config::default(),
    };
    if let Ok(u) = std::env::var("DATABASE_URL") {
        cfg.database_url = u;
    }
    if let Ok(p) = std::env::var("PORT") {
        cfg.port = p.parse().unwrap_or(cfg.port);
    }
    let database = db::Db::connect(&cfg.database_url, cfg.db_pool).await?;
    let entities = entity::entities();
    entity::sync_schema(&database, &entities).await?;
    if let Some(mods) = translate_modules {
        return translate_pages(&database, &mods).await;
    }
    std::fs::create_dir_all(&cfg.upload_dir)?;
    std::fs::create_dir_all(&cfg.module_dir)?;
    user::ensure_setup_token(&database).await;
    let st: St = Arc::new(web::AppState { db: database, cfg: cfg.clone(), entities, excel_jobs: Mutex::new(HashMap::new()), rt: tokio::runtime::Handle::current() });
    if let Some(cmd) = module_cmd {
        let Some(m) = cmd.get(1) else { anyhow::bail!("usage: rustportal module-export <module> | module-import <module> [dir]") };
        if cmd[0] == "module-export" {
            let dir = module::export(&st, m, true).await?;
            println!("exported {} to {}", m, dir.display());
        } else {
            let dir = cmd.get(2).map(std::path::PathBuf::from).unwrap_or_else(|| std::path::Path::new(&cfg.module_dir).join(m));
            let (summary, detail) = module::import_from(&st, m, &dir, true).await?;
            println!("{}\n{}", summary, detail);
        }
        return Ok(());
    }
    let st_for_shutdown = st.clone();
    scheduler::spawn(st.clone());
    audit::start(st.db.clone()).await;
    let app = Router::new()
        .nest_service("/static", tower_http::services::ServeDir::new(&cfg.static_dir))
        // like the Grails asset pipeline: /assets/x is looked up in js, libs, css and images
        .nest_service("/assets", {
            use tower_http::services::ServeDir;
            let d = |sub: &str| std::path::Path::new(&cfg.static_dir).join(sub);
            ServeDir::new(d("js")).fallback(ServeDir::new(d("libs")).fallback(ServeDir::new(d("css")).fallback(ServeDir::new(d("images")).fallback(ServeDir::new(&cfg.static_dir)))))
        })
        .fallback(any(dispatch))
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .with_state(st);
    let addr: SocketAddr = format!("{}:{}", cfg.address, cfg.port).parse()?;
    println!("rustportal listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    // TCP_NODELAY, as Tomcat and most servers default to: without it responses that
    // span several packets can stall ~40ms on delayed ACKs
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .tcp_nodelay(true)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    // write out audit rows still queued
    audit::drain(&st_for_shutdown.db).await;
    Ok(())
}

/// `rustportal gsp-check <modules dir | module dir | file.gsp> [--show]`:
/// translate the template pages of Grails module exports and report what
/// could not be translated, without touching the database.
fn gsp_check(args: &[String]) -> anyhow::Result<()> {
    let show = args.iter().any(|a| a == "--show");
    let verbose = args.iter().any(|a| a == "--pages");
    let Some(target) = args.iter().find(|a| !a.starts_with("--")) else { anyhow::bail!("usage: rustportal gsp-check <dir or file> [--show] [--pages]") };
    let path = std::path::Path::new(target);
    if path.is_file() {
        let t = gsp::translate(&std::fs::read_to_string(path)?);
        println!("{}", t.text);
        for w in &t.warnings {
            eprintln!("warning: {}", w);
        }
        if let Err(e) = minijinja::Environment::new().template_from_str(&t.text) {
            eprintln!("DOES NOT COMPILE: {:#}", e);
        }
        return Ok(());
    }
    let mut dirs = vec![];
    if path.join("pagelist.json").is_file() {
        dirs.push(path.to_path_buf());
    } else {
        for e in std::fs::read_dir(path)?.flatten() {
            if e.path().join("pagelist.json").is_file() {
                dirs.push(e.path());
            }
        }
    }
    dirs.sort();
    let (mut total, mut clean, mut broken) = (0, 0, 0);
    let mut counts: HashMap<String, usize> = HashMap::new();
    for d in dirs {
        let pages: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(d.join("pagelist.json"))?)?;
        let (mut mt, mut mc) = (0, 0);
        for p in pages.as_array().cloned().unwrap_or_default() {
            if p["runable"].as_bool().unwrap_or(false) {
                continue;
            }
            let slug = p["slug"].as_str().unwrap_or("");
            let Ok(src) = std::fs::read_to_string(d.join("pages").join(format!("content_{}.gsp", slug))) else { continue };
            let t = gsp::translate(&src);
            let mut warns = t.warnings.clone();
            if let Err(e) = minijinja::Environment::new().template_from_str(&t.text) {
                broken += 1;
                warns.push(format!("DOES NOT COMPILE: {:#}", e));
            }
            total += 1;
            mt += 1;
            if warns.is_empty() {
                clean += 1;
                mc += 1;
            }
            if verbose && !warns.is_empty() {
                println!("  {}/{}: {} warning(s)", d.file_name().unwrap().to_string_lossy(), slug, warns.len());
            }
            for w in warns {
                if show {
                    println!("{}/{}: {}", d.file_name().unwrap().to_string_lossy(), slug, w);
                }
                let key = w.split(':').next().unwrap_or("").to_string();
                *counts.entry(key).or_default() += 1;
            }
        }
        println!("{:40} {:4} template pages, {:4} translated cleanly", d.file_name().unwrap().to_string_lossy(), mt, mc);
    }
    println!("\nTOTAL {} template pages: {} clean, {} with warnings ({} do not compile)", total, clean, total - clean, broken);
    let mut c: Vec<_> = counts.into_iter().collect();
    c.sort_by(|a, b| b.1.cmp(&a.1));
    for (k, n) in c.iter().take(40) {
        println!("{:5}  {}", n, k);
    }
    Ok(())
}

/// `rustportal translate-pages <module>...`: for a database copied from the
/// Grails portal, translate the GSP pages of these modules in place (same
/// rules as importing a Grails module; tracker definitions are left alone).
async fn translate_pages(db: &db::Db, modules: &[String]) -> anyhow::Result<()> {
    if modules.is_empty() {
        anyhow::bail!("usage: rustportal translate-pages <module> [module...]");
    }
    for m in modules {
        let pages = db.rows("select * from portal_page where module=:m order by slug", &row! {"m"=>m.clone()}).await?;
        let (mut tr, mut warned, mut stub) = (0, 0, 0);
        for p in &pages {
            let id = i(p, "id").unwrap_or(0);
            let slug = s(p, "slug");
            let content = s(p, "content");
            let mut upd = db::Row::new();
            if db::b(p, "runable") {
                if !content.starts_with("// NOT PORTED") && !content.trim().is_empty() {
                    upd.insert("content".into(), serde_json::json!(gsp::stub_script(&content, "runable page", true)));
                    stub += 1;
                }
            } else if gsp::looks_like_gsp(&content) {
                let t = gsp::translate(&content);
                let mut warns = t.warnings;
                if let Err(e) = minijinja::Environment::new().template_from_str(&t.text) {
                    warns.push(format!("does not compile: {:#}", e));
                }
                for w in &warns {
                    println!("{}/{}: {}", m, slug, w);
                }
                if !warns.is_empty() {
                    warned += 1;
                }
                tr += 1;
                upd.insert("content".into(), serde_json::json!(if db::b(p, "fullpage") { t.text } else { gsp::strip_layout(&t.text) }));
            }
            let pre = s(p, "preprocess");
            if !pre.trim().is_empty() && !pre.starts_with("// NOT PORTED") {
                upd.insert("preprocess".into(), serde_json::json!(gsp::stub_script(&pre, "preprocess script", false)));
                stub += 1;
            }
            if !upd.is_empty() {
                db.update("portal_page", id, &upd).await?;
            }
            for d in db.rows("select id, name, query from portal_page_data where page_id=:p", &row! {"p"=>id}).await? {
                let q = s(&d, "query");
                if !q.contains("{{") && !q.trim().is_empty() {
                    let t = gsp::translate_expr(&q);
                    for w in &t.warnings {
                        println!("{}/{} data source {}: {}", m, slug, s(&d, "name"), w);
                    }
                    db.update("portal_page_data", i(&d, "id").unwrap_or(0), &row! {"query"=>t.text}).await?;
                }
            }
        }
        println!("{}: {} pages, {} templates translated ({} with warnings), {} Groovy scripts commented out", m, pages.len(), tr, warned, stub);
    }
    Ok(())
}

/// Ctrl-C or SIGTERM: stop accepting requests, let running ones finish.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}
