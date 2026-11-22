//! Authentication, user administration, and the user's own account screens
//! matching g6ikram's UserController + views/user/*.gsp.

use crate::db::{self, b, i, lit, s, Db, Row};
use crate::row;
use crate::web::{self, enc, esc, now_str, Ctx};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use std::collections::HashSet;

const TOKEN_FILE: &str = "setup-token.txt";

pub async fn has_admin(db: &crate::db::Db) -> bool {
    db.count("select count(*) from portal_user where is_admin is true", &Row::new()).await.unwrap_or(1) > 0
}

/// On a brand new instance print a one-time token for creating the first administrator.
pub async fn ensure_setup_token(db: &crate::db::Db) {
    if has_admin(db).await {
        let _ = std::fs::remove_file(TOKEN_FILE);
        return;
    }
    let token = match std::fs::read_to_string(TOKEN_FILE) {
        Ok(t) if t.trim().len() >= 32 => t.trim().to_string(),
        _ => {
            let t: String = web::token().chars().take(48).collect();
            let _ = std::fs::write(TOKEN_FILE, &t);
            t
        }
    };
    println!("*\n* This instance has no system administrator yet.\n* Open  /setup  and enter this token to create the first administrator:\n*     {}\n*", token);
}

fn card(title: &str, body: &str) -> String {
    format!("<div class='container'><div class='row justify-content-center'><div class='col-md-6'><div class='card'><div class='card-body'><h3 class='card-title'>{}</h3>{}</div></div></div></div></div>", title, body)
}

pub async fn setup(mut ctx: Ctx) -> Response {
    if has_admin(ctx.db()).await {
        return ctx.status(StatusCode::NOT_FOUND, "Setup has already been completed").await;
    }
    if ctx.method == "POST" {
        let want = std::fs::read_to_string(TOKEN_FILE).unwrap_or_default();
        let sid = { let x = ctx.p("userid"); if x.is_empty() { ctx.p("staffid") } else { x } };
        let (name, pw) = (ctx.p("name"), ctx.p("password"));
        let err = if want.trim().is_empty() || ctx.p("token").trim() != want.trim() {
            Some("The setup token is not correct")
        } else if sid.len() < 3 || !sid.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            Some("User ID must be at least 3 letters, numbers, dots, underscores or dashes")
        } else if name.trim().is_empty() {
            Some("Name is required")
        } else if pw.len() < 8 {
            Some("The password must be at least 8 characters")
        } else if pw != ctx.p("password2") {
            Some("The passwords do not match")
        } else {
            None
        };
        match err {
            Some(e) => ctx.flash(e),
            None => {
                let hash = bcrypt::hash(&pw, 12).unwrap_or_default();
                let r = ctx.db().insert("portal_user", &row! {"userid"=>sid,"name"=>name.trim(),"email"=>ctx.p("email"),"password"=>hash,"is_admin"=>true,"is_active"=>true,"reset_password"=>false,"date_joined"=>now_str(),"last_updated"=>now_str()}).await;
                return match r {
                    Ok(uid) => {
                        let _ = std::fs::remove_file(TOKEN_FILE);
                        let _ = ctx.db().insert("user_role", &row! {"user_id"=>uid,"module"=>"portal","role"=>"Admin"}).await;
                        let _ = ctx.db().insert("user_role", &row! {"user_id"=>uid,"module"=>"portal","role"=>"Developer"}).await;
                        crate::admin::ensure_module(ctx.db(), "portal").await;
                        ctx.flash("Administrator created. Please log in.");
                        ctx.redirect("/login").await
                    }
                    Err(e) => {
                        ctx.flash(&e.to_string());
                        ctx.redirect("/setup").await
                    }
                };
            }
        }
        return ctx.redirect("/setup").await;
    }
    let f = format!("<p>Create the first administrator. The token was printed on the server console and written to <code>setup-token.txt</code>.</p><form method='post' action='/setup'>{}<label class='form-label'>Setup token</label><input class='form-control mb-2' name='token' required/><label class='form-label'>User ID (login name)</label><input class='form-control mb-2' name='userid' required minlength='3'/><label class='form-label'>Name</label><input class='form-control mb-2' name='name' required/><label class='form-label'>E-mail</label><input type='email' class='form-control mb-2' name='email'/><label class='form-label'>Password</label><input type='password' class='form-control mb-2' name='password' required minlength='8'/><label class='form-label'>Repeat password</label><input type='password' class='form-control mb-3' name='password2' required/><button class='btn btn-primary'>Create administrator</button></form>", ctx.csrf_field());
    ctx.html("Setup", card("First-time setup", &f)).await
}

