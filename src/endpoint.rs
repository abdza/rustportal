//! Module-owned raw HTTP endpoints: /svc/{module}/{slug}/... served by a
//! CGI program or proxied to another HTTP service.

use crate::db::{b, csv, i, s, Row};
use crate::row;
use crate::web::{self, esc, Ctx};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde_json::Value;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Splits a CGI `target` into argv on spaces, honouring double quotes.
/// Matches g5portal's `PortalEndpoint.argv`: allows naming an interpreter ahead of
/// a script (`python3 /path/to/hgweb.cgi`) while keeping quoted paths with spaces
/// (`"C:\Program Files\Python39\python.exe" C:\scmrepos\hgweb.wsgi`) intact.
pub fn parse_argv(target: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quoted = false;
    let mut has = false;
    for c in target.trim().chars() {
        if c == '"' {
            quoted = !quoted;
            has = true;
        } else if c == ' ' && !quoted {
            if has {
                out.push(std::mem::take(&mut cur));
                has = false;
            }
        } else {
            cur.push(c);
            has = true;
        }
    }
    if has {
        out.push(cur);
    }
    out
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Inspect a `portal_endpoint` row and return `(problems, notes)` describing how
/// it would execute on this server (matching g5portal's `checkTarget`).
pub fn check_target(ep: &Row) -> (Vec<String>, Vec<String>) {
    let mut problems = vec![];
    let mut notes = vec![];
    let htype = s(ep, "handler_type");
    let target = s(ep, "target");
    if htype == "Proxy" {
        if target.trim().is_empty() {
            problems.push("No target URL set.".to_string());
        } else {
            notes.push(format!("Forwards to {}", target));
        }
    } else {
        let argv = parse_argv(&target);
        if argv.is_empty() {
            problems.push("No target set.".to_string());
        } else {
            let runs = argv.iter().map(|a| format!("[{}]", a)).collect::<Vec<_>>().join(" ");
            notes.push(format!("Runs: {}", runs));
            let prog = Path::new(&argv[0]);
            if !prog.exists() {
                let mut msg = format!("Program not found on this server: {}", argv[0]);
                if target.contains(' ') && !target.contains('"') {
                    msg.push_str(" - the target contains a space and is split into arguments; quote the path if it is one program.");
                }
                problems.push(msg);
            } else {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let exec = std::fs::metadata(prog).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false);
                    if !exec {
                        problems.push(format!("Program is not executable by the portal's account: {}", argv[0]));
                    } else {
                        notes.push("Program exists and is executable.".to_string());
                    }
                }
                #[cfg(not(unix))]
                {
                    notes.push("Program exists.".to_string());
                }
            }
        }
        let wd = s(ep, "working_dir");
        if !wd.trim().is_empty() && !Path::new(wd.trim()).is_dir() {
            problems.push(format!("Working directory does not exist: {}", wd));
        }
    }
    for f in ["env_json", "header_env_json"] {
        let v = s(ep, f);
        if !v.trim().is_empty() {
            if let Err(e) = serde_json::from_str::<Value>(&v) {
                problems.push(format!("{} is not valid JSON: {}", f, e));
            }
        }
    }
    match s(ep, "auth_mode").as_str() {
        "Token" if s(ep, "auth_token").is_empty() => {
            problems.push("Auth mode is Token but no token is set - every request will be refused.".to_string());
        }
        "None" => {
            notes.push("Anyone who can reach the URL can use this endpoint (auth mode None).".to_string());
        }
        _ => {}
    }
    if !b(ep, "enabled") {
        notes.push("Disabled - nothing is served until enabled.".to_string());
    }
    (problems, notes)
}

pub fn check_target_html(ep: &Row) -> String {
    let (problems, notes) = check_target(ep);
    let mut o = String::new();
    if !problems.is_empty() {
        o.push_str("<div style='border-left:4px solid #b91c1c;background:#ffebe9;padding:10px 14px;margin:10px 0'><b>Endpoint problems:</b><ul style='margin:4px 0 0 18px'>");
        for p in problems {
            o.push_str(&format!("<li>{}</li>", esc(&p)));
        }
        o.push_str("</ul></div>");
    }
    if !notes.is_empty() {
        o.push_str("<div style='border-left:4px solid #0969da;background:#ddf4ff;padding:10px 14px;margin:10px 0'><ul style='margin:0 0 0 18px'>");
        for n in notes {
            o.push_str(&format!("<li>{}</li>", esc(&n)));
        }
        o.push_str("</ul></div>");
    }
    o
}

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

