//! Google OAuth 2.0 login (/auth/google, /auth/google/callback, /googleOAuth/*).
//! Matches g6ikram's GoogleOAuthController: signs in an existing user by email or
//! creates a new active account with a unique login ID and random bcrypt password.

use crate::db::{b, i, s, Db, Row};
use crate::row;
use crate::web::{self, enc, now_str, Ctx};
use axum::response::Response;
use serde_json::{json, Value};

const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const USERINFO_ENDPOINT: &str = "https://www.googleapis.com/oauth2/v3/userinfo";

async fn unique_userid(db: &Db, email: &str) -> String {
    let local = email.split('@').next().unwrap_or("");
    let mut base: String = local
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(40)
        .collect();
    if base.is_empty() {
        base = "user".into();
    }
    let mut candidate = base.clone();
    let mut suffix = 1usize;
    while web::find_user_by_staffid(db, &candidate).await.is_some() {
        suffix += 1;
        candidate = format!("{}{}", base, suffix);
    }
    candidate
}

pub async fn initiate(mut ctx: Ctx) -> Response {
    let cfg = &ctx.st.cfg.google_oauth;
    if cfg.client_id.is_empty() || cfg.client_secret.is_empty() || cfg.callback_url.is_empty() {
        ctx.flash("Configuration error: Google OAuth configuration is missing");
        return ctx.redirect("/login").await;
    }
    let state = web::token();
    ctx.sess.insert("oauth_state".into(), json!(state.clone()));
    let url = format!(
        "{}?client_id={}&response_type=code&scope={}&redirect_uri={}&access_type=offline&prompt=consent&state={}",
        AUTH_ENDPOINT,
        enc(&cfg.client_id),
        enc("email profile"),
        enc(&cfg.callback_url),
        enc(&state)
    );
    ctx.redirect(&url).await
}

pub async fn callback(mut ctx: Ctx) -> Response {
    let cfg = ctx.st.cfg.google_oauth.clone();
    if cfg.client_id.is_empty() || cfg.client_secret.is_empty() || cfg.callback_url.is_empty() {
        ctx.flash("Authentication error: Google OAuth configuration is missing");
        return ctx.redirect("/login").await;
    }
    let code = ctx.p("code");
    if code.is_empty() {
        ctx.flash("Authorization code not received");
        return ctx.redirect("/login").await;
    }
    let expected_state = s(&ctx.sess, "oauth_state");
    let got_state = ctx.p("state");
    if !expected_state.is_empty() && !got_state.is_empty() && expected_state != got_state {
        ctx.flash("Authentication error: invalid OAuth state");
        return ctx.redirect("/login").await;
    }

    let client = match reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build() {
        Ok(c) => c,
        Err(e) => {
            ctx.flash(&format!("Authentication error: {}", e));
            return ctx.redirect("/login").await;
        }
    };
    let form_body = format!(
        "code={}&client_id={}&client_secret={}&redirect_uri={}&grant_type=authorization_code",
        enc(&code),
        enc(&cfg.client_id),
        enc(&cfg.client_secret),
        enc(&cfg.callback_url)
    );
    let tok_res = client
        .post(TOKEN_ENDPOINT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await;
    let tok_json: Value = match tok_res {
        Ok(r) if r.status().is_success() => match r.json().await {
            Ok(v) => v,
            Err(e) => {
                ctx.flash(&format!("Authentication error: {}", e));
                return ctx.redirect("/login").await;
            }
        },
        Ok(r) => {
            let status = r.status();
            let txt = r.text().await.unwrap_or_default();
            web::log_error(ctx.db(), "googleOAuth", "callback", &format!("token exchange {}: {}", status, txt), None, None, None).await;
            ctx.flash("Authentication error: failed to exchange authorization code");
            return ctx.redirect("/login").await;
        }
        Err(e) => {
            web::log_error(ctx.db(), "googleOAuth", "callback", &e.to_string(), None, None, None).await;
            ctx.flash(&format!("Authentication error: {}", e));
            return ctx.redirect("/login").await;
        }
    };

    let access_token = tok_json["access_token"].as_str().unwrap_or("");
    if access_token.is_empty() {
        ctx.flash("Authentication error: no access token returned");
        return ctx.redirect("/login").await;
    }

    let info_res = client.get(USERINFO_ENDPOINT).bearer_auth(access_token).send().await;
    let user_info: Value = match info_res {
        Ok(r) if r.status().is_success() => match r.json().await {
            Ok(v) => v,
            Err(e) => {
                ctx.flash(&format!("Authentication error: {}", e));
                return ctx.redirect("/login").await;
            }
        },
        Ok(r) => {
            let status = r.status();
            web::log_error(ctx.db(), "googleOAuth", "callback", &format!("userinfo {}", status), None, None, None).await;
            ctx.flash("Authentication error: failed to read Google profile");
            return ctx.redirect("/login").await;
        }
        Err(e) => {
            web::log_error(ctx.db(), "googleOAuth", "callback", &e.to_string(), None, None, None).await;
            ctx.flash(&format!("Authentication error: {}", e));
            return ctx.redirect("/login").await;
        }
    };

    let email = user_info["email"].as_str().unwrap_or("").trim().to_string();
    let name = user_info["name"].as_str().unwrap_or(&email).trim().to_string();
    if email.is_empty() {
        ctx.flash("Authentication error: Google account has no email");
        return ctx.redirect("/login").await;
    }

    let existing = ctx
        .db()
        .first("select * from iap_user where lower(email)=lower(:e) order by id limit 1", &row! {"e"=>email.clone()})
        .await
        .ok()
        .flatten();
    let uid = match existing {
        Some(u) => {
            if !b(&u, "isactive") {
                ctx.flash("This account is inactive");
                return ctx.redirect("/login").await;
            }
            i(&u, "id").unwrap_or(0)
        }
        None => {
            let userid = unique_userid(ctx.db(), &email).await;
            let rand_pw = bcrypt::hash(uuid::Uuid::new_v4().to_string(), 12).unwrap_or_default();
            match ctx
                .db()
                .insert(
                    "iap_user",
                    &row! {
                        "staffid"=>userid,
                        "emp_name"=>name,
                        "email"=>email,
                        "password"=>rand_pw,
                        "is_admin"=>false,
                        "isactive"=>true,
                        "date_joined"=>now_str(),
                        "last_updated"=>now_str()
                    },
                )
                .await
            {
                Ok(id) => id,
                Err(e) => {
                    web::log_error(ctx.db(), "googleOAuth", "callback", &e.to_string(), None, None, None).await;
                    ctx.flash(&format!("Authentication error: {}", e));
                    return ctx.redirect("/login").await;
                }
            }
        }
    };

    let back = s(&ctx.sess, "redirect_after_login");
    ctx.sid = web::token();
    ctx.sess = Row::new();
    ctx.sess.insert("userid".into(), json!(uid));
    ctx.csrf();
    let _ = ctx.db().exec("update iap_user set lastlogin=now() where id=:id", &row! {"id"=>uid}).await;
    web::user_log(ctx.db(), "login", "Logged in via Google OAuth", Some(uid), None, &ctx.ip).await;
    let to = if back.starts_with('/') && !back.starts_with("//") { back } else { "/".to_string() };
    ctx.redirect(&to).await
}