pub async fn login(mut ctx: Ctx) -> Response {
    if ctx.method == "POST" {
        let sid = ["username", "userid", "staffid"].iter().map(|k| ctx.p(k)).find(|v| !v.is_empty()).unwrap_or_default();
        let user = web::find_user_by_staffid(ctx.db(), sid.trim()).await;
        let ok = match &user {
            Some(u) => b(u, "is_active") && !s(u, "password").is_empty() && bcrypt::verify(ctx.p("password"), &s(u, "password")).unwrap_or(false),
            None => {
                // spend comparable time whether or not the account exists
                let _ = bcrypt::verify("x", "$2b$12$C6UzMDM.H6dfI/f/IKcEeO7ZPp0vT8d8mB9pP1b6oQe5Yq3q9i0mW");
                false
            }
        };
        if !ok {
            web::user_log(ctx.db(), "login", &format!("Failed login for {}", sid.chars().take(40).collect::<String>()), None, user.as_ref().and_then(|u| i(u, "id")), &ctx.ip).await;
            ctx.flash("Invalid username or password");
            return ctx.redirect("/login").await;
        }
        let u = user.unwrap();
        let uid = i(&u, "id").unwrap_or(0);
        let back = s(&ctx.sess, "redirect_after_login");
        // fresh session on login
        ctx.sid = web::token();
        ctx.sess = Row::new();
        ctx.sess.insert("userid".into(), json!(uid));
        ctx.csrf();
        let _ = ctx.db().exec("update portal_user set lastlogin=now() where id=:id", &row! {"id"=>uid}).await;
        web::user_log(ctx.db(), "login", "Logged in", Some(uid), None, &ctx.ip).await;
        if b(&u, "reset_password") {
            return ctx.redirect("/user/change_password").await;
        }
        let to = if back.starts_with('/') && !back.starts_with("//") { back } else { "/".to_string() };
        return ctx.redirect(&to).await;
    }
    if ctx.user.is_some() {
        return ctx.redirect("/").await;
    }
    if !has_admin(ctx.db()).await {
        return ctx.redirect("/setup").await;
    }
    // the Grails portal's login page, on its "empty" layout
    let brand = web::branding(&ctx.st).await;
    let reg = if ctx.st.cfg.allow_registration { "\n                    <div class=\"col-12\">\n                      <p class=\"small mb-0\">Don't have account? <a href=\"/user/register\">Create an account</a></p>\n                    </div>" } else { "" };
    let google = if !ctx.st.cfg.google_oauth.client_id.is_empty() { "\n                    <div class=\"col-12\">\n                      <a href=\"/auth/google\" class=\"btn btn-primary w-100\">Sign in with Google</a>\n                    </div>" } else { "" };
    let app = esc(brand["app_name"].as_str().unwrap_or(&ctx.st.cfg.app_name));
    let logo = esc(brand["logo"].as_str().unwrap_or("/static/images/logo.png"));
    let csrf = ctx.csrf_field();
    let page = format!(r#"
    <div id="content" role="main">
      <section class="section register min-vh-100 d-flex flex-column align-items-center justify-content-center py-4">
        <div class="container">
          <div class="row justify-content-center">
            <div class="col-lg-4 col-md-6 d-flex flex-column align-items-center justify-content-center">

              <div class="d-flex justify-content-center py-4">
                <a href="/" class="logo d-flex align-items-center w-auto">
                  <img src="{logo}" alt="{app}"/>
                  <span class="d-none d-lg-block">{app}</span>
                </a>
              </div><!-- End Logo -->

              <div class="card mb-3">

                <div class="card-body">

                  <div class="pt-4 pb-2">
                    <h5 class="card-title text-center pb-0 fs-4">Login to Your Account</h5>
                    <p class="text-center small">Enter your username & password to login</p>
                  </div>

                  <form action="/login" method="post" class="row g-3 needs-validation" >{csrf}

                    <div class="col-12">
                      <label for="yourUsername" class="form-label">Username</label>
                      <div class="input-group has-validation">
                        <span class="input-group-text" id="inputGroupPrepend">@</span>
                        <input type="text" placeholder="Username" name="username" class="form-control" id="username" pattern="[a-zA-Z0-9._\-]+" title="Alphanumeric characters only" value="" required="required" />
                        <div class="invalid-feedback">Username must be alphanumeric.</div>
                      </div>
                    </div>

                    <div class="col-12">
                      <label for="yourPassword" class="form-label">Password</label>
                      <input type="password" placeholder="Password" name="password" class="form-control" id="password" value="" />
                      <div class="invalid-feedback">Please enter your password!</div>
                    </div>

                    <div class="col-12">
                      <button class="btn btn-primary w-100" type="submit">Login</button>
                    </div>{reg}{google}
                  </form>

                </div>
              </div>

            </div>
          </div>
        </div>

      </section>

    </div>
"#);
    ctx.empty_page("Login", page).await
}

pub async fn action(mut ctx: Ctx, act: &str, id: Option<String>) -> Response {
    match act {
        "login" | "authenticate" => return login(ctx).await,
        "logout" => {
            if let Some(u) = &ctx.user {
                let name = s(u, "name");
                web::user_log(ctx.db(), "login", "Logged out", ctx.uid(), None, &ctx.ip).await;
                let _ = ctx.db().exec("delete from portal_session where id=:id", &row! {"id"=>ctx.sid.clone()}).await;
                ctx.sid = web::token();
                ctx.sess = Row::new();
                ctx.user = None;
                let _ = name;
            }
            return ctx.redirect("/").await;
        }
        "register" => {
            if !ctx.st.cfg.allow_registration {
                return ctx.status(StatusCode::NOT_FOUND, "Registration is not enabled").await;
            }
            if ctx.method == "POST" {
                let sid = { let x = ctx.p("userid"); if x.is_empty() { ctx.p("staffid") } else { x } };
                let (name, pw) = (ctx.p("name"), ctx.p("password"));
                let taken = web::find_user_by_staffid(ctx.db(), &sid).await.is_some();
                if taken || sid.len() < 3 || !sid.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) || name.trim().is_empty() || pw.len() < 8 {
                    ctx.flash("Registration failed: user ID must be 3+ letters/numbers and unused, name is required, password 8+ characters");
                    return ctx.redirect("/user/register").await;
                }
                let _ = ctx.db().insert("portal_user", &row! {"userid"=>sid,"name"=>name.trim(),"email"=>ctx.p("email"),"password"=>bcrypt::hash(&pw, 12).unwrap_or_default(),"is_admin"=>false,"is_active"=>true,"reset_password"=>false,"date_joined"=>now_str()}).await;
                ctx.flash("Account created. Please log in.");
                return ctx.redirect("/login").await;
            }
            let f = format!("<form method='post' action='/user/register'>{}<label class='form-label'>User ID</label><input class='form-control mb-2' name='userid' required/><label class='form-label'>Name</label><input class='form-control mb-2' name='name' required/><label class='form-label'>E-mail</label><input type='email' class='form-control mb-2' name='email'/><label class='form-label'>Password</label><input type='password' class='form-control mb-3' name='password' required minlength='8'/><button class='btn btn-primary'>Register</button></form>", ctx.csrf_field());
            return ctx.html("Register", card("Register", &f)).await;
        }
        _ => {}
    }
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let uid = ctx.uid().unwrap_or(0);
    let num_id = id.as_deref().and_then(|x| x.parse::<i64>().ok()).or_else(|| ctx.pi("id"));
    match act {
        "" | "index" | "list" => index(ctx).await,
        "show" => show(ctx, num_id.unwrap_or(0)).await,
        "create" => create_or_edit(ctx, None).await,
        "edit" => create_or_edit(ctx, num_id).await,
        "save" => save_user(ctx, None).await,
        "update" => save_user(ctx, num_id).await,
        "delete" => delete_user(ctx, num_id.unwrap_or(0)).await,
        "clearsession" => clear_session(ctx, num_id.unwrap_or(0)).await,
        "updatelanid" => update_lanid(ctx, num_id.unwrap_or(0)).await,
        "completelist" => user_picker_list(ctx, false).await,
        "activelist" => user_picker_list(ctx, true).await,
        "my_profile" | "my_profile_save" => my_profile(ctx, uid).await,
        "change_password" | "update_password" => change_password(ctx, uid).await,
        "switchuser" => {
            if !is_switchable(&ctx).await || ctx.sess.contains_key("adminlink") {
                ctx.flash("Need to be a SuperUser to switch users (restore admin first if already switched)");
                return ctx.redirect("/").await;
            }
            let Some(target) = web::find_user_by_staffid(ctx.db(), &id.unwrap_or_default()).await else {
                ctx.flash("User not found");
                return ctx.redirect("/user/index").await;
            };
            let me = ctx.user.clone().unwrap();
            ctx.sess.insert("adminlink".into(), json!(s(&me, "userid")));
            ctx.sess.insert("realuserid".into(), json!(uid));
            ctx.sess.insert("userid".into(), target.get("id").cloned().unwrap_or(Value::Null));
            web::user_log(ctx.db(), "switch", "Switched user", Some(uid), i(&target, "id"), &ctx.ip).await;
            ctx.flash(&format!("Now acting as {}", s(&target, "name")));
            ctx.redirect("/").await
        }
        "restoreadmin" => {
            if let Some(rid) = i(&ctx.sess, "realuserid") {
                ctx.sess.insert("userid".into(), json!(rid));
                ctx.sess.remove("adminlink");
                ctx.sess.remove("realuserid");
                ctx.flash("Admin restored");
            }
            ctx.redirect("/user/index").await
        }
        "changerole" => {
            if let Some(target_id) = ctx.pi("chosenrole") {
                if let Ok(Some(tnu)) = ctx.db().first("select id, role from portal_tree_node_user where id=:id and user_id=:u limit 1", &row! {"id"=>target_id,"u"=>uid}).await {
                    let _ = ctx.db().update("portal_user", uid, &row! {"role"=>s(&tnu, "role"),"roletargetid"=>target_id}).await;
                    crate::cache::USERS.clear();
                }
            }
            let back = ctx.p("frompage");
            let to = if back.starts_with('/') && !back.starts_with("//") { back } else { "/".to_string() };
            ctx.redirect(&to).await
        }
        _ => ctx.status(StatusCode::NOT_FOUND, "Unknown action").await,
    }
}

