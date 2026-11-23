//! Request context: session, current user, merged parameters, uploads,
//! CSRF protection, audit trail and the page layout.

use crate::db::{self, b, i, s, Db, Row};
use crate::entity::Entity;
use crate::row;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequest, Multipart, Request};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct MailCfg {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from: String,
    pub starttls: bool,
    pub disabled: bool,
}
impl Default for MailCfg {
    fn default() -> Self {
        MailCfg { host: "127.0.0.1".into(), port: 1025, username: String::new(), password: String::new(), from: "portal@localhost".into(), starttls: false, disabled: false }
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct GoogleOAuthCfg {
    pub client_id: String,
    pub client_secret: String,
    pub callback_url: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub port: u16,
    pub address: String,
    pub database_url: String,
    pub db_pool: u32,
    /// session time zone for the database (now(), current_date). Unset: this
    /// machine's zone with g6_schema (as the Grails portal's JDBC driver uses),
    /// otherwise UTC.
    pub db_timezone: Option<String>,
    pub upload_dir: String,
    pub module_dir: String,
    /// link on "Designed by ... Team" in the footer
    pub credits_url: String,
    /// footer "Copyright <b>...</b>" and "Designed by ..." (default: the app name / "<app name> Team")
    pub copyright: String,
    pub team: String,
    /// header logo and favicon (default: /static/images/logo.png and favicon.svg)
    pub logo_url: String,
    pub favicon_url: String,
    /// folders searched first for /assets/x, before this portal's own static files -
    /// e.g. a Grails portal's grails-app/assets/{images,javascripts,stylesheets,libs}
    pub assets_dirs: Vec<String>,
    pub static_dir: String,
    /// the tables pages and their data sources are kept in, so this portal can run
    /// on a Grails portal's database with its own copy of the pages beside the other's
    pub page_table: String,
    pub page_data_table: String,
    /// g6 Grails portal compatibility mode (GSP role rules, raw HTML field
    /// display, g6 file link rules, etc.)
    pub g6_schema: bool,
    /// run scheduled pages and send queued e-mail. Off when another portal on the
    /// same database already does, or every job would run twice.
    pub run_scheduler: bool,
    /// run Telegram bot poller, outbox sender, and email mirroring. Defaults to run_scheduler.
    pub run_telegram: Option<bool>,
    /// Content-Security-Policy header sent with every page (none when unset).
    /// A g6 portal sends one that keeps pages to their own site's scripts,
    /// styles, images and fonts; set the same here so pages look the same.
    pub content_security_policy: Option<String>,
    /// a g6 portal's server.user_profile ("module.slug"): the tracker holding each
    /// user's member record (portal_user.profile_id), used by role rules
    pub user_profile: Option<String>,
    pub allow_registration: bool,
    /// When true, Active Directory authentication is skipped completely and only
    /// local bcrypt passwords are checked (matching g5portal's `server.disable_lanid`).
    pub disable_lanid: bool,
    pub session_hours: i64,
    /// Optional session timeout in minutes (overrides `session_hours` when set).
    pub session_minutes: Option<i64>,
    pub app_name: String,
    pub mail: MailCfg,
    pub google_oauth: GoogleOAuthCfg,
    pub active_directory: crate::ad::ActiveDirectoryCfg,
}
impl Default for Config {
    fn default() -> Self {
        Config {
            port: 9090,
            address: "0.0.0.0".into(),
            database_url: "postgres://postgres@localhost/rustportal".into(),
            db_pool: 20,
            db_timezone: None,
            upload_dir: "uploads".into(),
            module_dir: "modulemigration".into(),
            credits_url: String::new(),
            copyright: String::new(),
            team: String::new(),
            logo_url: String::new(),
            favicon_url: String::new(),
            assets_dirs: vec![],
            static_dir: "static".into(),
            page_table: "portal_page".into(),
            page_data_table: "portal_page_data".into(),
            g6_schema: false,
            run_scheduler: true,
            run_telegram: None,
            content_security_policy: None,
            user_profile: None,
            allow_registration: false,
            disable_lanid: false,
            session_hours: 12,
            session_minutes: None,
            app_name: "Portal".into(),
            mail: MailCfg::default(),
            google_oauth: GoogleOAuthCfg::default(),
            active_directory: crate::ad::ActiveDirectoryCfg::default(),
        }
    }
}

pub struct ExcelJob {
    pub done: bool,
    pub file: Option<String>,
    pub error: Option<String>,
    pub filename: String,
}

pub struct AppState {
    pub db: Db,
    pub cfg: Config,
    pub entities: Vec<Entity>,
    pub excel_jobs: Mutex<HashMap<String, ExcelJob>>,
    pub rt: tokio::runtime::Handle,
}
pub type St = Arc<AppState>;

impl AppState {
    pub fn entity(&self, name: &str) -> Option<&Entity> {
        self.entities.iter().find(|e| e.name.eq_ignore_ascii_case(name))
    }
}

/// An uploaded file. Small files stay in memory; anything over SPILL_BYTES is
/// streamed to a temporary file as it arrives, so a large spreadsheet never sits in
/// memory whole. The temporary file is removed when the Upload is dropped (unless
/// `save_to` moved it into place).
pub struct Upload {
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
    pub temp: Option<std::path::PathBuf>,
    pub size: u64,
    head: Vec<u8>,
}

const SPILL_BYTES: usize = 8 * 1024 * 1024;

impl Upload {
    pub fn from_bytes(filename: &str, content_type: &str, bytes: Vec<u8>) -> Upload {
        Upload { filename: filename.into(), content_type: content_type.into(), size: bytes.len() as u64, head: bytes.iter().take(16).cloned().collect(), bytes, temp: None }
    }

    /// The first bytes of the file (for type sniffing).
    pub fn head(&self) -> &[u8] {
        &self.head
    }

    /// Write the file to `target` (a rename when it was spilled to disk).
    pub async fn save_to(&self, target: &std::path::Path) -> std::io::Result<()> {
        match &self.temp {
            Some(t) => {
                if tokio::fs::rename(t, target).await.is_err() {
                    tokio::fs::copy(t, target).await?;
                }
                Ok(())
            }
            None => tokio::fs::write(target, &self.bytes).await,
        }
    }

    /// The whole file in memory (for small files such as module zips).
    pub async fn read_all(&self) -> std::io::Result<Vec<u8>> {
        match &self.temp {
            Some(t) => tokio::fs::read(t).await,
            None => Ok(self.bytes.clone()),
        }
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        if let Some(t) = &self.temp {
            let _ = std::fs::remove_file(t);
        }
    }
}

async fn read_upload(field: &mut axum::extract::multipart::Field<'_>, tmpdir: &str, filename: String, content_type: String) -> Result<Upload, String> {
    use tokio::io::AsyncWriteExt;
    let mut buf: Vec<u8> = vec![];
    let mut head: Vec<u8> = vec![];
    let mut size = 0u64;
    let mut temp: Option<(std::path::PathBuf, tokio::io::BufWriter<tokio::fs::File>)> = None;
    while let Some(chunk) = field.chunk().await.map_err(|e| e.to_string())? {
        size += chunk.len() as u64;
        if head.len() < 16 {
            head.extend(chunk.iter().take(16 - head.len()));
        }
        match temp.as_mut() {
            Some((_, f)) => f.write_all(&chunk).await.map_err(|e| e.to_string())?,
            None => {
                buf.extend_from_slice(&chunk);
                if buf.len() > SPILL_BYTES {
                    tokio::fs::create_dir_all(tmpdir).await.map_err(|e| e.to_string())?;
                    let path = std::path::PathBuf::from(tmpdir).join(format!("upload_{}", token()));
                    let mut f = tokio::io::BufWriter::with_capacity(1 << 20, tokio::fs::File::create(&path).await.map_err(|e| e.to_string())?);
                    f.write_all(&buf).await.map_err(|e| e.to_string())?;
                    buf = vec![];
                    temp = Some((path, f));
                }
            }
        }
    }
    let temp = match temp {
        Some((path, mut f)) => {
            f.flush().await.map_err(|e| e.to_string())?;
            Some(path)
        }
        None => None,
    };
    Ok(Upload { filename, content_type, bytes: buf, temp, size, head })
}

pub struct Ctx {
    pub st: St,
    pub sid: String,
    pub sess: Row,
    sess_orig: String,
    sess_touch: bool,
    pub user: Option<Row>,
    pub params: Row,
    pub files: HashMap<String, Upload>,
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HeaderMap,
    pub ip: String,
    pub body: Vec<u8>,
    pub start: Instant,
    pub side_menu: Option<String>,
    pub module: Option<String>,
    pub slug: Option<String>,
}

pub fn esc(x: &str) -> String {
    let mut o = String::with_capacity(x.len() + 8);
    for c in x.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

pub fn enc(x: &str) -> String {
    urlencoding::encode(x).into_owned()
}

pub fn now_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn set_disposition(resp: &mut Response, filename: Option<&str>, inline: bool) {
    if let Some(f) = filename {
        let safe: String = f.chars().map(|c| if c == '"' || c == '\\' || c.is_control() { '_' } else { c }).collect();
        let disp = format!("{}; filename=\"{}\"; filename*=UTF-8''{}", if inline { "inline" } else { "attachment" }, safe.replace(|c: char| !c.is_ascii(), "_"), enc(&safe));
        if let Ok(v) = HeaderValue::from_str(&disp) {
            resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
        }
    }
}

pub fn token() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

fn add_param(p: &mut Row, k: String, v: String) {
    match p.get_mut(&k) {
        Some(Value::Array(a)) => a.push(Value::String(v)),
        Some(old) => {
            let prev = old.take();
            *old = Value::Array(vec![prev, Value::String(v)]);
        }
        None => {
            p.insert(k, Value::String(v));
        }
    }
}

pub fn parse_urlencoded(q: &str, p: &mut Row) {
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let dk = urlencoding::decode(&k.replace('+', " ")).map(|x| x.into_owned()).unwrap_or_default();
        let dv = urlencoding::decode(&v.replace('+', " ")).map(|x| x.into_owned()).unwrap_or_default();
        add_param(p, dk, dv);
    }
}

/// Query string for a set of parameters (used by pagination/links).
pub fn qs(p: &Row, skip: &[&str]) -> String {
    let mut parts = vec![];
    for (k, v) in p {
        if skip.contains(&k.as_str()) || k == "csrf_token" {
            continue;
        }
        match v {
            Value::Array(a) => {
                for x in a {
                    parts.push(format!("{}={}", enc(k), enc(&db::vs(x))));
                }
            }
            Value::Null => {}
            v => parts.push(format!("{}={}", enc(k), enc(&db::vs(v)))),
        }
    }
    parts.join("&")
}

fn err_page(code: StatusCode, msg: &str) -> Response {
    (code, axum::response::Html(format!("<html><body style='font-family:sans-serif;padding:40px'><h2>{}</h2><p>{}</p><p><a href='/'>Home</a></p></body></html>", code, esc(msg)))).into_response()
}

#[axum::async_trait]
impl FromRequest<St> for Ctx {
    type Rejection = Response;

    async fn from_request(req: Request, st: &St) -> Result<Self, Response> {
        let start = Instant::now();
        let method = req.method().as_str().to_string();
        let path = req.uri().path().to_string();
        let query = req.uri().query().unwrap_or("").to_string();
        let headers = req.headers().clone();
        let ip = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(',').next().unwrap_or("").trim().to_string())
            .or_else(|| req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip().to_string()))
            .unwrap_or_default();
        let mut params = Row::new();
        parse_urlencoded(&query, &mut params);
        let mut files = HashMap::new();
        let mut body = vec![];
        let ctype = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
        let is_svc = path.starts_with("/svc/");
        if !is_svc && ctype.starts_with("multipart/form-data") {
            let mut mp = Multipart::from_request(req, st).await.map_err(|e| err_page(StatusCode::BAD_REQUEST, &e.to_string()))?;
            let tmpdir = format!("{}/tmp", st.cfg.upload_dir);
            while let Ok(Some(mut field)) = mp.next_field().await {
                let name = field.name().unwrap_or("").to_string();
                let filename = field.file_name().map(|x| x.to_string());
                let ct = field.content_type().unwrap_or("application/octet-stream").to_string();
                match filename {
                    Some(fname) => {
                        let up = read_upload(&mut field, &tmpdir, fname.clone(), ct).await.map_err(|e| err_page(StatusCode::BAD_REQUEST, &e))?;
                        if !fname.is_empty() {
                            params.insert(name.clone(), Value::String(fname));
                            files.insert(name, up);
                        }
                    }
                    None => {
                        let data = field.bytes().await.map_err(|e| err_page(StatusCode::BAD_REQUEST, &e.to_string()))?;
                        add_param(&mut params, name, String::from_utf8_lossy(&data).into_owned())
                    }
                }
            }
        } else {
            let bytes = Bytes::from_request(req, st).await.map_err(|e| err_page(StatusCode::BAD_REQUEST, &e.to_string()))?;
            if !is_svc && ctype.starts_with("application/x-www-form-urlencoded") {
                parse_urlencoded(&String::from_utf8_lossy(&bytes), &mut params);
            } else if !is_svc && ctype.starts_with("application/json") {
                if let Ok(Value::Object(o)) = serde_json::from_slice::<Value>(&bytes) {
                    for (k, v) in o {
                        params.insert(k, v);
                    }
                }
                body = bytes.to_vec();
            } else {
                body = bytes.to_vec();
            }
        }

        // session
        let mut sid = String::new();
        if let Some(c) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
            for part in c.split(';') {
                if let Some(v) = part.trim().strip_prefix("RPSESSION=") {
                    sid = v.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                }
            }
        }
        let mut sess = Row::new();
        let mut sess_orig = String::new();
        let mut sess_touch = false;
        if !sid.is_empty() {
            let ttl_mins = session_timeout_minutes(&st.db, &st.cfg).await;
            if let Some((raw, obj, stale)) = load_session(&st.db, &sid, ttl_mins).await {
                sess_orig = raw;
                sess = obj;
                sess_touch = stale;
            } else {
                sid.clear();
            }
        }
        if sid.is_empty() {
            sid = token();
        }
        let mut user = None;
        if let Some(uid) = i(&sess, "userid") {
            user = load_user(&st.db, uid).await;
            if user.is_none() {
                sess.remove("userid");
            }
        }
        let ctx = Ctx { st: st.clone(), sid, sess, sess_orig, sess_touch, user, params, files, method, path, query, headers, ip, body, start, side_menu: None, module: None, slug: None };
        if ctx.method == "POST" && !is_svc {
            let sent = {
                let mut p = s(&ctx.params, "csrf_token");
                if p.is_empty() {
                    // a Grails page posting with its synchronizer token field
                    p = s(&ctx.params, "SYNCHRONIZER_TOKEN");
                }
                if p.is_empty() {
                    ctx.headers.get("x-csrf-token").and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
                } else {
                    p.split(',').next().unwrap_or("").to_string()
                }
            };
            let want = s(&ctx.sess, "csrf");
            if want.is_empty() || sent != want {
                return Err(err_page(StatusCode::FORBIDDEN, "Invalid session for the form. Please go back, reload the page and try again."));
            }
        }
        Ok(ctx)
    }
}

