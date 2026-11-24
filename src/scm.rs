//! Source Code Management (maker/checker) v2 VCS driver and workflow engine.
//!
//! Ported from g5portal's `scm` module (`scm_lib.gsp`). One repository per
//! project, served by rustportal over HTTP at `/svc/scm/{hg|git}/{key}`.
//! Pushes are quarantined on arrival (Mercurial `secret` phase / Git
//! `refs/g5/incoming/*` + `uploadpack.hideRefs`) and published in place only
//! when a checker approves the change request.

use crate::db::{i, s, vs, Db, Row};
use crate::row;
use crate::tracker::Tracker;
use crate::web::{self, esc};
use chrono::{Local, TimeZone};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

pub const MAX_DIFF: usize = 400_000;
const LOCK_STALE_MS: u128 = 900_000;
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vcs {
    Mercurial,
    Git,
}

impl Vcs {
    pub fn from_repo(repo: &Row) -> Self {
        if s(repo, "vcs_type").eq_ignore_ascii_case("git") {
            Vcs::Git
        } else {
            Vcs::Mercurial
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Vcs::Mercurial => "Mercurial",
            Vcs::Git => "Git",
        }
    }

    pub fn endpoint_slug(self) -> &'static str {
        match self {
            Vcs::Mercurial => "hg",
            Vcs::Git => "git",
        }
    }

    pub fn quarantine_desc(self) -> &'static str {
        match self {
            Vcs::Mercurial => "the secret phase",
            Vcs::Git => "a hidden refs/g5/incoming/* namespace",
        }
    }

    pub fn bin(self) -> &'static str {
        match self {
            Vcs::Mercurial => "hg",
            Vcs::Git => "git",
        }
    }
}

#[derive(Clone, Debug)]
pub struct CmdRes {
    pub cmd: String,
    pub rc: i32,
    pub out: String,
    pub err: String,
    pub ms: i64,
    #[allow(dead_code)]
    pub timedout: bool,
}

fn node_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9a-fA-F]{6,40}$").unwrap())
}

fn key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z0-9_-]+$").unwrap())
}

fn all_zeros(x: &str) -> bool {
    !x.is_empty() && x.bytes().all(|b| b == b'0')
}

pub fn trunc(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{}\n...[truncated at {} characters]...", head, max)
    }
}

pub fn safenode(n: &str) -> Option<String> {
    let t = n.trim();
    if !t.is_empty() && node_re().is_match(t) {
        Some(t.to_string())
    } else {
        None
    }
}

pub fn safe_path(p: Option<&str>) -> Option<String> {
    let Some(raw) = p else { return Some(String::new()) };
    let norm = raw.trim().replace('\\', "/");
    let t = norm.trim_start_matches('/');
    if t.is_empty() {
        return Some(String::new());
    }
    if t.contains("..") || t.starts_with('/') || t.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return None;
    }
    if t.len() > 1024 {
        return None;
    }
    Some(t.trim_end_matches('/').to_string())
}

pub fn flag(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64() == Some(1),
        Some(Value::String(s)) => matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "on"),
        _ => false,
    }
}

fn take12(x: &str) -> String {
    x.chars().take(12).collect()
}

fn fmt_epoch(secs: i64) -> String {
    match Local.timestamp_opt(secs, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => String::new(),
    }
}

fn now_hm() -> String {
    Local::now().format("%Y-%m-%d %H:%M").to_string()
}

fn first_nonempty_line(text: &str) -> String {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").to_string()
}

pub fn looks_binary(text: &str) -> bool {
    let end = text.len().min(8000);
    text.as_bytes()[..end].contains(&0)
}

pub fn mint_token() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

pub fn default_branch(repo: &Row, vcs: Vcs) -> String {
    let b = s(repo, "default_branch").trim().to_string();
    if b.is_empty() {
        if vcs == Vcs::Git { "main".to_string() } else { "default".to_string() }
    } else {
        b
    }
}

fn commit_user(repo: &Row) -> String {
    let u = s(repo, "commit_user").trim().to_string();
    if u.is_empty() { "rustportal SCM <scm@localhost>".to_string() } else { u }
}

pub fn parse_diffstat(txt: &str) -> (i64, i64, i64) {
    static RF: OnceLock<Regex> = OnceLock::new();
    static RA: OnceLock<Regex> = OnceLock::new();
    static RD: OnceLock<Regex> = OnceLock::new();
    let files = RF
        .get_or_init(|| Regex::new(r"(\d+)\s+files?\s+changed").unwrap())
        .captures(txt)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<i64>().ok())
        .unwrap_or(0);
    let added = RA
        .get_or_init(|| Regex::new(r"(\d+)\s+insertions?\(\+\)").unwrap())
        .captures(txt)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<i64>().ok())
        .unwrap_or(0);
    let removed = RD
        .get_or_init(|| Regex::new(r"(\d+)\s+deletions?\(-\)").unwrap())
        .captures(txt)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<i64>().ok())
        .unwrap_or(0);
    (files, added, removed)
}

/// Colour a unified diff into HTML. Every line is HTML-escaped first.
pub fn diff_html(raw: &str, max_lines: usize) -> String {
    let mut out = String::new();
    for dl in raw.lines().take(max_lines) {
        let (col, bg, wt) = if dl.starts_with("diff ") || dl.starts_with("index ") {
            ("#57606a", "#f6f8fa", "bold")
        } else if dl.starts_with("+++") || dl.starts_with("---") {
            ("#57606a", "transparent", "bold")
        } else if dl.starts_with("@@") {
            ("#8250df", "#f6f0ff", "normal")
        } else if dl.starts_with('+') {
            ("#0a5c2e", "#e6ffec", "normal")
        } else if dl.starts_with('-') {
            ("#82071e", "#ffebe9", "normal")
        } else {
            ("#24292f", "transparent", "normal")
        };
        out.push_str(&format!(
            "<div style=\"color:{};background:{};font-weight:{};padding:0 6px;white-space:pre\">{}</div>",
            col,
            bg,
            wt,
            if dl.is_empty() { "&nbsp;".to_string() } else { esc(dl) }
        ));
    }
    out
}

/// Return a Bootstrap Icons `<i>` tag with an appropriate icon and colour for a
/// repository folder or file (by extension or well-known filename).
pub fn file_icon_html(name: &str, is_dir: bool) -> String {
    let (cls, color) = if is_dir {
        ("bi-folder-fill", "#e3a008")
    } else {
        let base = name.rsplit('/').next().unwrap_or(name);
        let base = base.rsplit('\\').next().unwrap_or(base);
        let low = base.to_ascii_lowercase();
        let ext = low.rsplit_once('.').map(|(pre, e)| if pre.is_empty() { "" } else { e }).unwrap_or("");
        match ext {
            // Web / templates
            "html" | "htm" | "xhtml" | "gsp" | "jsp" | "ejs" | "hbs" | "ftl" | "jinja" | "jinja2" | "j2" | "vue" | "svelte" => ("bi-filetype-html", "#e34c26"),
            "css" | "less" => ("bi-filetype-css", "#264de4"),
            "scss" => ("bi-filetype-scss", "#c6538c"),
            "sass" => ("bi-filetype-sass", "#c6538c"),
            "js" | "mjs" | "cjs" => ("bi-filetype-js", "#d4a017"),
            "jsx" => ("bi-filetype-jsx", "#0ea5e9"),
            "ts" | "mts" | "cts" | "tsx" => ("bi-filetype-tsx", "#3178c6"),
            // Backend / JVM / systems / scripting
            "java" => ("bi-filetype-java", "#b07219"),
            "groovy" | "gradle" => ("bi-file-earmark-code", "#4298b8"),
            "kt" | "kts" => ("bi-file-earmark-code", "#7f52ff"),
            "rs" => ("bi-file-earmark-code", "#c2410c"),
            "go" => ("bi-file-earmark-code", "#00add8"),
            "rhai" => ("bi-file-earmark-code", "#6f42c1"),
            "py" | "pyw" | "pyi" | "cgi" | "wsgi" => ("bi-filetype-py", "#3572a5"),
            "rb" | "rake" | "gemspec" => ("bi-filetype-rb", "#cc342d"),
            "php" | "phtml" => ("bi-filetype-php", "#6e5494"),
            "cs" | "csx" => ("bi-filetype-cs", "#178600"),
            "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "swift" | "scala" | "dart" | "lua" | "pl" | "pm" | "r" | "m" | "mm" | "zig" | "nim" | "ex" | "exs" | "erl" | "hs" | "clj" | "vb" | "vbs" => ("bi-file-earmark-code", "#4f46e5"),
            "sh" | "bash" | "zsh" | "ksh" | "fish" | "bat" | "cmd" | "ps1" | "psm1" | "psd1" => ("bi-filetype-sh", "#2e7d32"),
            "sql" | "ddl" | "dml" | "plsql" => ("bi-filetype-sql", "#0284c7"),
            // Data / config / markup
            "json" | "jsonc" | "json5" => ("bi-filetype-json", "#ca8a04"),
            "yml" | "yaml" => ("bi-filetype-yml", "#cb171e"),
            "xml" | "xsd" | "xsl" | "xslt" | "pom" | "plist" | "csproj" | "vbproj" | "fsproj" | "sln" | "config" | "wsdl" => ("bi-filetype-xml", "#ea580c"),
            "toml" | "ini" | "cfg" | "conf" | "properties" | "env" => ("bi-file-earmark-ruled", "#64748b"),
            "md" | "markdown" => ("bi-filetype-md", "#0969da"),
            "mdx" => ("bi-filetype-mdx", "#0969da"),
            "csv" | "tsv" => ("bi-filetype-csv", "#15803d"),
            "txt" | "log" | "rst" | "adoc" | "text" => ("bi-filetype-txt", "#57606a"),
            "lock" => ("bi-file-earmark-lock", "#64748b"),
            "diff" | "patch" => ("bi-file-earmark-diff", "#0f766e"),
            // Office / documents
            "pdf" => ("bi-filetype-pdf", "#dc2626"),
            "doc" => ("bi-filetype-doc", "#1d4ed8"),
            "docx" | "odt" | "rtf" => ("bi-filetype-docx", "#1d4ed8"),
            "xls" => ("bi-filetype-xls", "#15803d"),
            "xlsx" | "xlsm" | "ods" => ("bi-filetype-xlsx", "#15803d"),
            "ppt" => ("bi-filetype-ppt", "#c2410c"),
            "pptx" | "odp" => ("bi-filetype-pptx", "#c2410c"),
            // Images / media / fonts
            "png" => ("bi-filetype-png", "#7c3aed"),
            "jpg" | "jpeg" => ("bi-filetype-jpg", "#7c3aed"),
            "gif" => ("bi-filetype-gif", "#7c3aed"),
            "svg" => ("bi-filetype-svg", "#d97706"),
            "bmp" => ("bi-filetype-bmp", "#7c3aed"),
            "tif" | "tiff" => ("bi-filetype-tiff", "#7c3aed"),
            "psd" => ("bi-filetype-psd", "#2563eb"),
            "ai" => ("bi-filetype-ai", "#ea580c"),
            "heic" | "heif" => ("bi-filetype-heic", "#7c3aed"),
            "webp" | "ico" | "icns" | "avif" | "raw" => ("bi-file-earmark-image", "#7c3aed"),
            "mp3" => ("bi-filetype-mp3", "#db2777"),
            "wav" => ("bi-filetype-wav", "#db2777"),
            "aac" => ("bi-filetype-aac", "#db2777"),
            "ogg" | "flac" | "m4a" | "wma" => ("bi-file-earmark-music", "#db2777"),
            "mp4" => ("bi-filetype-mp4", "#db2777"),
            "mov" => ("bi-filetype-mov", "#db2777"),
            "avi" | "mkv" | "webm" | "wmv" | "flv" | "m4v" => ("bi-file-earmark-play", "#db2777"),
            "ttf" => ("bi-filetype-ttf", "#6b7280"),
            "otf" => ("bi-filetype-otf", "#6b7280"),
            "woff" | "woff2" => ("bi-filetype-woff", "#6b7280"),
            "eot" => ("bi-file-earmark-font", "#6b7280"),
            // Archives / binaries / keys
            "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "jar" | "war" | "ear" | "apk" | "deb" | "rpm" | "nupkg" => ("bi-file-earmark-zip", "#b45309"),
            "exe" | "msi" => ("bi-filetype-exe", "#475569"),
            "dll" | "so" | "dylib" | "bin" | "class" | "o" | "a" | "obj" | "lib" | "pyc" | "pyo" | "wasm" | "db" | "sqlite" | "sqlite3" | "dat" => ("bi-file-earmark-binary", "#475569"),
            "key" | "pem" | "crt" | "cer" | "pub" | "p12" | "pfx" | "jks" | "keystore" => ("bi-filetype-key", "#b91c1c"),
            _ => match low.as_str() {
                "dockerfile" | "makefile" | "jenkinsfile" | "vagrantfile" | "procfile" | "rakefile" | "gemfile" | "gradlew" | "mvnw" => ("bi-file-earmark-code", "#0284c7"),
                ".gitignore" | ".hgignore" | ".dockerignore" | ".editorconfig" | ".gitattributes" | ".htaccess" | ".env" | ".npmrc" | ".nvmrc" | ".hgrc" | ".babelrc" | ".eslintrc" | ".prettierrc" => ("bi-file-earmark-ruled", "#64748b"),
                "license" | "licence" | "copying" | "notice" | "authors" | "readme" | "changelog" | "todo" => ("bi-file-earmark-text", "#57606a"),
                _ if low.starts_with("dockerfile.") => ("bi-file-earmark-code", "#0284c7"),
                _ if low.starts_with(".env.") => ("bi-file-earmark-ruled", "#64748b"),
                _ => ("bi-file-earmark", "#57606a"),
            },
        }
    };
    format!("<i class=\"bi {}\" style=\"color:{};margin-right:6px\" aria-hidden=\"true\"></i>", cls, color)
}

/// Human-friendly file type label for the SCM explorer table.
pub fn file_type_label(name: &str, is_dir: bool) -> String {
    if is_dir {
        return "Folder".to_string();
    }
    let base = name.rsplit('/').next().unwrap_or(name);
    let base = base.rsplit('\\').next().unwrap_or(base);
    if let Some((pre, ext)) = base.rsplit_once('.') {
        if !pre.is_empty() && !ext.is_empty() && ext.len() <= 10 {
            return ext.to_ascii_uppercase();
        }
    }
    "File".to_string()
}

/// Validate a repository filesystem path for `scm_repository.repo_path`.
pub fn check_repo_path(path: &str) -> Option<String> {
    let p = path.trim();
    if p.is_empty() {
        return None;
    }
    let f = Path::new(p);
    if f.exists() && !f.is_dir() {
        return Some("Exists but is not a directory".to_string());
    }
    if let Some(parent) = f.parent().filter(|x| !x.as_os_str().is_empty()) {
        if !parent.exists() {
            return Some(format!("Parent directory {} does not exist", parent.display()));
        }
        if !dir_writable(parent) {
            return Some(format!("The portal cannot write into {}", parent.display()));
        }
    }
    None
}

fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".scm_write_probe_{}", uuid::Uuid::new_v4().simple()));
    match OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

pub async fn default_repo_root(db: &Db) -> String {
    if let Some(v) = web::setting_str(db, "scm_repo_root").await.filter(|x| !x.trim().is_empty()) {
        return v.trim().to_string();
    }
    std::env::current_dir()
        .map(|d| d.join("scmrepos").to_string_lossy().to_string())
        .unwrap_or_else(|_| "scmrepos".to_string())
}

// ---------------------------------------------------------------------------
// Process runner (no shell, explicit argv + timeout)
// ---------------------------------------------------------------------------

pub async fn run_cmd(argv: &[&str], cwd: Option<&str>, timeout_sec: u64, extra_env: Option<&HashMap<String, String>>) -> CmdRes {
    let t0 = Instant::now();
    let cmd_str = argv.join(" ");
    if argv.is_empty() {
        return CmdRes { cmd: cmd_str, rc: -1, out: String::new(), err: "empty command".into(), ms: 0, timedout: false };
    }
    let mut cmd = tokio::process::Command::new(argv[0]);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    if let Some(dir) = cwd.map(|x| x.trim()).filter(|x| !x.is_empty()) {
        if Path::new(dir).is_dir() {
            cmd.current_dir(dir);
        }
    }
    cmd.env("HGPLAIN", "1")
        .env("HGENCODING", "UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "echo")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(env) = extra_env {
        for (k, v) in env {
            cmd.env(k, v);
        }
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return CmdRes { cmd: cmd_str, rc: -1, out: String::new(), err: e.to_string(), ms: t0.elapsed().as_millis() as i64, timedout: false };
        }
    };
    match tokio::time::timeout(Duration::from_secs(timeout_sec.max(1)), child.wait_with_output()).await {
        Ok(Ok(o)) => CmdRes {
            cmd: cmd_str,
            rc: o.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&o.stdout).into_owned(),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
            ms: t0.elapsed().as_millis() as i64,
            timedout: false,
        },
        Ok(Err(e)) => CmdRes { cmd: cmd_str, rc: -1, out: String::new(), err: e.to_string(), ms: t0.elapsed().as_millis() as i64, timedout: false },
        Err(_) => CmdRes {
            cmd: cmd_str,
            rc: -1,
            out: String::new(),
            err: format!("Command timed out after {}s", timeout_sec),
            ms: t0.elapsed().as_millis() as i64,
            timedout: true,
        },
    }
}