fn split_tokens(raw: &str) -> Vec<String> {
    raw.replace(['[', ']', '\'', '"'], "")
        .split([',', ';', ' ', '\t', '\r', '\n'])
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

/// Matches g6ikram's `User.switchable()` (admin or in `portal.switchusers`, unless in `portal.switch_blacklist`).
pub async fn is_switchable(ctx: &Ctx) -> bool {
    let Some(u) = ctx.user.as_ref() else { return false };
    let uid_str = s(u, "userid").trim().to_string();
    if let Some(bl) = web::setting_str(ctx.db(), "portal.switch_blacklist").await {
        if split_tokens(&bl).iter().any(|x| x == &uid_str) {
            return false;
        }
    }
    if b(u, "is_admin") {
        return true;
    }
    if let Some(sw) = web::setting_str(ctx.db(), "portal.switchusers").await {
        return split_tokens(&sw).iter().any(|x| x == &uid_str);
    }
    false
}

fn user_label(field: &str) -> &'static str {
    match field {
        "userid" => "User ID",
        "name" => "Name",
        "email" => "Email",
        "role" => "Role",
        "roletargetid" => "Roletargetid",
        "lastlogin" => "Lastlogin",
        "date_joined" => "Datejoined",
        "telegram_chat_id" => "Telegramchatid",
        "telegram_username" => "Telegramusername",
        "telegram_linked_at" => "Telegramlinkedat",
        "telegram_link_token" => "Telegramlinktoken",
        "telegram_token_expiry" => "Telegramtokenexpiry",
        "is_active" => "Is Active",
        "is_admin" => "Is Admin",
        "reset_password" => "Reset Password",
        "directline" => "Direct Line",
        "handphone" => "Handphone",
        "company_handphone" => "Company Handphone",
        "emergency_contact" => "Emergency Contact Number",
        "emergency_name" => "Emergency Contact Name",
        "address" => "Home Address",
        "state" => "State",
        "lanid" => "Lanid",
        "lanidexception" => "Lanidexception",
        "resetexception" => "Resetexception",
        "profile_id" => "Profile ID",
        "password" => "Password",
        "secretquestion" => "Secretquestion",
        "secretanswer" => "Secretanswer",
        _ => "",
    }
}

fn fmt_user_dt(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return String::new();
    }
    if let Some(dt) = crate::script::parse_dt(t) {
        return dt.format("%Y-%m-%d %H:%M:%S").to_string();
    }
    // Strip fractional seconds / timezone suffix on ISO/Postgres timestamps (e.g. 2026-11-22T06:40:36.123456Z)
    let norm = t.replace('T', " ");
    let base = norm.split('.').next().unwrap_or(&norm);
    let base = base.trim_end_matches('Z');
    let base = if base.len() > 19 { &base[..19] } else { base };
    match crate::script::parse_dt(base) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => t.to_string(),
    }
}

async fn node_full_path(db: &Db, node_id: i64) -> String {
    let mut parts = vec![];
    let mut cur = Some(node_id);
    let mut seen = HashSet::new();
    while let Some(nid) = cur {
        if !seen.insert(nid) || parts.len() > 20 {
            break;
        }
        let Ok(Some(n)) = db.get("portal_tree_node", nid).await else { break };
        parts.push(s(&n, "name"));
        cur = i(&n, "parent_id");
    }
    parts.reverse();
    parts.join(" / ")
}