/// Effective session inactivity timeout in minutes.
/// Priority: `portal.session_minutes` in `portal_setting` (if > 0),
/// else `session_minutes` in config.toml, else `session_hours * 60` (min 1 minute).
pub async fn session_timeout_minutes(db: &Db, cfg: &Config) -> i64 {
    if let Some(v) = setting(db, "portal.session_minutes").await {
        let parsed = match &v {
            Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.round() as i64)),
            Value::String(x) => x.trim().parse::<i64>().ok(),
            _ => None,
        };
        if let Some(m) = parsed.filter(|&m| m > 0) {
            return m;
        }
    }
    if let Some(m) = cfg.session_minutes {
        return m.max(1);
    }
    cfg.session_hours.max(1) * 60
}

/// Load a non-expired session by id; deletes the row and returns `None` if expired.
/// The boolean indicates whether `updated` should be refreshed to slide the inactivity window.
async fn load_session(db: &Db, sid: &str, ttl_mins: i64) -> Option<(String, Row, bool)> {
    let ttl = ttl_mins.max(1);
    let touch_secs = (ttl * 60 / 4).clamp(15, 60);
    let q = format!(
        "select data, case when updated is not null and updated >= now() - interval '{} minutes' then 1 else 0 end as alive, case when updated < now() - interval '{} seconds' then 1 else 0 end as stale from portal_session where id=:id",
        ttl, touch_secs
    );
    let r = db.first(&q, &row! {"id"=>sid}).await.ok().flatten()?;
    if !b(&r, "alive") {
        let _ = db.exec("delete from portal_session where id=:id", &row! {"id"=>sid}).await;
        return None;
    }
    let raw = s(&r, "data");
    let obj = match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(o)) => o,
        _ => Row::new(),
    };
    Some((raw, obj, b(&r, "stale")))
}