fn jsonout(res: &CmdRes) -> Vec<Value> {
    if res.rc != 0 {
        return vec![];
    }
    serde_json::from_str::<Vec<Value>>(&res.out).unwrap_or_default()
}

fn write_exec(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

fn upsert_hgrc(path: &Path, key: &str, line: Option<&str>) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut kept: Vec<String> = existing.lines().filter(|l| !l.trim().starts_with(key)).map(|l| l.to_string()).collect();
    if let Some(line) = line {
        if let Some(idx) = kept.iter().position(|l| l.trim() == "[hooks]") {
            kept.insert(idx + 1, line.to_string());
        } else {
            if kept.last().map(|l| !l.trim().is_empty()).unwrap_or(false) {
                kept.push(String::new());
            }
            kept.push("[hooks]".to_string());
            kept.push(line.to_string());
        }
    }
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut text = kept.join("\n");
    text.push('\n');
    std::fs::write(path, text)
}

/// The Mercurial changegroup hook, written into `.hg/g5hook.py`.
/// Runs inside Mercurial's own Python interpreter — identical on Linux and Windows.
fn hg_hook_py(hook_url: Option<&str>) -> String {
    let escaped = hook_url.unwrap_or("").replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        r#"# Installed by rustportal scm. Do NOT edit - Reconfigure rewrites this file.
#
# Forces everything outside the approved mainline back to the secret phase,
# then tells the portal a push arrived. The whole quarantine is re-derived every time
# rather than just the arriving changesets, because a client that published
# its own commits locally carries that phase forward on its next push.
import os

G5_HOOK_URL = "{}"


def changegroup(ui, repo, hooktype=None, node=None, source=None, url=None, **kw):
    root = repo.root
    if not isinstance(root, str):
        root = root.decode('utf-8')
    mainline = None
    try:
        with open(os.path.join(root, '.hg', 'g5mainline')) as fh:
            mainline = fh.read().strip()
    except Exception:
        mainline = None
    revset = ('not ancestors(%s)' % mainline) if mainline else 'not public()'
    try:
        from mercurial import phases
        with repo.lock(), repo.transaction(b'g5quarantine') as tr:
            nodes = [repo[rev].node()
                     for rev in repo.revs(revset.encode('utf-8'))]
            if nodes:
                phases.retractboundary(repo, tr, phases.secret, nodes)
    except Exception as exc:
        # Loud: an unquarantined push is the one failure that must not be quiet.
        ui.warn(b'g5: FAILED to quarantine this push: %s\n'
                % str(exc).encode('utf-8', 'replace'))
        ui.warn(b'g5: the changesets are NOT hidden - tell your administrator.\n')
    if G5_HOOK_URL:
        try:
            import urllib.request
            urllib.request.urlopen(G5_HOOK_URL, timeout=15).read()
        except Exception:
            pass
    return 0
"#,
        escaped
    )
}

// ---------------------------------------------------------------------------
// Per-repository atomic lockfile
// ---------------------------------------------------------------------------