async fn index(ctx: Ctx) -> Response {
    let db = &ctx.st.db.clone();
    let Some(u) = ctx.user.as_ref() else { return ctx.need_login().await };
    let is_admin = ctx.is_admin();
    let can_switch = is_switchable(&ctx).await && !ctx.sess.contains_key("adminlink");
    if !is_admin && !can_switch && web::admin_list(db, u).await.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this list").await;
    }

    let q = ctx.p("q");
    let rolefilter = ctx.p("rolefilter");
    let is_active_p = ctx.p("is_active");
    let mut cond: Vec<String> = vec![];
    if is_active_p == "0" {
        cond.push("is_active is not true".into());
    } else if is_active_p != "all" {
        cond.push("is_active is true".into());
    }
    if !rolefilter.is_empty() && rolefilter != "All" {
        cond.push(format!("id in (select user_id from portal_tree_node_user where role={})", lit(&json!(rolefilter))));
    }
    if !q.trim().is_empty() {
        let like = lit(&json!(format!("%{}%", q.trim().replace(' ', "%"))));
        cond.push(format!("(userid ilike {l} or name ilike {l} or email ilike {l} or lanid ilike {l})", l = like));
    }
    let wh = if cond.is_empty() { String::new() } else { format!(" where {}", cond.join(" and ")) };
    let max = ctx.pi("max").unwrap_or(10).clamp(1, 500);
    let offset = ctx.pi("offset").unwrap_or(0).max(0);
    let sort_p = ctx.p("sort");
    let order_dir = if ctx.p("order") == "desc" { "desc" } else { "asc" };
    let sort_col = match sort_p.as_str() {
        "userID" | "userid" => "userid",
        "name" => "name",
        "email" => "email",
        "role" => "role",
        _ => "name",
    };
    let order_sql = format!("\"{}\" {}", sort_col, order_dir);

    let total = db.count(&format!("select count(*) from portal_user{}", wh), &db::no_params()).await.unwrap_or(0);
    let rows = db
        .rows(&format!("select * from portal_user{} order by {} limit {} offset {}", wh, order_sql, max, offset), &db::no_params())
        .await
        .unwrap_or_default();

    let rolelist: Vec<String> = db
        .rows("select distinct role from portal_tree_node_user where role is not null and role != '' and role not like 'roleadmin%' order by role", &db::no_params())
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| s(&r, "role"))
        .collect();

    let has_um_admin = web::module_roles(db, u, "user_management").await.iter().any(|r| r == "Admin");

    let mut nav = String::from(
        "<div class=\"nav\" role=\"navigation\"><ul>\n\
         <li><a class=\"home\" href=\"/user/index\">Home</a></li>\n",
    );
    if is_admin {
        nav.push_str("<li><a class=\"create\" href=\"/user/create\">New User</a></li>\n");
    }
    if has_um_admin {
        nav.push_str("<li><a class=\"list\" href=\"/view/user_management/dashboard\">User Management Dashboard</a></li>\n");
    }
    nav.push_str("</ul></div>");

    let mut role_opts = String::from("<option value=\"\">All</option>");
    for r in &rolelist {
        role_opts.push_str(&format!("<option value=\"{}\"{}>{}</option>", esc(r), if r == &rolefilter { " selected" } else { "" }, esc(r)));
    }

    let q_keep = web::qs(&ctx.params, &["sort", "order", "offset", "max"]);
    let th_sort = |prop: &str, col: &str, label: &str| -> String {
        let sorted = sort_p == prop || (sort_p.is_empty() && col == "name");
        let next_ord = if sorted && order_dir == "asc" { "desc" } else { "asc" };
        let cls = if sorted { format!(" sortable sorted {}", order_dir) } else { " sortable".into() };
        let extra = if q_keep.is_empty() { String::new() } else { format!("&amp;{}", esc(&q_keep)) };
        format!("<th class=\"{cls}\"><a href=\"/user/index?sort={prop}&amp;max={max}&amp;order={next_ord}{extra}\">{label}</a></th>")
    };

    let mut tbody = String::new();
    for (idx, r) in rows.iter().enumerate() {
        let rid = i(r, "id").unwrap_or(0);
        let uid_str = s(r, "userid");
        let nm = esc(&s(r, "name"));
        let name_cell = if can_switch && !uid_str.is_empty() {
            format!("<a href=\"/user/switchuser/{}\">{nm}</a>", enc(&uid_str))
        } else {
            nm
        };
        let active = b(r, "is_active");
        let row_cls = format!("{} {}", if idx % 2 == 0 { "even" } else { "odd" }, if active { "" } else { "inactive-user" });
        tbody.push_str(&format!(
            "<tr class=\"{row_cls}\">\
             <td><a href=\"/user/show/{rid}\">{}</a>&nbsp;</td>\
             <td>{name_cell}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td></tr>\n",
            esc(&uid_str),
            esc(&s(r, "email")),
            esc(&s(r, "role"))
        ));
    }

    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let pag = if total > max {
        format!("<div class=\"pagination\">{}</div>", crate::tags::paginate("/user/index", &ctx.params, total, max, offset))
    } else {
        String::new()
    };

    let sel_all = if is_active_p == "all" { " selected" } else { "" };
    let sel_true = if is_active_p.is_empty() || is_active_p == "1" { " selected" } else { "" };
    let sel_false = if is_active_p == "0" { " selected" } else { "" };

    let body = format!(
        r##"<style>tr.inactive-user td {{ background-color: #f8d7da; color: #58151c; }}</style>
<div id="content" role="main"><div class="container">
<section class="row"><a href="#list-user" class="skip" tabindex="-1">Skip to content&hellip;</a>{nav}</section>
<section class="row"><div id="list-user" class="col-12 content scaffold-list" role="main">
  <h1>User List</h1>
  {flash_html}
  <form name="searchform" id="searchform" action="/user/index" method="get">
    <fieldset class="form">
      <div class="fieldcontain" id="listrole">
        <label>Role:</label><select name="rolefilter" id="rolefilter">{role_opts}</select>
      </div>
      <div class="fieldcontain" id="isactivediv">
        <label>Is Active:</label>
        <select name="is_active" id="is_active">
          <option value="all"{sel_all}>All</option>
          <option value="1"{sel_true}>True</option>
          <option value="0"{sel_false}>False</option>
        </select>
      </div>
      <div class="fieldcontain" id="listsearch">
        <label>Search:</label><input type="text" name="q" id="q" value="{q}"/><input id="dosearch" name="dosearch" type="submit" value="Search"/>
      </div>
    </fieldset>
  </form>
  <table class="responsive">
    <thead><tr>{th1}{th2}{th3}{th4}</tr></thead>
    <tbody>{tbody}</tbody>
  </table>
  {pag}
</div></section></div></div>
<script>$('#is_active,#rolefilter').on('change', function(){{ $('#searchform').submit(); }});</script>"##,
        q = esc(&q),
        th1 = th_sort("userID", "userid", "User ID"),
        th2 = th_sort("name", "name", "Name"),
        th3 = th_sort("email", "email", "Email"),
        th4 = th_sort("role", "role", "Role"),
    );
    ctx.html("User List", body).await
}