/// Remove expired rows from `portal_session`.
pub async fn prune_sessions(db: &Db, ttl_mins: i64) {
    let ttl = ttl_mins.max(1);
    let _ = db
        .exec(
            &format!("delete from portal_session where updated is null or updated < now() - interval '{} minutes'", ttl),
            &Row::new(),
        )
        .await;
}

/// Load a user row for use by pages and scripts. Secrets are stripped and
/// compatibility aliases are added (`emp_name`, `isAdmin`, `isActive`, `isactive`, `userID`, `staffid`, `staffID`).
pub async fn load_user(db: &Db, id: i64) -> Option<Row> {
    let v = crate::cache::USERS
        .get_or(format!("u:{}", id), async {
            match db.get("portal_user", id).await.ok().flatten() {
                Some(mut u) => {
                    clean_user(&mut u);
                    Value::Object(u)
                }
                None => Value::Null,
            }
        })
        .await;
    match v {
        Value::Object(u) => Some(u),
        _ => None,
    }
}

/// A per-user list of strings, read through the user cache.
async fn user_list<F: std::future::Future<Output = Vec<String>>>(key: String, load: F) -> Vec<String> {
    let v = crate::cache::USERS.get_or(key, async { json!(load.await) }).await;
    v.as_array().map(|a| a.iter().map(db::vs).collect()).unwrap_or_default()
}