fn query_param(qs: &str, name: &str) -> Option<String> {
    for pair in qs.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let dk = urlencoding::decode(k).unwrap_or_else(|_| k.into());
            if dk == name {
                let dv = urlencoding::decode(v).unwrap_or_else(|_| v.into());
                return Some(dv.into_owned());
            }
        }
    }
    None
}

fn host_and_port(ctx: &Ctx) -> (String, String) {
    let default_port = ctx.st.cfg.port.to_string();
    let Some(host) = ctx.headers.get(header::HOST).and_then(|v| v.to_str().ok()).map(|x| x.trim()).filter(|x| !x.is_empty()) else {
        return ("localhost".to_string(), default_port);
    };
    if let Some(rest) = host.strip_prefix('[') {
        if let Some((ip, after)) = rest.split_once(']') {
            let port = after.strip_prefix(':').map(|p| p.to_string()).unwrap_or(default_port);
            return (format!("[{}]", ip), port);
        }
    }
    if let Some((h, p)) = host.rsplit_once(':') {
        if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() {
            return (h.to_string(), p.to_string());
        }
    }
    (host.to_string(), default_port)
}

pub async fn serve(ctx: Ctx, module: String, slug: String, rest: String) -> Response {
    let ep = match ctx.db().first("select * from portal_endpoint where module=:m and slug=:s and enabled is true limit 1", &row! {"m"=>module.clone(),"s"=>slug.clone()}).await {
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
            let got = query_param(&ctx.query, "token")
                .or_else(|| ctx.headers.get("x-g5-token").and_then(|v| v.to_str().ok()).map(|x| x.to_string()))
                .or_else(|| ctx.headers.get("x-auth-token").and_then(|v| v.to_str().ok()).map(|x| x.to_string()))
                .or_else(|| ctx.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(|x| x.to_string()))
                .unwrap_or_default();
            if want.is_empty() || !constant_time_eq(want.as_bytes(), got.as_bytes()) {
                return plain(StatusCode::UNAUTHORIZED, "Unauthorized");
            }
        }
        _ => {
            let creds = ctx.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Basic ").or_else(|| v.strip_prefix("basic ")))
                .and_then(|x| base64::engine::general_purpose::STANDARD.decode(x.trim()).ok()).and_then(|x| String::from_utf8(x).ok());
            let mut ok = false;
            if let Some(raw_creds) = creds.as_deref() {
                if let Some((u, p)) = raw_creds.split_once(':') {
                    if let Some(user) = web::find_user_for_login(ctx.db(), u).await {
                        let via = format!("endpoint {}/{}", s(&ep, "module"), s(&ep, "slug"));
                        if b(&user, "is_active")
                            && crate::ad::endpoint_basic_check(&ctx.st, &user, raw_creds, p, &via).await
                            && role_ok(&ctx, &ep, &user).await
                        {
                            ok = true;
                            remote_user = s(&user, "userid");
                        }
                    }
                }
            }
            if !ok {
                let realm = { let r = s(&ep, "realm"); if r.is_empty() { format!("{}/{}", module, slug) } else { r.replace('"', "") } };
                return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, format!("Basic realm=\"{}\"", realm))], "Unauthorized").into_response();
            }
        }
    }
    let timeout = std::time::Duration::from_secs(i(&ep, "timeout_seconds").unwrap_or(300).clamp(1, 86400) as u64);
    let ctype = ctx.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if s(&ep, "handler_type") == "Proxy" {
        let url = format!("{}{}{}", s(&ep, "target").trim_end_matches('/'), rest, if ctx.query.is_empty() { String::new() } else { format!("?{}", ctx.query) });
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(timeout).build().unwrap_or_default();
        let method = reqwest::Method::from_bytes(ctx.method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut rq = client.request(method, &url).body(ctx.body.clone());
        if !ctype.is_empty() {
            rq = rq.header("content-type", ctype);
        }
        if !remote_user.is_empty() {
            rq = rq.header("x-remote-user", &remote_user).header("x-g5-user", &remote_user);
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
    // CGI (RFC 3875)
    let argv = parse_argv(&s(&ep, "target"));
    if argv.is_empty() {
        return plain(StatusCode::INTERNAL_SERVER_ERROR, "Endpoint has no target");
    }
    let (server_name, server_port) = host_and_port(&ctx);
    let path_info = if rest.is_empty() { "/".to_string() } else { rest };
    let script_name = format!("/svc/{}/{}", module, slug);

    let mut cmd = tokio::process::Command::new(&argv[0]);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("SERVER_PROTOCOL", "HTTP/1.1")
        .env("SERVER_SOFTWARE", "rustportal")
        .env("SERVER_NAME", &server_name)
        .env("SERVER_PORT", &server_port)
        .env("SCRIPT_NAME", &script_name)
        .env("REQUEST_METHOD", &ctx.method)
        .env("PATH_INFO", &path_info)
        .env("QUERY_STRING", &ctx.query)
        .env("REMOTE_ADDR", &ctx.ip)
        .env("REMOTE_USER", &remote_user)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    for k in ["SYSTEMROOT", "WINDIR", "COMSPEC", "TEMP", "TMP", "PATHEXT", "SYSTEMDRIVE", "PROGRAMDATA"] {
        if let Ok(v) = std::env::var(k) {
            cmd.env(k, v);
        }
    }
    #[cfg(unix)]
    if let Ok(v) = std::env::var("TMPDIR") {
        cmd.env("TMPDIR", v);
    }
    if !ctype.is_empty() {
        cmd.env("CONTENT_TYPE", &ctype);
    }
    if !ctx.body.is_empty() || ctx.headers.contains_key(header::CONTENT_LENGTH) {
        cmd.env("CONTENT_LENGTH", ctx.body.len().to_string());
    }
    // Forward request headers as HTTP_* (required by hgweb / wire protocols).
    // Omit Authorization, Content-Type, Content-Length (RFC 3875 §4.1.18), and
    // Expect (the HTTP server already handled 100-continue; forwarding it makes
    // hgweb's wsgicgi prepend a stray "Status: 100 Continue" to stdout).
    let mut http_hdrs: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for (k, v) in &ctx.headers {
        if k == header::AUTHORIZATION
            || k == header::CONTENT_TYPE
            || k == header::CONTENT_LENGTH
            || k == header::EXPECT
        {
            continue;
        }
        if let Ok(val) = v.to_str() {
            let env_key = format!("HTTP_{}", k.as_str().to_ascii_uppercase().replace('-', "_"));
            http_hdrs.entry(env_key).or_default().push(val.to_string());
        }
    }
    for (k, vals) in http_hdrs {
        cmd.env(k, vals.join(", "));
    }
    if !s(&ep, "working_dir").trim().is_empty() {
        let wd = s(&ep, "working_dir");
        if Path::new(wd.trim()).is_dir() {
            cmd.current_dir(wd.trim());
        }
    }
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&s(&ep, "header_env_json")) {
        for (h, envname) in map {
            let hlow = h.to_ascii_lowercase();
            if let Some(v) = ctx.headers.get(hlow.as_str()).and_then(|v| v.to_str().ok()) {
                cmd.env(crate::db::vs(&envname), v);
            }
        }
    }
    if let Ok(Value::Object(env)) = serde_json::from_str::<Value>(&s(&ep, "env_json")) {
        for (k, v) in env {
            cmd.env(k, crate::db::vs(&v));
        }
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return plain(StatusCode::INTERNAL_SERVER_ERROR, &format!("Cannot start handler: {}", e)),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let body = ctx.body;
        tokio::spawn(async move {
            let _ = stdin.write_all(&body).await;
            let _ = stdin.shutdown().await;
        });
    }
    // Drain stderr concurrently so the CGI child never blocks on a full stderr pipe buffer.
    let stderr_task = child.stderr.take().map(|mut err_pipe| {
        tokio::spawn(async move {
            let mut captured = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = err_pipe.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                if captured.len() < 4096 {
                    let take = (4096 - captured.len()).min(n);
                    captured.extend_from_slice(&buf[..take]);
                }
            }
            captured
        })
    });
    let Some(mut stdout) = child.stdout.take() else {
        return plain(StatusCode::INTERNAL_SERVER_ERROR, "Cannot read handler output");
    };

    // Read CGI response headers incrementally so large responses (e.g. `hg clone` / `git clone`)
    // begin streaming to the client immediately instead of buffering the entire payload in RAM.
    const MAX_CGI_HEAD: usize = 64 * 1024;
    let mut hdr_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut tmp = [0u8; 8192];
    let mut eof = false;
    let mut split: Option<(usize, usize)> = None;
    let head_deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(head_deadline, stdout.read(&mut tmp)).await {
            Ok(Ok(0)) => {
                eof = true;
                break;
            }
            Ok(Ok(n)) => {
                hdr_buf.extend_from_slice(&tmp[..n]);
                split = find_cgi_header_end(&hdr_buf);
                if split.is_some() || hdr_buf.len() >= MAX_CGI_HEAD {
                    break;
                }
            }
            Ok(Err(e)) => {
                let _ = child.kill().await;
                log_cgi_stderr(&module, &slug, stderr_task).await;
                return plain(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
            }
            Err(_) => {
                let _ = child.kill().await;
                log_cgi_stderr(&module, &slug, stderr_task).await;
                return plain(StatusCode::GATEWAY_TIMEOUT, "Handler timed out");
            }
        }
    }

    if hdr_buf.is_empty() {
        let _ = child.wait().await;
        log_cgi_stderr(&module, &slug, stderr_task).await;
        return plain(StatusCode::BAD_GATEWAY, "Endpoint produced no response");
    }

    let (head, leftover) = match split {
        Some((p, n)) => (String::from_utf8_lossy(&hdr_buf[..p]).to_string(), hdr_buf[p + n..].to_vec()),
        None => (String::new(), hdr_buf),
    };

    if eof {
        let _ = child.wait().await;
        log_cgi_stderr(&module, &slug, stderr_task).await;
        return build_cgi_response_with_body(&head, axum::body::Body::from(leftover));
    }

    // Stream remaining stdout chunks directly to the HTTP response with an inactivity timeout
    // per chunk; dropping the response (e.g. client disconnect) drops `child` (kill_on_drop).
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(8);
    let log_mod = module.clone();
    let log_slug = slug.clone();
    tokio::spawn(async move {
        if !leftover.is_empty() && tx.send(Ok(axum::body::Bytes::from(leftover))).await.is_err() {
            return;
        }
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match tokio::time::timeout(timeout, stdout.read(&mut chunk)).await {
                Ok(Ok(0)) => {
                    let _ = child.wait().await;
                    log_cgi_stderr(&log_mod, &log_slug, stderr_task).await;
                    break;
                }
                Ok(Ok(n)) => {
                    if tx.send(Ok(axum::body::Bytes::copy_from_slice(&chunk[..n]))).await.is_err() {
                        let _ = child.kill().await;
                        break;
                    }
                }
                Ok(Err(e)) => {
                    let _ = child.kill().await;
                    log_cgi_stderr(&log_mod, &log_slug, stderr_task).await;
                    let _ = tx.send(Err(e)).await;
                    break;
                }
                Err(_) => {
                    let _ = child.kill().await;
                    eprintln!("[endpoint {}/{}] stream timed out after {}s of inactivity", log_mod, log_slug, timeout.as_secs());
                    log_cgi_stderr(&log_mod, &log_slug, stderr_task).await;
                    let _ = tx.send(Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "Handler timed out"))).await;
                    break;
                }
            }
        }
    });

    build_cgi_response_with_body(&head, axum::body::Body::from_stream(RxStream(rx)))
}