async fn show(mut ctx: Ctx, id: i64) -> Response {
    let db = &ctx.st.db.clone();
    let Ok(Some(u)) = db.get("portal_user", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Not found").await;
    };
    let is_admin = ctx.is_admin();
    let is_self = ctx.uid() == Some(id);
    let can_switch = is_switchable(&ctx).await && !ctx.sess.contains_key("adminlink");
    if !is_admin && !is_self && !can_switch {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this user").await;
    }

    let csrf = ctx.csrf_field();
    let uid_str = s(&u, "userid");
    let has_session_lock = !s(&u, "active_session_id").trim().is_empty();

    let mut nav = String::from(
        "<div class=\"nav\" role=\"navigation\"><ul>\n\
         <li><a class=\"home\" href=\"/\">Home</a></li>\n\
         <li><a class=\"list\" href=\"/user/index\">User List</a></li>\n",
    );
    if is_admin {
        nav.push_str(&format!(
            "<li><a class=\"create\" href=\"/user/create\">New User</a></li>\n\
             <li><a class=\"list\" href=\"/user/updatelanid/{id}\">Update LAN ID</a></li>\n"
        ));
    }
    if is_admin && has_session_lock {
        nav.push_str(&format!(
            "<li><form action=\"/user/clearsession/{id}\" method=\"post\" style=\"display:inline\">{csrf}<input type=\"submit\" class=\"list\" value=\"Clear Session Lock\"/></form></li>\n"
        ));
    }
    if can_switch && !uid_str.is_empty() {
        nav.push_str(&format!("<li><a class=\"list\" href=\"/user/switchuser/{}\">Switch</a></li>\n", enc(&uid_str)));
    }
    nav.push_str("</ul></div>");

    let prop_li = |field: &str, val_html: String| -> String {
        let lbl = user_label(field);
        format!(
            "    <li class=\"fieldcontain\">\n        <span id=\"{field}-label\" class=\"property-label\">{lbl}</span>\n        <div class=\"property-value\" aria-labelledby=\"{field}-label\">{val_html}</div>\n    </li>\n"
        )
    };

    // Block 1: public/summary fields (matching g6ikram user/show.gsp first <f:display>)
    let name_val = if can_switch && !uid_str.is_empty() {
        format!("<a href=\"/user/switchuser/{}\">{}</a>", enc(&uid_str), esc(&s(&u, "name")))
    } else {
        esc(&s(&u, "name"))
    };
    let is_active_val = if b(&u, "is_active") { "True" } else { "False" }.to_string();
    let mut ol1 = String::from("<ol class=\"property-list user\">\n");
    ol1.push_str(&prop_li("userid", esc(&uid_str)));
    ol1.push_str(&prop_li("name", name_val));
    ol1.push_str(&prop_li("email", esc(&s(&u, "email"))));
    ol1.push_str(&prop_li("role", esc(&s(&u, "role"))));
    ol1.push_str(&prop_li("lastlogin", esc(&fmt_user_dt(&s(&u, "lastlogin")))));
    ol1.push_str(&prop_li("date_joined", esc(&fmt_user_dt(&s(&u, "date_joined")))));
    ol1.push_str(&prop_li("telegram_chat_id", esc(&s(&u, "telegram_chat_id"))));
    ol1.push_str(&prop_li("telegram_username", esc(&s(&u, "telegram_username"))));
    ol1.push_str(&prop_li("telegram_linked_at", esc(&fmt_user_dt(&s(&u, "telegram_linked_at")))));
    ol1.push_str(&prop_li("telegram_link_token", esc(&s(&u, "telegram_link_token"))));
    ol1.push_str(&prop_li("telegram_token_expiry", esc(&fmt_user_dt(&s(&u, "telegram_token_expiry")))));
    ol1.push_str(&prop_li("is_active", is_active_val));
    ol1.push_str("</ol>\n");

    // Block 2: private contact fields (admin or self, matching second <f:display>)
    let mut ol2 = String::new();
    if is_admin || is_self {
        ol2.push_str("<ol class=\"property-list user\">\n");
        ol2.push_str(&prop_li("directline", esc(&s(&u, "directline"))));
        ol2.push_str(&prop_li("handphone", esc(&s(&u, "handphone"))));
        ol2.push_str(&prop_li("company_handphone", esc(&s(&u, "company_handphone"))));
        ol2.push_str(&prop_li("emergency_contact", esc(&s(&u, "emergency_contact"))));
        ol2.push_str(&prop_li("emergency_name", esc(&s(&u, "emergency_name"))));
        ol2.push_str(&prop_li("address", esc(&s(&u, "address")).replace('\n', "<br/>")));
        ol2.push_str(&prop_li("state", esc(&s(&u, "state"))));
        ol2.push_str(&prop_li("lanid", esc(&s(&u, "lanid"))));
        ol2.push_str("</ol>\n");
    }

    // Tree Roles & Module Roles (matching g6ikram user/show.gsp lines 47-60)
    let mut roles_html = String::new();
    if can_switch {
        let troles = db
            .rows(
                "select tnu.id, tnu.role, n.id as nid, n.name as nname, t.root_id \
                 from portal_tree_node_user tnu \
                 join portal_tree_node n on n.id=tnu.node_id \
                 join portal_tree t on t.id=n.tree_id \
                 where tnu.user_id=:u order by tnu.id",
                &row! {"u"=>id},
            )
            .await
            .unwrap_or_default();
        roles_html.push_str("<h3>Tree Roles</h3><ol>");
        for tr in &troles {
            let nid = i(tr, "nid").unwrap_or(0);
            let root_id = i(tr, "root_id").unwrap_or(nid);
            let full = node_full_path(db, nid).await;
            let label = format!("{} - {}", s(tr, "nname"), s(tr, "role"));
            roles_html.push_str(&format!(
                "<li><a href=\"/portalTreeNode/show/{root_id}?nodeid={nid}\">{}</a> - {}</li>",
                esc(&label),
                esc(&full)
            ));
        }
        roles_html.push_str("</ol>");

        let mroles = db
            .rows("select id, module, role from user_role where user_id=:u order by module, role", &row! {"u"=>id})
            .await
            .unwrap_or_default();
        roles_html.push_str("<h3>Module Roles</h3><ol>");
        for mr in &mroles {
            roles_html.push_str(&format!(
                "<li><a href=\"/userRole/show/{}\">{} - {}</a></li>",
                i(mr, "id").unwrap_or(0),
                esc(&s(mr, "module")),
                esc(&s(mr, "role"))
            ));
        }
        roles_html.push_str("</ol>");
    }

    let buttons = if is_admin {
        format!(
            "<form action=\"/user/delete/{id}\" method=\"post\">{csrf}\
             <fieldset class=\"buttons\">\
             <a class=\"edit\" href=\"/user/edit/{id}\">Edit</a>\
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
<section class="row"><a href="#show-user" class="skip" tabindex="-1">Skip to content&hellip;</a>{nav}</section>
<section class="row"><div id="show-user" class="col-12 content scaffold-show" role="main">
  <h1>Show User</h1>
  {flash_html}
  {ol1}{ol2}{roles_html}{buttons}
</div></section></div></div>"##
    );
    ctx.html("Show User", body).await
}

async fn create_or_edit(mut ctx: Ctx, id: Option<i64>) -> Response {
    let db = &ctx.st.db.clone();
    let is_admin = ctx.is_admin();
    let is_new = id.is_none();
    if is_new {
        if !is_admin && web::admin_list(db, ctx.user.as_ref().unwrap()).await.is_empty() {
            return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
        }
    } else if !is_admin {
        return ctx.status(StatusCode::FORBIDDEN, "Administrator rights required").await;
    }

    let u = match id {
        Some(uid) => match db.get("portal_user", uid).await {
            Ok(Some(r)) => r,
            _ => return ctx.status(StatusCode::NOT_FOUND, "Not found").await,
        },
        None => Row::new(),
    };

    let txt = |name: &str, req: bool| -> String {
        let lbl = user_label(name);
        let req_cls = if req { " required" } else { "" };
        let req_star = if req { "<span class=\"required-indicator\">*</span>" } else { "" };
        let req_attr = if req { " required=\"required\"" } else { "" };
        format!(
            "<div class=\"fieldcontain{req_cls}\"><label for=\"{name}\">{lbl}{req_star}</label><input type=\"text\" name=\"{name}\" id=\"{name}\" value=\"{}\"{req_attr}/></div>",
            esc(&s(&u, name))
        )
    };
    let chk = |name: &str| -> String {
        let lbl = user_label(name);
        let checked = if b(&u, name) { " checked=\"checked\"" } else { "" };
        format!(
            "<div class=\"fieldcontain\"><label for=\"{name}\">{lbl}</label><input type=\"hidden\" name=\"_{name}\" /><input type=\"checkbox\" name=\"{name}\" id=\"{name}\"{checked}/></div>"
        )
    };
    let num = |name: &str| -> String {
        let lbl = user_label(name);
        format!(
            "<div class=\"fieldcontain\"><label for=\"{name}\">{lbl}</label><input type=\"number\" name=\"{name}\" id=\"{name}\" value=\"{}\"/></div>",
            esc(&s(&u, name))
        )
    };
    let area = |name: &str| -> String {
        let lbl = user_label(name);
        format!(
            "<div class=\"fieldcontain\"><label for=\"{name}\">{lbl}</label><textarea name=\"{name}\" id=\"{name}\">{}</textarea></div>",
            esc(&s(&u, name))
        )
    };

    let mut fields = String::new();
    fields.push_str(&txt("userid", true));
    fields.push_str(&txt("name", true));
    fields.push_str(&txt("email", false));
    if is_new {
        fields.push_str("<div class=\"fieldcontain required\"><label for=\"password\">Password<span class=\"required-indicator\">*</span></label><input type=\"password\" name=\"password\" id=\"password\" autocomplete=\"new-password\" required=\"required\"/></div>");
    } else {
        fields.push_str("<div class=\"fieldcontain\"><label for=\"password\">Password</label><input type=\"password\" name=\"password\" id=\"password\" autocomplete=\"new-password\" placeholder=\"leave blank to keep current password\"/></div>");
        fields.push_str(&txt("role", false));
        fields.push_str(&num("roletargetid"));
        fields.push_str(&chk("is_admin"));
        fields.push_str(&chk("is_active"));
        fields.push_str(&chk("reset_password"));
        fields.push_str(&chk("lanidexception"));
        fields.push_str(&chk("resetexception"));
    }
    fields.push_str(&txt("directline", false));
    fields.push_str(&txt("handphone", false));
    fields.push_str(&txt("company_handphone", false));
    fields.push_str(&txt("emergency_contact", false));
    fields.push_str(&txt("emergency_name", false));
    fields.push_str(&area("address"));
    fields.push_str(&txt("state", false));
    fields.push_str(&txt("lanid", false));
    fields.push_str(&num("profile_id"));
    if !is_new {
        fields.push_str(&txt("telegram_chat_id", false));
        fields.push_str(&txt("telegram_username", false));
    }

    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let action = match id {
        Some(uid) => format!("/user/update/{uid}"),
        None => "/user/save".into(),
    };
    let kind = if is_new { "create" } else { "edit" };
    let heading = if is_new { "Create User" } else { "Edit User" };
    let btn = if is_new { "Create" } else { "Update" };
    let new_li = if !is_new && is_admin { "<li><a class=\"create\" href=\"/user/create\">New User</a></li>" } else { "" };

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#{kind}-user" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/user/index">User List</a></li>
  {new_li}
</ul></div></section>
<section class="row"><div id="{kind}-user" class="col-12 content scaffold-{kind}" role="main">
  <h1>{heading}</h1>
  {flash_html}
  <form action="{action}" method="post">
    {csrf}
    <fieldset class="form">{fields}</fieldset>
    <fieldset class="buttons"><input class="save" type="submit" value="{btn}" /></fieldset>
  </form>
</div></section></div></div>"##
    );
    ctx.html(heading, body).await
}

