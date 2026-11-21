//! Module-owned raw HTTP endpoints: /svc/{module}/{slug}/... served by a
//! CGI program or proxied to another HTTP service.

use crate::db::{b, csv, i, s, Row};
use crate::row;
use crate::web::{self, Ctx};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde_json::Value;
use tokio::io::AsyncWriteExt;

async fn role_ok(ctx: &Ctx, ep: &Row, user: &Row) -> bool {
    let roles = csv(&s(ep, "allowed_roles"));
    if roles.is_empty() || b(user, "is_admin") {
        return true;
    }
    let mine = web::module_roles(ctx.db(), user, &s(ep, "module")).await;
    roles.iter().any(|r| mine.contains(r))
}

fn plain(code: StatusCode, msg: &str) -> Response {
    (code, msg.to_string()).into_response()
}

pub async fn serve(ctx: Ctx, module: String, slug: String, rest: String) -> Response {
    let ep = match ctx.db().first("select * from portal_endpoint where module=:m and slug=:s and enabled is true limit 1", &row! {"m"=>module,"s"=>slug}).await {
        Ok(Some(e)) => e,
        _ => return plain(StatusCode::NOT_FOUND, "No such endpoint"),
    };
    if ctx.body.len() as i64 > i(&ep, "max_body_mb").unwrap_or(512).max(1) * 1024 * 1024 {
        return plain(StatusCode::PAYLOAD_TOO_LARGE, "Body too large");
    }
    let mut remote_user = String::new();
    match s(&ep, "auth_mode").as_str() {
        "None" => {}
        "Session" => match &ctx.user {
            Some(u) if role_ok(&ctx, &ep, u).await => remote_user = s(u, "userid"),
            _ => return plain(StatusCode::FORBIDDEN, "Forbidden"),
        },
        "Token" => {
            let want = s(&ep, "auth_token");
            let got = ctx.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(|x| x.to_string())
                .or_else(|| ctx.headers.get("x-auth-token").and_then(|v| v.to_str().ok()).map(|x| x.to_string())).unwrap_or_default();
            if want.is_empty() || got != want {
                return plain(StatusCode::UNAUTHORIZED, "Unauthorized");
            }
        }
        _ => {
            let creds = ctx.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Basic "))
                .and_then(|x| base64::engine::general_purpose::STANDARD.decode(x).ok()).and_then(|x| String::from_utf8(x).ok());
            let mut ok = false;
            if let Some((u, p)) = creds.as_deref().and_then(|c| c.split_once(':')) {
                if let Some(user) = web::find_user_by_staffid(ctx.db(), u).await {
                    if b(&user, "is_active") && bcrypt::verify(p, &s(&user, "password")).unwrap_or(false) && role_ok(&ctx, &ep, &user).await {
                        ok = true;
                        remote_user = u.to_string();
                    }
                }
            }
            if !ok {
                let realm = { let r = s(&ep, "realm"); if r.is_empty() { "Portal".to_string() } else { r.replace('"', "") } };
                return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, format!("Basic realm=\"{}\"", realm))], "Unauthorized").into_response();
            }
        }
    }
    let timeout = std::time::Duration::from_secs(i(&ep, "timeout_seconds").unwrap_or(300).clamp(1, 3600) as u64);
    let ctype = ctx.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if s(&ep, "handler_type") == "Proxy" {
        let url = format!("{}{}{}", s(&ep, "target").trim_end_matches('/'), rest, if ctx.query.is_empty() { String::new() } else { format!("?{}", ctx.query) });
        let client = reqwest::Client::builder().timeout(timeout).build().unwrap_or_default();
        let method = reqwest::Method::from_bytes(ctx.method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut rq = client.request(method, &url).body(ctx.body.clone());
        if !ctype.is_empty() {
            rq = rq.header("content-type", ctype);
        }
        if !remote_user.is_empty() {
            rq = rq.header("x-remote-user", remote_user);
        }
        return match rq.send().await {
            Ok(r) => {
                let code = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::OK);
                let ct = r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream").to_string();
                let body = r.bytes().await.map(|x| x.to_vec()).unwrap_or_default();
                (code, [(header::CONTENT_TYPE, ct)], body).into_response()
            }
            Err(e) => plain(StatusCode::BAD_GATEWAY, &format!("Upstream error: {}", e)),
        };
    }
    // CGI
    let mut cmd = tokio::process::Command::new(s(&ep, "target"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("REQUEST_METHOD", &ctx.method)
        .env("PATH_INFO", &rest)
        .env("QUERY_STRING", &ctx.query)
        .env("CONTENT_TYPE", &ctype)
        .env("CONTENT_LENGTH", ctx.body.len().to_string())
        .env("REMOTE_ADDR", &ctx.ip)
        .env("REMOTE_USER", &remote_user)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if !s(&ep, "working_dir").is_empty() {
        cmd.current_dir(s(&ep, "working_dir"));
    }
    if let Ok(Value::Object(env)) = serde_json::from_str::<Value>(&s(&ep, "env_json")) {
        for (k, v) in env {
            cmd.env(k, crate::db::vs(&v));
        }
    }
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&s(&ep, "header_env_json")) {
        for (h, envname) in map {
            if let Some(v) = ctx.headers.get(h.as_str()).and_then(|v| v.to_str().ok()) {
                cmd.env(crate::db::vs(&envname), v);
            }
        }
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return plain(StatusCode::INTERNAL_SERVER_ERROR, &format!("Cannot start handler: {}", e)),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let body = ctx.body.clone();
        tokio::spawn(async move {
            let _ = stdin.write_all(&body).await;
        });
    }
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o.stdout,
        Ok(Err(e)) => return plain(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(_) => return plain(StatusCode::GATEWAY_TIMEOUT, "Handler timed out"),
    };
    // split CGI headers from the body
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").map(|p| (p, 4)).or_else(|| out.windows(2).position(|w| w == b"\n\n").map(|p| (p, 2)));
    let (head, body) = match split {
        Some((p, n)) => (String::from_utf8_lossy(&out[..p]).to_string(), out[p + n..].to_vec()),
        None => (String::new(), out),
    };
    let mut resp = body.into_response();
    for line in head.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("status") {
                if let Some(code) = v.trim().split(' ').next().and_then(|c| c.parse::<u16>().ok()).and_then(|c| StatusCode::from_u16(c).ok()) {
                    *resp.status_mut() = code;
                }
            } else if let (Ok(name), Ok(val)) = (header::HeaderName::from_bytes(k.trim().as_bytes()), header::HeaderValue::from_str(v.trim())) {
                resp.headers_mut().insert(name, val);
            }
        }
    }
    resp
}