struct RxStream(tokio::sync::mpsc::Receiver<Result<axum::body::Bytes, std::io::Error>>);

impl futures_util::Stream for RxStream {
    type Item = Result<axum::body::Bytes, std::io::Error>;

    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

async fn log_cgi_stderr(module: &str, slug: &str, task: Option<tokio::task::JoinHandle<Vec<u8>>>) {
    if let Some(t) = task {
        if let Ok(Ok(raw)) = tokio::time::timeout(std::time::Duration::from_secs(2), t).await {
            if !raw.is_empty() {
                let err = String::from_utf8_lossy(&raw);
                let clip: String = err.chars().take(2000).collect();
                eprintln!("[endpoint {}/{}] stderr: {}", module, slug, clip.trim());
            }
        }
    }
}

fn find_cgi_header_end(buf: &[u8]) -> Option<(usize, usize)> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| (p, 4))
        .or_else(|| buf.windows(2).position(|w| w == b"\n\n").map(|p| (p, 2)))
}

fn build_cgi_response_with_body(head: &str, body: axum::body::Body) -> Response {
    // Construct an empty-header response rather than `Vec<u8>::into_response()`, which
    // pre-populates `content-type: application/octet-stream` and would produce a
    // duplicate Content-Type header ahead of the CGI script's own Content-Type.
    let mut resp = Response::new(body);
    for line in head.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("status") {
                if let Some(code) = v.trim().split(' ').next().and_then(|c| c.parse::<u16>().ok()).and_then(|c| StatusCode::from_u16(c).ok()) {
                    *resp.status_mut() = code;
                }
            } else if let (Ok(name), Ok(val)) = (header::HeaderName::from_bytes(k.trim().as_bytes()), header::HeaderValue::from_str(v.trim())) {
                resp.headers_mut().append(name, val);
            }
        }
    }
    resp
}