pub fn clean_user(u: &mut Row) {
    for k in ["password", "password5", "secretanswer", "version"] {
        u.remove(k);
    }
    let name = u.get("name").cloned().unwrap_or(Value::Null);
    u.insert("emp_name".into(), name);
    let adm = u.get("is_admin").cloned().unwrap_or(Value::Bool(false));
    u.insert("isAdmin".into(), adm);
    let act = u.get("is_active").cloned().unwrap_or(Value::Bool(false));
    u.insert("isActive".into(), act.clone());
    u.insert("isactive".into(), act);
    let uid = u.get("userid").cloned().unwrap_or(Value::Null);
    u.insert("userID".into(), uid.clone());
    u.insert("staffid".into(), uid.clone());
    u.insert("staffID".into(), uid);
}

pub async fn find_user_by_staffid(db: &Db, staffid: &str) -> Option<Row> {
    db.first("select * from portal_user where userid=:s order by id limit 1", &row! {"s"=>staffid}).await.ok().flatten()
}

/// Look up a user for login / Basic auth: matches `userid` first, then falls
/// back to Active Directory `lanid` (case-insensitive), matching g5portal's
/// `UserController.authenticate` and `PortalEndpointController`.
pub async fn find_user_for_login(db: &Db, username: &str) -> Option<Row> {
    let u = username.trim();
    if u.is_empty() {
        return None;
    }
    if let Some(r) = find_user_by_staffid(db, u).await {
        return Some(r);
    }
    db.first("select * from portal_user where lower(lanid)=lower(:s) order by id limit 1", &row! {"s"=>u}).await.ok().flatten()
}

pub async fn setting(db: &Db, name: &str) -> Option<Value> {
    let r = crate::cache::SETTINGS
        .get_or(name.to_string(), async {
            let r = match name.split_once('.') {
                Some((m, n)) => match db.first("select * from portal_setting where module=:m and name=:n order by id limit 1", &row! {"m"=>m,"n"=>n}).await {
                    Ok(Some(row)) => Ok(Some(row)),
                    Ok(None) => db.first("select * from portal_setting where name=:n order by id limit 1", &row! {"n"=>name}).await,
                    Err(e) => Err(e),
                },
                None => db.first("select * from portal_setting where name=:n order by id limit 1", &row! {"n"=>name}).await,
            };
            r.ok().flatten()
        })
        .await;
    r.map(|r| setting_value(&r))
}

pub async fn setting_or(db: &Db, name: &str, default: Value) -> Value {
    setting(db, name).await.unwrap_or(default)
}

pub async fn setting_str(db: &Db, name: &str) -> Option<String> {
    setting(db, name).await.map(|v| db::vs(&v)).filter(|x| !x.is_empty())
}

fn datum(v: &str, datum_type: &str) -> Value {
    match datum_type {
        "Integer" => v.trim().parse::<i64>().map(Value::from).unwrap_or(Value::String(v.to_string())),
        "Float" => v.trim().parse::<f64>().map(Value::from).unwrap_or(Value::String(v.to_string())),
        // as Groovy's tokenize(':'): "00:01:02" -> ["00", "01", "02"], empty pieces dropped
        "Array" => Value::Array(v.split(':').filter(|x| !x.is_empty()).map(|x| Value::String(x.to_string())).collect()),
        _ => Value::String(v.to_string()),
    }
}

pub fn setting_value(r: &Row) -> Value {
    let text = s(r, "text");
    let dt = s(r, "datum_type");
    match s(r, "type").as_str() {
        "Array" => Value::Array(db::csv(&text).iter().map(|v| datum(v, &dt)).collect()),
        "Hashmap" => {
            let mut m = Row::new();
            for tok in db::csv(&text) {
                if let Some((k, v)) = tok.split_once('|') {
                    m.insert(k.to_string(), datum(v, &dt));
                }
            }
            Value::Object(m)
        }
        "Date" => r.get("date_value").cloned().unwrap_or(Value::Null),
        "Number" => r.get("number").cloned().unwrap_or(Value::Null),
        _ => Value::String(text),
    }
}

pub async fn module_roles(db: &Db, user: &Row, module: &str) -> Vec<String> {
    user_list(format!("m:{}:{}", i(user, "id").unwrap_or(0), module), async {
    let uid = i(user, "id").unwrap_or(0);
    let mut roles = granted_roles(db, uid, module).await;
    let superuser = b(user, "is_admin") && setting(db, "enablesuperuser").await.map(|v| db::vs(&v) == "true" || v == json!(1) || v == json!("1")).unwrap_or(false);
    if (superuser || roles.iter().any(|r| r == "Developer")) && !roles.iter().any(|r| r == "Admin") {
        roles.push("Admin".into());
    }
    roles
    })
    .await
}

/// The roles a user was given in a module (user_role rows only), cached.
pub async fn granted_roles(db: &Db, uid: i64, module: &str) -> Vec<String> {
    user_list(format!("g:{}:{}", uid, module), async {
        let rows = db.rows("select role from user_role where user_id=:u and module=:m", &row! {"u"=>uid,"m"=>module}).await.unwrap_or_default();
        rows.iter().map(|r| s(r, "role")).collect()
    })
    .await
}

pub async fn admin_list(db: &Db, user: &Row) -> Vec<String> {
    user_list(format!("a:{}", i(user, "id").unwrap_or(0)), async {
    let uid = i(user, "id").unwrap_or(0);
    let rows = db.rows("select distinct module from user_role where user_id=:u and role in ('Admin','Developer')", &row! {"u"=>uid}).await.unwrap_or_default();
    let mut l: Vec<String> = rows.iter().map(|r| s(r, "module")).collect();
    if l.is_empty() && b(user, "is_admin") {
        l.push("portal".into());
    }
    l
    })
    .await
}

pub async fn developer_list(db: &Db, user: &Row) -> Vec<String> {
    user_list(format!("d:{}", i(user, "id").unwrap_or(0)), async {
    let uid = i(user, "id").unwrap_or(0);
    let rows = db.rows("select distinct module from user_role where user_id=:u and role='Developer'", &row! {"u"=>uid}).await.unwrap_or_default();
    rows.iter().map(|r| s(r, "module")).collect()
    })
    .await
}

/// The user's organisational role: their first tree node assignment.
pub async fn current_role(db: &Db, user: &Row) -> Option<Row> {
    let v = crate::cache::USERS
        .get_or(format!("r:{}", i(user, "id").unwrap_or(0)), async {
    let uid = i(user, "id").unwrap_or(0);
    db.first(
        "select tnu.role, tnu.node_id, n.name as node_name, n.tree_id from portal_tree_node_user tnu left join portal_tree_node n on n.id=tnu.node_id where tnu.user_id=:u order by tnu.id limit 1",
        &row! {"u"=>uid},
    )
    .await
    .ok()
    .flatten()
            .map(Value::Object)
            .unwrap_or(Value::Null)
        })
        .await;
    v.as_object().cloned()
}