struct RepoLock {
    path: PathBuf,
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn acquire_lock(repo_path: &str, wait_secs: u64) -> Option<RepoLock> {
    let w = repo_path.trim();
    if w.is_empty() {
        return None;
    }
    let lock_path = PathBuf::from(format!("{}.scmlock", w));
    if let Some(p) = lock_path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    loop {
        match OpenOptions::new().write(true).create_new(true).open(&lock_path) {
            Ok(mut f) => {
                let _ = writeln!(f, "locked {}", Local::now().format("%Y-%m-%d %H:%M:%S"));
                return Some(RepoLock { path: lock_path });
            }
            Err(_) => {
                if let Ok(meta) = std::fs::metadata(&lock_path) {
                    if let Ok(mtime) = meta.modified() {
                        let age = SystemTime::now().duration_since(mtime).unwrap_or_default().as_millis();
                        if age > LOCK_STALE_MS {
                            let _ = std::fs::remove_file(&lock_path);
                            continue;
                        }
                    }
                }
                if Instant::now() >= deadline {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Low-level VCS operations (Mercurial & Git)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct ChangesetInfo {
    node: String,
    full_node: String,
    author: String,
    date: String,
    branch: String,
    phase: String,
    parents: Vec<String>,
    desc: String,
}

fn is_repo(vcs: Vcs, path: &str) -> bool {
    let p = Path::new(path.trim());
    match vcs {
        Vcs::Mercurial => p.join(".hg").exists(),
        Vcs::Git => p.join(".git").exists() || (p.join("HEAD").exists() && p.join("objects").is_dir()),
    }
}

async fn init_repo(vcs: Vcs, path: &str) -> CmdRes {
    let p = path.trim();
    match vcs {
        Vcs::Mercurial => run_cmd(&["hg", "init", p], None, 120, None).await,
        Vcs::Git => run_cmd(&["git", "init", "--bare", p], None, 120, None).await,
    }
}

async fn clone_from(vcs: Vcs, source: &str, path: &str) -> CmdRes {
    let (src, dst) = (source.trim(), path.trim());
    match vcs {
        Vcs::Mercurial => run_cmd(&["hg", "clone", "--noupdate", src, dst], None, 900, None).await,
        Vcs::Git => run_cmd(&["git", "clone", "--bare", src, dst], None, 900, None).await,
    }
}

async fn probe_source(vcs: Vcs, source: &str) -> CmdRes {
    let src = source.trim();
    match vcs {
        Vcs::Mercurial => run_cmd(&["hg", "identify", src], None, 120, None).await,
        Vcs::Git => run_cmd(&["git", "ls-remote", "--heads", src], None, 120, None).await,
    }
}

async fn verify_repo_cmd(vcs: Vcs, path: &str) -> CmdRes {
    let p = path.trim();
    match vcs {
        Vcs::Mercurial => run_cmd(&["hg", "-R", p, "identify", "-i"], None, 60, None).await,
        Vcs::Git => run_cmd(&["git", "-C", p, "rev-parse", "--git-dir"], None, 60, None).await,
    }
}

pub async fn configure_repo(repo: &Row, hook_url: Option<&str>) -> Value {
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    let mut notes: Vec<String> = vec![];
    match vcs {
        Vcs::Mercurial => {
            let hgrc = Path::new(&rpath).join(".hg").join("hgrc");
            let base = "[phases]\npublish = false\n\n[web]\nallow-push = *\npush_ssl = false\n";
            if !hgrc.exists() {
                if let Some(p) = hgrc.parent() {
                    let _ = std::fs::create_dir_all(p);
                }
                if let Err(e) = std::fs::write(&hgrc, base) {
                    return json!({"ok": false, "notes": notes, "endpoint_slug": vcs.endpoint_slug(), "error": e.to_string()});
                }
            } else {
                let cur = std::fs::read_to_string(&hgrc).unwrap_or_default();
                if !cur.contains("publish = false") {
                    let _ = std::fs::write(&hgrc, format!("{}\n{}", base, cur));
                }
            }
            notes.push("Repository set non-publishing.".to_string());

            let hook_py = Path::new(&rpath).join(".hg").join("g5hook.py");
            if let Err(e) = std::fs::write(&hook_py, hg_hook_py(hook_url)) {
                return json!({"ok": false, "notes": notes, "endpoint_slug": vcs.endpoint_slug(), "error": e.to_string()});
            }
            let _ = upsert_hgrc(&hgrc, "changegroup.g5quarantine", None);
            let _ = upsert_hgrc(&hgrc, "changegroup.g5notify", None);
            let _ = upsert_hgrc(&hgrc, "prepushkey.g5guard", None);
            let _ = upsert_hgrc(&hgrc, "changegroup.g5", Some("changegroup.g5 = python:.hg/g5hook.py:changegroup"));
            notes.push("Anything outside the approved mainline is forced back to secret on every push.".to_string());
            if hook_url.map(|x| !x.trim().is_empty()).unwrap_or(false) {
                notes.push("The portal is told about a push once the quarantine is in place.".to_string());
            }
        }
        Vcs::Git => {
            let br = default_branch(repo, vcs);
            let head_ref = format!("refs/heads/{}", br);
            run_cmd(&["git", "-C", &rpath, "config", "uploadpack.hideRefs", "refs/g5"], None, 60, None).await;
            run_cmd(&["git", "-C", &rpath, "config", "http.receivepack", "true"], None, 60, None).await;
            run_cmd(&["git", "-C", &rpath, "config", "receive.denyDeleteCurrent", "refuse"], None, 60, None).await;
            run_cmd(&["git", "-C", &rpath, "symbolic-ref", "HEAD", &head_ref], None, 60, None).await;
            notes.push("refs/g5/* hidden from clients; quarantined work cannot be fetched.".to_string());

            let hooks = if Path::new(&rpath).join(".git").exists() {
                Path::new(&rpath).join(".git").join("hooks")
            } else {
                Path::new(&rpath).join("hooks")
            };
            let pre_receive = "#!/bin/sh\n\
# Installed by rustportal scm. Pushes may only land in the quarantine\n\
# namespace; branches move when a checker approves, never by a push.\n\
status=0\n\
while read old new ref; do\n\
  case \"$ref\" in\n\
    refs/g5/incoming/*) ;;\n\
    *)\n\
      echo \"REJECTED: $ref is not yours to move. Push to refs/g5/incoming/<name> instead;\"\n\
      echo \"          a checker approves it in the portal and the portal publishes it.\"\n\
      status=1 ;;\n\
  esac\n\
done\n\
exit $status\n";
            if let Err(e) = write_exec(&hooks.join("pre-receive"), pre_receive) {
                return json!({"ok": false, "notes": notes, "endpoint_slug": vcs.endpoint_slug(), "error": e.to_string()});
            }
            notes.push("Direct pushes to a branch are refused.".to_string());

            if let Some(u) = hook_url.filter(|x| !x.trim().is_empty()) {
                let post_receive = format!(
                    "#!/bin/sh\n# Installed by rustportal scm.\ncurl -sS \"{}\" >/dev/null 2>&1 || true\nexit 0\n",
                    u.replace('"', "\\\"")
                );
                let _ = write_exec(&hooks.join("post-receive"), &post_receive);
                notes.push("The portal is told about a push as it happens.".to_string());
            }
        }
    }
    json!({"ok": true, "notes": notes, "endpoint_slug": vcs.endpoint_slug()})
}

async fn quarantined_heads(repo: &Row) -> (Vec<String>, CmdRes) {
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    match vcs {
        Vcs::Mercurial => {
            let res = run_cmd(&["hg", "-R", &rpath, "log", "-r", "sort(heads(secret()), date)", "-Tjson"], None, 300, None).await;
            let nodes: Vec<String> = jsonout(&res)
                .into_iter()
                .filter_map(|c| c.get("node").and_then(|v| v.as_str()).map(|x| x.to_string()))
                .filter(|x| !x.is_empty())
                .collect();
            (nodes, res)
        }
        Vcs::Git => {
            let res = run_cmd(&["git", "-C", &rpath, "for-each-ref", "--format=%(objectname)", "refs/g5/incoming/"], None, 120, None).await;
            let pub_res = run_cmd(&["git", "-C", &rpath, "for-each-ref", "--format=%(objectname)", "refs/heads/"], None, 120, None).await;
            let published: Vec<String> = pub_res.out.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
            let mut seen = BTreeSet::new();
            let mut raw_nodes = vec![];
            for line in res.out.lines() {
                let n = line.trim();
                if let Some(sn) = safenode(n) {
                    if seen.insert(sn.clone()) {
                        raw_nodes.push(sn);
                    }
                }
            }
            let mut nodes = vec![];
            for n in raw_nodes {
                if published.is_empty() {
                    nodes.push(n);
                    continue;
                }
                let mut is_pub = false;
                for p in &published {
                    let anc = run_cmd(&["git", "-C", &rpath, "merge-base", "--is-ancestor", &n, p], None, 60, None).await;
                    if anc.rc == 0 {
                        is_pub = true;
                        break;
                    }
                }
                if !is_pub {
                    nodes.push(n);
                }
            }
            (nodes, res)
        }
    }
}

async fn published_heads(repo: &Row) -> (Vec<String>, CmdRes) {
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    match vcs {
        Vcs::Mercurial => {
            let res = run_cmd(&["hg", "-R", &rpath, "log", "-r", "heads(public())", "-Tjson"], None, 120, None).await;
            let nodes: Vec<String> = jsonout(&res)
                .into_iter()
                .filter_map(|c| c.get("node").and_then(|v| v.as_str()).map(|x| x.to_string()))
                .filter(|x| !x.is_empty())
                .collect();
            (nodes, res)
        }
        Vcs::Git => {
            let br = default_branch(repo, vcs);
            let bref = format!("refs/heads/{}", br);
            let res = run_cmd(&["git", "-C", &rpath, "rev-parse", "--verify", "-q", &bref], None, 120, None).await;
            let nodes = safenode(&res.out).into_iter().collect();
            (nodes, res)
        }
    }
}

fn parse_hg_changeset(c: &Value) -> ChangesetInfo {
    let full = c.get("node").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let date_secs = c.get("date").and_then(|v| v.as_array()).and_then(|a| a.first()).and_then(|v| v.as_i64());
    let parents: Vec<String> = c
        .get("parents")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(take12).collect())
        .unwrap_or_default();
    ChangesetInfo {
        node: take12(&full),
        full_node: full,
        author: c.get("user").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        date: date_secs.map(fmt_epoch).unwrap_or_default(),
        branch: c.get("branch").and_then(|v| v.as_str()).filter(|x| !x.is_empty()).unwrap_or("default").to_string(),
        phase: c.get("phase").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        parents,
        desc: c.get("desc").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    }
}

async fn changesets_for_head(repo: &Row, head: &str) -> (Vec<ChangesetInfo>, Option<CmdRes>) {
    let Some(h) = safenode(head) else { return (vec![], None) };
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    match vcs {
        Vcs::Mercurial => {
            let rs = format!("sort(ancestors({}) and secret(), date)", h);
            let res = run_cmd(&["hg", "-R", &rpath, "log", "-r", &rs, "-Tjson"], None, 300, None).await;
            let list = jsonout(&res).iter().map(parse_hg_changeset).collect();
            (list, Some(res))
        }
        Vcs::Git => {
            let br = default_branch(repo, vcs);
            let bref = format!("refs/heads/{}", br);
            let hasbr = run_cmd(&["git", "-C", &rpath, "rev-parse", "--verify", "-q", &bref], None, 60, None).await;
            let excl = format!("^refs/heads/{}", br);
            let mut argv = vec!["git", "-C", &rpath, "log", "--reverse", "--format=%H%x01%an <%ae>%x01%at%x01%P%x01%B%x02", &h];
            if hasbr.rc == 0 {
                argv.push(&excl);
            }
            let res = run_cmd(&argv, None, 300, None).await;
            let mut list = vec![];
            for rec in res.out.split('\x02') {
                let rec = rec.trim();
                if rec.is_empty() {
                    continue;
                }
                let p: Vec<&str> = rec.splitn(5, '\x01').collect();
                if p.len() < 5 {
                    continue;
                }
                let epoch = p[2].trim().parse::<i64>().unwrap_or(0);
                let parents: Vec<String> = p[3].split_whitespace().map(take12).collect();
                list.push(ChangesetInfo {
                    node: take12(p[0]),
                    full_node: p[0].to_string(),
                    author: p[1].to_string(),
                    date: fmt_epoch(epoch),
                    branch: br.clone(),
                    phase: "quarantined".to_string(),
                    parents,
                    desc: p[4].trim().to_string(),
                });
            }
            (list, Some(res))
        }
    }
}

async fn already_published(repo: &Row, node: &str) -> (bool, Option<CmdRes>) {
    let Some(n) = safenode(node) else { return (false, None) };
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    match vcs {
        Vcs::Mercurial => {
            let rs = format!("({}) and public()", n);
            let res = run_cmd(&["hg", "-R", &rpath, "log", "-r", &rs, "-Tjson"], None, 120, None).await;
            let yes = res.rc == 0 && !jsonout(&res).is_empty();
            (yes, Some(res))
        }
        Vcs::Git => {
            let br = default_branch(repo, vcs);
            let bref = format!("refs/heads/{}", br);
            let res = run_cmd(&["git", "-C", &rpath, "merge-base", "--is-ancestor", &n, &bref], None, 120, None).await;
            (res.rc == 0, Some(res))
        }
    }
}

async fn is_fast_forward(repo: &Row, pubheads: &[String], node: &str) -> (bool, Option<CmdRes>) {
    let Some(n) = safenode(node) else { return (false, None) };
    let safe: Vec<String> = pubheads.iter().filter_map(|x| safenode(x)).collect();
    if safe.is_empty() {
        return (true, None);
    }
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    match vcs {
        Vcs::Mercurial => {
            let rs = format!("({}) - ancestors({})", safe.join("+"), n);
            let res = run_cmd(&["hg", "-R", &rpath, "log", "-r", &rs, "-Tjson"], None, 120, None).await;
            if res.rc != 0 {
                return (false, Some(res));
            }
            let ff = jsonout(&res).is_empty();
            (ff, Some(res))
        }
        Vcs::Git => {
            let mut last = None;
            let mut all_anc = true;
            for h in &safe {
                let r = run_cmd(&["git", "-C", &rpath, "merge-base", "--is-ancestor", h, &n], None, 120, None).await;
                if r.rc != 0 {
                    all_anc = false;
                }
                last = Some(r);
            }
            (all_anc, last)
        }
    }
}

async fn unpublished_ancestors(repo: &Row, node: &str, ownnodes: &[String]) -> (Vec<String>, Option<CmdRes>) {
    let Some(n) = safenode(node) else { return (vec![], None) };
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    let own: Vec<String> = ownnodes.iter().filter_map(|x| safenode(x)).collect();
    match vcs {
        Vcs::Mercurial => {
            let mut rs = format!("ancestors({}) and secret()", n);
            if !own.is_empty() {
                rs.push_str(&format!(" - ({})", own.join("+")));
            }
            let res = run_cmd(&["hg", "-R", &rpath, "log", "-r", &rs, "-Tjson"], None, 120, None).await;
            let list: Vec<String> = jsonout(&res)
                .into_iter()
                .filter_map(|c| c.get("node").and_then(|v| v.as_str()).map(take12))
                .filter(|x| !x.is_empty())
                .collect();
            (list, Some(res))
        }
        Vcs::Git => {
            let br = default_branch(repo, vcs);
            let bref = format!("refs/heads/{}", br);
            let own12: BTreeSet<String> = own.iter().map(|x| take12(x)).collect();
            let hasbr = run_cmd(&["git", "-C", &rpath, "rev-parse", "--verify", "-q", &bref], None, 60, None).await;
            let excl = format!("^refs/heads/{}", br);
            let mut argv = vec!["git", "-C", &rpath, "log", "--format=%H", &n];
            if hasbr.rc == 0 {
                argv.push(&excl);
            }
            let res = run_cmd(&argv, None, 120, None).await;
            let list: Vec<String> = res
                .out
                .lines()
                .map(|l| take12(l.trim()))
                .filter(|x| !x.is_empty() && !own12.contains(x))
                .collect();
            (list, Some(res))
        }
    }
}

async fn publish_node(repo: &Row, node: &str) -> CmdRes {
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    let Some(n) = safenode(node) else {
        return CmdRes { cmd: "publish".into(), rc: -1, out: String::new(), err: "invalid node".into(), ms: 0, timedout: false };
    };
    match vcs {
        Vcs::Mercurial => {
            let res = run_cmd(&["hg", "-R", &rpath, "phase", "--public", "-r", &n], None, 300, None).await;
            if res.rc == 0 {
                let marker = Path::new(&rpath).join(".hg").join("g5mainline");
                if let Err(e) = std::fs::write(&marker, format!("{}\n", n)) {
                    eprintln!("[scm] could not update mainline marker {}: {}", marker.display(), e);
                }
            }
            res
        }
        Vcs::Git => {
            let br = default_branch(repo, vcs);
            let bref = format!("refs/heads/{}", br);
            run_cmd(&["git", "-C", &rpath, "update-ref", &bref, &n], None, 300, None).await
        }
    }
}

struct MergeRes {
    ok: bool,
    step: &'static str,
    node: Option<String>,
    conflicts: Vec<String>,
    res: CmdRes,
}

async fn merge_into_published(repo: &Row, pubhead: &str, node: &str, msg: &str) -> MergeRes {
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    let (Some(ph), Some(nd)) = (safenode(pubhead), safenode(node)) else {
        return MergeRes {
            ok: false,
            step: "validate",
            node: None,
            conflicts: vec![],
            res: CmdRes { cmd: "merge".into(), rc: -1, out: String::new(), err: "invalid revision".into(), ms: 0, timedout: false },
        };
    };
    let who = commit_user(repo);
    match vcs {
        Vcs::Mercurial => {
            let up = run_cmd(&["hg", "-R", &rpath, "update", "-C", "-r", &ph], None, 300, None).await;
            if up.rc != 0 {
                return MergeRes { ok: false, step: "update", node: None, conflicts: vec![], res: up };
            }
            let mg = run_cmd(&["hg", "-R", &rpath, "merge", "--tool=internal:merge", "-r", &nd], None, 600, None).await;
            if mg.rc != 0 {
                let conf = run_cmd(&["hg", "--config", "ui.slash=1", "-R", &rpath, "resolve", "--list"], None, 120, None).await;
                let files: Vec<String> = conf.out.lines().filter_map(|l| l.strip_prefix("U ")).map(|x| x.trim().replace('\\', "/")).collect();
                run_cmd(&["hg", "-R", &rpath, "merge", "--abort"], None, 300, None).await;
                return MergeRes { ok: false, step: "merge", node: None, conflicts: files, res: mg };
            }
            let cm = run_cmd(&["hg", "-R", &rpath, "commit", "-u", &who, "-m", msg], None, 300, None).await;
            if cm.rc != 0 {
                run_cmd(&["hg", "-R", &rpath, "merge", "--abort"], None, 300, None).await;
                return MergeRes { ok: false, step: "commit", node: None, conflicts: vec![], res: cm };
            }
            let tip = run_cmd(&["hg", "-R", &rpath, "log", "-r", ".", "-Tjson"], None, 120, None).await;
            let merged = jsonout(&tip).first().and_then(|c| c.get("node")).and_then(|v| v.as_str()).map(|x| x.to_string());
            MergeRes { ok: true, step: "done", node: merged, conflicts: vec![], res: cm }
        }
        Vcs::Git => {
            let br = default_branch(repo, vcs);
            let bref = format!("refs/heads/{}", br);
            let (nm, em) = parse_name_email(&who);
            let work_base = {
                let wp = s(repo, "work_path").trim().to_string();
                if wp.is_empty() {
                    std::env::temp_dir().join("rustportal_scm")
                } else {
                    PathBuf::from(wp)
                }
            };
            let wt = work_base.join(format!("merge-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
            if let Some(p) = wt.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            let wt_str = wt.to_string_lossy().to_string();
            let add = run_cmd(&["git", "-C", &rpath, "worktree", "add", "--detach", &wt_str, &ph], None, 600, None).await;
            if add.rc != 0 {
                return MergeRes { ok: false, step: "worktree", node: None, conflicts: vec![], res: add };
            }
            let name_cfg = format!("user.name={}", nm);
            let email_cfg = format!("user.email={}", em);
            let author = format!("{} <{}>", nm, em);
            let result = {
                let mg = run_cmd(&["git", "-C", &wt_str, "-c", &name_cfg, "-c", &email_cfg, "merge", "--no-ff", "--no-commit", &nd], None, 600, None).await;
                if mg.rc != 0 {
                    let conf = run_cmd(&["git", "-C", &wt_str, "diff", "--name-only", "--diff-filter=U"], None, 120, None).await;
                    let files: Vec<String> = conf.out.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
                    MergeRes { ok: false, step: "merge", node: None, conflicts: files, res: mg }
                } else {
                    let cm = run_cmd(&["git", "-C", &wt_str, "-c", &name_cfg, "-c", &email_cfg, "commit", "--no-edit", "--author", &author, "-m", msg], None, 300, None).await;
                    if cm.rc != 0 {
                        MergeRes { ok: false, step: "commit", node: None, conflicts: vec![], res: cm }
                    } else {
                        let head = run_cmd(&["git", "-C", &wt_str, "rev-parse", "HEAD"], None, 120, None).await;
                        let merged = head.out.trim().to_string();
                        let upd = run_cmd(&["git", "-C", &rpath, "update-ref", &bref, &merged], None, 300, None).await;
                        if upd.rc != 0 {
                            MergeRes { ok: false, step: "publish", node: None, conflicts: vec![], res: upd }
                        } else {
                            MergeRes { ok: true, step: "done", node: Some(merged), conflicts: vec![], res: cm }
                        }
                    }
                }
            };
            run_cmd(&["git", "-C", &rpath, "worktree", "remove", "--force", &wt_str], None, 300, None).await;
            result
        }
    }
}

fn parse_name_email(who: &str) -> (String, String) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^(.*?)\s*<([^>]+)>\s*$").unwrap());
    if let Some(c) = re.captures(who.trim()) {
        let nm = c.get(1).map(|m| m.as_str().trim()).filter(|x| !x.is_empty()).unwrap_or("rustportal SCM");
        let em = c.get(2).map(|m| m.as_str().trim()).filter(|x| !x.is_empty()).unwrap_or("scm@localhost");
        return (nm.to_string(), em.to_string());
    }
    ("rustportal SCM".to_string(), "scm@localhost".to_string())
}

async fn diff_range(repo: &Row, a: Option<&str>, b: Option<&str>, stat: bool) -> CmdRes {
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    let na = a.and_then(safenode).filter(|x| !all_zeros(x));
    let nb = b.and_then(safenode);
    match vcs {
        Vcs::Mercurial => {
            let ra = na.unwrap_or_else(|| "null".to_string());
            let rb = nb.unwrap_or_else(|| "tip".to_string());
            let mut argv = vec!["hg", "-R", &rpath, "diff"];
            if stat {
                argv.push("--stat");
            }
            argv.extend(["-r", &ra, "-r", &rb]);
            run_cmd(&argv, None, 300, None).await
        }
        Vcs::Git => {
            let base = na.unwrap_or_else(|| EMPTY_TREE.to_string());
            let head = nb.unwrap_or_else(|| "HEAD".to_string());
            let range = format!("{}..{}", base, head);
            let mut argv = vec!["git", "-C", &rpath, "diff"];
            if stat {
                argv.push("--stat");
            }
            argv.push(&range);
            run_cmd(&argv, None, 300, None).await
        }
    }
}

async fn diff_change(repo: &Row, node: &str, stat: bool) -> CmdRes {
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    let Some(n) = safenode(node) else {
        return CmdRes { cmd: "diff".into(), rc: -1, out: String::new(), err: "invalid node".into(), ms: 0, timedout: false };
    };
    match vcs {
        Vcs::Mercurial => {
            let mut argv = vec!["hg", "-R", &rpath, "diff"];
            if stat {
                argv.push("--stat");
            }
            argv.extend(["-c", &n]);
            run_cmd(&argv, None, 300, None).await
        }
        Vcs::Git => {
            let mut argv = vec!["git", "-C", &rpath, "show", "--format="];
            if stat {
                argv.push("--stat");
            }
            argv.push(&n);
            run_cmd(&argv, None, 300, None).await
        }
    }
}

#[derive(Default)]
struct FilesChanged {
    added: Vec<String>,
    modified: Vec<String>,
    removed: Vec<String>,
    other: Vec<String>,
}

async fn files_of(repo: &Row, node: &str) -> (FilesChanged, CmdRes) {
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    let mut out = FilesChanged::default();
    let Some(n) = safenode(node) else {
        return (out, CmdRes { cmd: "files".into(), rc: -1, out: String::new(), err: "invalid node".into(), ms: 0, timedout: false });
    };
    match vcs {
        Vcs::Mercurial => {
            let res = run_cmd(&["hg", "--config", "ui.slash=1", "-R", &rpath, "status", "--change", &n], None, 300, None).await;
            for ln in res.out.lines() {
                if ln.len() < 3 {
                    continue;
                }
                let code = &ln[0..1];
                let fnm = ln[2..].trim().replace('\\', "/");
                match code {
                    "A" => out.added.push(fnm),
                    "M" => out.modified.push(fnm),
                    "R" => out.removed.push(fnm),
                    _ => out.other.push(fnm),
                }
            }
            (out, res)
        }
        Vcs::Git => {
            let res = run_cmd(&["git", "-C", &rpath, "show", "--name-status", "--format=", &n], None, 300, None).await;
            for ln in res.out.lines() {
                let p: Vec<&str> = ln.split('\t').collect();
                if p.len() < 2 {
                    continue;
                }
                let code = p[0].trim().chars().next().unwrap_or('?');
                let fnm = p.last().unwrap_or(&"").trim().to_string();
                match code {
                    'A' => out.added.push(fnm),
                    'M' => out.modified.push(fnm),
                    'D' => out.removed.push(fnm),
                    _ => out.other.push(fnm),
                }
            }
            (out, res)
        }
    }
}

// ---------------------------------------------------------------------------
// Portal helpers: URLs, endpoint verification, logging, user mapping
// ---------------------------------------------------------------------------

pub async fn portal_base_url(db: &Db, preferred: Option<&str>, request_base: &str) -> Option<String> {
    let clean = |v: &str| {
        let t = v.trim().trim_end_matches('/');
        if t.is_empty() { None } else { Some(t.to_string()) }
    };
    if let Some(p) = preferred.and_then(clean) {
        return Some(p);
    }
    if let Some(v) = web::setting_str(db, "scm_hook_base_url").await.and_then(|x| clean(&x)) {
        return Some(v);
    }
    if let Some(v) = web::setting_str(db, "server_url").await.and_then(|x| clean(&x)) {
        return Some(v);
    }
    clean(request_base)
}

pub async fn clone_url_for(db: &Db, repo: &Row, base: Option<&str>, request_base: &str) -> Option<String> {
    let vcs = Vcs::from_repo(repo);
    let b = match base.map(|x| x.trim().trim_end_matches('/')).filter(|x| !x.is_empty()) {
        Some(x) => x.to_string(),
        None => portal_base_url(db, None, request_base).await?,
    };
    let key = s(repo, "repo_key");
    let leaf = if vcs == Vcs::Git { format!("{}.git", key) } else { key };
    Some(format!("{}/svc/scm/{}/{}", b, vcs.endpoint_slug(), leaf))
}

fn norm_path(p: &str) -> Option<String> {
    let mut s = p.trim().replace('\\', "/");
    if s.is_empty() {
        return None;
    }
    while s.contains("//") {
        let idx = s.find("//").unwrap();
        if idx == 0 {
            // Preserve UNC prefix if any
            if let Some(next) = s[2..].find("//") {
                s.replace_range(2 + next..2 + next + 2, "/");
            } else {
                break;
            }
        } else {
            s.replace_range(idx..idx + 2, "/");
        }
    }
    while s.ends_with('/') && s.len() > 1 {
        s.pop();
    }
    Some(s.to_ascii_lowercase())
}

fn endpoint_env(ep: &Row) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let raw = s(ep, "env_json");
    if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(raw.trim()) {
        for (k, v) in m {
            out.insert(k, vs(&v));
        }
    }
    out
}

fn hgweb_targets(cfg_path: &Path) -> Vec<(String, &'static str)> {
    let Ok(text) = std::fs::read_to_string(cfg_path) else { return vec![] };
    let mut out = vec![];
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_ascii_lowercase();
            continue;
        }
        if section != "paths" && section != "collections" {
            continue;
        }
        let Some((_, val_raw)) = line.split_once('=') else { continue };
        let val = val_raw.trim();
        if val.is_empty() {
            continue;
        }
        if section == "collections" {
            if let Some(d) = norm_path(val) {
                out.push((d, "tree"));
            }
        } else if val.ends_with("/**") || val.ends_with("\\**") {
            if let Some(d) = norm_path(&val[..val.len() - 3]) {
                out.push((d, "tree"));
            }
        } else if val.ends_with("/*") || val.ends_with("\\*") {
            if let Some(d) = norm_path(&val[..val.len() - 2]) {
                out.push((d, "children"));
            }
        } else if let Some(d) = norm_path(val) {
            out.push((d, "exact"));
        }
    }
    out
}

pub fn endpoint_serves(ep: &Row, repo: &Row, vcs: Vcs) -> (Vec<String>, Vec<String>) {
    let mut problems = vec![];
    let mut notes = vec![];
    let rpath_raw = s(repo, "repo_path");
    let Some(rp) = norm_path(&rpath_raw) else { return (problems, notes) };
    let env = endpoint_env(ep);

    if vcs == Vcs::Git {
        let rootraw = env.get("GIT_PROJECT_ROOT").cloned().unwrap_or_default();
        match norm_path(&rootraw) {
            None => notes.push("Endpoint sets no GIT_PROJECT_ROOT, so what it serves cannot be checked from here.".to_string()),
            Some(root) if rp == root || rp.starts_with(&format!("{}/", root)) => {
                notes.push("The endpoint's GIT_PROJECT_ROOT covers this repository.".to_string());
            }
            Some(_) => {
                problems.push(format!(
                    "The endpoint serves GIT_PROJECT_ROOT {}, which does not contain {}. Clones and pushes on the clone URL are NOT reaching this repository.",
                    rootraw, rpath_raw
                ));
            }
        }
        return (problems, notes);
    }

    // Mercurial: check wrapper reads HGWEB_CONFIG
    let argv = crate::endpoint::parse_argv(&s(ep, "target"));
    if argv.len() > 1 {
        let script = Path::new(argv.last().unwrap());
        if script.is_file() {
            if let Ok(body) = std::fs::read_to_string(script) {
                if !body.is_empty() && !body.contains("HGWEB_CONFIG") {
                    problems.push(format!(
                        "The hgweb wrapper {} never reads HGWEB_CONFIG, so it serves whatever configuration is hardcoded inside it and ignores this endpoint's. Change it to read os.environ['HGWEB_CONFIG'].",
                        script.display()
                    ));
                }
            }
        }
    }

    let cfgraw = env.get("HGWEB_CONFIG").cloned().unwrap_or_default();
    if cfgraw.trim().is_empty() {
        notes.push("Endpoint sets no HGWEB_CONFIG, so what hgweb publishes cannot be checked from here.".to_string());
        return (problems, notes);
    }
    let cfg = Path::new(cfgraw.trim());
    if !cfg.is_file() {
        problems.push(format!("The endpoint's HGWEB_CONFIG does not exist on this server: {}", cfgraw));
        return (problems, notes);
    }
    let targets = hgweb_targets(cfg);
    if targets.is_empty() {
        notes.push(format!("No [paths] or [collections] entries found in {}.", cfgraw));
        return (problems, notes);
    }
    let parent = rp.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_else(|| rp.clone());
    let served = targets.iter().any(|(dir, mode)| match *mode {
        "exact" => *dir == rp,
        "children" => *dir == parent,
        _ => rp == *dir || rp.starts_with(&format!("{}/", dir)),
    });
    if served {
        notes.push(format!("hgweb ({}) publishes this repository.", cfgraw));
    } else {
        let mut seen = BTreeSet::new();
        let serving: Vec<String> = targets
            .iter()
            .map(|(dir, mode)| format!("{}{}", dir, if *mode == "children" { "/*" } else if *mode == "tree" { "/**" } else { "" }))
            .filter(|x| seen.insert(x.clone()))
            .collect();
        problems.push(format!(
            "hgweb ({}) does not publish {} - it serves {}. Clones and pushes on the clone URL are reaching a DIFFERENT repository, or nothing at all.",
            cfgraw,
            rpath_raw,
            serving.join(", ")
        ));
    }
    (problems, notes)
}

async fn logop(db: &Db, repo_id: Option<i64>, change_id: Option<i64>, opname: &str, res: &CmdRes, curuser_id: Option<i64>, ok_override: Option<bool>) {
    let Some(t) = Tracker::load(db, "scm", "scm_log").await else { return };
    let ok = ok_override.unwrap_or(res.rc == 0);
    let data = row! {
        "repository_id" => repo_id,
        "change_id" => change_id,
        "operation" => opname,
        "command" => trunc(&res.cmd, 4000),
        "exit_code" => res.rc as i64,
        "duration_ms" => res.ms,
        "stdout_text" => trunc(&res.out, 60000),
        "stderr_text" => trunc(&res.err, 20000),
        "run_by" => curuser_id,
        "run_date" => now_hm(),
        "record_status" => if ok { "Done" } else { "Error" },
    };
    if let Err(e) = t.save(db, &data).await {
        eprintln!("[scm] could not write scm_log: {}", e);
    }
}

pub async fn map_user(db: &Db, vcs_user: &str) -> Option<i64> {
    let trimmed = vcs_user.trim();
    if trimmed.is_empty() {
        return None;
    }
    static EMAIL_RE: OnceLock<Regex> = OnceLock::new();
    let email = EMAIL_RE
        .get_or_init(|| Regex::new(r"<([^>]+)>").unwrap())
        .captures(trimmed)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .filter(|x| !x.is_empty());
    static STRIP_EMAIL: OnceLock<Regex> = OnceLock::new();
    let namepart = STRIP_EMAIL.get_or_init(|| Regex::new(r"<[^>]*>").unwrap()).replace_all(trimmed, "").trim().to_string();

    let mut candidates = vec![];
    if let Some(ref em) = email {
        candidates.push(em.to_ascii_lowercase());
    }
    candidates.push(trimmed.to_ascii_lowercase());
    if !namepart.is_empty() {
        candidates.push(namepart.to_ascii_lowercase());
    }

    if let Some(t) = Tracker::load(db, "scm", "scm_user_map").await {
        if let Ok(maps) = t.rows(db, Some(&row! {"record_status"=>"Active"}), None, None, None).await {
            for mp in maps {
                let key = s(&mp, "vcs_user").trim().to_ascii_lowercase();
                if !key.is_empty() && candidates.contains(&key) {
                    if let Some(sid) = i(&mp, "staff_id") {
                        return Some(sid);
                    }
                }
            }
        }
    }
    if let Some(ref em) = email {
        if let Ok(Some(u)) = db.first("select id from portal_user where lower(email)=lower(:e) limit 1", &row! {"e"=>em.clone()}).await {
            if let Some(id) = i(&u, "id") {
                return Some(id);
            }
        }
    }
    if !namepart.is_empty() {
        if let Ok(Some(u)) = db
            .first("select id from portal_user where lower(lanid)=lower(:n) or userid=:n limit 1", &row! {"n"=>namepart})
            .await
        {
            if let Some(id) = i(&u, "id") {
                return Some(id);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Dependency gate
// ---------------------------------------------------------------------------

pub async fn dependency_check(db: &Db, repo: &Row, change: &Row) -> Value {
    let crid = i(change, "id").unwrap_or(0);
    let repoid = i(repo, "id").unwrap_or(0);
    let mut own = vec![];
    if crid > 0 {
        if let Some(t) = Tracker::load(db, "scm", "scm_changeset").await {
            if let Ok(rows) = t.rows(db, Some(&row! {"change_id"=>crid}), None, None, None).await {
                for r in rows {
                    let n = s(&r, "node");
                    if !n.is_empty() {
                        own.push(n);
                    }
                }
            }
        }
    }

    let last_node = s(change, "last_node");
    let (ancs, res) = unpublished_ancestors(repo, &last_node, &own).await;
    if let Some(r) = res {
        if r.rc != 0 {
            let msg = if r.err.trim().is_empty() { r.out } else { r.err };
            return json!({"ok": false, "reason": format!("Could not verify dependencies:\n{}", msg)});
        }
    }
    if ancs.is_empty() {
        return json!({"ok": true, "reason": Value::Null});
    }

    let cs_tracker = Tracker::load(db, "scm", "scm_changeset").await;
    let cr_tracker = Tracker::load(db, "scm", "scm_change").await;
    let mut blockers: BTreeMap<String, Option<Row>> = BTreeMap::new();
    for n in ancs {
        let mut matched = false;
        if let Some(ref cst) = cs_tracker {
            if let Ok(Some(cs)) = cst.first_row(db, Some(&row! {"repository_id"=>repoid, "node"=>n.clone()}), None).await {
                if let Some(other_id) = i(&cs, "change_id") {
                    matched = true;
                    if let Some(ref crt) = cr_tracker {
                        if let Ok(Some(other)) = crt.first_row(db, Some(&row! {"id"=>other_id}), None).await {
                            if s(&other, "record_status") != "Merged" {
                                blockers.insert(other_id.to_string(), Some(other));
                            }
                        }
                    }
                }
            }
        }
        if !matched {
            blockers.insert(format!("untracked:{}", n), None);
        }
    }
    if blockers.is_empty() {
        return json!({"ok": true, "reason": Value::Null});
    }
    let lines: Vec<String> = blockers
        .iter()
        .map(|(k, v)| match v {
            Some(other) => format!("  - change request #{} is {}: {}", k, s(other, "record_status"), s(other, "title")),
            None => format!("  - changeset {} is not published and is not tracked here", k.trim_start_matches("untracked:")),
        })
        .collect();
    json!({
        "ok": false,
        "reason": format!(
            "This push builds on work that has not been approved yet:\n{}\n\nApprove or reject those first, then use Re-check.",
            lines.join("\n")
        )
    })
}

async fn record_changeset(db: &Db, repo: &Row, change_id: i64, cs: &ChangesetInfo, seq: i64) -> Option<i64> {
    let t = Tracker::load(db, "scm", "scm_changeset").await?;
    let node_arg = if cs.full_node.is_empty() { cs.node.as_str() } else { cs.full_node.as_str() };
    let (fo, _) = files_of(repo, node_arg).await;
    let st = diff_change(repo, node_arg, true).await;
    let df = diff_change(repo, node_arg, false).await;
    let (_, added, removed) = parse_diffstat(&st.out);
    let mut flist = vec![];
    for f in &fo.added {
        flist.push(format!("A  {}", f));
    }
    for f in &fo.modified {
        flist.push(format!("M  {}", f));
    }
    for f in &fo.removed {
        flist.push(format!("R  {}", f));
    }
    for f in &fo.other {
        flist.push(format!("?  {}", f));
    }
    let author_uid = map_user(db, &cs.author).await;
    let data = row! {
        "change_id" => change_id,
        "repository_id" => i(repo, "id"),
        "node" => cs.node.clone(),
        "full_node" => cs.full_node.clone(),
        "seq" => seq,
        "author" => cs.author.clone(),
        "author_user" => author_uid,
        "commit_date" => if cs.date.is_empty() { Value::Null } else { json!(cs.date) },
        "branch" => cs.branch.clone(),
        "phase" => cs.phase.clone(),
        "parents" => cs.parents.join(", "),
        "description" => cs.desc.clone(),
        "files_added" => fo.added.len() as i64,
        "files_modified" => fo.modified.len() as i64,
        "files_removed" => fo.removed.len() as i64,
        "lines_added" => added,
        "lines_removed" => removed,
        "file_list" => flist.join("\n"),
        "diff_stat" => st.out,
        "diff_text" => trunc(&df.out, MAX_DIFF),
        "diff_truncated" => df.out.chars().count() > MAX_DIFF,
        "record_status" => "New",
    };
    t.save(db, &data).await.ok()
}

// ---------------------------------------------------------------------------
// High-level operations: verify, poll, approve, provision
// ---------------------------------------------------------------------------

pub async fn verify(db: &Db, repo: &Row) -> Value {
    let vcs = Vcs::from_repo(repo);
    let mut problems: Vec<String> = vec![];
    let mut notes: Vec<String> = vec![];

    let which = run_cmd(&[vcs.bin(), "--version"], None, 30, None).await;
    if which.rc != 0 {
        problems.push(format!("The {} client is not runnable by the portal server.", vcs.bin()));
    } else {
        let ver = first_nonempty_line(&which.out);
        if !ver.is_empty() {
            notes.push(ver);
        }
    }

    let rpath = s(repo, "repo_path").trim().to_string();
    if rpath.is_empty() {
        problems.push("Repository path is not set.".to_string());
    } else if !Path::new(&rpath).exists() {
        problems.push(format!("Repository path does not exist: {}", rpath));
    } else if !is_repo(vcs, &rpath) {
        problems.push(format!("Not a {} repository: {}", vcs.name(), rpath));
    } else {
        let vr = verify_repo_cmd(vcs, &rpath).await;
        if vr.rc != 0 {
            problems.push(format!("Repository could not be read: {}", trunc(&vr.err, 500)));
        }
    }

    if !problems.is_empty() {
        let msg = problems.iter().map(|p| format!("- {}", p)).collect::<Vec<_>>().join("\n");
        return json!({"ok": false, "health": "Problem", "message": msg});
    }

    let endpoint = db
        .first(
            "select * from portal_endpoint where module='scm' and slug=:s and enabled is true limit 1",
            &row! {"s"=>vcs.endpoint_slug()},
        )
        .await
        .ok()
        .flatten();
    match endpoint {
        None => {
            problems.push(format!(
                "No enabled PortalEndpoint scm/{} - nothing is serving this repository.",
                vcs.endpoint_slug()
            ));
        }
        Some(ep) => {
            notes.push(format!("Served by endpoint scm/{}.", vcs.endpoint_slug()));
            let (ep_probs, ep_notes) = endpoint_serves(&ep, repo, vcs);
            problems.extend(ep_probs);
            notes.extend(ep_notes);
        }
    }

    let (ph_nodes, _) = published_heads(repo).await;
    if ph_nodes.is_empty() {
        notes.push("Nothing published yet - the first approval seeds the mainline.".to_string());
    } else {
        let short: Vec<String> = ph_nodes.iter().map(|n| take12(n)).collect();
        notes.push(format!("Published head: {}", short.join(", ")));
    }
    let (qh_nodes, _) = quarantined_heads(repo).await;
    notes.push(format!("Quarantined heads awaiting review: {} ({}).", qh_nodes.len(), vcs.quarantine_desc()));

    if !problems.is_empty() {
        let msg = problems.iter().map(|p| format!("- {}", p)).collect::<Vec<_>>().join("\n");
        return json!({"ok": false, "health": "Problem", "message": msg});
    }
    let msg = notes.iter().filter(|n| !n.is_empty()).map(|n| format!("- {}", n)).collect::<Vec<_>>().join("\n");
    json!({"ok": true, "health": "OK", "message": msg})
}

pub async fn poll(db: &Db, repo: &Row, _curuser_id: Option<i64>, source: &str, lock_wait: u64) -> Value {
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    let Some(_lock) = acquire_lock(&rpath, lock_wait).await else {
        return json!({
            "ok": true, "created": [], "skipped": 0, "changesets": 0, "busy": true,
            "message": "Another SCM operation is already running for this repository."
        });
    };

    let (qh_nodes, qh_res) = quarantined_heads(repo).await;
    if qh_res.rc != 0 {
        return json!({
            "ok": false, "created": [], "skipped": 0, "changesets": 0, "busy": false,
            "message": format!("Could not list quarantined heads: {}", trunc(&qh_res.err, 2000))
        });
    }

    let repoid = i(repo, "id").unwrap_or(0);
    let mut known: BTreeSet<String> = BTreeSet::new();
    if let Some(cst) = Tracker::load(db, "scm", "scm_changeset").await {
        if let Ok(rows) = cst.rows(db, Some(&row! {"repository_id"=>repoid}), None, None, None).await {
            for r in rows {
                let n = s(&r, "node");
                if !n.is_empty() {
                    known.insert(n);
                }
            }
        }
    }

    if qh_nodes.is_empty() {
        return json!({
            "ok": true, "created": [], "skipped": 0, "changesets": 0, "busy": false,
            "message": "Nothing is awaiting review."
        });
    }

    let Some(cr_tracker) = Tracker::load(db, "scm", "scm_change").await else {
        return json!({
            "ok": false, "created": [], "skipped": 0, "changesets": 0, "busy": false,
            "message": "Tracker scm:scm_change not found."
        });
    };

    let mut assigned: BTreeSet<String> = BTreeSet::new();
    let mut created: Vec<Value> = vec![];
    let mut skipped: usize = 0;
    let mut changesets: usize = 0;

    for head in &qh_nodes {
        let (mem_list, mem_res) = changesets_for_head(repo, head).await;
        if mem_res.as_ref().map(|r| r.rc != 0).unwrap_or(false) {
            continue;
        }
        let fresh: Vec<ChangesetInfo> = mem_list
            .iter()
            .filter(|c| !known.contains(&c.node) && !assigned.contains(&c.node))
            .cloned()
            .collect();
        skipped += mem_list.len() - fresh.len();
        if fresh.is_empty() {
            continue;
        }
        for c in &fresh {
            assigned.insert(c.node.clone());
        }

        let first = &fresh[0];
        let last = fresh.last().unwrap();
        let ownnodes: BTreeSet<String> = fresh.iter().map(|c| c.node.clone()).collect();
        let base = first.parents.iter().find(|p| !ownnodes.contains(*p) && !all_zeros(p)).cloned();

        let dstat = diff_range(repo, base.as_deref(), Some(head), true).await;
        let ddiff = diff_range(repo, base.as_deref(), Some(head), false).await;
        let (files_changed, lines_added, lines_removed) = parse_diffstat(&dstat.out);

        let mut author_set = BTreeSet::new();
        let mut authors = vec![];
        for c in &fresh {
            if !c.author.is_empty() && author_set.insert(c.author.clone()) {
                authors.push(c.author.clone());
            }
        }
        let mut title = first_nonempty_line(&first.desc);
        if title.is_empty() {
            title = "(no commit message)".to_string();
        }
        if title.chars().count() > 180 {
            title = format!("{}...", title.chars().take(180).collect::<String>());
        }
        if fresh.len() > 1 {
            title.push_str(&format!(" (+{} more)", fresh.len() - 1));
        }

        let pushed_by = if authors.len() == 1 { map_user(db, &authors[0]).await } else { None };
        let pushed_date = if last.date.is_empty() { now_hm() } else { last.date.clone() };
        let branch = if last.branch.is_empty() { "default".to_string() } else { last.branch.clone() };

        let cr_row = row! {
            "repository_id" => repoid,
            "title" => title.clone(),
            "branch" => branch.clone(),
            "source" => source,
            "pushed_by_vcs" => authors.join("; "),
            "pushed_by" => pushed_by,
            "pushed_date" => pushed_date,
            "base_node" => base.clone(),
            "first_node" => first.node.clone(),
            "last_node" => head.clone(),
            "changeset_count" => fresh.len() as i64,
            "files_changed" => files_changed,
            "lines_added" => lines_added,
            "lines_removed" => lines_removed,
            "diff_stat" => dstat.out,
            "full_diff" => trunc(&ddiff.out, MAX_DIFF),
            "diff_truncated" => ddiff.out.chars().count() > MAX_DIFF,
            "record_status" => "Pending Review",
        };
        let Ok(crid) = cr_tracker.save(db, &cr_row).await else { continue };
        for (idx, cs) in fresh.iter().enumerate() {
            record_changeset(db, repo, crid, cs, (idx + 1) as i64).await;
        }
        changesets += fresh.len();

        // Pin commits in Git so a later push moving the incoming ref cannot strand them.
        if vcs == Vcs::Git {
            let pin = format!("refs/g5/cr/{}", crid);
            run_cmd(&["git", "-C", &rpath, "update-ref", &pin, head], None, 120, None).await;
        }

        let dep = dependency_check(db, repo, &row! {"id"=>crid, "last_node"=>head.clone()}).await;
        let blocked = !dep.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        if blocked {
            let reason = dep.get("reason").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let _ = cr_tracker.save(db, &row! {"id"=>crid, "record_status"=>"Blocked", "blocked_reason"=>reason}).await;
        }
        created.push(json!({
            "id": crid,
            "branch": branch,
            "count": fresh.len(),
            "blocked": blocked,
            "title": title,
        }));
    }

    let mut msg = format!("Raised {} change request(s) covering {} changeset(s).", created.len(), changesets);
    if skipped > 0 {
        msg.push_str(&format!(" Skipped {} already recorded.", skipped));
    }
    json!({
        "ok": true,
        "created": created,
        "skipped": skipped,
        "changesets": changesets,
        "busy": false,
        "message": msg,
    })
}

pub async fn approve(db: &Db, repo: &Row, change: &Row, curuser: Option<&Row>, lock_wait: u64) -> Value {
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    let repoid = i(repo, "id");
    let crid = i(change, "id");
    let curuser_id = curuser.and_then(|u| i(u, "id"));
    let curuser_name = curuser.map(|u| s(u, "name")).filter(|x| !x.is_empty()).unwrap_or_else(|| "rustportal".to_string());

    let Some(_lock) = acquire_lock(&rpath, lock_wait).await else {
        return json!({
            "ok": false, "status": "Failed", "merge_node": Value::Null, "conflicts": [], "notes": [],
            "message": "Another SCM operation is still running for this repository, so nothing was published and the mainline is unchanged. Try again in a moment."
        });
    };

    let head = s(change, "last_node");
    if safenode(&head).is_none() {
        return json!({
            "ok": false, "status": "Failed", "merge_node": Value::Null, "conflicts": [], "notes": [],
            "message": "This change request has no usable head revision."
        });
    }

    let (already, _) = already_published(repo, &head).await;
    if already {
        return json!({
            "ok": true, "status": "Merged", "merge_node": head, "conflicts": [], "message": "",
            "notes": ["These changesets were already published, so nothing needed doing."]
        });
    }

    let dep = dependency_check(db, repo, change).await;
    if !dep.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        return json!({
            "ok": false, "status": "Blocked", "merge_node": Value::Null, "conflicts": [], "notes": [],
            "message": dep.get("reason").and_then(|v| v.as_str()).unwrap_or("Blocked")
        });
    }

    let (pubheads, _) = published_heads(repo).await;
    let ff_allowed = !flag(repo.get("always_merge_commit"));
    let (ff, _) = is_fast_forward(repo, &pubheads, &head).await;

    let mut notes: Vec<String> = vec![];
    let merge_node: Option<String>;

    if (ff && ff_allowed) || pubheads.is_empty() {
        let pr = publish_node(repo, &head).await;
        logop(db, repoid, crid, "publish", &pr, curuser_id, None).await;
        if pr.rc != 0 {
            let detail = if pr.err.trim().is_empty() { pr.out } else { pr.err };
            return json!({
                "ok": false, "status": "Failed", "merge_node": Value::Null, "conflicts": [], "notes": notes,
                "message": format!("Publishing failed, the mainline is unchanged:\n{}", trunc(&detail, 3000))
            });
        }
        merge_node = Some(head.clone());
        notes.push(if pubheads.is_empty() {
            "This is the first approved work; it seeds the mainline.".to_string()
        } else {
            "The mainline had not moved, so it advanced straight to this work.".to_string()
        });
    } else {
        let title = s(change, "title");
        let msg = format!(
            "SCM: merge change request #{}{} (approved by {})",
            crid.unwrap_or(0),
            if title.is_empty() { String::new() } else { format!(" - {}", title) },
            curuser_name
        );
        let mr = merge_into_published(repo, &pubheads[0], &head, &msg).await;
        logop(db, repoid, crid, "merge", &mr.res, curuser_id, Some(mr.ok)).await;
        if !mr.ok {
            if mr.step == "merge" {
                let detail = if !mr.conflicts.is_empty() {
                    format!("Conflicting files:\n{}", mr.conflicts.iter().map(|f| format!("  {}", f)).collect::<Vec<_>>().join("\n"))
                } else {
                    let raw = if mr.res.err.trim().is_empty() { &mr.res.out } else { &mr.res.err };
                    trunc(raw, 3000)
                };
                return json!({
                    "ok": false, "status": "Conflict", "merge_node": Value::Null,
                    "conflicts": mr.conflicts, "notes": notes,
                    "message": format!("The merge hit conflicts, so nothing was published - the mainline is untouched.\n\n{}\n\nAsk the developer to pull the mainline into their work, resolve, and push again.", detail)
                });
            }
            let raw = if mr.res.err.trim().is_empty() { &mr.res.out } else { &mr.res.err };
            return json!({
                "ok": false, "status": "Failed", "merge_node": Value::Null, "conflicts": [], "notes": notes,
                "message": format!("Could not merge ({}), the mainline is unchanged:\n{}", mr.step, trunc(raw, 3000))
            });
        }
        if vcs == Vcs::Mercurial {
            if let Some(ref mn) = mr.node {
                let pr = publish_node(repo, mn).await;
                logop(db, repoid, crid, "publish", &pr, curuser_id, None).await;
                if pr.rc != 0 {
                    let raw = if pr.err.trim().is_empty() { &pr.out } else { &pr.err };
                    return json!({
                        "ok": false, "status": "Failed", "merge_node": Value::Null, "conflicts": [], "notes": notes,
                        "message": format!("Merged, but publishing the phase failed:\n{}", trunc(raw, 2000))
                    });
                }
            }
        }
        let short = mr.node.as_deref().map(take12).unwrap_or_default();
        notes.push(format!("Merged into the mainline as {}.", short));
        merge_node = mr.node;
    }

    // Git: clean up quarantine refs now that the work is published.
    if vcs == Vcs::Git {
        if let Some(cid) = crid {
            let pin = format!("refs/g5/cr/{}", cid);
            run_cmd(&["git", "-C", &rpath, "update-ref", "-d", &pin], None, 120, None).await;
        }
        let refs = run_cmd(&["git", "-C", &rpath, "for-each-ref", "--format=%(refname) %(objectname)", "refs/g5/incoming/"], None, 120, None).await;
        for ln in refs.out.lines() {
            let mut sp = ln.split_whitespace();
            if let (Some(rname), Some(oid)) = (sp.next(), sp.next()) {
                if oid == head {
                    run_cmd(&["git", "-C", &rpath, "update-ref", "-d", rname], None, 120, None).await;
                }
            }
        }
    }

    json!({
        "ok": true,
        "status": "Merged",
        "merge_node": merge_node,
        "conflicts": [],
        "notes": notes,
        "message": "",
    })
}

pub async fn provision(db: &Db, o: &Row, curuser_id: Option<i64>, request_base: &str) -> Value {
    let mut steps: Vec<String> = vec![];
    let mut warnings: Vec<String> = vec![];
    let fail = |msg: String, steps: Vec<String>, warnings: Vec<String>| {
        json!({"ok": false, "message": msg, "steps": steps, "warnings": warnings, "repo": Value::Null})
    };

    let name = s(o, "name").trim().to_string();
    let key = s(o, "repo_key").trim().to_ascii_lowercase();
    let vcs_str = {
        let v = s(o, "vcs_type").trim().to_string();
        if v.is_empty() { "Mercurial".to_string() } else { v }
    };
    let vcs = if vcs_str.eq_ignore_ascii_case("git") { Vcs::Git } else { Vcs::Mercurial };
    let base = s(o, "base_path").trim().trim_end_matches(['/', '\\']).to_string();
    let source = s(o, "source").trim().to_string();
    let branch = {
        let b = s(o, "default_branch").trim().to_string();
        if b.is_empty() { if vcs == Vcs::Git { "main".into() } else { "default".into() } } else { b }
    };

    if name.is_empty() {
        return fail("A repository name is required.".into(), steps, warnings);
    }
    if key.is_empty() {
        return fail("A repository key is required.".into(), steps, warnings);
    }
    if !key_re().is_match(&key) {
        return fail("The key may only contain lowercase letters, digits, dash and underscore.".into(), steps, warnings);
    }
    if base.is_empty() {
        return fail("A base directory is required.".into(), steps, warnings);
    }

    let Some(repo_tracker) = Tracker::load(db, "scm", "scm_repository").await else {
        return fail("Tracker scm:scm_repository is missing. Re-import the scm module.".into(), steps, warnings);
    };

    if let Ok(Some(_)) = repo_tracker.first_row(db, Some(&row! {"repo_key"=>key.clone(), "record_status"=>"!Delete"}), None).await {
        return fail(format!("A repository with the key '{}' already exists.", key), steps, warnings);
    }

    let repo_path = format!("{}/{}{}", base, key, if vcs == Vcs::Git { ".git" } else { "" });
    let work_path = format!("{}/.work/{}", base, key);

    if Path::new(&repo_path).exists() {
        return fail(format!("{} already exists. Remove it, or pick a different key.", repo_path), steps, warnings);
    }
    let bdir = Path::new(&base);
    if !bdir.exists() && std::fs::create_dir_all(bdir).is_err() {
        return fail(format!("Could not create {}.", base), steps, warnings);
    }
    if !dir_writable(bdir) {
        return fail(format!("The portal cannot write into {}.", base), steps, warnings);
    }

    if !source.is_empty() {
        let pr = probe_source(vcs, &source).await;
        logop(db, None, None, "probe-source", &pr, curuser_id, None).await;
        if pr.rc != 0 {
            let detail = if pr.err.trim().is_empty() { &pr.out } else { &pr.err };
            return fail(format!("Could not read the source repository at {}:\n{}", source, trunc(detail, 1500)), steps, warnings);
        }
        steps.push(format!("Source repository at {} is readable.", source));
    }

    let r1 = if !source.is_empty() {
        clone_from(vcs, &source, &repo_path).await
    } else {
        init_repo(vcs, &repo_path).await
    };
    logop(db, None, None, if source.is_empty() { "init" } else { "clone" }, &r1, curuser_id, None).await;
    if r1.rc != 0 {
        let detail = if r1.err.trim().is_empty() { &r1.out } else { &r1.err };
        return fail(format!("Could not create the repository:\n{}", trunc(detail, 1500)), steps, warnings);
    }
    steps.push(if source.is_empty() {
        format!("Created an empty repository at {}.", repo_path)
    } else {
        format!("Created {} from {}.", repo_path, source)
    });

    let hook_tok = mint_token();
    let pref = s(o, "hook_url_base");
    let base_url = portal_base_url(db, if pref.is_empty() { None } else { Some(pref.as_str()) }, request_base).await;
    let draft = row! {
        "repo_key" => key.clone(),
        "vcs_type" => vcs.name(),
        "repo_path" => repo_path.clone(),
        "default_branch" => branch.clone(),
        "name" => name.clone(),
    };
    let clone_url = clone_url_for(db, &draft, base_url.as_deref(), request_base).await;
    let cuser = {
        let u = s(o, "commit_user").trim().to_string();
        if u.is_empty() { "rustportal SCM <scm@localhost>".to_string() } else { u }
    };

    let rec_in = row! {
        "name" => name.clone(),
        "repo_key" => key.clone(),
        "vcs_type" => vcs.name(),
        "description" => o.get("description").cloned().unwrap_or(Value::Null),
        "repo_path" => repo_path.clone(),
        "work_path" => work_path,
        "clone_url" => clone_url,
        "endpoint_slug" => vcs.endpoint_slug(),
        "default_branch" => branch,
        "commit_user" => cuser,
        "hook_token" => hook_tok.clone(),
        "notify_emails" => o.get("notify_emails").cloned().unwrap_or(Value::Null),
        "record_status" => "Active",
    };
    let rec_id = match repo_tracker.save(db, &rec_in).await {
        Ok(id) => id,
        Err(e) => return fail(format!("Could not register repository in database: {}", e), steps, warnings),
    };
    let mut rec = repo_tracker.get(db, rec_id).await.unwrap_or(rec_in);
    rec.insert("id".into(), json!(rec_id));
    steps.push(format!("Registered in the portal as record #{}.", rec_id));

    let hook_url = base_url.as_ref().map(|b| format!("{}/run/scm/hook?key={}&token={}", b, key, hook_tok));
    if hook_url.is_none() {
        warnings.push("Could not work out this portal's own URL, so the push hook was not installed. Set the 'scm_hook_base_url' setting and use Reconfigure.".to_string());
    }
    let cf = configure_repo(&rec, hook_url.as_deref()).await;
    if let Some(arr) = cf.get("notes").and_then(|v| v.as_array()) {
        for n in arr {
            steps.push(vs(n));
        }
    }

    // Anything an imported Mercurial repository already contained is the approved baseline.
    if !source.is_empty() && vcs == Vcs::Mercurial {
        let pub_res = run_cmd(&["hg", "-R", &repo_path, "phase", "--public", "-r", "all()"], None, 300, None).await;
        logop(db, Some(rec_id), None, "publish-baseline", &pub_res, curuser_id, None).await;
        let tip = run_cmd(&["hg", "-R", &repo_path, "log", "-r", "heads(public())", "-Tjson"], None, 120, None).await;
        let heads: Vec<String> = jsonout(&tip)
            .into_iter()
            .filter_map(|c| c.get("node").and_then(|v| v.as_str()).map(|x| x.to_string()))
            .filter(|x| !x.is_empty())
            .collect();
        if let Some(h0) = heads.first() {
            let _ = std::fs::write(Path::new(&repo_path).join(".hg").join("g5mainline"), format!("{}\n", h0));
            let _ = repo_tracker.save(db, &row! {"id"=>rec_id, "last_main_node"=>h0.clone()}).await;
            rec.insert("last_main_node".into(), json!(h0));
        }
        steps.push("Imported history marked as the approved baseline.".to_string());
    }

    let v = Box::pin(verify(db, &rec)).await;
    let health = v.get("health").and_then(|x| x.as_str()).unwrap_or("Problem").to_string();
    let vmsg = v.get("message").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let _ = repo_tracker.save(db, &row! {"id"=>rec_id, "health"=>health.clone(), "last_poll_status"=>vmsg.clone()}).await;
    steps.push(format!("Health check: {}.", health));
    if health != "OK" {
        warnings.push(format!("The health check did not pass, so this repository is not ready yet:\n{}", vmsg));
    }

    let pl = Box::pin(poll(db, &rec, curuser_id, "Poll", 30)).await;
    steps.push(format!("Initial scan: {}", pl.get("message").and_then(|x| x.as_str()).unwrap_or("")));

    json!({
        "ok": true,
        "message": format!("Repository '{}' is ready.", name),
        "steps": steps,
        "warnings": warnings,
        "repo": Value::Object(rec),
    })
}

// ---------------------------------------------------------------------------
// Browsing & Search (guarded by safe_path + browsable_rev + published scope)
// ---------------------------------------------------------------------------

pub async fn browsable_rev(db: &Db, repo: &Row, rev: Option<&str>) -> Option<Value> {
    let want = rev.map(|x| x.trim()).unwrap_or("");
    if want.is_empty() {
        let (ph, _) = published_heads(repo).await;
        return ph.into_iter().next().map(|n| json!({"node": n, "published": true, "change_id": Value::Null}));
    }
    let n = safenode(want)?;
    if already_published(repo, &n).await.0 {
        return Some(json!({"node": n, "published": true, "change_id": Value::Null}));
    }
    let repoid = i(repo, "id").unwrap_or(0);
    if let Some(cst) = Tracker::load(db, "scm", "scm_changeset").await {
        if let Ok(Some(cs)) = cst.first_row(db, Some(&row! {"repository_id"=>repoid, "node"=>take12(&n)}), None).await {
            return Some(json!({"node": n, "published": false, "change_id": cs.get("change_id").cloned().unwrap_or(Value::Null)}));
        }
    }
    if let Some(crt) = Tracker::load(db, "scm", "scm_change").await {
        if let Ok(Some(cr)) = crt.first_row(db, Some(&row! {"repository_id"=>repoid, "last_node"=>n.clone()}), None).await {
            return Some(json!({"node": n, "published": false, "change_id": cr.get("id").cloned().unwrap_or(Value::Null)}));
        }
    }
    None
}

fn parse_hg_tree(output: &str, sp: &str) -> Vec<Value> {
    let prefix = if sp.is_empty() { String::new() } else { format!("{}/", sp) };
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    let mut files: Vec<(String, String)> = vec![];
    for line in output.lines() {
        let f = line.trim().replace('\\', "/");
        let f = f.trim_start_matches('/');
        if f.is_empty() || !f.starts_with(&prefix) {
            continue;
        }
        let rest = &f[prefix.len()..];
        if rest.is_empty() {
            continue;
        }
        if let Some((d, _)) = rest.split_once('/') {
            dirs.insert(d.to_string());
        } else {
            files.push((rest.to_string(), format!("{}{}", prefix, rest)));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut entries: Vec<Value> = dirs
        .into_iter()
        .map(|d| json!({"name": d, "path": format!("{}{}", prefix, d), "type": "dir", "size": Value::Null}))
        .collect();
    for (nm, fp) in files {
        entries.push(json!({"name": nm, "path": fp, "type": "file", "size": Value::Null}));
    }
    entries
}

pub async fn browse_tree(repo: &Row, rev: &str, path: Option<&str>) -> Value {
    let Some(sp) = safe_path(path) else {
        return json!({"error": "That path is not allowed.", "entries": []});
    };
    let Some(r) = safenode(rev) else {
        return json!({"error": "Invalid revision.", "entries": []});
    };
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    match vcs {
        Vcs::Mercurial => {
            let res = run_cmd(&["hg", "--config", "ui.slash=1", "files", "-r", &r], Some(&rpath), 120, None).await;
            if res.rc != 0 {
                let err = if res.err.trim().is_empty() { &res.out } else { &res.err };
                return json!({"error": trunc(err, 800), "entries": []});
            }
            let entries = parse_hg_tree(&res.out, &sp);
            json!({"entries": entries, "path": sp})
        }
        Vcs::Git => {
            let spec = if sp.is_empty() { String::new() } else { format!("{}/", sp) };
            let mut argv = vec!["git", "-C", &rpath, "-c", "core.quotepath=false", "ls-tree", "--long", &r];
            if !spec.is_empty() {
                argv.extend(["--", &spec]);
            }
            let res = run_cmd(&argv, None, 120, None).await;
            if res.rc != 0 {
                let err = if res.err.trim().is_empty() { &res.out } else { &res.err };
                return json!({"error": trunc(err, 800), "entries": []});
            }
            let mut dirs: Vec<Value> = vec![];
            let mut files: Vec<(String, Value)> = vec![];
            for ln in res.out.lines() {
                let Some((meta_s, full)) = ln.split_once('\t') else { continue };
                let meta: Vec<&str> = meta_s.split_whitespace().collect();
                if meta.len() < 3 {
                    continue;
                }
                let name = full.rsplit_once('/').map(|(_, n)| n).unwrap_or(full).to_string();
                let is_dir = meta[1] == "tree";
                let size = meta.get(3).copied().filter(|x| *x != "-");
                let obj = json!({"name": name, "path": full, "type": if is_dir { "dir" } else { "file" }, "size": size});
                if is_dir {
                    dirs.push(obj);
                } else {
                    files.push((name, obj));
                }
            }
            files.sort_by(|a, b| a.0.cmp(&b.0));
            dirs.extend(files.into_iter().map(|(_, v)| v));
            json!({"entries": dirs, "path": sp})
        }
    }
}

pub async fn browse_file(repo: &Row, rev: &str, path: Option<&str>, max_chars: usize) -> Value {
    let Some(sp) = safe_path(path).filter(|x| !x.is_empty()) else {
        return json!({"error": "That path is not allowed."});
    };
    let Some(r) = safenode(rev) else {
        return json!({"error": "Invalid revision."});
    };
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    let res = match vcs {
        Vcs::Mercurial => {
            let pspec = format!("path:{}", sp);
            run_cmd(&["hg", "--config", "ui.slash=1", "cat", "-r", &r, "--", &pspec], Some(&rpath), 120, None).await
        }
        Vcs::Git => {
            let spec = format!("{}:{}", r, sp);
            run_cmd(&["git", "-C", &rpath, "show", &spec], None, 120, None).await
        }
    };
    if res.rc != 0 {
        let err = if res.err.trim().is_empty() { &res.out } else { &res.err };
        return json!({"error": trunc(err, 800)});
    }
    let bin = looks_binary(&res.out);
    let total_chars = res.out.chars().count();
    let truncated = total_chars > max_chars;
    let text: String = if truncated { res.out.chars().take(max_chars).collect() } else { res.out };
    let lines = if bin { 0 } else { text.lines().count() };
    json!({"text": text, "truncated": truncated, "binary": bin, "path": sp, "lines": lines})
}

pub async fn browse_log(repo: &Row, rev: &str, path: Option<&str>, limit: usize, offset: usize) -> Value {
    let Some(sp) = safe_path(path) else {
        return json!({"error": "That path is not allowed.", "list": []});
    };
    let Some(r) = safenode(rev) else {
        return json!({"error": "Invalid revision.", "list": []});
    };
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    let fetch = limit + 1;
    let (mut rows, res) = match vcs {
        Vcs::Mercurial => {
            let rs = if sp.is_empty() {
                format!("limit(reverse(ancestors({})), {}, {})", r, fetch, offset)
            } else {
                let esc_p = sp.replace('\\', "\\\\").replace('"', "\\\"");
                format!("limit(reverse(ancestors({}) and file(\"path:{}\")), {}, {})", r, esc_p, fetch, offset)
            };
            let res = run_cmd(&["hg", "log", "-r", &rs, "-Tjson"], Some(&rpath), 300, None).await;
            let list: Vec<Value> = jsonout(&res)
                .iter()
                .map(|c| {
                    let info = parse_hg_changeset(c);
                    json!({
                        "node": info.node, "full_node": info.full_node, "author": info.author,
                        "date": info.date, "branch": info.branch, "desc": info.desc,
                        "subject": first_nonempty_line(&info.desc),
                    })
                })
                .collect();
            (list, res)
        }
        Vcs::Git => {
            let lim_s = fetch.to_string();
            let skip_s = format!("--skip={}", offset);
            let mut argv = vec!["git", "-C", &rpath, "log", "-n", &lim_s];
            if offset > 0 {
                argv.push(&skip_s);
            }
            argv.extend(["--format=%H%x01%an <%ae>%x01%at%x01%B%x02", &r]);
            if !sp.is_empty() {
                argv.extend(["--", &sp]);
            }
            let res = run_cmd(&argv, None, 300, None).await;
            let br = default_branch(repo, vcs);
            let mut list = vec![];
            for rec in res.out.split('\x02') {
                let rec = rec.trim();
                if rec.is_empty() {
                    continue;
                }
                let p: Vec<&str> = rec.splitn(4, '\x01').collect();
                if p.len() < 4 {
                    continue;
                }
                let epoch = p[2].trim().parse::<i64>().unwrap_or(0);
                let desc = p[3].trim().to_string();
                list.push(json!({
                    "node": take12(p[0]), "full_node": p[0], "author": p[1],
                    "date": fmt_epoch(epoch), "branch": br, "desc": desc,
                    "subject": first_nonempty_line(&desc),
                }));
            }
            (list, res)
        }
    };
    if res.rc != 0 {
        let err = if res.err.trim().is_empty() { &res.out } else { &res.err };
        return json!({"error": trunc(err, 800), "list": []});
    }
    let has_more = rows.len() > limit;
    if has_more {
        rows.truncate(limit);
    }
    json!({
        "list": rows, "path": sp, "offset": offset, "limit": limit,
        "has_more": has_more, "has_prev": offset > 0,
    })
}

pub async fn browse_commit(repo: &Row, rev: &str, max_chars: usize) -> Value {
    let Some(r) = safenode(rev) else {
        return json!({"error": "Invalid revision."});
    };
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();
    let info: Option<ChangesetInfo> = match vcs {
        Vcs::Mercurial => {
            let res = run_cmd(&["hg", "log", "-r", &r, "-Tjson"], Some(&rpath), 120, None).await;
            jsonout(&res).first().map(parse_hg_changeset)
        }
        Vcs::Git => {
            let res = run_cmd(&["git", "-C", &rpath, "log", "-n", "1", "--format=%H%x01%an <%ae>%x01%at%x01%P%x01%B%x02", &r], None, 120, None).await;
            res.out.split('\x02').find(|x| !x.trim().is_empty()).and_then(|rec| {
                let p: Vec<&str> = rec.trim().splitn(5, '\x01').collect();
                if p.len() < 5 {
                    return None;
                }
                let epoch = p[2].trim().parse::<i64>().unwrap_or(0);
                Some(ChangesetInfo {
                    node: take12(p[0]),
                    full_node: p[0].to_string(),
                    author: p[1].to_string(),
                    date: fmt_epoch(epoch),
                    branch: default_branch(repo, vcs),
                    phase: String::new(),
                    parents: p[3].split_whitespace().map(take12).collect(),
                    desc: p[4].trim().to_string(),
                })
            })
        }
    };
    let Some(ci) = info else {
        return json!({"error": "That revision could not be read."});
    };
    let (fo, _) = files_of(repo, &r).await;
    let st = diff_change(repo, &r, true).await;
    let df = diff_change(repo, &r, false).await;
    let mut files = vec![];
    for f in fo.added {
        files.push(json!({"state": "added", "path": f}));
    }
    for f in fo.modified {
        files.push(json!({"state": "modified", "path": f}));
    }
    for f in fo.removed {
        files.push(json!({"state": "removed", "path": f}));
    }
    for f in fo.other {
        files.push(json!({"state": "other", "path": f}));
    }
    let truncated = df.out.chars().count() > max_chars;
    let diff: String = if truncated { df.out.chars().take(max_chars).collect() } else { df.out };
    let diff_line_count = diff.lines().count();
    let desc_lines = ci.desc.lines().count();
    let subject = {
        let s = first_nonempty_line(&ci.desc);
        if s.is_empty() { "(no commit message)".to_string() } else { s }
    };
    json!({
        "info": {
            "node": ci.node,
            "full_node": ci.full_node,
            "author": ci.author,
            "date": ci.date,
            "branch": ci.branch,
            "phase": ci.phase,
            "parents": ci.parents,
            "desc": ci.desc,
            "subject": subject,
            "multiline": desc_lines > 1,
        },
        "files": files,
        "stat": st.out,
        "diff": diff,
        "diff_line_count": diff_line_count,
        "truncated": truncated,
    })
}

pub async fn browse_search(repo: &Row, rev: &str, q: &str, mode: &str, limit: usize, depth: usize) -> Value {
    let q = q.trim();
    if q.is_empty() {
        return json!({"list": [], "mode": mode, "depth": depth});
    }
    let Some(r) = safenode(rev) else {
        return json!({"list": [], "mode": mode, "depth": depth, "note": "Invalid revision."});
    };
    let vcs = Vcs::from_repo(repo);
    let rpath = s(repo, "repo_path").trim().to_string();

    let (list, res): (Vec<Value>, CmdRes) = match (vcs, mode) {
        (Vcs::Mercurial, "messages") => {
            let escaped = q.replace('\\', "\\\\").replace('"', "\\\"");
            let rs = format!("reverse(ancestors({}) and keyword(\"{}\"))", r, escaped);
            let lim_s = limit.to_string();
            let res = run_cmd(&["hg", "log", "-r", &rs, "-l", &lim_s, "-Tjson"], Some(&rpath), 300, None).await;
            let list = jsonout(&res)
                .iter()
                .map(|c| {
                    let info = parse_hg_changeset(c);
                    json!({
                        "node": info.node, "full_node": info.full_node, "author": info.author,
                        "date": info.date, "desc": info.desc, "subject": first_nonempty_line(&info.desc),
                    })
                })
                .collect();
            (list, res)
        }
        (Vcs::Mercurial, "history") => {
            // Strictly confined to public() so quarantined / rejected secrets are never searchable!
            let rs = if depth > 0 {
                format!("limit(reverse(public()), {})", depth)
            } else {
                "public()".to_string()
            };
            let res = run_cmd(&["hg", "--config", "ui.slash=1", "grep", "--all", "--rev", &rs, "-Tjson", "--", q], Some(&rpath), 300, None).await;
            let list = jsonout(&res)
                .into_iter()
                .take(limit)
                .map(|m| {
                    let text: String = m
                        .get("texts")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().filter_map(|x| x.get("text").and_then(|t| t.as_str())).collect::<String>())
                        .unwrap_or_default();
                    let p = m.get("path").and_then(|v| v.as_str()).map(|x| x.replace('\\', "/"));
                    json!({
                        "path": p, "line": m.get("lineno"),
                        "node": m.get("node").and_then(|v| v.as_str()).map(take12).unwrap_or_default(),
                        "author": m.get("user"), "text": text,
                    })
                })
                .collect();
            (list, res)
        }
        (Vcs::Mercurial, _) => {
            let res = run_cmd(&["hg", "--config", "ui.slash=1", "grep", "--rev", &r, "-Tjson", "--", q], Some(&rpath), 120, None).await;
            let list = jsonout(&res)
                .into_iter()
                .take(limit)
                .map(|m| {
                    let text: String = m
                        .get("texts")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().filter_map(|x| x.get("text").and_then(|t| t.as_str())).collect::<String>())
                        .unwrap_or_default();
                    let p = m.get("path").and_then(|v| v.as_str()).map(|x| x.replace('\\', "/"));
                    json!({
                        "path": p, "line": m.get("lineno"), "text": text,
                        "node": m.get("node").and_then(|v| v.as_str()).map(take12).unwrap_or_default(),
                    })
                })
                .collect();
            (list, res)
        }
        (Vcs::Git, "messages") => {
            let lim_s = limit.to_string();
            let grep_arg = format!("--grep={}", q);
            let res = run_cmd(&["git", "-C", &rpath, "log", "-n", &lim_s, &grep_arg, "-i", "--format=%H%x01%an <%ae>%x01%at%x01%B%x02", &r], None, 300, None).await;
            let mut list = vec![];
            for rec in res.out.split('\x02') {
                let rec = rec.trim();
                if rec.is_empty() {
                    continue;
                }
                let p: Vec<&str> = rec.splitn(4, '\x01').collect();
                if p.len() < 4 {
                    continue;
                }
                let epoch = p[2].trim().parse::<i64>().unwrap_or(0);
                let desc = p[3].trim().to_string();
                list.push(json!({
                    "node": take12(p[0]), "full_node": p[0], "author": p[1],
                    "date": fmt_epoch(epoch), "desc": desc, "subject": first_nonempty_line(&desc),
                }));
            }
            (list, res)
        }
        (Vcs::Git, "history") => {
            // Strictly confined to published branch refs/heads/<br>!
            let br = default_branch(repo, vcs);
            let mut scope = format!("refs/heads/{}", br);
            if depth > 0 {
                let skip_s = format!("--skip={}", depth);
                let bound = run_cmd(&["git", "-C", &rpath, "rev-list", "--max-count=1", &skip_s, &scope], None, 120, None).await;
                if let Some(b) = safenode(&bound.out) {
                    scope = format!("{}..refs/heads/{}", b, br);
                }
            }
            let lim_s = limit.to_string();
            let res = run_cmd(&["git", "-C", &rpath, "log", "-n", &lim_s, "-S", q, "--format=%H%x01%an <%ae>%x01%at%x01%B%x02", &scope], None, 300, None).await;
            let mut list = vec![];
            for rec in res.out.split('\x02') {
                let rec = rec.trim();
                if rec.is_empty() {
                    continue;
                }
                let p: Vec<&str> = rec.splitn(4, '\x01').collect();
                if p.len() < 4 {
                    continue;
                }
                list.push(json!({
                    "node": take12(p[0]), "author": p[1], "path": Value::Null, "line": Value::Null,
                    "text": first_nonempty_line(p[3].trim()),
                }));
            }
            (list, res)
        }
        (Vcs::Git, _) => {
            let res = run_cmd(&["git", "-C", &rpath, "grep", "-n", "-I", "--no-color", "-e", q, &r], None, 120, None).await;
            let mut list = vec![];
            for ln in res.out.lines() {
                if list.len() >= limit {
                    break;
                }
                let Some(a) = ln.find(':') else { continue };
                let Some(b_rel) = ln[a + 1..].find(':') else { continue };
                let b = a + 1 + b_rel;
                let Some(c_rel) = ln[b + 1..].find(':') else { continue };
                let c = b + 1 + c_rel;
                list.push(json!({
                    "path": &ln[a + 1..b],
                    "line": &ln[b + 1..c],
                    "text": &ln[c + 1..],
                    "node": take12(&r),
                }));
            }
            (list, res)
        }
    };

    if res.rc != 0 && !res.err.trim().is_empty() {
        return json!({"list": list, "mode": mode, "depth": depth, "note": trunc(&res.err, 400)});
    }
    json!({"list": list, "mode": mode, "depth": depth})
}

// ---------------------------------------------------------------------------
// Integration tests against REAL Mercurial and Git repositories
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::{AppState, Config};
    use std::sync::{Arc, Mutex};

    async fn setup_test_env() -> (crate::web::St, PathBuf) {
        let db = Db::connect("sqlite::memory:", 1, None).await.expect("sqlite memory");
        let entities = crate::entity::entities();
        crate::entity::sync_schema(&db, &entities).await.expect("sync schema");
        let st = Arc::new(AppState {
            db,
            cfg: Config::default(),
            entities,
            excel_jobs: Mutex::new(HashMap::new()),
            rt: tokio::runtime::Handle::current(),
        });
        let mod_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("modulemigration/scm");
        if mod_dir.is_dir() {
            crate::module::import_from(&st, "scm", &mod_dir, false).await.expect("import scm module");
        }
        let tmp = std::env::temp_dir().join(format!("rp_scm_test_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&tmp).unwrap();
        (st, tmp)
    }

    #[test]
    fn guards_and_parsers() {
        assert_eq!(safe_path(Some("src/main.rs")), Some("src/main.rs".into()));
        assert_eq!(safe_path(Some("docs\\PortalPage_UserManual.md")), Some("docs/PortalPage_UserManual.md".into()));
        assert_eq!(safe_path(Some("/leading/slash/")), Some("leading/slash".into()));
        assert_eq!(safe_path(Some("../etc/passwd")), None);
        assert_eq!(safe_path(Some("a/../../b")), None);
        assert_eq!(safe_path(Some("..\\etc\\passwd")), None);
        assert_eq!(safe_path(Some("bad\x00path")), None);
        assert_eq!(safe_path(Some("bad\npath")), None);
        assert_eq!(safe_path(None), Some(String::new()));

        // Windows backslash paths from `hg files` must group into folders cleanly
        let win_hg = "README.md\r\ndocs\\PortalPage.md\r\ndocs\\sub\\deep.md\r\nsrc\\main.rs\r\n";
        let root_entries = parse_hg_tree(win_hg, "");
        assert_eq!(
            root_entries,
            vec![
                json!({"name": "docs", "path": "docs", "type": "dir", "size": Value::Null}),
                json!({"name": "src", "path": "src", "type": "dir", "size": Value::Null}),
                json!({"name": "README.md", "path": "README.md", "type": "file", "size": Value::Null}),
            ]
        );
        let docs_entries = parse_hg_tree(win_hg, "docs");
        assert_eq!(
            docs_entries,
            vec![
                json!({"name": "sub", "path": "docs/sub", "type": "dir", "size": Value::Null}),
                json!({"name": "PortalPage.md", "path": "docs/PortalPage.md", "type": "file", "size": Value::Null}),
            ]
        );
        let deep_entries = parse_hg_tree(win_hg, "docs/sub");
        assert_eq!(
            deep_entries,
            vec![json!({"name": "deep.md", "path": "docs/sub/deep.md", "type": "file", "size": Value::Null})]
        );

        assert!(safenode("abcdef123456").is_some());
        assert!(safenode("abcde").is_none());
        assert!(safenode("abcdef; rm -rf /").is_none());

        assert_eq!(parse_diffstat(" 3 files changed, 42 insertions(+), 7 deletions(-)"), (3, 42, 7));
        assert!(looks_binary("hello\0world"));
        assert!(!looks_binary("plain utf8 text"));

        let h = diff_html("diff --git a/x b/x\n@@ -1 +1 @@\n-<script>\n+ok", 10);
        assert!(h.contains("&lt;script&gt;"));
        assert!(!h.contains("<script>"));
    }

    #[tokio::test]
    async fn endpoint_serves_checks_git_and_hg() {
        let tmp = std::env::temp_dir().join(format!("rp_ep_test_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&tmp).unwrap();

        // Git check
        let git_ep = row! {"env_json" => json!({"GIT_PROJECT_ROOT": tmp.to_string_lossy()}).to_string()};
        let ok_repo = row! {"repo_path" => tmp.join("app.git").to_string_lossy().to_string()};
        let bad_repo = row! {"repo_path" => "/other/place/app.git"};
        assert!(endpoint_serves(&git_ep, &ok_repo, Vcs::Git).0.is_empty());
        assert!(!endpoint_serves(&git_ep, &bad_repo, Vcs::Git).0.is_empty());

        // Hg check: wrapper reading HGWEB_CONFIG + hgweb.config publishing /*
        let cfg = tmp.join("hgweb.config");
        std::fs::write(&cfg, format!("[paths]\n/ = {}/*\n", tmp.display())).unwrap();
        let good_wrap = tmp.join("hgweb.cgi");
        std::fs::write(&good_wrap, "import os\ncfg = os.environ['HGWEB_CONFIG']\n").unwrap();
        let bad_wrap = tmp.join("hardcoded.cgi");
        std::fs::write(&bad_wrap, "cfg = b'/somewhere/else/hgweb.config'\n").unwrap();

        let hg_ep_ok = row! {
            "target" => format!("python3 {}", good_wrap.display()),
            "env_json" => json!({"HGWEB_CONFIG": cfg.to_string_lossy()}).to_string(),
        };
        let hg_repo = row! {"repo_path" => tmp.join("proj").to_string_lossy().to_string()};
        assert!(endpoint_serves(&hg_ep_ok, &hg_repo, Vcs::Mercurial).0.is_empty());
        assert!(!endpoint_serves(&hg_ep_ok, &bad_repo, Vcs::Mercurial).0.is_empty());

        let hg_ep_bad = row! {
            "target" => format!("python3 {}", bad_wrap.display()),
            "env_json" => json!({"HGWEB_CONFIG": cfg.to_string_lossy()}).to_string(),
        };
        assert!(!endpoint_serves(&hg_ep_bad, &hg_repo, Vcs::Mercurial).0.is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn full_lifecycle_git() {
        if run_cmd(&["git", "--version"], None, 10, None).await.rc != 0 {
            return;
        }
        let (st, tmp) = setup_test_env().await;
        if Tracker::load(&st.db, "scm", "scm_repository").await.is_none() {
            let _ = std::fs::remove_dir_all(&tmp);
            return;
        }
        let root = tmp.join("repos");
        std::fs::create_dir_all(&root).unwrap();

        // Register enabled git endpoint pointing at root
        st.db.insert("portal_endpoint", &row! {
            "module" => "scm", "slug" => "git", "name" => "Git", "handler_type" => "CGI",
            "target" => "/usr/lib/git-core/git-http-backend", "auth_mode" => "Basic",
            "env_json" => json!({"GIT_PROJECT_ROOT": root.to_string_lossy(), "GIT_HTTP_EXPORT_ALL": "1"}).to_string(),
            "enabled" => true,
        }).await.unwrap();

        // 1. Provision empty Git repository
        let prov = provision(&st.db, &row! {
            "name" => "Demo Git", "repo_key" => "demo-git", "vcs_type" => "Git",
            "base_path" => root.to_string_lossy().to_string(), "hook_url_base" => "http://localhost:9092",
        }, None, "http://localhost:9092").await;
        assert_eq!(prov["ok"], json!(true), "provision failed: {}", prov);
        let repo = prov["repo"].as_object().unwrap().clone();
        let rpath = s(&repo, "repo_path");

        // Verify passes
        let v = verify(&st.db, &repo).await;
        assert_eq!(v["health"], json!("OK"), "verify failed: {}", v);

        // 2. Developer clones, commits, and tries direct push to main -> MUST BE REFUSED
        let dev = tmp.join("dev-git");
        let cl = run_cmd(&["git", "clone", &rpath, dev.to_str().unwrap()], None, 60, None).await;
        assert_eq!(cl.rc, 0, "clone failed: {}", cl.err);
        run_cmd(&["git", "-C", dev.to_str().unwrap(), "config", "user.name", "Alice"], None, 10, None).await;
        run_cmd(&["git", "-C", dev.to_str().unwrap(), "config", "user.email", "alice@example.com"], None, 10, None).await;
        run_cmd(&["git", "-C", dev.to_str().unwrap(), "checkout", "-b", "main"], None, 10, None).await;
        std::fs::write(dev.join("hello.txt"), "hello world\n").unwrap();
        run_cmd(&["git", "-C", dev.to_str().unwrap(), "add", "hello.txt"], None, 10, None).await;
        run_cmd(&["git", "-C", dev.to_str().unwrap(), "commit", "-m", "Initial hello"], None, 10, None).await;

        let direct = run_cmd(&["git", "-C", dev.to_str().unwrap(), "push", "origin", "HEAD:refs/heads/main"], None, 30, None).await;
        assert_ne!(direct.rc, 0, "direct push to main must be rejected!");

        // 3. Push to quarantine refs/g5/incoming/alice -> succeeds, invisible to published_heads
        let qp = run_cmd(&["git", "-C", dev.to_str().unwrap(), "push", "origin", "HEAD:refs/g5/incoming/alice"], None, 30, None).await;
        assert_eq!(qp.rc, 0, "quarantine push failed: {}", qp.err);
        assert!(published_heads(&repo).await.0.is_empty());

        // 4. Poll raises 1 change request
        let p1 = poll(&st.db, &repo, None, "Poll", 10).await;
        assert_eq!(p1["created"].as_array().unwrap().len(), 1, "poll: {}", p1);
        let cr1_id = p1["created"][0]["id"].as_i64().unwrap();
        let cr_t = Tracker::load(&st.db, "scm", "scm_change").await.unwrap();
        let cr1 = cr_t.get(&st.db, cr1_id).await.unwrap();
        assert_eq!(s(&cr1, "record_status"), "Pending Review");

        // 5. Approve seeds mainline (fast-forward)
        let app1 = approve(&st.db, &repo, &cr1, Some(&row! {"id"=>2, "name"=>"Bob"}), 10).await;
        assert_eq!(app1["ok"], json!(true), "approve 1 failed: {}", app1);
        assert_eq!(published_heads(&repo).await.0.len(), 1);
        cr_t.save(&st.db, &row! {"id"=>cr1_id, "record_status"=>"Merged"}).await.unwrap();

        // 6. Canary secret pushed to quarantine (NOT approved) MUST NOT appear in search history/content!
        std::fs::write(dev.join("secret.txt"), "CANARY_SECRET_TOKEN_999\n").unwrap();
        run_cmd(&["git", "-C", dev.to_str().unwrap(), "add", "secret.txt"], None, 10, None).await;
        run_cmd(&["git", "-C", dev.to_str().unwrap(), "commit", "-m", "Add secret canary"], None, 10, None).await;
        let qp2 = run_cmd(&["git", "-C", dev.to_str().unwrap(), "push", "origin", "HEAD:refs/g5/incoming/alice"], None, 30, None).await;
        assert_eq!(qp2.rc, 0);

        let pub_rev = published_heads(&repo).await.0[0].clone();
        let s_hist = browse_search(&repo, &pub_rev, "CANARY_SECRET_TOKEN_999", "history", 50, 1000).await;
        assert!(s_hist["list"].as_array().unwrap().is_empty(), "quarantined secret leaked in history search!");
        let s_cont = browse_search(&repo, &pub_rev, "CANARY_SECRET_TOKEN_999", "content", 50, 1000).await;
        assert!(s_cont["list"].as_array().unwrap().is_empty(), "quarantined secret leaked in content search!");

        // Unpolled quarantine rev MUST NOT be browsable
        let secret_sha = run_cmd(&["git", "-C", dev.to_str().unwrap(), "rev-parse", "HEAD"], None, 10, None).await.out.trim().to_string();
        assert!(browsable_rev(&st.db, &repo, Some(&secret_sha)).await.is_none());

        // 7. Stacked push dependency gate + browsing
        let p2 = poll(&st.db, &repo, None, "Poll", 10).await;
        let cr2_id = p2["created"][0]["id"].as_i64().unwrap();
        // Now tracked -> browsable under review
        let brv = browsable_rev(&st.db, &repo, Some(&secret_sha)).await.unwrap();
        assert_eq!(brv["published"], json!(false));

        // Stack another commit on top of unapproved secret commit
        std::fs::write(dev.join("stacked.txt"), "stacked\n").unwrap();
        run_cmd(&["git", "-C", dev.to_str().unwrap(), "add", "stacked.txt"], None, 10, None).await;
        run_cmd(&["git", "-C", dev.to_str().unwrap(), "commit", "-m", "Stacked commit"], None, 10, None).await;
        run_cmd(&["git", "-C", dev.to_str().unwrap(), "push", "-f", "origin", "HEAD:refs/g5/incoming/alice"], None, 30, None).await;

        let p3 = poll(&st.db, &repo, None, "Poll", 10).await;
        assert_eq!(p3["created"][0]["blocked"], json!(true), "stacked CR must be blocked! {}", p3);
        let cr3_id = p3["created"][0]["id"].as_i64().unwrap();
        let cr3 = cr_t.get(&st.db, cr3_id).await.unwrap();
        let app3 = approve(&st.db, &repo, &cr3, Some(&row! {"id"=>2, "name"=>"Bob"}), 10).await;
        assert_eq!(app3["status"], json!("Blocked"));
        assert!(cr2_id > 0);

        // 8. Browse tree, file, log, commit
        let tr = browse_tree(&repo, &pub_rev, None).await;
        assert_eq!(tr["entries"].as_array().unwrap().len(), 1);
        let fl = browse_file(&repo, &pub_rev, Some("hello.txt"), 10000).await;
        assert_eq!(fl["text"], json!("hello world\n"));
        let lg = browse_log(&repo, &pub_rev, None, 10, 0).await;
        assert_eq!(lg["list"].as_array().unwrap().len(), 1);
        let cm = browse_commit(&repo, &pub_rev, 10000).await;
        assert!(cm["diff"].as_str().unwrap().contains("+hello world"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn full_lifecycle_mercurial_including_phase_leak_and_conflict() {
        if run_cmd(&["hg", "--version"], None, 10, None).await.rc != 0 {
            return;
        }
        let (st, tmp) = setup_test_env().await;
        if Tracker::load(&st.db, "scm", "scm_repository").await.is_none() {
            let _ = std::fs::remove_dir_all(&tmp);
            return;
        }
        let root = tmp.join("hgrepos");
        std::fs::create_dir_all(&root).unwrap();

        // 1. Provision empty Mercurial repository
        let prov = provision(&st.db, &row! {
            "name" => "Demo Hg", "repo_key" => "demo-hg", "vcs_type" => "Mercurial",
            "base_path" => root.to_string_lossy().to_string(), "hook_url_base" => "",
        }, None, "").await;
        assert_eq!(prov["ok"], json!(true), "hg provision failed: {}", prov);
        let repo = prov["repo"].as_object().unwrap().clone();
        let rpath = s(&repo, "repo_path");

        // 2. Developer clones, commits, pushes -> hook forces changeset to secret phase
        let dev1 = tmp.join("dev1");
        let dev1_s = dev1.to_str().unwrap();
        let cl1 = run_cmd(&["hg", "clone", &rpath, dev1_s], None, 60, None).await;
        assert_eq!(cl1.rc, 0, "hg clone failed: {} {}", cl1.out, cl1.err);
        std::fs::write(dev1.join("app.txt"), "line1\n").unwrap();
        let ad1 = run_cmd(&["hg", "add", "app.txt"], Some(dev1_s), 10, None).await;
        assert_eq!(ad1.rc, 0, "hg add failed: {} {}", ad1.out, ad1.err);
        let cm1 = run_cmd(&["hg", "commit", "-u", "Alice <alice@example.com>", "-m", "First hg commit"], Some(dev1_s), 10, None).await;
        assert_eq!(cm1.rc, 0, "hg commit failed: {} {}", cm1.out, cm1.err);
        let ps1 = run_cmd(&["hg", "push"], Some(dev1_s), 30, None).await;
        assert_eq!(ps1.rc, 0, "hg push failed: {} {}", ps1.out, ps1.err);
        assert!(published_heads(&repo).await.0.is_empty(), "pushed hg commit must be quarantined (secret)!");
        assert_eq!(quarantined_heads(&repo).await.0.len(), 1);

        // 3. Poll & approve -> seeds mainline + writes .hg/g5mainline
        let p1 = poll(&st.db, &repo, None, "Poll", 10).await;
        let cr1_id = p1["created"][0]["id"].as_i64().unwrap();
        let cr_t = Tracker::load(&st.db, "scm", "scm_change").await.unwrap();
        let cr1 = cr_t.get(&st.db, cr1_id).await.unwrap();
        let app1 = approve(&st.db, &repo, &cr1, Some(&row! {"id"=>2, "name"=>"Bob"}), 10).await;
        assert_eq!(app1["ok"], json!(true), "hg approve 1 failed: {}", app1);
        assert_eq!(published_heads(&repo).await.0.len(), 1);
        assert!(Path::new(&rpath).join(".hg/g5mainline").is_file());
        cr_t.save(&st.db, &row! {"id"=>cr1_id, "record_status"=>"Merged"}).await.unwrap();

        // 4. Phase-leak regression + canary secret isolation:
        //    client locally runs `hg phase -p` and pushes -> MUST STAY SECRET and unsearchable!
        std::fs::write(dev1.join("app.txt"), "line1\nfrom_dev1\nHG_CANARY_SECRET_777\n").unwrap();
        let cm2 = run_cmd(&["hg", "commit", "-u", "Alice <alice@example.com>", "-m", "Dev1 change"], Some(dev1_s), 10, None).await;
        assert_eq!(cm2.rc, 0, "hg commit 2 failed: {}", cm2.err);
        run_cmd(&["hg", "phase", "-p", "-r", "."], Some(dev1_s), 10, None).await;
        let ps2 = run_cmd(&["hg", "push"], Some(dev1_s), 30, None).await;
        assert_eq!(ps2.rc, 0, "hg push 2 failed: {} {}", ps2.out, ps2.err);
        assert_eq!(published_heads(&repo).await.0.len(), 1, "phase-leak regression: client-published commit leaked to public!");
        assert_eq!(quarantined_heads(&repo).await.0.len(), 1);

        let pub_rev = published_heads(&repo).await.0[0].clone();
        let s_hist = browse_search(&repo, &pub_rev, "HG_CANARY_SECRET_777", "history", 50, 1000).await;
        assert!(s_hist["list"].as_array().unwrap().is_empty(), "quarantined hg secret leaked in history search!");
        let s_cont = browse_search(&repo, &pub_rev, "HG_CANARY_SECRET_777", "content", 50, 1000).await;
        assert!(s_cont["list"].as_array().unwrap().is_empty(), "quarantined hg secret leaked in content search!");

        // Browse published hg repo (tree, file, file log, commit)
        let tr = browse_tree(&repo, &pub_rev, None).await;
        assert_eq!(tr["entries"].as_array().unwrap().len(), 1);
        let fl = browse_file(&repo, &pub_rev, Some("app.txt"), 10000).await;
        assert_eq!(fl["text"], json!("line1\n"));
        let lg = browse_log(&repo, &pub_rev, Some("app.txt"), 10, 0).await;
        assert_eq!(lg["list"].as_array().unwrap().len(), 1);
        let cm = browse_commit(&repo, &pub_rev, 10000).await;
        assert!(cm["diff"].as_str().unwrap().contains("+line1"));

        // 5. Conflict test: dev2 clones mainline (sees only commit 1!), makes conflicting edit, pushes
        let dev2 = tmp.join("dev2");
        let dev2_s = dev2.to_str().unwrap();
        let cl2 = run_cmd(&["hg", "clone", &rpath, dev2_s], None, 60, None).await;
        assert_eq!(cl2.rc, 0, "hg clone 2 failed: {} {}", cl2.out, cl2.err);
        std::fs::write(dev2.join("app.txt"), "line1\nfrom_dev2_conflict\n").unwrap();
        let cm3 = run_cmd(&["hg", "commit", "-u", "Carol <carol@example.com>", "-m", "Dev2 conflicting change"], Some(dev2_s), 10, None).await;
        assert_eq!(cm3.rc, 0, "hg commit 3 failed: {}", cm3.err);
        let ps3 = run_cmd(&["hg", "push", "-f"], Some(dev2_s), 30, None).await;
        assert_eq!(ps3.rc, 0, "hg push 3 failed: {} {}", ps3.out, ps3.err);

        // Poll picks up both; approve dev1 first, then dev2 must hit Conflict and leave mainline untouched!
        let p2 = poll(&st.db, &repo, None, "Poll", 10).await;
        let crs = p2["created"].as_array().unwrap();
        assert_eq!(crs.len(), 2, "expected 2 CRs: {}", p2);
        let id_a = crs[0]["id"].as_i64().unwrap();
        let id_b = crs[1]["id"].as_i64().unwrap();
        let cra = cr_t.get(&st.db, id_a).await.unwrap();
        let crb = cr_t.get(&st.db, id_b).await.unwrap();

        let app_a = approve(&st.db, &repo, &cra, Some(&row! {"id"=>2, "name"=>"Bob"}), 10).await;
        assert_eq!(app_a["ok"], json!(true), "first concurrent approve failed: {}", app_a);
        let pub_before = published_heads(&repo).await.0;

        let app_b = approve(&st.db, &repo, &crb, Some(&row! {"id"=>2, "name"=>"Bob"}), 10).await;
        assert_eq!(app_b["ok"], json!(false));
        assert_eq!(app_b["status"], json!("Conflict"), "expected Conflict: {}", app_b);
        assert_eq!(app_b["conflicts"], json!(["app.txt"]));
        assert_eq!(published_heads(&repo).await.0, pub_before, "mainline must stay byte-identical after conflict!");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn scm_pages_and_scripts_end_to_end() {
        // Unoptimized debug builds give Rhai/MiniJinja large stack frames; run the
        // full multi-step async test with an 8 MB thread stack so `cargo test` passes
        // without requiring RUST_MIN_STACK.
        let handle = std::thread::Builder::new()
            .name("scm_e2e".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_stack_size(8 * 1024 * 1024)
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(scm_pages_and_scripts_end_to_end_inner());
            })
            .unwrap();
        handle.join().unwrap();
    }

    async fn scm_pages_and_scripts_end_to_end_inner() {
        if run_cmd(&["git", "--version"], None, 10, None).await.rc != 0 {
            return;
        }
        let (st, tmp) = setup_test_env().await;
        if Tracker::load(&st.db, "scm", "scm_repository").await.is_none() {
            let _ = std::fs::remove_dir_all(&tmp);
            return;
        }
        // Create two users: pusher (Alice, id=1) and checker (Bob, id=2, Admin)
        let alice_id = st.db.insert("portal_user", &row! {
            "userid"=>"alice", "name"=>"Alice Dev", "email"=>"alice@example.com",
            "password"=>"", "is_active"=>true, "is_admin"=>false
        }).await.unwrap();
        let bob_id = st.db.insert("portal_user", &row! {
            "userid"=>"bob", "name"=>"Bob Checker", "email"=>"bob@example.com",
            "password"=>"", "is_active"=>true, "is_admin"=>true
        }).await.unwrap();
        st.db.insert("user_role", &row! {"user_id"=>alice_id, "module"=>"scm", "role"=>"Approver"}).await.unwrap();
        st.db.insert("user_role", &row! {"user_id"=>bob_id, "module"=>"scm", "role"=>"Admin"}).await.unwrap();
        let alice = st.db.get("portal_user", alice_id).await.unwrap().unwrap();
        let bob = st.db.get("portal_user", bob_id).await.unwrap().unwrap();

        let root = tmp.join("web_repos");
        std::fs::create_dir_all(&root).unwrap();

        let render = |user: &Row, slug: &str, params: Row, extra: Row| {
            let st = st.clone();
            let u = user.clone();
            let slug = slug.to_string();
            Box::pin(async move {
                let page = crate::page::find_page(&st.db, "scm", &slug).await.expect(&format!("page {}", slug));
                let sc = crate::script::Sc::new(st.clone(), Some(u), params, "tok".into(), format!("/view/scm/{}", slug));
                crate::page::render_page(&sc, &page, extra).await.expect(&format!("render {}", slug))
            })
        };

        // 1. Render dashboard (empty), setup, and POST do_setup (creates Git repo)
        let (_, dash0) = render(&bob, "dashboard", Row::new(), Row::new()).await;
        assert!(dash0.contains("Source Code Management"));
        let (_, setup_html) = render(&bob, "setup", Row::new(), Row::new()).await;
        assert!(setup_html.contains("csrf_token"));

        let (_, do_setup_html) = render(&bob, "do_setup", row! {
            "name"=>"Payments", "repo_key"=>"payments", "vcs_type"=>"Git",
            "mode"=>"empty", "base_path"=>root.to_string_lossy().to_string(),
        }, Row::new()).await;
        assert!(do_setup_html.contains("Payments is ready"), "do_setup: {}", do_setup_html);

        let repo_t = Tracker::load(&st.db, "scm", "scm_repository").await.unwrap();
        let repo = repo_t.first_row(&st.db, Some(&row! {"repo_key"=>"payments"}), None).await.unwrap().unwrap();
        let repo_id = i(&repo, "id").unwrap();
        let rpath = s(&repo, "repo_path");
        let hook_tok = s(&repo, "hook_token");
        assert!(!hook_tok.is_empty());

        // 2. Push a commit as Alice -> trigger /run/scm/hook with bad token (refused) and good token (creates CR)
        let dev = tmp.join("dev_payments");
        run_cmd(&["git", "clone", &rpath, dev.to_str().unwrap()], None, 60, None).await;
        run_cmd(&["git", "-C", dev.to_str().unwrap(), "config", "user.name", "Alice Dev"], None, 10, None).await;
        run_cmd(&["git", "-C", dev.to_str().unwrap(), "config", "user.email", "alice@example.com"], None, 10, None).await;
        run_cmd(&["git", "-C", dev.to_str().unwrap(), "checkout", "-b", "main"], None, 10, None).await;
        std::fs::write(dev.join("README.md"), "# Payments\nhello\n").unwrap();
        run_cmd(&["git", "-C", dev.to_str().unwrap(), "add", "README.md"], None, 10, None).await;
        run_cmd(&["git", "-C", dev.to_str().unwrap(), "commit", "-m", "Add README"], None, 10, None).await;
        let psh = run_cmd(&["git", "-C", dev.to_str().unwrap(), "push", "origin", "HEAD:refs/g5/incoming/alice"], None, 30, None).await;
        assert_eq!(psh.rc, 0, "push failed: {}", psh.err);

        let hook_page = crate::page::find_page(&st.db, "scm", "hook").await.unwrap();
        let sc_bad = crate::script::Sc::new(st.clone(), None, row! {"key"=>"payments", "token"=>"wrong"}, "".into(), "/run/scm/hook".into());
        assert_eq!(sc_bad.eval(&s(&hook_page, "content"), &Row::new()).unwrap(), json!("refused\n"));

        let sc_ok = crate::script::Sc::new(st.clone(), None, row! {"key"=>"payments", "token"=>hook_tok}, "".into(), "/run/scm/hook".into());
        let hook_out = sc_ok.eval(&s(&hook_page, "content"), &Row::new()).unwrap();
        assert!(hook_out.as_str().unwrap().starts_with("ok: Raised 1 change request"), "hook_out: {}", hook_out);

        // 3. Check validation gate & segregation of duties:
        //    - Alice (pusher) cannot validate or approve her own push.
        //    - Bob (approver) cannot approve before at least 1 validation.
        //    - Multiple other users (Carol & Dave) can validate once each.
        //    - Once validated, Bob can approve and merge into main.
        let carol_id = st.db.insert("portal_user", &row! {"userid"=>"carol", "name"=>"Carol User", "email"=>"carol@example.com", "is_active"=>true, "is_admin"=>false}).await.unwrap();
        let dave_id = st.db.insert("portal_user", &row! {"userid"=>"dave", "name"=>"Dave User", "email"=>"dave@example.com", "is_active"=>true, "is_admin"=>false}).await.unwrap();
        st.db.insert("user_role", &row! {"user_id"=>carol_id, "module"=>"scm", "role"=>"User"}).await.unwrap();
        st.db.insert("user_role", &row! {"user_id"=>dave_id, "module"=>"scm", "role"=>"User"}).await.unwrap();
        let carol = st.db.first("select * from portal_user where id=:id", &row! {"id"=>carol_id}).await.unwrap().unwrap();
        let dave = st.db.first("select * from portal_user where id=:id", &row! {"id"=>dave_id}).await.unwrap().unwrap();

        let cr_t = Tracker::load(&st.db, "scm", "scm_change").await.unwrap();
        let cr = cr_t.first_row(&st.db, Some(&row! {"repository_id"=>repo_id}), None).await.unwrap().unwrap();
        let cr_id = i(&cr, "id").unwrap();
        assert_eq!(i(&cr, "pushed_by"), Some(alice_id), "Alice should be mapped by email");

        let validate_tr = cr_t.transitions.iter().find(|t| t.name == "Validate").unwrap();
        let approve_tr = cr_t.transitions.iter().find(|t| t.name == "Approve").unwrap();
        let sc_alice = crate::script::Sc::new(st.clone(), Some(alice.clone()), Row::new(), "t".into(), "/".into());
        let sc_bob = crate::script::Sc::new(st.clone(), Some(bob.clone()), Row::new(), "t".into(), "/".into());
        let sc_carol = crate::script::Sc::new(st.clone(), Some(carol.clone()), Row::new(), "t".into(), "/".into());
        let sc_dave = crate::script::Sc::new(st.clone(), Some(dave.clone()), Row::new(), "t".into(), "/".into());

        // Before validation: pusher cannot validate/approve; Bob cannot approve yet; Carol can validate.
        assert!(!cr_t.test_enabled(&sc_alice, validate_tr, Some(&cr)).await, "pusher must not validate own push!");
        assert!(!cr_t.test_enabled(&sc_alice, approve_tr, Some(&cr)).await, "pusher must not approve own push!");
        assert!(!cr_t.test_enabled(&sc_bob, approve_tr, Some(&cr)).await, "approver must not approve before validation!");
        assert!(cr_t.test_enabled(&sc_carol, validate_tr, Some(&cr)).await, "another user must be allowed to validate!");

        // Server-side post_scm_approve also refuses unvalidated CR
        let post_app = crate::page::find_page(&st.db, "scm", "post_scm_approve").await.unwrap();
        sc_bob.eval(&s(&post_app, "content"), &row! {"datas"=>Value::Object(cr.clone())}).expect("post_scm_approve unvalidated");
        assert_eq!(s(&cr_t.get(&st.db, cr_id).await.unwrap(), "record_status"), "Pending Review");

        // 4a. Carol validates -> status becomes Validated (1), Carol can no longer validate again, Dave still can!
        let post_val = crate::page::find_page(&st.db, "scm", "post_scm_validate").await.unwrap();
        let mut cr_v1_in = cr_t.get(&st.db, cr_id).await.unwrap();
        cr_v1_in.insert("validation_remarks".into(), json!("Checked locally, LGTM"));
        sc_carol.eval(&s(&post_val, "content"), &row! {"datas"=>Value::Object(cr_v1_in)}).expect("post_scm_validate carol");
        let cr_v1 = cr_t.get(&st.db, cr_id).await.unwrap();
        assert_eq!(s(&cr_v1, "record_status"), "Validated");
        assert_eq!(i(&cr_v1, "validation_count"), Some(1));
        assert!(!cr_t.test_enabled(&sc_carol, validate_tr, Some(&cr_v1)).await, "same user must not validate twice!");
        assert!(cr_t.test_enabled(&sc_dave, validate_tr, Some(&cr_v1)).await, "second user must still be able to validate!");
        assert!(cr_t.test_enabled(&sc_bob, approve_tr, Some(&cr_v1)).await, "approver must be allowed to approve after >=1 validation!");

        // 4b. Dave also validates -> validation_count becomes 2
        let mut cr_v2_in = cr_v1.clone();
        cr_v2_in.insert("validation_remarks".into(), json!("Second pair of eyes ok"));
        sc_dave.eval(&s(&post_val, "content"), &row! {"datas"=>Value::Object(cr_v2_in)}).expect("post_scm_validate dave");
        let cr_v2 = cr_t.get(&st.db, cr_id).await.unwrap();
        assert_eq!(s(&cr_v2, "record_status"), "Validated");
        assert_eq!(i(&cr_v2, "validation_count"), Some(2));
        assert!(!cr_t.test_enabled(&sc_alice, approve_tr, Some(&cr_v2)).await, "pusher still cannot approve own push!");

        // 4c. Run post_scm_approve as Bob -> settles CR to Merged
        sc_bob.eval(&s(&post_app, "content"), &row! {"datas"=>Value::Object(cr_v2.clone())}).expect("post_scm_approve");
        let cr_after = cr_t.get(&st.db, cr_id).await.unwrap();
        assert_eq!(s(&cr_after, "record_status"), "Merged");

        // 5. Render all custom templates & explorer views (tree, file, log, commit, search)
        let (_, dash1) = render(&bob, "dashboard", Row::new(), Row::new()).await;
        assert!(dash1.contains("Payments"));
        let (_, rshow) = render(&bob, "scm_repository_show_default", row! {"id"=>repo_id}, row! {"datas"=>Value::Object(repo.clone())}).await;
        assert!(rshow.contains("Payments"));
        let (_, cshow) = render(&bob, "scm_change_show_default", row! {"id"=>cr_id}, row! {"datas"=>Value::Object(cr_after.clone())}).await;
        assert!(cshow.contains("Add README"));
        assert!(cshow.contains("Carol User") && cshow.contains("Dave User"), "change page should list both validators");

        let cs_t = Tracker::load(&st.db, "scm", "scm_changeset").await.unwrap();
        let cs = cs_t.first_row(&st.db, Some(&row! {"change_id"=>cr_id}), None).await.unwrap().unwrap();
        let (_, csshow) = render(&bob, "scm_changeset_show_default", row! {"id"=>i(&cs, "id").unwrap()}, row! {"datas"=>Value::Object(cs)}).await;
        assert!(csshow.contains("README.md"));

        for view in ["tree", "log", "commit"] {
            let (_, html) = render(&bob, "explore", row! {"repo"=>"payments", "view"=>view}, Row::new()).await;
            assert!(html.contains("Payments"), "explore view={} failed", view);
            if view == "tree" || view == "commit" {
                assert!(html.contains("bi-filetype-md"), "expected markdown icon in explore view={}", view);
            }
        }
        let (_, fhtml) = render(&bob, "explore", row! {"repo"=>"payments", "view"=>"file", "path"=>"README.md"}, Row::new()).await;
        assert!(fhtml.contains("# Payments"));
        assert!(fhtml.contains("bi-filetype-md"));
        for mode in ["content", "messages", "history"] {
            let (_, shtml) = render(&bob, "explore", row! {"repo"=>"payments", "view"=>"search", "q"=>"hello", "mode"=>mode}, Row::new()).await;
            assert!(shtml.contains("Payments"), "explore search mode={} failed", mode);
        }

        // 6. Run poll_all, post_scm_repo_verify, post_scm_repo_hooks, post_scm_repo_poll
        let poll_all = crate::page::find_page(&st.db, "scm", "poll_all").await.unwrap();
        let pa_out = sc_bob.eval(&s(&poll_all, "content"), &Row::new()).unwrap();
        assert!(pa_out.as_str().unwrap().contains("1 repository(ies) polled"));

        for slug in ["post_scm_repo_verify", "post_scm_repo_hooks", "post_scm_repo_poll"] {
            let pg = crate::page::find_page(&st.db, "scm", slug).await.unwrap();
            let cur_repo = repo_t.get(&st.db, repo_id).await.unwrap();
            sc_bob.eval(&s(&pg, "content"), &row! {"datas"=>Value::Object(cur_repo)}).expect(slug);
        }

        // 7. Custom error_function checks on scm_repository (notify_emails & repo_path)
        let em_field = repo_t.field("notify_emails").unwrap();
        let errs_bad = crate::tags::field_errors(&sc_bob, &repo_t, em_field, &row! {"notify_emails"=>"not-an-email"}).await;
        assert!(!errs_bad.is_empty(), "invalid email should fail validation");
        let errs_ok = crate::tags::field_errors(&sc_bob, &repo_t, em_field, &row! {"notify_emails"=>"a@b.com, c@d.com"}).await;
        assert!(errs_ok.is_empty(), "valid emails should pass validation: {:?}", errs_ok);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn file_icons_and_types() {
        assert!(file_icon_html("src", true).contains("bi-folder-fill"));
        assert_eq!(file_type_label("src", true), "Folder");
        assert!(file_icon_html("Main.java", false).contains("bi-filetype-java"));
        assert_eq!(file_type_label("Main.java", false), "JAVA");
        assert!(file_icon_html("app/views/index.gsp", false).contains("bi-filetype-html"));
        assert!(file_icon_html("src/endpoint.rs", false).contains("bi-file-earmark-code"));
        assert!(file_icon_html("hgweb.cgi", false).contains("bi-filetype-py"));
        assert!(file_icon_html("Dockerfile", false).contains("bi-file-earmark-code"));
        assert_eq!(file_type_label("Dockerfile", false), "File");
        assert!(file_icon_html(".gitignore", false).contains("bi-file-earmark-ruled"));
        assert_eq!(file_type_label(".gitignore", false), "File");
        assert!(file_icon_html("archive.zip", false).contains("bi-file-earmark-zip"));
        assert!(file_icon_html("report.xlsx", false).contains("bi-filetype-xlsx"));
        assert!(file_icon_html("logo.png", false).contains("bi-filetype-png"));
        assert!(file_icon_html("unknown.xyz123", false).contains("bi-file-earmark"));
    }
}