#[cfg(test)]
fn build_cgi_response(out: Vec<u8>) -> Response {
    let (head, body) = match find_cgi_header_end(&out) {
        Some((p, n)) => (String::from_utf8_lossy(&out[..p]).to_string(), out[p + n..].to_vec()),
        None => (String::new(), out),
    };
    build_cgi_response_with_body(&head, axum::body::Body::from(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_argv_handles_quotes_and_spaces() {
        assert_eq!(parse_argv("/usr/lib/git-core/git-http-backend"), vec!["/usr/lib/git-core/git-http-backend"]);
        assert_eq!(
            parse_argv("\"C:\\Program Files\\Python39\\python.exe\" C:\\scmrepos\\hgweb.wsgi"),
            vec!["C:\\Program Files\\Python39\\python.exe", "C:\\scmrepos\\hgweb.wsgi"]
        );
        assert_eq!(parse_argv("  /usr/bin/python3   /srv/scm/hgweb.cgi  "), vec!["/usr/bin/python3", "/srv/scm/hgweb.cgi"]);
        assert!(parse_argv("   ").is_empty());
    }

    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq(b"secret123", b"secret123"));
        assert!(!constant_time_eq(b"secret123", b"secret124"));
        assert!(!constant_time_eq(b"secret123", b"secret12"));
    }

    #[test]
    fn cgi_response_has_single_content_type_from_script() {
        let raw = b"Status: 200 Script output follows\r\nContent-Type: application/mercurial-0.1\r\nContent-Length: 5\r\n\r\nhello".to_vec();
        let resp = build_cgi_response(raw);
        assert_eq!(resp.status(), StatusCode::OK);
        let cts: Vec<&str> = resp.headers().get_all(header::CONTENT_TYPE).iter().map(|v| v.to_str().unwrap()).collect();
        assert_eq!(cts, vec!["application/mercurial-0.1"]);
    }

    #[tokio::test]
    async fn hgweb_cgi_clone_and_push_over_http() {
        use axum::routing::any;
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        let has_hg = tokio::process::Command::new("hg").arg("--version").output().await.map(|o| o.status.success()).unwrap_or(false);
        let has_hgweb = tokio::process::Command::new("python3")
            .args(["-c", "from mercurial.hgweb import hgweb, wsgicgi"])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !has_hg || !has_hgweb {
            return;
        }

        let tmp = std::env::temp_dir().join(format!("rp_hgweb_test_{}_{}", std::process::id(), crate::web::token()));
        let repos_dir = tmp.join("repos");
        let repo = repos_dir.join("g5portal");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |cmd: &str, args: &[&str], dir: &Path| {
            let o = std::process::Command::new(cmd).args(args).current_dir(dir).output().unwrap();
            assert!(o.status.success(), "{} {:?} failed: {}", cmd, args, String::from_utf8_lossy(&o.stderr));
        };
        run("hg", &["init"], &repo);
        std::fs::write(repo.join("hello.txt"), "initial\n").unwrap();
        run("hg", &["add", "hello.txt"], &repo);
        run("hg", &["commit", "-m", "init", "-u", "tester"], &repo);

        let cfg_path = repos_dir.join("hgweb.config");
        std::fs::write(&cfg_path, format!("[paths]\n/ = {}/*\n\n[web]\nallow-push = *\npush_ssl = false\n", repos_dir.display())).unwrap();
        let cgi_path = repos_dir.join("hgweb.cgi");
        std::fs::write(
            &cgi_path,
            "import os\nfrom mercurial.hgweb import hgweb, wsgicgi\napplication = hgweb(os.environ['HGWEB_CONFIG'].encode('utf-8'))\nwsgicgi.launch(application)\n",
        ).unwrap();

        let db_path = tmp.join("test.db");
        let db = crate::db::Db::connect(&format!("sqlite://{}", db_path.display()), 2, None).await.unwrap();
        let entities = crate::entity::entities();
        crate::entity::sync_schema(&db, &entities).await.unwrap();
        db.insert("portal_endpoint", &row! {
            "module" => "scm", "slug" => "hg", "name" => "HG", "handler_type" => "CGI",
            "target" => format!("python3 {}", cgi_path.display()),
            "working_dir" => repos_dir.to_string_lossy().to_string(),
            "env_json" => serde_json::json!({"HGWEB_CONFIG": cfg_path.to_string_lossy(), "HGENCODING": "UTF-8"}).to_string(),
            "auth_mode" => "None",
            "enabled" => true,
        }).await.unwrap();

        let st: crate::web::St = Arc::new(crate::web::AppState {
            db,
            cfg: crate::web::Config::default(),
            entities,
            excel_jobs: Mutex::new(HashMap::new()),
            rt: tokio::runtime::Handle::current(),
        });
        let app = axum::Router::new()
            .fallback(any(|ctx: Ctx| async move {
                let r = ctx.path.strip_prefix("/svc/scm/hg").unwrap_or("").to_string();
                serve(ctx, "scm".to_string(), "hg".to_string(), r).await
            }))
            .with_state(st);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let clone_dir = tmp.join("cloned");
        let url = format!("http://{}/svc/scm/hg/g5portal", addr);
        let cl = tokio::process::Command::new("hg")
            .args(["clone", &url, clone_dir.to_str().unwrap()])
            .output()
            .await
            .unwrap();
        assert!(cl.status.success(), "hg clone failed:\nstdout: {}\nstderr: {}", String::from_utf8_lossy(&cl.stdout), String::from_utf8_lossy(&cl.stderr));
        assert_eq!(std::fs::read_to_string(clone_dir.join("hello.txt")).unwrap(), "initial\n");

        // Also verify hg push (which sends POST with Expect: 100-continue)
        std::fs::write(clone_dir.join("pushed.txt"), "from clone\n").unwrap();
        run("hg", &["add", "pushed.txt"], &clone_dir);
        run("hg", &["commit", "-m", "push test", "-u", "tester"], &clone_dir);
        let ps = tokio::process::Command::new("hg")
            .args(["push"])
            .current_dir(&clone_dir)
            .output()
            .await
            .unwrap();
        assert!(ps.status.success(), "hg push failed:\nstdout: {}\nstderr: {}", String::from_utf8_lossy(&ps.stdout), String::from_utf8_lossy(&ps.stderr));

        server.abort();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn cgi_streams_large_output_beyond_per_chunk_timeout() {
        use axum::routing::any;
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        let has_py = tokio::process::Command::new("python3").arg("--version").output().await.map(|o| o.status.success()).unwrap_or(false);
        if !has_py {
            return;
        }

        let tmp = std::env::temp_dir().join(format!("rp_stream_test_{}_{}", std::process::id(), crate::web::token()));
        std::fs::create_dir_all(&tmp).unwrap();
        let script = tmp.join("slow_stream.py");
        std::fs::write(
            &script,
            "import sys, time\nsys.stdout.buffer.write(b'Status: 200 OK\\r\\nContent-Type: application/octet-stream\\r\\n\\r\\n')\nsys.stdout.buffer.flush()\nfor i in range(4):\n    time.sleep(0.45)\n    sys.stdout.buffer.write(f'chunk-{i}\\n'.encode())\n    sys.stdout.buffer.flush()\n",
        ).unwrap();

        let db_path = tmp.join("test.db");
        let db = crate::db::Db::connect(&format!("sqlite://{}", db_path.display()), 2, None).await.unwrap();
        let entities = crate::entity::entities();
        crate::entity::sync_schema(&db, &entities).await.unwrap();
        // timeout_seconds=1, total script runtime ~1.8s across 4 chunks -> would 504 if buffered!
        db.insert("portal_endpoint", &row! {
            "module" => "scm", "slug" => "stream", "name" => "Stream", "handler_type" => "CGI",
            "target" => format!("python3 {}", script.display()),
            "auth_mode" => "None",
            "timeout_seconds" => 1,
            "enabled" => true,
        }).await.unwrap();

        let st: crate::web::St = Arc::new(crate::web::AppState {
            db,
            cfg: crate::web::Config::default(),
            entities,
            excel_jobs: Mutex::new(HashMap::new()),
            rt: tokio::runtime::Handle::current(),
        });
        let app = axum::Router::new()
            .fallback(any(|ctx: Ctx| async move {
                serve(ctx, "scm".to_string(), "stream".to_string(), "/".to_string()).await
            }))
            .with_state(st);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let resp = reqwest::get(format!("http://{}/svc/scm/stream/", addr)).await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.text().await.unwrap();
        assert_eq!(body, "chunk-0\nchunk-1\nchunk-2\nchunk-3\n");

        server.abort();
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