async fn save_user(mut ctx: Ctx, id: Option<i64>) -> Response {
    if ctx.method != "POST" {
        return ctx.status(StatusCode::FORBIDDEN, "POST required").await;
    }
    let db = &ctx.st.db.clone();
    let is_admin = ctx.is_admin();
    if id.is_none() {
        if !is_admin && web::admin_list(db, ctx.user.as_ref().unwrap()).await.is_empty() {
            return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
        }
    } else if !is_admin {
        return ctx.status(StatusCode::FORBIDDEN, "Administrator rights required").await;
    }

    let uid_str = { let x = ctx.p("userid"); if x.trim().is_empty() { ctx.p("userID") } else { x } }.trim().to_string();
    let name = ctx.p("name").trim().to_string();
    if uid_str.len() < 3 || !uid_str.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        ctx.flash("User ID must be at least 3 letters, numbers, dots, underscores or dashes");
        return ctx.redirect(&id.map(|x| format!("/user/edit/{x}")).unwrap_or_else(|| "/user/create".into())).await;
    }
    if name.is_empty() {
        ctx.flash("Name is required");
        return ctx.redirect(&id.map(|x| format!("/user/edit/{x}")).unwrap_or_else(|| "/user/create".into())).await;
    }
    let dup = db
        .count("select count(*) from portal_user where userid=:s and id!=:id", &row! {"s"=>uid_str.clone(),"id"=>id.unwrap_or(0)})
        .await
        .unwrap_or(0);
    if dup > 0 {
        ctx.flash("That user ID is already in use");
        return ctx.redirect(&id.map(|x| format!("/user/edit/{x}")).unwrap_or_else(|| "/user/create".into())).await;
    }

    let mut d = Row::new();
    d.insert("userid".into(), json!(uid_str));
    d.insert("name".into(), json!(name.clone()));
    for k in ["email", "directline", "handphone", "company_handphone", "emergency_contact", "emergency_name", "address", "state", "lanid"] {
        let v = ctx.p(k);
        d.insert(k.into(), if v.trim().is_empty() { Value::Null } else { json!(v.trim()) });
    }
    d.insert("profile_id".into(), ctx.pi("profile_id").map(|n| json!(n)).unwrap_or(Value::Null));

    let pw = ctx.p("password");
    if id.is_none() {
        if pw.len() < 8 {
            ctx.flash("A password of at least 8 characters is required for a new user");
            return ctx.redirect("/user/create").await;
        }
        d.insert("password".into(), json!(bcrypt::hash(&pw, 12).unwrap_or_default()));
        d.insert("is_admin".into(), json!(false));
        d.insert("is_active".into(), json!(true));
        d.insert("reset_password".into(), json!(false));
        d.insert("date_joined".into(), json!(now_str()));
    } else if !pw.is_empty() {
        if pw.len() < 8 {
            ctx.flash("Password must be at least 8 characters");
            return ctx.redirect(&format!("/user/edit/{}", id.unwrap())).await;
        }
        d.insert("password".into(), json!(bcrypt::hash(&pw, 12).unwrap_or_default()));
    }

    let mut extra_note = String::new();
    if let Some(uid) = id {
        if is_admin {
            if ctx.params.contains_key("_is_admin") {
                d.insert("is_admin".into(), json!(ctx.params.contains_key("is_admin")));
            }
            if ctx.params.contains_key("_is_active") {
                d.insert("is_active".into(), json!(ctx.params.contains_key("is_active")));
            }
            if ctx.params.contains_key("_reset_password") {
                d.insert("reset_password".into(), json!(ctx.params.contains_key("reset_password")));
            }
            if ctx.params.contains_key("_lanidexception") {
                d.insert("lanidexception".into(), json!(ctx.params.contains_key("lanidexception")));
            }
            if ctx.params.contains_key("_resetexception") {
                d.insert("resetexception".into(), json!(ctx.params.contains_key("resetexception")));
            }
            let role_v = ctx.p("role");
            d.insert("role".into(), if role_v.trim().is_empty() { Value::Null } else { json!(role_v.trim()) });
            for tk in ["telegram_chat_id", "telegram_username"] {
                if ctx.params.contains_key(tk) {
                    let v = ctx.p(tk);
                    d.insert(tk.into(), if v.trim().is_empty() { Value::Null } else { json!(v.trim()) });
                }
            }
            if ctx.params.contains_key("roletargetid") {
                match ctx.pi("roletargetid") {
                    None => {
                        d.insert("roletargetid".into(), Value::Null);
                    }
                    Some(wanted) => {
                        let holder = db.first("select user_id from portal_tree_node_user where id=:id", &row! {"id"=>wanted}).await.ok().flatten().and_then(|r| i(&r, "user_id"));
                        if holder == Some(uid) {
                            d.insert("roletargetid".into(), json!(wanted));
                        } else {
                            extra_note = format!(" Role target {} is not one of {}'s tree positions - left unchanged.", wanted, name);
                        }
                    }
                }
            }
        }
    }
    d.insert("last_updated".into(), json!(now_str()));

    let res = match id {
        Some(uid) => db.update("portal_user", uid, &d).await.map(|_| uid),
        None => db.insert("portal_user", &d).await,
    };
    match res {
        Ok(rid) => {
            crate::cache::USERS.clear();
            web::user_log(db, "user", if id.is_some() { "User updated" } else { "User created" }, ctx.uid(), Some(rid), &ctx.ip).await;
            ctx.flash(&format!("User {}{}", if id.is_some() { "updated" } else { "created" }, extra_note));
            ctx.redirect(&format!("/user/show/{rid}")).await
        }
        Err(e) => {
            ctx.flash(&format!("Could not save: {}", e));
            ctx.redirect("/user/index").await
        }
    }
}

