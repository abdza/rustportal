//! Active Directory (LDAP / LDAPS) authentication, LAN ID synchronization,
//! and bulk staff lookup — matching g5portal's `Credentials.groovy`,
//! `UserController.updatelanid`, `User.adLookup`, and `PortalEndpointController`
//! Basic auth caching.
//!
//! Configuration comes from `portal_setting` (using the exact same keys as
//! g5portal: `adenable`, `enforce_lanid`, `adtimeout`, `adserver`/`adport`/
//! `adsecure`/`addn`/`adpassword`/`adbase`, and `adserver2`..`adbase2` for the
//! secondary domain) with optional defaults in `config.toml` under
//! `[active_directory]` and `disable_lanid`.

use crate::db::{b, i, s, Db, Row};
use crate::row;
use crate::web::{self, St};
use anyhow::{anyhow, Result};
use ldap3::{LdapConnAsync, LdapConnSettings, Scope, SearchEntry};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct AdDomainCfg {
    pub label: String,
    #[serde(alias = "host", alias = "adserver")]
    pub server: String,
    #[serde(alias = "adport")]
    pub port: u16,
    #[serde(alias = "adsecure")]
    pub secure: bool,
    #[serde(alias = "dn", alias = "addn")]
    pub bind_dn: String,
    #[serde(alias = "password", alias = "adpassword")]
    pub bind_password: String,
    #[serde(alias = "base", alias = "adbase")]
    pub base_dn: String,
}