pub async fn log_error(db: &Db, controller: &str, action: &str, msg: &str, module: Option<&str>, slug: Option<&str>, user_id: Option<i64>) {
    eprintln!("[error] {}/{} {}:{} -- {}", controller, action, module.unwrap_or(""), slug.unwrap_or(""), msg);
    let _ = db
        .insert(
            "portal_error_log",
            &row! {"controller"=>controller,"action"=>action,"errormsg"=>msg,"module"=>module,"slug"=>slug,"user_id"=>user_id,"date"=>now_str(),"resolved"=>false},
        )
        .await;
}

pub async fn user_log(db: &Db, category: &str, description: &str, user_id: Option<i64>, target: Option<i64>, ip: &str) {
    let _ = db
        .insert("user_log", &row! {"category"=>category,"description"=>description,"user_id"=>user_id,"targetuser_id"=>target,"ipaddr"=>ip,"date"=>now_str()})
        .await;
}

impl Ctx {
    pub fn db(&self) -> &Db {
        &self.st.db
    }
    pub fn uid(&self) -> Option<i64> {
        self.user.as_ref().and_then(|u| i(u, "id"))
    }
    pub fn is_admin(&self) -> bool {
        self.user.as_ref().map(|u| b(u, "is_admin")).unwrap_or(false)
    }
    pub fn p(&self, k: &str) -> String {
        s(&self.params, k)
    }
    pub fn pi(&self, k: &str) -> Option<i64> {
        i(&self.params, k)
    }
    pub fn csrf(&mut self) -> String {
        let c = s(&self.sess, "csrf");
        if !c.is_empty() {
            return c;
        }
        let t = token();
        self.sess.insert("csrf".into(), Value::String(t.clone()));
        t
    }
    pub fn csrf_field(&mut self) -> String {
        format!("<input type='hidden' name='csrf_token' value='{}'/>", self.csrf())
    }
    pub fn flash(&mut self, msg: &str) {
        self.sess.insert("flash".into(), Value::String(msg.to_string()));
    }
    pub fn take_flash(&mut self) -> String {
        self.sess.remove("flash").map(|v| db::vs(&v)).unwrap_or_default()
    }
    pub fn is_htmx(&self) -> bool {
        self.headers.contains_key("hx-request")
    }
    /// May this user administer things that belong to `module`?
    pub async fn can_admin(&self, module: &str) -> bool {
        match &self.user {
            None => false,
            Some(u) => b(u, "is_admin") || (!module.is_empty() && admin_list(self.db(), u).await.iter().any(|m| m == module)),
        }
    }