/// Matches g6ikram's UserController.delete: soft-deactivates the user (`is_active = false`).
async fn delete_user(mut ctx: Ctx, id: i64) -> Response {
    if ctx.method != "POST" || !ctx.is_admin() {
        return ctx.status(StatusCode::FORBIDDEN, "Administrator rights required").await;
    }
    let db = &ctx.st.db.clone();
    if let Ok(Some(_)) = db.get("portal_user", id).await {
        let _ = db.update("portal_user", id, &row! {"is_active"=>false,"last_updated"=>now_str()}).await;
        crate::cache::USERS.clear();
        web::user_log(db, "user", "User deactivated", ctx.uid(), Some(id), &ctx.ip).await;
        ctx.flash(&format!("User {} deleted", id));
    }
    ctx.redirect("/user/index").await
}

async fn clear_session(mut ctx: Ctx, id: i64) -> Response {
    if ctx.method != "POST" || !ctx.is_admin() {
        ctx.flash("Need to be admin");
        return ctx.redirect("/").await;
    }
    let db = &ctx.st.db.clone();
    if let Ok(Some(u)) = db.get("portal_user", id).await {
        let _ = db.update("portal_user", id, &row! {"active_session_id"=>Value::Null,"active_session_updated"=>Value::Null}).await;
        ctx.flash(&format!("Session lock cleared for {}", s(&u, "name")));
    }
    ctx.redirect(&format!("/user/show/{id}")).await
}

async fn update_lanid(mut ctx: Ctx, id: i64) -> Response {
    if !ctx.is_admin() {
        return ctx.status(StatusCode::FORBIDDEN, "Administrator rights required").await;
    }
    let db = &ctx.st.db.clone();
    if let Ok(Some(u)) = db.get("portal_user", id).await {
        ctx.flash(&format!("LDAP LAN ID lookup is not configured for {}", s(&u, "name")));
    }
    ctx.redirect(&format!("/user/show/{id}")).await
}

async fn user_picker_list(ctx: Ctx, active_only: bool) -> Response {
    let db = &ctx.st.db.clone();
    let q = ctx.p("q");
    let like = lit(&json!(format!("%{}%", q.trim().replace(' ', "%"))));
    let mut cond = vec![format!("(name ilike {l} or userid ilike {l} or email ilike {l} or lanid ilike {l})", l = like)];
    if active_only {
        cond.push("is_active is true".into());
    }
    let role_p = ctx.p("role");
    if !role_p.trim().is_empty() {
        let roles: Vec<String> = role_p.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
        if !roles.is_empty() {
            cond.push(format!("id in (select user_id from portal_tree_node_user where role in {})", lit(&json!(roles))));
        }
    }
    let lim = if active_only { 20 } else { 9 };
    let mut rows = db
        .rows(&format!("select id, name from portal_user where {} order by name limit {}", cond.join(" and "), lim), &db::no_params())
        .await
        .unwrap_or_default();
    if let Some(vid) = ctx.pi("value") {
        if !rows.iter().any(|r| i(r, "id") == Some(vid)) {
            if let Ok(Some(cur)) = db.get("portal_user", vid).await {
                rows.insert(0, cur);
            }
        }
    }
    let list: Vec<Value> = rows.iter().map(|r| json!({"id": i(r, "id"), "value": i(r, "id"), "name": s(r, "name")})).collect();
    ctx.json(json!({"users": list})).await
}