impl Default for AdDomainCfg {
    fn default() -> Self {
        AdDomainCfg {
            label: "primary".into(),
            server: String::new(),
            port: 636,
            secure: true,
            bind_dn: String::new(),
            bind_password: String::new(),
            base_dn: String::new(),
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ActiveDirectoryCfg {
    pub enabled: bool,
    pub enforce_lanid: bool,
    pub timeout_ms: u64,
    /// Accept internal corporate / self-signed LDAPS certificates (matching
    /// Apache Directory API's default trust behavior in g5portal).
    pub tls_no_verify: bool,
    pub domains: Vec<AdDomainCfg>,
}

impl Default for ActiveDirectoryCfg {
    fn default() -> Self {
        ActiveDirectoryCfg {
            enabled: false,
            enforce_lanid: false,
            timeout_ms: 5000,
            tls_no_verify: true,
            domains: vec![],
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedAd {
    pub disable_lanid: bool,
    pub enabled: bool,
    pub enforce_lanid: bool,
    pub timeout_ms: u64,
    pub tls_no_verify: bool,
    pub domains: Vec<AdDomainCfg>,
}

#[derive(Clone, Debug)]
pub struct AuthOutcome {
    pub authenticated: bool,
    pub via_ad: bool,
    /// True when `enforce_lanid` is active for this user (has a LAN ID and is
    /// not marked `lanidexception`), so login hints should direct them to use
    /// their LAN ID password.
    pub enforce_lanid_active: bool,
}

fn truthy_setting(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_i64().map(|x| x != 0).unwrap_or(false),
        Value::String(s) => matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        _ => false,
    }
}

async fn setting_bool_opt(db: &Db, name: &str) -> Option<bool> {
    web::setting(db, name).await.map(|v| truthy_setting(&v))
}

async fn setting_u64_opt(db: &Db, name: &str) -> Option<u64> {
    web::setting(db, name).await.and_then(|v| match v {
        Value::Number(n) => n.as_u64().or_else(|| n.as_i64().filter(|x| *x > 0).map(|x| x as u64)),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    })
}

/// Derive a domain search base DN from a service account bind DN's `DC=...` suffix,
/// e.g. `CN=svc,OU=LdapService,DC=CIMBMY,DC=CIMBDomain,DC=com` -> `DC=CIMBMY,DC=CIMBDomain,DC=com`.
fn derive_base_dn(bind_dn: &str) -> Option<String> {
    let parts: Vec<&str> = bind_dn.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
    let idx = parts.iter().position(|p| p.len() >= 3 && p[..3].eq_ignore_ascii_case("dc="))?;
    let tail = parts[idx..].join(",");
    if tail.is_empty() { None } else { Some(tail) }
}

/// If `base_dn` was not explicitly configured and could not be derived from `bind_dn`,
/// query Active Directory's RootDSE for `defaultNamingContext`.
async fn effective_base_dn(ldap: &mut ldap3::Ldap, dom: &AdDomainCfg) -> Result<String> {
    if !dom.base_dn.trim().is_empty() {
        return Ok(dom.base_dn.trim().to_string());
    }
    if let Some(derived) = derive_base_dn(&dom.bind_dn) {
        return Ok(derived);
    }
    let (entries, _) = ldap
        .search("", Scope::Base, "(objectClass=*)", vec!["defaultNamingContext"])
        .await
        .map_err(|e| anyhow!("RootDSE query failed: {}", e))?
        .non_error()
        .map_err(|e| anyhow!("RootDSE query rejected: {}", e))?;
    for raw in entries {
        if raw.is_ref() || raw.is_intermediate() {
            continue;
        }
        let entry = SearchEntry::construct(raw);
        if let Some(nc) = first_attr(&entry, "defaultNamingContext") {
            return Ok(nc);
        }
    }
    Err(anyhow!("No AD search base DN configured (set adbase / base_dn) and RootDSE returned no defaultNamingContext"))
}

async fn resolve_domain(
    db: &Db,
    fallback: Option<&AdDomainCfg>,
    default_label: &str,
    k_server: &str,
    k_port: &str,
    k_secure: &str,
    k_dn: &str,
    k_pw: &str,
    k_base: &str,
) -> Option<AdDomainCfg> {
    let fb = fallback.cloned().unwrap_or_else(|| AdDomainCfg {
        label: default_label.into(),
        ..Default::default()
    });
    let server = web::setting_str(db, k_server).await.unwrap_or_else(|| fb.server.clone());
    if server.trim().is_empty() {
        return None;
    }
    let port = setting_u64_opt(db, k_port).await.map(|p| p as u16).unwrap_or(if fb.port == 0 { 636 } else { fb.port });
    let secure = setting_bool_opt(db, k_secure).await.unwrap_or(fb.secure);
    let bind_dn = web::setting_str(db, k_dn).await.unwrap_or_else(|| fb.bind_dn.clone());
    let bind_password = web::setting_str(db, k_pw).await.unwrap_or_else(|| fb.bind_password.clone());
    let mut base_dn = web::setting_str(db, k_base).await.unwrap_or_else(|| fb.base_dn.clone());
    if base_dn.trim().is_empty() {
        if let Some(derived) = derive_base_dn(&bind_dn) {
            base_dn = derived;
        }
    }
    let label = if fb.label.trim().is_empty() { default_label.into() } else { fb.label };
    Some(AdDomainCfg {
        label,
        server: server.trim().to_string(),
        port,
        secure,
        bind_dn: bind_dn.trim().to_string(),
        bind_password,
        base_dn: base_dn.trim().to_string(),
    })
}

/// Resolve effective AD settings by overlaying live `portal_setting` entries
/// over `config.toml`'s `[active_directory]` defaults.
pub async fn resolve_settings(st: &St) -> ResolvedAd {
    let db = &st.db;
    let ac = &st.cfg.active_directory;
    let enabled = setting_bool_opt(db, "adenable").await.unwrap_or(ac.enabled);
    let enforce_lanid = setting_bool_opt(db, "enforce_lanid").await.unwrap_or(ac.enforce_lanid);
    let timeout_ms = setting_u64_opt(db, "adtimeout").await.unwrap_or(ac.timeout_ms).clamp(500, 60_000);
    let tls_no_verify = setting_bool_opt(db, "ad_tls_no_verify").await.unwrap_or(ac.tls_no_verify);

    let mut domains = vec![];
    if let Some(d1) = resolve_domain(
        db,
        ac.domains.first(),
        "primary",
        "adserver",
        "adport",
        "adsecure",
        "addn",
        "adpassword",
        "adbase",
    )
    .await
    {
        domains.push(d1);
    }
    if let Some(d2) = resolve_domain(
        db,
        ac.domains.get(1),
        "secondary",
        "adserver2",
        "adport2",
        "adsecure2",
        "addn2",
        "adpassword2",
        "adbase2",
    )
    .await
    {
        domains.push(d2);
    }
    for extra in ac.domains.iter().skip(2) {
        if !extra.server.trim().is_empty() {
            domains.push(extra.clone());
        }
    }

    ResolvedAd {
        disable_lanid: st.cfg.disable_lanid,
        enabled,
        enforce_lanid,
        timeout_ms,
        tls_no_verify,
        domains,
    }
}

/// Escape an LDAP search filter value per RFC 4515 so user input can never
/// alter the filter syntax.
pub fn escape_ldap_filter(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for c in input.chars() {
        match c {
            '\\' => out.push_str("\\5c"),
            '*' => out.push_str("\\2a"),
            '(' => out.push_str("\\28"),
            ')' => out.push_str("\\29"),
            '\0' => out.push_str("\\00"),
            c => out.push(c),
        }
    }
    out
}

async fn connect_ldap(dom: &AdDomainCfg, timeout_ms: u64, tls_no_verify: bool) -> Result<ldap3::Ldap> {
    let scheme = if dom.secure { "ldaps" } else { "ldap" };
    let url = format!("{}://{}:{}", scheme, dom.server.trim(), dom.port);
    let settings = LdapConnSettings::new()
        .set_conn_timeout(Duration::from_millis(timeout_ms))
        .set_no_tls_verify(tls_no_verify);
    let (conn, mut ldap) = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        LdapConnAsync::with_settings(settings, &url),
    )
    .await
    .map_err(|_| anyhow!("Timed out connecting to {} after {} ms", url, timeout_ms))?
    .map_err(|e| anyhow!("LDAP connect {} failed: {}", url, e))?;
    ldap3::drive!(conn);
    ldap.with_timeout(Duration::from_millis(timeout_ms));
    Ok(ldap)
}

async fn bind_as_domain(
    db: &Db,
    user: &Row,
    lanid: &str,
    password: &str,
    tag: &str,
    dom: &AdDomainCfg,
    timeout_ms: u64,
    tls_no_verify: bool,
) -> bool {
    let staffid = s(user, "userid");
    let uid = i(user, "id");
    eprintln!(
        "{} ({}): connecting to {}:{} (ssl={})",
        tag, dom.label, dom.server, dom.port, dom.secure
    );
    let mut ldap = match connect_ldap(dom, timeout_ms, tls_no_verify).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{} ({}): error connecting - {}", tag, dom.label, e);
            web::log_error(
                db,
                "user",
                "authenticate",
                &format!("Error connecting to ldap server {}: {}", dom.label, e),
                None,
                None,
                uid,
            )
            .await;
            return false;
        }
    };

    let result = async {
        // 1. Bind as service account
        ldap.simple_bind(&dom.bind_dn, &dom.bind_password)
            .await
            .map_err(|e| anyhow!("service bind transport error: {}", e))?
            .success()
            .map_err(|e| anyhow!("service bind rejected for {}: {}", dom.bind_dn, e))?;
        eprintln!("{} ({}): bound as service account {}", tag, dom.label, dom.bind_dn);

        // 2. Search for user DN by sAMAccountName
        let base_dn = effective_base_dn(&mut ldap, dom).await?;
        let filter = format!("(sAMAccountName={})", escape_ldap_filter(lanid));
        eprintln!("{} ({}): searching base {} filter {}", tag, dom.label, base_dn, filter);
        let (entries, _res) = ldap
            .search(&base_dn, Scope::Subtree, &filter, vec!["dn", "sAMAccountName"])
            .await
            .map_err(|e| anyhow!("search transport error: {}", e))?
            .non_error()
            .map_err(|e| anyhow!("search failed: {}", e))?;

        let mut found = false;
        for raw_entry in entries {
            // Skip AD domain-root referrals (DomainDnsZones, ForestDnsZones, Configuration)
            if raw_entry.is_ref() || raw_entry.is_intermediate() {
                continue;
            }
            let entry = SearchEntry::construct(raw_entry);
            found = true;
            eprintln!("{} ({}): found entry {}, attempting user bind", tag, dom.label, entry.dn);
            match ldap.simple_bind(&entry.dn, password).await {
                Ok(bind_res) if bind_res.rc == 0 => {
                    eprintln!("{} ({}): succeeded for {} ({})", tag, dom.label, staffid, lanid);
                    return Ok(true);
                }
                Ok(bind_res) => {
                    eprintln!(
                        "{} ({}): user bind failed for {} - rc={} {}",
                        tag, dom.label, lanid, bind_res.rc, bind_res.text
                    );
                }
                Err(e) => {
                    eprintln!("{} ({}): user bind error for {} - {}", tag, dom.label, lanid, e);
                }
            }
        }
        if !found {
            eprintln!("{} ({}): no entry found for filter {}", tag, dom.label, filter);
        }
        Ok::<bool, anyhow::Error>(false)
    }
    .await;

    let _ = ldap.unbind().await;
    match result {
        Ok(ok) => ok,
        Err(e) => {
            eprintln!("{} ({}): error searching - {}", tag, dom.label, e);
            web::log_error(
                db,
                "user",
                "authenticate",
                &format!("Error connecting to ldap server {}: {}", dom.label, e),
                None,
                None,
                uid,
            )
            .await;
            false
        }
    }
}

/// The single shared password verification function for both `/login` and
/// `/svc/*` Basic authentication, matching g5portal's `Credentials.check`.
///
/// 1. If AD is enabled (`!disable_lanid && adenable`), the user has a `lanid`,
///    and at least one AD domain is configured, try each AD domain in order
///    (service bind -> search `(sAMAccountName=<lanid>)` -> user bind).
/// 2. If AD does not succeed, fall back to the account's local bcrypt password
///    hash unless `enforce_lanid` is enabled for a user who has a `lanid` (and
///    is not flagged `lanidexception`).
pub async fn check_credentials(st: &St, user: &Row, password: &str, via: &str) -> AuthOutcome {
    let staffid = s(user, "userid");
    let lanid = s(user, "lanid").trim().to_string();
    let lanid_exc = b(user, "lanidexception");
    if password.is_empty() {
        return AuthOutcome {
            authenticated: false,
            via_ad: false,
            enforce_lanid_active: false,
        };
    }

    let cfg = resolve_settings(st).await;
    let tag = if via == "login" {
        "AD login".to_string()
    } else {
        format!("AD check ({})", via)
    };
    let enforce_active = cfg.enforce_lanid && !lanid.is_empty() && !lanid_exc;

    if cfg.disable_lanid {
        eprintln!("{tag} for {staffid}: skipped - disable_lanid is true, local password only");
    } else if lanid.is_empty() {
        eprintln!("{tag} for {staffid}: skipped - no lanid on record (use Update LAN ID first)");
    } else if !cfg.enabled {
        eprintln!("{tag} for {staffid}: skipped - adenable setting is off");
    } else if cfg.domains.is_empty() {
        eprintln!("{tag} for {staffid}: skipped - no AD server configured (adserver / [active_directory])");
    } else {
        for (idx, dom) in cfg.domains.iter().enumerate() {
            if idx == 0 {
                eprintln!("{tag} for {staffid} (lanid {lanid}): attempting {} domain", dom.label);
            } else {
                eprintln!("{tag} for {staffid} (lanid {lanid}): previous domain did not succeed, attempting {} domain", dom.label);
            }
            if bind_as_domain(&st.db, user, &lanid, password, &tag, dom, cfg.timeout_ms, cfg.tls_no_verify).await {
                eprintln!("{tag} for {staffid}: succeeded via Active Directory");
                return AuthOutcome {
                    authenticated: true,
                    via_ad: true,
                    enforce_lanid_active: enforce_active,
                };
            }
        }
        eprintln!("{tag} for {staffid}: did not succeed via Active Directory, falling back to local password check");
    }

    // Local bcrypt password fallback (matching g5portal Credentials.groovy lines 68-70)
    let pw_hash = s(user, "password");
    let local_ok = !pw_hash.is_empty() && !enforce_active && bcrypt::verify(password, &pw_hash).unwrap_or(false);
    AuthOutcome {
        authenticated: local_ok,
        via_ad: false,
        enforce_lanid_active: enforce_active,
    }
}

// ---------------------------------------------------------------------------
// Endpoint Basic auth short-lived verification cache (5 minutes, SHA-256)
// matching g5portal's PortalEndpointController.recentlyVerified
// ---------------------------------------------------------------------------

const ENDPOINT_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

fn endpoint_cache() -> &'static Mutex<HashMap<[u8; 32], Instant>> {
    static CACHE: OnceLock<Mutex<HashMap<[u8; 32], Instant>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cred_hash(raw_userpass: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(raw_userpass.as_bytes());
    h.finalize().into()
}

pub async fn endpoint_basic_check(st: &St, user: &Row, raw_userpass: &str, password: &str, via: &str) -> bool {
    let key = cred_hash(raw_userpass);
    {
        let mut map = endpoint_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&at) = map.get(&key) {
            if at.elapsed() <= ENDPOINT_CACHE_TTL {
                return true;
            }
            map.remove(&key);
        }
    }
    let out = check_credentials(st, user, password, via).await;
    if out.authenticated {
        let mut map = endpoint_cache().lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > 2048 {
            map.retain(|_, at| at.elapsed() <= ENDPOINT_CACHE_TTL);
        }
        map.insert(key, Instant::now());
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// Update LAN ID for one user (`/user/updatelanid/{id}` & Rhai `update_lanid`)
// matching g5portal's `UserController.updatelanid` / `User.updatelanid`
// ---------------------------------------------------------------------------

/// Look up `user`'s `sAMAccountName` in Active Directory by `(employeeID=<userid>)`,
/// save it to `portal_user.lanid`, and return `Ok(Some(new_lanid))` when updated
/// or `Ok(None)` when not found in any configured domain.
pub async fn update_user_lanid(st: &St, user: &Row) -> Result<Option<String>> {
    let uid = i(user, "id").ok_or_else(|| anyhow!("User has no id"))?;
    let staffid = s(user, "userid").trim().to_string();
    let name = s(user, "name");
    if staffid.is_empty() {
        return Err(anyhow!("User has no userid/staffID"));
    }

    let cfg = resolve_settings(st).await;
    if cfg.domains.is_empty() {
        return Err(anyhow!("Active Directory is not configured (set adserver in Settings or [active_directory] in config.toml)"));
    }

    let filter = format!("(employeeID={})", escape_ldap_filter(&staffid));
    for dom in &cfg.domains {
        eprintln!(
            "Update LAN ID for {} ({}): attempting {} domain, filter {}",
            staffid, name, dom.label, filter
        );
        let mut ldap = match connect_ldap(dom, cfg.timeout_ms, cfg.tls_no_verify).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Update LAN ID ({}): error connecting - {}", dom.label, e);
                web::log_error(
                    &st.db,
                    "user",
                    "updatelanid",
                    &format!("Error connecting to {} ldap server: {}", dom.label, e),
                    None,
                    None,
                    Some(uid),
                )
                .await;
                continue;
            }
        };

        let found_lanid: Result<Option<String>> = async {
            ldap.simple_bind(&dom.bind_dn, &dom.bind_password)
                .await
                .map_err(|e| anyhow!("service bind transport error: {}", e))?
                .success()
                .map_err(|e| anyhow!("service bind rejected: {}", e))?;
            let base_dn = effective_base_dn(&mut ldap, dom).await?;
            let (entries, _res) = ldap
                .search(&base_dn, Scope::Subtree, &filter, vec!["sAMAccountName"])
                .await
                .map_err(|e| anyhow!("search transport error: {}", e))?
                .non_error()
                .map_err(|e| anyhow!("search failed: {}", e))?;
            for raw_entry in entries {
                if raw_entry.is_ref() || raw_entry.is_intermediate() {
                    continue;
                }
                let entry = SearchEntry::construct(raw_entry);
                if let Some(sam) = first_attr(&entry, "sAMAccountName") {
                    eprintln!("Update LAN ID ({}): found entry {}, sAMAccountName={}", dom.label, entry.dn, sam);
                    return Ok(Some(sam));
                }
            }
            Ok(None)
        }
        .await;

        let _ = ldap.unbind().await;
        match found_lanid {
            Ok(Some(new_lanid)) => {
                st.db.update("portal_user", uid, &row! {"lanid"=>new_lanid.clone(),"last_updated"=>web::now_str()}).await?;
                crate::cache::USERS.clear();
                eprintln!("Update LAN ID ({}): saved lanid={} for {}", dom.label, new_lanid, staffid);
                return Ok(Some(new_lanid));
            }
            Ok(None) => {
                eprintln!("Update LAN ID ({}): no entry found for filter {}", dom.label, filter);
            }
            Err(e) => {
                eprintln!("Update LAN ID ({}): error searching for {} - {}", dom.label, staffid, e);
                web::log_error(
                    &st.db,
                    "user",
                    "updatelanid",
                    &format!("Error connecting to {} ldap server: {}", dom.label, e),
                    None,
                    None,
                    Some(uid),
                )
                .await;
            }
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Bulk AD lookup (`ad_lookup`) matching g5portal's `User.adLookup(staffIds)`
// ---------------------------------------------------------------------------

fn first_attr(entry: &SearchEntry, name: &str) -> Option<String> {
    entry
        .attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .and_then(|(_, vals)| vals.first())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Bulk Active Directory lookup by `employeeID` across all configured domains.
/// Returns `{"found": {staffID: {"mail", "lanid", "name", "disabled", "domain"}}, "errors": [...]}`.
pub async fn ad_lookup(st: &St, staff_ids: &[String], batch_size: usize) -> Value {
    let mut found = serde_json::Map::new();
    let mut errors: Vec<Value> = vec![];

    let mut pending: Vec<String> = staff_ids
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        .collect();
    pending.sort();
    pending.dedup();

    if pending.is_empty() {
        return json!({"found": found, "errors": errors});
    }

    let cfg = resolve_settings(st).await;
    if cfg.domains.is_empty() {
        errors.push(json!("No Active Directory domains configured"));
        return json!({"found": found, "errors": errors});
    }

    let bs = batch_size.clamp(1, 200);
    for dom in &cfg.domains {
        let todo: Vec<String> = pending.iter().filter(|id| !found.contains_key(*id)).cloned().collect();
        if todo.is_empty() {
            break;
        }
        let mut ldap = match connect_ldap(dom, cfg.timeout_ms, cfg.tls_no_verify).await {
            Ok(l) => l,
            Err(e) => {
                let msg = format!("Could not search the {} domain: {}", dom.label, e);
                errors.push(json!(msg));
                web::log_error(&st.db, "user", "adLookup", &format!("Error searching ldap server {}: {}", dom.label, e), None, None, None).await;
                continue;
            }
        };

        let res: Result<()> = async {
            ldap.simple_bind(&dom.bind_dn, &dom.bind_password)
                .await
                .map_err(|e| anyhow!("service bind transport error: {}", e))?
                .success()
                .map_err(|e| anyhow!("service bind rejected: {}", e))?;
            let base_dn = effective_base_dn(&mut ldap, dom).await?;

            for batch in todo.chunks(bs) {
                let parts: String = batch
                    .iter()
                    .map(|id| format!("(employeeID={})", escape_ldap_filter(id)))
                    .collect();
                let filter = if batch.len() == 1 { parts } else { format!("(|{})", parts) };
                let (entries, _sr) = ldap
                    .search(
                        &base_dn,
                        Scope::Subtree,
                        &filter,
                        vec!["employeeID", "mail", "sAMAccountName", "displayName", "userAccountControl"],
                    )
                    .await
                    .map_err(|e| anyhow!("search transport error: {}", e))?
                    .non_error()
                    .map_err(|e| anyhow!("search failed: {}", e))?;

                for raw_entry in entries {
                    if raw_entry.is_ref() || raw_entry.is_intermediate() {
                        continue;
                    }
                    let entry = SearchEntry::construct(raw_entry);
                    let Some(eid) = first_attr(&entry, "employeeID") else { continue };
                    if found.contains_key(&eid) {
                        continue;
                    }
                    let uac = first_attr(&entry, "userAccountControl").and_then(|u| u.parse::<u64>().ok());
                    let disabled = uac.map(|bits| (bits & 2) != 0).unwrap_or(false);
                    found.insert(
                        eid,
                        json!({
                            "mail": first_attr(&entry, "mail"),
                            "lanid": first_attr(&entry, "sAMAccountName"),
                            "name": first_attr(&entry, "displayName"),
                            "disabled": disabled,
                            "domain": dom.label,
                        }),
                    );
                }
            }
            Ok(())
        }
        .await;

        let _ = ldap.unbind().await;
        if let Err(e) = res {
            let msg = format!("Could not search the {} domain: {}", dom.label, e);
            errors.push(json!(msg));
            web::log_error(&st.db, "user", "adLookup", &format!("Error searching ldap server {}: {}", dom.label, e), None, None, None).await;
        }
    }

    json!({"found": found, "errors": errors})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ldap_filter_escaping_rfc4515() {
        assert_eq!(escape_ldap_filter("jdoe"), "jdoe");
        assert_eq!(escape_ldap_filter("a*b(c)d\\e\0f"), "a\\2ab\\28c\\29d\\5ce\\00f");
    }

    #[test]
    fn derive_base_dn_from_bind_dn_and_toml_aliases() {
        assert_eq!(
            derive_base_dn("CN=svc,OU=LdapService,DC=CIMBMY,DC=CIMBDomain,DC=com").as_deref(),
            Some("DC=CIMBMY,DC=CIMBDomain,DC=com")
        );
        assert_eq!(derive_base_dn("svc@corp.local"), None);

        let d: AdDomainCfg = toml::from_str(
            r#"
            adserver = "ldaps.example.com"
            adport = 636
            adsecure = true
            addn = "CN=svc,DC=example,DC=com"
            adpassword = "pw"
            adbase = "DC=example,DC=com"
            "#,
        )
        .expect("parse alias keys");
        assert_eq!(d.server, "ldaps.example.com");
        assert_eq!(d.bind_dn, "CN=svc,DC=example,DC=com");
        assert_eq!(d.base_dn, "DC=example,DC=com");
    }

    #[tokio::test]
    async fn resolve_settings_merges_config_and_portal_settings() {
        let db = Db::connect("sqlite::memory:", 1, None).await.expect("connect sqlite");
        crate::entity::sync_schema(&db, &crate::entity::entities()).await.expect("sync");

        let mut cfg = crate::web::Config::default();
        cfg.active_directory.domains.push(AdDomainCfg {
            label: "corp".into(),
            server: "ldaps.corp.local".into(),
            port: 636,
            secure: true,
            bind_dn: "CN=svc,DC=corp,DC=local".into(),
            bind_password: "secret".into(),
            base_dn: "DC=corp,DC=local".into(),
        });
        let st = std::sync::Arc::new(crate::web::AppState {
            db: db.clone(),
            cfg,
            entities: crate::entity::entities(),
            excel_jobs: Mutex::new(HashMap::new()),
            rt: tokio::runtime::Handle::current(),
        });

        let r1 = resolve_settings(&st).await;
        assert!(!r1.enabled);
        assert!(!r1.enforce_lanid);
        assert_eq!(r1.domains.len(), 1);
        assert_eq!(r1.domains[0].server, "ldaps.corp.local");

        // portal_setting overrides config.toml live, just like g5portal
        db.insert("portal_setting", &row! {"module"=>"portal","name"=>"adenable","type"=>"Number","number"=>1}).await.unwrap();
        db.insert("portal_setting", &row! {"module"=>"portal","name"=>"enforce_lanid","type"=>"Text","text"=>"true"}).await.unwrap();
        db.insert("portal_setting", &row! {"module"=>"portal","name"=>"adserver","type"=>"Text","text"=>"ad1.example.com"}).await.unwrap();
        db.insert("portal_setting", &row! {"module"=>"portal","name"=>"adserver2","type"=>"Text","text"=>"ad2.example.com"}).await.unwrap();
        db.insert("portal_setting", &row! {"module"=>"portal","name"=>"adport2","type"=>"Number","number"=>389}).await.unwrap();
        db.insert("portal_setting", &row! {"module"=>"portal","name"=>"adsecure2","type"=>"Text","text"=>"false"}).await.unwrap();

        let r2 = resolve_settings(&st).await;
        assert!(r2.enabled);
        assert!(r2.enforce_lanid);
        assert_eq!(r2.domains.len(), 2);
        assert_eq!(r2.domains[0].server, "ad1.example.com");
        assert_eq!(r2.domains[1].server, "ad2.example.com");
        assert_eq!(r2.domains[1].port, 389);
        assert!(!r2.domains[1].secure);
    }

    #[tokio::test]
    async fn local_password_fallback_respects_enforce_lanid_and_exception() {
        let db = Db::connect("sqlite::memory:", 1, None).await.expect("connect sqlite");
        crate::entity::sync_schema(&db, &crate::entity::entities()).await.expect("sync");
        let hash = bcrypt::hash("LocalPass123!", 4).unwrap();
        let uid = db
            .insert(
                "portal_user",
                &row! {"userid"=>"staff01","name"=>"Alice","password"=>hash,"lanid"=>"alice.lan","is_active"=>true,"lanidexception"=>false},
            )
            .await
            .unwrap();
        let st = std::sync::Arc::new(crate::web::AppState {
            db: db.clone(),
            cfg: crate::web::Config::default(),
            entities: crate::entity::entities(),
            excel_jobs: Mutex::new(HashMap::new()),
            rt: tokio::runtime::Handle::current(),
        });

        let u = db.get("portal_user", uid).await.unwrap().unwrap();
        // AD disabled, enforce_lanid off -> local password succeeds
        let o1 = check_credentials(&st, &u, "LocalPass123!", "login").await;
        assert!(o1.authenticated);
        assert!(!o1.via_ad);

        // Turn enforce_lanid on -> local password rejected for user with lanid
        db.insert("portal_setting", &row! {"module"=>"portal","name"=>"enforce_lanid","type"=>"Number","number"=>1}).await.unwrap();
        let o2 = check_credentials(&st, &u, "LocalPass123!", "login").await;
        assert!(!o2.authenticated);
        assert!(o2.enforce_lanid_active);

        // Mark user with lanidexception = true -> local password allowed again
        db.update("portal_user", uid, &row! {"lanidexception"=>true}).await.unwrap();
        let u2 = db.get("portal_user", uid).await.unwrap().unwrap();
        let o3 = check_credentials(&st, &u2, "LocalPass123!", "login").await;
        assert!(o3.authenticated);
        assert!(!o3.enforce_lanid_active);
    }
}