    pub async fn finish(self, mut resp: Response) -> Response {
        let data = Value::Object(self.sess.clone()).to_string();
        if data != self.sess_orig {
            let p = row! {"id"=>self.sid.clone(),"d"=>data};
            if self.db().is_mssql() {
                let _ = self
                    .db()
                    .exec(
                        "update portal_session set data=:d, updated=now() where id=:id; if @@rowcount = 0 insert into portal_session (id,data,updated) values (:id,:d,now())",
                        &p,
                    )
                    .await;
            } else {
                let _ = self
                    .db()
                    .exec(
                        "insert into portal_session (id,data,updated) values (:id,:d,now()) on conflict (id) do update set data=excluded.data, updated=now()",
                        &p,
                    )
                    .await;
            }
        } else if self.sess_touch && self.user.is_some() {
            let _ = self
                .db()
                .exec("update portal_session set updated=now() where id=:id", &row! {"id"=>self.sid.clone()})
                .await;
        }
        if let Ok(v) = HeaderValue::from_str(&format!("RPSESSION={}; Path=/; HttpOnly; SameSite=Lax", self.sid)) {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
        resp.headers_mut().insert("x-frame-options", HeaderValue::from_static("SAMEORIGIN"));
        resp.headers_mut().insert("x-content-type-options", HeaderValue::from_static("nosniff"));
        if let Some(v) = self.st.cfg.content_security_policy.as_deref().and_then(|c| HeaderValue::from_str(c).ok()) {
            resp.headers_mut().insert("content-security-policy", v);
        }
        // audit trail, written in the background
        if !self.path.starts_with("/static/") {
            let segs: Vec<&str> = self.path.split('/').filter(|x| !x.is_empty()).collect();
            let params = if self.method == "GET" {
                let mut p = self.params.clone();
                p.remove("password");
                let t = Value::Object(p).to_string();
                t.chars().take(1000).collect::<String>()
            } else {
                "It was a post request".to_string()
            };
            let rec = row! {
                "controller"=>segs.first().copied().unwrap_or(""),
                "action"=>segs.get(1).copied().unwrap_or(""),
                "params"=>params,
                "module"=>self.module.clone(),
                "slug"=>self.slug.clone(),
                "ipaddr"=>self.ip.clone(),
                "uri"=>self.path.chars().take(255).collect::<String>(),
                "useragent"=>self.headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("").chars().take(255).collect::<String>(),
                "date"=>now_str(),
                "user_id"=>self.uid(),
                "realuser_id"=>i(&self.sess, "realuserid"),
                "duration_ms"=>self.start.elapsed().as_millis() as i64,
                "status"=>resp.status().as_u16() as i64,
            };
            crate::audit::enqueue(rec);
        }
        resp
    }

    pub async fn redirect(self, to: &str) -> Response {
        let to = if to.is_empty() { "/" } else { to };
        let resp = if self.is_htmx() {
            (StatusCode::OK, [("HX-Redirect", to.to_string())], "").into_response()
        } else {
            (StatusCode::SEE_OTHER, [(header::LOCATION, to.to_string())]).into_response()
        };
        self.finish(resp).await
    }

    pub async fn json(self, v: Value) -> Response {
        let resp = ([(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response();
        self.finish(resp).await
    }

    /// A plain-text answer with a status code (as Grails' render(status:, text:)).
    pub async fn text_status(self, code: StatusCode, body: &str) -> Response {
        let resp = (code, [(header::CONTENT_TYPE, "text/plain;charset=utf-8".to_string())], body.to_string()).into_response();
        self.finish(resp).await
    }

    pub async fn text(self, ctype: &str, body: String) -> Response {
        let resp = ([(header::CONTENT_TYPE, ctype.to_string())], body).into_response();
        self.finish(resp).await
    }

    pub async fn bytes(self, ctype: &str, filename: Option<&str>, data: Vec<u8>, inline: bool) -> Response {
        let mut resp = ([(header::CONTENT_TYPE, ctype.to_string())], data).into_response();
        set_disposition(&mut resp, filename, inline);
        self.finish(resp).await
    }

    /// Send a file from disk without reading it into memory. With `delete`, the
    /// file is removed once opened (the open handle keeps it readable).
    pub async fn file_stream(self, ctype: &str, filename: Option<&str>, path: &str, delete: bool) -> Response {
        let file = match tokio::fs::File::open(path).await {
            Ok(f) => f,
            Err(_) => return self.status(StatusCode::NOT_FOUND, "File not found").await,
        };
        let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
        if delete {
            let _ = tokio::fs::remove_file(path).await;
        }
        let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::with_capacity(file, 256 * 1024));
        let mut resp = ([(header::CONTENT_TYPE, ctype.to_string()), (header::CONTENT_LENGTH, len.to_string())], body).into_response();
        set_disposition(&mut resp, filename, false);
        self.finish(resp).await
    }

    pub async fn status(self, code: StatusCode, msg: &str) -> Response {
        let body = format!("<div class='container'><div class='alert alert-warning mt-4'><h4>{}</h4><p>{}</p></div></div>", code, esc(msg));
        let mut resp = self.html(&code.to_string(), body).await;
        *resp.status_mut() = code;
        resp
    }

    /// Send the user to the login page, remembering where they wanted to go.
    pub async fn need_login(mut self) -> Response {
        let back = if self.query.is_empty() { self.path.clone() } else { format!("{}?{}", self.path, self.query) };
        if self.method == "GET" {
            self.sess.insert("redirect_after_login".into(), Value::String(back));
        }
        self.flash("Please login to access the system");
        self.redirect("/login").await
    }

    /// Render `body` inside the site layout.
    pub async fn html(mut self, title: &str, body: String) -> Response {
        let page = self.layout(title, body).await;
        let resp = axum::response::Html(page).into_response();
        self.finish(resp).await
    }

    /// Content without the layout chrome (HTMX fragments, full pages).
    /// A page without the header, sidebar and footer (the Grails portal's "empty" layout).
    pub async fn empty_page(mut self, title: &str, body: String) -> Response {
        let brand = branding(&self.st).await;
        let ctx = json!({"title": title, "content": body, "flash": self.take_flash(), "csrf": self.csrf(), "app_name": brand["app_name"], "brand": brand, "main_css": main_css(&self.st.cfg)});
        let mut env = minijinja::Environment::new();
        env.set_auto_escape_callback(|_| minijinja::AutoEscape::Html);
        let _ = env.add_template("empty", include_str!("../templates/empty.html"));
        let page = match env.get_template("empty").and_then(|t| t.render(ctx)) {
            Ok(x) => x,
            Err(e) => format!("<pre>layout error: {}</pre>", esc(&e.to_string())),
        };
        let resp = axum::response::Html(page).into_response();
        self.finish(resp).await
    }

    /// Raw HTML a page script wrote itself (a Grails page's own <html>...): it carries
    /// no layout, so the session token for its forms and fetch/XHR posts is added here.
    pub async fn raw_html_with_token(mut self, body: String) -> Response {
        let tok = self.csrf();
        let body = inject_csrf(&body, &tok);
        self.raw_html(body).await
    }

    pub async fn raw_html(self, body: String) -> Response {
        let resp = axum::response::Html(body).into_response();
        self.finish(resp).await
    }

    pub async fn layout(&mut self, title: &str, body: String) -> String {
        let flash = self.take_flash();
        let csrf = self.csrf();
        let db = self.st.db.clone();
        let megamenu = crate::tree::mega_menu(&db, "portal", "main_menu").await;
        let mut sidemenu = String::new();
        if let Some(m) = self.side_menu.clone() {
            let module = self.module.clone().unwrap_or_default();
            let module_admin = self.is_admin() || (!module.is_empty() && self.can_admin(&module).await);
            sidemenu = crate::tree::side_menu(&db, &format!("{}_menu", m), module_admin).await;
        }
        let mut adminish = false;
        let mut dev = false;
        let mut user_json = Value::Null;
        let mut role = String::new();
        let mut role_label = String::new();
        let mut tree_roles: Vec<Value> = vec![];
        let mut chosen_role_id: i64 = 0;
        if let Some(u) = &self.user {
            let al = admin_list(&db, u).await;
            let dl = developer_list(&db, u).await;
            adminish = b(u, "is_admin") || !al.is_empty() || !dl.is_empty();
            dev = b(u, "is_admin") || !dl.is_empty();
            user_json = Value::Object(u.clone());
            chosen_role_id = i(u, "roletargetid").unwrap_or(0);
            if let Some(uid) = i(u, "id") {
                let rows = db
                    .rows(
                        "select tnu.id, tnu.role, n.name as node_name from portal_tree_node_user tnu left join portal_tree_node n on n.id=tnu.node_id where tnu.user_id=:u order by tnu.id",
                        &row! {"u"=>uid},
                    )
                    .await
                    .unwrap_or_default();
                for r in rows {
                    let lbl = {
                        let nn = s(&r, "node_name");
                        let rl = s(&r, "role");
                        if nn.is_empty() { rl } else { format!("{} - {}", nn, rl) }
                    };
                    let short = if lbl.chars().count() > 33 {
                        let c: Vec<char> = lbl.chars().collect();
                        format!("{}...{}", c[..13].iter().collect::<String>(), c[c.len() - 12..].iter().collect::<String>())
                    } else {
                        lbl
                    };
                    tree_roles.push(json!({"id": i(&r, "id").unwrap_or(0), "label": short}));
                }
            }
            role = current_role(&db, u).await.map(|r| s(&r, "role")).unwrap_or_default();
            // what the Grails portal shows under the user's name
            role_label = if b(u, "is_admin") {
                "Portal Admin".to_string()
            } else if !role.is_empty() {
                role.clone()
            } else if !al.is_empty() {
                "Admin".to_string()
            } else {
                "None".to_string()
            };
        }
        // as the Grails portal: the module, then "_slug" when there is one
        let pageslug = match (&self.module, &self.slug) {
            (Some(m), Some(sl)) => format!("{}_{}", m, sl),
            (Some(m), None) => m.clone(),
            _ => String::new(),
        };
        let brand = branding(&self.st).await;
        let ctx = json!({
            "title": title, "content": body, "flash": flash, "csrf": csrf, "megamenu": megamenu, "sidemenu": sidemenu, "main_css": main_css(&self.st.cfg),
            "user": user_json, "adminish": adminish, "dev": dev, "superadmin": self.is_admin(), "role": role, "role_label": role_label,
            "tree_roles": tree_roles, "chosen_role_id": chosen_role_id, "request_path": self.path,
            "pageslug": pageslug, "credits_url": brand["homepage"], "brand": brand,
            "switched": self.sess.contains_key("adminlink"), "app_name": brand["app_name"],
            "allow_registration": self.st.cfg.allow_registration,
            "google_oauth_enabled": !self.st.cfg.google_oauth.client_id.is_empty(),
        });
        let mut env = minijinja::Environment::new();
        env.set_auto_escape_callback(|_| minijinja::AutoEscape::Html);
        let _ = env.add_template("layout", include_str!("../templates/layout.html"));
        match env.get_template("layout").and_then(|t| t.render(ctx)) {
            Ok(x) => x,
            Err(e) => format!("<pre>layout error: {}</pre>", esc(&e.to_string())),
        }
    }
}

/// Header, footer and tab branding, as the g6 Grails portal resolves it: a value
/// saved in the `branding` module's settings, else config.toml, else the default.
pub async fn branding(st: &St) -> Value {
    let stored = crate::cache::SETTINGS
        .get_or("\u{1}branding".to_string(), async {
            st.db.rows("select name, text from portal_setting where module='branding'", &db::no_params()).await.ok().map(|rows| {
                let mut m = Row::new();
                for r in rows {
                    let v = s(&r, "text").trim().to_string();
                    if !v.is_empty() {
                        m.insert(s(&r, "name"), Value::String(v));
                    }
                }
                m
            })
        })
        .await
        .unwrap_or_default();
    let cfg = &st.cfg;
    let pick = |key: &str, cfgval: &str, default: String| -> String {
        let v = s(&stored, key);
        if !v.is_empty() { v } else if !cfgval.is_empty() { cfgval.to_string() } else { default }
    };
    let app_name = pick("app_name", &cfg.app_name, "Portal".into());
    let logo_slug = s(&stored, "logo_slug");
    let favicon_slug = s(&stored, "favicon_slug");
    let logo_url_cfg = pick("logo_url", &cfg.logo_url, String::new());
    let logo = if !logo_slug.is_empty() {
        format!("/download/portal/{}", logo_slug)
    } else if !logo_url_cfg.is_empty() {
        logo_url_cfg.clone()
    } else {
        "/static/images/logo.png".into()
    };
    let favicon = if !favicon_slug.is_empty() {
        format!("/download/portal/{}", favicon_slug)
    } else {
        pick("favicon_url", &cfg.favicon_url, "/static/images/favicon.svg".into())
    };
    let mut out = Row::new();
    out.insert("app_name".into(), json!(app_name));
    out.insert("copyright".into(), json!(pick("copyright", &cfg.copyright, app_name.clone())));
    out.insert("team".into(), json!(pick("team", &cfg.team, format!("{} Team", app_name))));
    out.insert("homepage".into(), json!(pick("homepage", &cfg.credits_url, String::new())));
    out.insert("logo_slug".into(), json!(logo_slug));
    out.insert("logo_url".into(), json!(logo_url_cfg));
    out.insert("favicon_slug".into(), json!(favicon_slug));
    out.insert("logo".into(), json!(logo));
    out.insert("favicon".into(), json!(favicon));
    for (key, fallback, _, _) in crate::branding::COLOR_DEFAULTS {
        let stored_val = s(&stored, &format!("color_{}", key));
        let col = if crate::branding::valid_color(&stored_val) { stored_val.to_ascii_lowercase() } else { (*fallback).to_string() };
        out.insert((*key).into(), json!(col));
    }
    let prim = s(&out, "primary");
    out.insert("primary_tint".into(), json!(crate::branding::tint(&prim)));
    let out_val = Value::Object(out.clone());
    let css_vars = crate::branding::css_variables(&out_val);
    let style_tag = if css_vars.is_empty() { String::new() } else { format!("<style>{}</style>", css_vars) };
    out.insert("css_vars".into(), json!(css_vars));
    out.insert("style_tag".into(), json!(style_tag));
    Value::Object(out)
}

/// The script that gives every POST form, fetch() and XMLHttpRequest of a page the
/// session token (the same as the layouts carry).
pub fn csrf_script(token: &str) -> String {
    format!(r#"<script>
    var CSRF_TOKEN = {tok};
    document.addEventListener("submit", function (ev) {{
      var f = ev.target;
      if (f && f.tagName === "FORM" && (f.method || "").toLowerCase() === "post" && !f.querySelector("input[name=csrf_token]")) {{
        var i = document.createElement("input");
        i.type = "hidden"; i.name = "csrf_token"; i.value = CSRF_TOKEN;
        f.appendChild(i);
      }}
    }}, true);
    (function () {{
      var same = function (u) {{ try {{ return new URL(u, location.href).origin === location.origin; }} catch (e) {{ return true; }} }};
      if (window.fetch) {{
        var f0 = window.fetch;
        window.fetch = function (input, init) {{
          init = init || {{}};
          var url = typeof input === "string" ? input : (input && input.url) || "";
          var method = (init.method || (input && input.method) || "GET").toUpperCase();
          if (method !== "GET" && method !== "HEAD" && same(url)) {{
            var h = new Headers(init.headers || (typeof input !== "string" && input.headers) || {{}});
            if (!h.has("X-CSRF-Token")) h.set("X-CSRF-Token", CSRF_TOKEN);
            init.headers = h;
          }}
          return f0.call(this, input, init);
        }};
      }}
      var open0 = XMLHttpRequest.prototype.open, send0 = XMLHttpRequest.prototype.send, set0 = XMLHttpRequest.prototype.setRequestHeader;
      XMLHttpRequest.prototype.open = function (m, u) {{ this.__m = String(m).toUpperCase(); this.__u = u; this.__tok = false; return open0.apply(this, arguments); }};
      XMLHttpRequest.prototype.setRequestHeader = function (k, v) {{ if (String(k).toLowerCase() === "x-csrf-token") this.__tok = true; return set0.apply(this, arguments); }};
      XMLHttpRequest.prototype.send = function () {{
        if (!this.__tok && this.__m && this.__m !== "GET" && this.__m !== "HEAD" && same(this.__u)) {{ try {{ set0.call(this, "X-CSRF-Token", CSRF_TOKEN); }} catch (e) {{}} }}
        return send0.apply(this, arguments);
      }};
    }})();
  </script>"#, tok = serde_json::to_string(token).unwrap_or_default())
}

/// Put the token script into a page: after <head ...>, else after <body ...>, else at
/// the end when the page has a form or script. Anything else (JSON, plain text)
/// is left as it is.
pub fn inject_csrf(html: &str, token: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let after_tag = |tag: &str| -> Option<usize> {
        let mut from = 0;
        while let Some(p) = lower[from..].find(tag) {
            let at = from + p;
            let next = lower.as_bytes().get(at + tag.len()).copied();
            if matches!(next, Some(b'>') | Some(b' ') | Some(b'\n') | Some(b'\t') | Some(b'\r')) {
                return lower[at..].find('>').map(|e| at + e + 1);
            }
            from = at + tag.len();
        }
        None
    };
    let script = csrf_script(token);
    if let Some(i) = after_tag("<head").or_else(|| after_tag("<body")) {
        return format!("{}{}{}", &html[..i], script, &html[i..]);
    }
    if lower.contains("<form") || lower.contains("<script") {
        return format!("{}{}", html, script);
    }
    html.to_string()
}

#[cfg(test)]
mod csrf_tests {
    use super::*;
    #[test]
    fn injects_where_it_fits() {
        assert!(inject_csrf("<!DOCTYPE html><html><head><title>x</title></head></html>", "t").starts_with("<!DOCTYPE html><html><head><script>"));
        assert!(inject_csrf("<html><body class='a'>hi</body></html>", "t").starts_with("<html><body class='a'><script>"));
        assert!(!inject_csrf("<header>x</header><body>", "t").starts_with("<header><script>"));
        assert_eq!(inject_csrf("{\"status\":\"Voting\"}", "t"), "{\"status\":\"Voting\"}");
        assert!(inject_csrf("<div><form method=post></form></div>", "t").ends_with("</script>"));
    }

    #[tokio::test]
    async fn session_expires_on_load_and_prune() {
        let db = Db::connect("sqlite::memory:", 1, None).await.expect("connect sqlite");
        crate::entity::sync_schema(&db, &crate::entity::entities()).await.expect("sync schema");

        // Timeout resolution: default session_hours -> config session_minutes -> portal_setting override
        let mut cfg = Config::default();
        assert_eq!(session_timeout_minutes(&db, &cfg).await, 12 * 60);
        cfg.session_minutes = Some(30);
        assert_eq!(session_timeout_minutes(&db, &cfg).await, 30);
        db.insert(
            "portal_setting",
            &row! {"module"=>"portal","name"=>"session_minutes","type"=>"Text","text"=>"45"},
        )
        .await
        .unwrap();
        assert_eq!(session_timeout_minutes(&db, &cfg).await, 45);

        // Fresh session loads cleanly and doesn't need immediate touch
        db.exec(
            "insert into portal_session (id,data,updated) values ('fresh','{\"userid\":1}',now())",
            &db::no_params(),
        )
        .await
        .unwrap();
        let (_, obj, stale) = load_session(&db, "fresh", 30).await.expect("fresh session");
        assert_eq!(i(&obj, "userid"), Some(1));
        assert!(!stale);

        // Active session older than the touch throttle is alive and flagged for sliding refresh
        db.exec(
            "insert into portal_session (id,data,updated) values ('active','{\"userid\":1}',now() - interval '10 minutes')",
            &db::no_params(),
        )
        .await
        .unwrap();
        let (_, _, stale) = load_session(&db, "active", 30).await.expect("active session");
        assert!(stale);

        // Session past 30-minute timeout is rejected on load and deleted from the database
        db.exec(
            "insert into portal_session (id,data,updated) values ('expired','{\"userid\":2}',now() - interval '31 minutes')",
            &db::no_params(),
        )
        .await
        .unwrap();
        assert!(load_session(&db, "expired", 30).await.is_none());
        assert_eq!(db.count("select count(*) from portal_session where id='expired'", &db::no_params()).await.unwrap(), 0);

        // Session with null updated is also treated as expired and deleted
        db.exec(
            "insert into portal_session (id,data,updated) values ('nullts','{\"userid\":3}',null)",
            &db::no_params(),
        )
        .await
        .unwrap();
        assert!(load_session(&db, "nullts", 30).await.is_none());
        assert_eq!(db.count("select count(*) from portal_session where id='nullts'", &db::no_params()).await.unwrap(), 0);

        // prune_sessions removes stale/null rows while keeping fresh/active ones
        db.exec(
            "insert into portal_session (id,data,updated) values ('stale1','{\"userid\":4}',now() - interval '40 minutes')",
            &db::no_params(),
        )
        .await
        .unwrap();
        db.exec(
            "insert into portal_session (id,data,updated) values ('stale2','{\"userid\":5}',null)",
            &db::no_params(),
        )
        .await
        .unwrap();
        prune_sessions(&db, 30).await;
        assert_eq!(db.count("select count(*) from portal_session", &db::no_params()).await.unwrap(), 2);
        assert!(load_session(&db, "fresh", 30).await.is_some());
        assert!(load_session(&db, "active", 30).await.is_some());
    }
}

/// The main stylesheet: a g6 portal's own main.css (from its asset folders) when
/// sharing its database, so forms and lists are styled as there.
fn main_css(cfg: &Config) -> &'static str {
    if cfg.g6_schema && cfg.assets_dirs.iter().any(|d| std::path::Path::new(d).join("main.css").exists()) { "/assets/main.css" } else { "/static/css/main.css" }
}
