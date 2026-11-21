//! Authentication and the user's own account screens.

use crate::db::{b, i, s, Row};
use crate::row;
use crate::web::{self, esc, now_str, Ctx};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};

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
    match act {
        "my_profile" | "my_profile_save" => {
            const EDITABLE: &[&str] = &["email", "handphone", "directline", "company_handphone", "address", "emergency_name", "emergency_contact"];
            if ctx.method == "POST" {
                let mut d = Row::new();
                for k in EDITABLE {
                    d.insert(k.to_string(), json!(ctx.p(k)));
                }
                d.insert("last_info_update".into(), json!(now_str()));
                let _ = ctx.db().update("portal_user", uid, &d).await;
                ctx.flash("Profile updated");
                return ctx.redirect("/user/my_profile").await;
            }
            let u = ctx.user.clone().unwrap();
            let roles = ctx.db().rows("select module, role from user_role where user_id=:u order by module, role", &row! {"u"=>uid}).await.unwrap_or_default();
            let mut f = format!("<p><b>{}</b> &middot; {}</p><form method='post' action='/user/my_profile'>{}", esc(&s(&u, "name")), esc(&s(&u, "userid")), ctx.csrf_field());
            for k in EDITABLE {
                f.push_str(&format!("<label class='form-label'>{}</label><input class='form-control mb-2' name='{}' value='{}'/>", crate::entity::label_of(k), k, esc(&s(&u, k))));
            }
            f.push_str("<button class='btn btn-primary'>Save</button></form><h5 class='mt-4'>My roles</h5><ul>");
            for r in roles {
                f.push_str(&format!("<li>{}: {}</li>", esc(&s(&r, "module")), esc(&s(&r, "role"))));
            }
            f.push_str("</ul>");
            ctx.html("My Profile", card("My Profile", &f)).await
        }
        "change_password" | "update_password" => {
            if ctx.method == "POST" {
                let full = ctx.db().get("portal_user", uid).await.ok().flatten().unwrap_or_default();
                let new = ctx.p("password");
                let msg = if !bcrypt::verify(ctx.p("current"), &s(&full, "password")).unwrap_or(false) {
                    "The current password is not correct"
                } else if new.len() < 8 {
                    "The new password must be at least 8 characters"
                } else if new != ctx.p("password2") {
                    "The passwords do not match"
                } else {
                    let _ = ctx.db().update("portal_user", uid, &row! {"password"=>bcrypt::hash(&new, 12).unwrap_or_default(),"reset_password"=>false,"last_updated"=>now_str()}).await;
                    web::user_log(ctx.db(), "password", "Password changed", Some(uid), None, &ctx.ip).await;
                    ctx.flash("Password changed");
                    return ctx.redirect("/").await;
                };
                ctx.flash(msg);
                return ctx.redirect("/user/change_password").await;
            }
            let f = format!("<form method='post' action='/user/change_password'>{}<label class='form-label'>Current password</label><input type='password' class='form-control mb-2' name='current' required/><label class='form-label'>New password</label><input type='password' class='form-control mb-2' name='password' required minlength='8'/><label class='form-label'>Repeat new password</label><input type='password' class='form-control mb-3' name='password2' required/><button class='btn btn-primary'>Change password</button></form>", ctx.csrf_field());
            ctx.html("Change Password", card("Change Password", &f)).await
        }
        "switchuser" => {
            if !ctx.is_admin() || ctx.sess.contains_key("adminlink") {
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