async fn my_profile(mut ctx: Ctx, uid: i64) -> Response {
    let db = &ctx.st.db.clone();
    const EDITABLE: &[&str] = &["name", "email", "directline", "handphone", "company_handphone", "emergency_contact", "emergency_name", "address", "state"];
    if ctx.method == "POST" {
        let mut d = Row::new();
        for k in EDITABLE {
            if ctx.params.contains_key(*k) {
                let v = ctx.p(k);
                d.insert((*k).to_string(), if v.trim().is_empty() { Value::Null } else { json!(v.trim()) });
            }
        }
        d.insert("last_info_update".into(), json!(now_str()));
        let _ = db.update("portal_user", uid, &d).await;
        crate::cache::USERS.clear();
        ctx.flash("Your profile has been updated");
        return ctx.redirect("/user/my_profile").await;
    }
    let u = db.get("portal_user", uid).await.ok().flatten().unwrap_or_else(|| ctx.user.clone().unwrap_or_default());
    let roles = db.rows("select id, module, role from user_role where user_id=:u order by module, role", &row! {"u"=>uid}).await.unwrap_or_default();
    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };

    let mut fields = format!(
        "<div class=\"fieldcontain\"><label for=\"userid\">User ID</label><input type=\"text\" id=\"userid\" value=\"{}\" readonly/></div>",
        esc(&s(&u, "userid"))
    );
    for k in EDITABLE {
        let lbl = user_label(k);
        if *k == "address" {
            fields.push_str(&format!("<div class=\"fieldcontain\"><label for=\"{k}\">{lbl}</label><textarea name=\"{k}\" id=\"{k}\">{}</textarea></div>", esc(&s(&u, k))));
        } else {
            fields.push_str(&format!("<div class=\"fieldcontain\"><label for=\"{k}\">{lbl}</label><input type=\"text\" name=\"{k}\" id=\"{k}\" value=\"{}\"/></div>", esc(&s(&u, k))));
        }
    }
    let roles_sec = if !roles.is_empty() {
        let mut rrows = String::new();
        for r in &roles {
            rrows.push_str(&format!(
                "<tr><td>{}</td><td><a href=\"/userRole/show/{}\">{}</a></td></tr>",
                esc(&s(r, "module")),
                i(r, "id").unwrap_or(0),
                esc(&s(r, "role"))
            ));
        }
        format!(
            "<h3>My Module Roles</h3>\
             <p>These module roles apply automatically at all times and don't require switching, so they won't show up next to your name above.</p>\
             <table class='table'><tr><th>Module</th><th>Role</th></tr>{rrows}</table>"
        )
    } else {
        String::new()
    };

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#edit-user" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul><li><a class="home" href="/">Home</a></li></ul></div></section>
<section class="row"><div id="edit-user" class="col-12 content scaffold-edit" role="main">
  <h1>My Profile</h1>
  {flash_html}
  <form action="/user/my_profile_save" method="post">
    {csrf}
    <fieldset class="form">{fields}</fieldset>
    <fieldset class="buttons"><input class="save" type="submit" value="Update" /></fieldset>
  </form>
  {roles_sec}
</div></section></div></div>"##
    );
    ctx.html("My Profile", body).await
}

async fn change_password(mut ctx: Ctx, uid: i64) -> Response {
    let db = &ctx.st.db.clone();
    if ctx.method == "POST" {
        let full = db.get("portal_user", uid).await.ok().flatten().unwrap_or_default();
        let cur = { let a = ctx.p("currentPassword"); if a.is_empty() { ctx.p("current") } else { a } };
        let new = { let a = ctx.p("newPassword"); if a.is_empty() { ctx.p("password") } else { a } };
        let conf = { let a = ctx.p("confirmPassword"); if a.is_empty() { ctx.p("password2") } else { a } };
        let msg = if !bcrypt::verify(&cur, &s(&full, "password")).unwrap_or(false) {
            "Current password is incorrect"
        } else if new.len() < 8 {
            "Password must be at least 8 characters long"
        } else if new != conf {
            "New passwords do not match"
        } else {
            let _ = db
                .update("portal_user", uid, &row! {"password"=>bcrypt::hash(&new, 12).unwrap_or_default(),"reset_password"=>false,"last_info_update"=>now_str(),"last_updated"=>now_str()})
                .await;
            web::user_log(db, "password", "Password changed", Some(uid), None, &ctx.ip).await;
            ctx.flash("Password successfully changed");
            return ctx.redirect("/").await;
        };
        ctx.flash(msg);
        return ctx.redirect("/user/change_password").await;
    }
    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"alert alert-primary\" role=\"status\">{}</div>", esc(&flash)) };
    let body = format!(
        r##"<div id="content" role="main"><div class="container"><section class="row">
<div id="edit-user" class="col-12 content scaffold-edit" role="main">
  <h1>Change Password</h1>
  {flash_html}
  <div class="card"><div class="card-body">
    <form action="/user/update_password" method="post" class="row g-3">
      {csrf}
      <div class="col-12"><label for="currentPassword" class="form-label">Current Password:</label><input type="password" name="currentPassword" id="currentPassword" class="form-control" required/></div>
      <div class="col-12"><label for="newPassword" class="form-label">New Password:</label><input type="password" name="newPassword" id="newPassword" class="form-control" required minlength="8"/>
        <div class="form-text">Password must be at least 8 characters long.</div></div>
      <div class="col-12"><label for="confirmPassword" class="form-label">Confirm New Password:</label><input type="password" name="confirmPassword" id="confirmPassword" class="form-control" required/></div>
      <div class="col-12"><button type="submit" class="btn btn-primary">Change Password</button> <a href="/" class="btn btn-secondary">Cancel</a></div>
    </form>
  </div></div>
</div></section></div></div>"##
    );
    ctx.html("Change Password", body).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_labels_match_g6ikram() {
        assert_eq!(user_label("userid"), "User ID");
        assert_eq!(user_label("is_active"), "Is Active");
        assert_eq!(user_label("directline"), "Direct Line");
        assert_eq!(user_label("company_handphone"), "Company Handphone");
        assert_eq!(user_label("emergency_contact"), "Emergency Contact Number");
        assert_eq!(user_label("emergency_name"), "Emergency Contact Name");
        assert_eq!(user_label("address"), "Home Address");
        assert_eq!(user_label("telegram_chat_id"), "Telegramchatid");
    }

    #[test]
    fn split_tokens_handles_commas_and_spaces() {
        assert_eq!(split_tokens("u1, u2; u3\nu4"), vec!["u1", "u2", "u3", "u4"]);
        assert!(split_tokens("  , ; ").is_empty());
    }

    #[test]
    fn fmt_user_dt_normalizes_iso_and_pg() {
        assert_eq!(fmt_user_dt("2026-11-22T06:40:36.123456Z"), "2026-11-22 06:40:36");
        assert_eq!(fmt_user_dt("2026-11-22 06:40:36+08"), "2026-11-22 06:40:36");
        assert_eq!(fmt_user_dt(""), "");
    }
}
