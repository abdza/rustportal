//! Module export/import. A module is a folder of JSON definitions plus page
//! sources, in the same layout the Grails portal used
//! (`trackerlist.json`, `pagelist.json`, `settinglist.json`, `pages/`),
//! so module folders can be version-controlled and moved between instances.

use crate::db::{self, b, i, s, Db, Row};
use crate::row;
use crate::tracker::Tracker;
use crate::web::{self, esc, now_str, Ctx, St};
use anyhow::{anyhow, Result};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub fn folder(st: &St, module: &str) -> PathBuf {
    PathBuf::from(&st.cfg.module_dir).join(module)
}

#[derive(Clone, Debug, Default)]
pub struct ExportOpts {
    pub files: bool,
    pub roles: bool,
    pub trees: bool,
    pub menus: bool,
}

#[derive(Clone, Debug)]
pub struct ImportOpts {
    pub files: bool,
    pub roles: bool,
    pub trees: bool,
    pub menus: bool,
    pub endpoints: bool,
    pub setting_choices: HashMap<String, String>,
    pub scheduler_choices: HashMap<String, String>,
}

impl Default for ImportOpts {
    fn default() -> Self {
        ImportOpts {
            files: true,
            roles: false,
            trees: true,
            menus: true,
            endpoints: true,
            setting_choices: HashMap::new(),
            scheduler_choices: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SettingPreview {
    pub key: String,
    pub module: String,
    pub name: String,
    pub status: &'static str, // "changed" | "new" | "same"
    pub current: String,
    pub incoming: String,
}

#[derive(Clone, Debug)]
pub struct SchedulerPreview {
    pub key: String,
    pub module: String,
    pub name: String,
    pub status: &'static str, // "changed" | "new" | "same"
    pub current: String,
    pub incoming: String,
    pub lastrun: String,
}

pub fn setting_key(module: &str, name: &str) -> String {
    format!("{}::{}", module, name)
}

pub fn scheduler_key(module: &str, name: &str) -> String {
    format!("{}::{}", module, name)
}

fn setting_canon(typ: &str, text: &str, number: &str, date_val: &str, datum_type: &str) -> String {
    format!("{}\x01{}\x01{}\x01{}\x01{}", typ, text, number, date_val, datum_type)
}

fn setting_display(typ: &str, text: &str, number: &str, date_val: &str) -> String {
    match typ {
        "Number" => number.to_string(),
        "Date" => date_val.to_string(),
        _ => text.to_string(),
    }
}

fn sched_hour(r: &Row) -> String {
    let h = s(r, "hour");
    if h.trim().is_empty() { s(r, "hour_of_day") } else { h }
}

fn sched_canon(slugs: &str, hour: &str, dow: &str, dom: &str, enabled: bool, catch_up: bool) -> String {
    format!("{}\x01{}\x01{}\x01{}\x01{}\x01{}", slugs, hour, dow, dom, if enabled { "1" } else { "0" }, if catch_up { "1" } else { "0" })
}

fn sched_display(slugs: &str, hour: &str, dow: &str, dom: &str, enabled: bool, catch_up: bool) -> String {
    format!(
        "{} | hour {}, day of week {}, day of month {} | {}{}",
        slugs,
        hour,
        dow,
        dom,
        if enabled { "enabled" } else { "disabled" },
        if catch_up { ", catches up missed runs" } else { "" }
    )
}

fn pick(r: &Row, keys: &[&str]) -> Row {
    let mut o = Row::new();
    for k in keys {
        o.insert(k.to_string(), r.get(*k).cloned().unwrap_or(Value::Null));
    }
    o
}

fn write_json(path: &Path, v: &Value) -> Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(v)?)?;
    Ok(())
}

fn read_json(path: &Path) -> Option<Vec<Row>> {
    let t = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&t).ok()? {
        Value::Array(a) => Some(a.into_iter().filter_map(|v| v.as_object().cloned()).collect()),
        _ => None,
    }
}

const TRACKER_KEYS: &[&str] = &[
    "name", "slug", "tracker_type", "module", "side_menu", "listfields", "allowedroles", "hiddenlistfields", "excelfields", "filterfields", "searchfields",
    "row_validation_fields", "datatable", "trailtable", "defaultsort", "rolesort", "allowadd", "downloadexcel", "anonymous_list", "anonymous_view",
    "require_login", "excel_audit", "defaultlimit", "tickactions", "actionbuttons", "condition_q", "rowclassval",
];
const FIELD_KEYS: &[&str] = &[
    "name", "label", "field_type", "field_options", "field_format", "field_default", "hyperscript", "field_display", "field_query", "classes", "hide_heading",
    "params_override", "url_value", "is_encrypted", "role_query", "encode_exception", "suppress_follow_link", "field_description", "field_tooltip",
    "field_placeholder", "row_validation_regex", "field_order",
];
const ERROR_KEYS: &[&str] = &["description", "error_type", "format", "error_msg", "error_function", "allow_submission"];
const STATUS_KEYS: &[&str] = &["name", "displayfields", "updateallowedroles", "editroles", "editfields", "flow", "updateable", "attachable", "suppressupdatebutton", "actiontransitions"];
const ROLE_KEYS: &[&str] = &["name", "role_type", "role_rule", "role_desc"];
const TRANS_KEYS: &[&str] = &[
    "name", "display_name", "editfields", "displayfields", "requiredfields", "richtextfields", "enabledcondition", "updatetrails", "submitbuttontext",
    "cancelbuttontext", "redirect_after", "button_class", "immediate_submission", "gotoprevstatuslist", "same_status", "cancelbutton",
];
const PAGE_KEYS: &[&str] = &["title", "slug", "allowedroles", "module", "side_menu", "requirelogin", "published", "runable", "render", "fullpage", "redirectafter"];
const SETTING_KEYS: &[&str] = &["module", "name", "text", "date_value", "number", "type", "datum_type"];
const SCHED_KEYS: &[&str] = &["module", "name", "slugs", "hour", "day_of_week", "day_of_month", "enabled", "catch_up"];
const ENDPOINT_KEYS: &[&str] = &["module", "slug", "name", "description", "handler_type", "target", "working_dir", "env_json", "header_env_json", "auth_mode", "allowed_roles", "realm", "enabled", "timeout_seconds", "max_body_mb"];

async fn page_slug(db: &Db, id: Option<i64>) -> Value {
    match id {
        Some(x) => db.get(db::page_t(), x).await.ok().flatten().map(|p| json!(s(&p, "slug"))).unwrap_or(Value::Null),
        None => Value::Null,
    }
}

pub async fn export(st: &St, module: &str, with_roles: bool) -> Result<PathBuf> {
    let dir = folder(st, module);
    export_to(st, module, &dir, ExportOpts { files: false, roles: with_roles, trees: true, menus: false }).await?;
    Ok(dir)
}

fn node_data_get(data: &str, key: &str) -> Option<String> {
    for pair in data.split(';') {
        if let Some((k, v)) = pair.split_once('=') {
            if k.trim().eq_ignore_ascii_case(key) {
                let val = v.trim();
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}

fn owns_menu_node(node: &Row, module: &str) -> bool {
    node_data_get(&s(node, "data"), "module").as_deref() == Some(module)
}

fn menu_stamp(module: &str, icon: Option<&str>, existing: &str) -> String {
    let kept: Vec<String> = existing
        .split(';')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let kl = k.trim().to_ascii_lowercase();
            if kl == "module" || kl == "icon" {
                None
            } else {
                Some(format!("{}={}", k.trim(), v.trim()))
            }
        })
        .collect();
    let mut parts = vec![format!("module={}", module)];
    if let Some(ic) = icon.filter(|x| !x.trim().is_empty()) {
        parts.push(format!("icon={}", ic.trim()));
    }
    parts.extend(kept);
    parts.join(";")
}

async fn export_filelinks(db: &Db, module: &str, dir: &Path) -> Result<()> {
    let filelinks = db
        .rows("select * from file_link where module=:m order by slug, name, id", &row! {"m"=>module})
        .await?;
    let path = dir.join("filelinklist.json");
    if filelinks.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let files_dir = dir.join("files");
    std::fs::create_dir_all(&files_dir)?;
    let mut arr = vec![];
    for fl in &filelinks {
        let src = PathBuf::from(s(fl, "path"));
        if !src.is_file() {
            continue;
        }
        let fid = i(fl, "id").unwrap_or(0);
        let name = s(fl, "name");
        let dst_name = format!("fl_{}_{}", fid, name.replace(' ', "_"));
        let dst = files_dir.join(&dst_name);
        let _ = std::fs::copy(&src, &dst);
        arr.push(json!({
            "id": fid,
            "module": s(fl, "module"),
            "slug": s(fl, "slug"),
            "name": name,
            "path": s(fl, "path"),
            "exportpath": dst.to_string_lossy().to_string(),
            "allowedroles": fl.get("allowedroles").cloned().unwrap_or(Value::Null),
            "filegroup": fl.get("filegroup").cloned().unwrap_or(Value::Null),
            "sortnum": fl.get("sortnum").cloned().unwrap_or(Value::Null),
        }));
    }
    write_json(&path, &Value::Array(arr))
}

async fn export_menu(db: &Db, module: &str, dir: &Path) -> Result<()> {
    let path = dir.join("menulist.json");
    let tree_name = format!("{}_menu", module);
    let Some(tree) = db
        .first("select id, root_id from portal_tree where module='portal' and name=:n limit 1", &row! {"n"=>tree_name})
        .await?
    else {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    };
    let Some(root_id) = i(&tree, "root_id") else {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    };
    let Some(root) = db.get("portal_tree_node", root_id).await? else {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    };
    let top_nodes = db
        .rows("select * from portal_tree_node where parent_id=:p order by lft, id", &row! {"p"=>root_id})
        .await?;
    let mut items = vec![];
    for node in &top_nodes {
        if !owns_menu_node(node, module) {
            continue;
        }
        let nid = i(node, "id").unwrap_or(0);
        let children = db
            .rows("select * from portal_tree_node where parent_id=:p order by lft, id", &row! {"p"=>nid})
            .await?;
        if !children.is_empty() {
            for ch in &children {
                if !owns_menu_node(ch, module) {
                    continue;
                }
                items.push(json!({
                    "name": s(ch, "name"),
                    "link": ch.get("slug").cloned().unwrap_or(Value::Null),
                    "icon": node_data_get(&s(ch, "data"), "icon"),
                    "group": s(node, "name"),
                    "mainrole": ch.get("mainrole").cloned().unwrap_or(Value::Null),
                    "hiderole": ch.get("hiderole").cloned().unwrap_or(Value::Null),
                }));
            }
        } else {
            items.push(json!({
                "name": s(node, "name"),
                "link": node.get("slug").cloned().unwrap_or(Value::Null),
                "icon": node_data_get(&s(node, "data"), "icon"),
                "group": Value::Null,
                "mainrole": node.get("mainrole").cloned().unwrap_or(Value::Null),
                "hiderole": node.get("hiderole").cloned().unwrap_or(Value::Null),
            }));
        }
    }
    if items.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    write_json(
        &path,
        &json!({
            "label": s(&root, "name"),
            "icon": node_data_get(&s(&root, "data"), "icon"),
            "items": items
        }),
    )
}

pub async fn export_to(st: &St, module: &str, dir: &Path, opts: ExportOpts) -> Result<()> {
    let db = &st.db;
    std::fs::create_dir_all(dir.join("pages"))?;
    // pages
    let pages = db.rows(&format!("select * from {} where module=:m order by slug", db::page_t()), &row! {"m"=>module}).await?;
    let mut plist = vec![];
    for p in &pages {
        let mut o = pick(p, PAGE_KEYS);
        o.insert("lastUpdated".into(), p.get("last_updated").cloned().unwrap_or(Value::Null));
        let ds = db.rows(&format!("select name, query, return_one from {} where page_id=:p order by id", db::page_data_t()), &row! {"p"=>i(p, "id")}).await?;
        o.insert("datasources".into(), if ds.is_empty() { Value::Null } else { Value::Array(ds.into_iter().map(Value::Object).collect()) });
        let slug = s(p, "slug");
        for old in ["html", "rhai", "gsp"] {
            let _ = std::fs::remove_file(dir.join("pages").join(format!("content_{}.{}", slug, old)));
        }
        std::fs::write(dir.join("pages").join(format!("content_{}.{}", slug, if b(p, "runable") { "rhai" } else { "html" })), s(p, "content"))?;
        let pp = s(p, "preprocess");
        let ppfile = dir.join("pages").join(format!("pp_{}.rhai", slug));
        if pp.trim().is_empty() {
            let _ = std::fs::remove_file(&ppfile);
        } else {
            std::fs::write(ppfile, pp)?;
        }
        plist.push(Value::Object(o));
    }
    if !plist.is_empty() {
        write_json(&dir.join("pagelist.json"), &Value::Array(plist))?;
    } else {
        let _ = std::fs::remove_file(dir.join("pagelist.json"));
    }
    // trackers
    let trows = db.rows("select * from portal_tracker where module=:m order by slug", &row! {"m"=>module}).await?;
    let mut tlist = vec![];
    for tr in trows {
        let Some(t) = Tracker::from_row(db, tr).await else { continue };
        let mut o = pick(&t.row, TRACKER_KEYS);
        o.insert("initial_status".into(), json!(t.status_name(t.initial_status())).as_str().filter(|x| !x.is_empty()).map(|x| json!(x)).unwrap_or(Value::Null));
        o.insert("postprocess".into(), page_slug(db, i(&t.row, "postprocess_id")).await);
        o.insert("defaultfield".into(), i(&t.row, "defaultfield_id").and_then(|x| t.field_by_id(x)).map(|f| json!(s(f, "name"))).unwrap_or(Value::Null));
        let mut fields = vec![];
        for f in &t.fields {
            let mut fo = pick(f, FIELD_KEYS);
            let errs = db.rows("select * from portal_tracker_error where field_id=:f order by id", &row! {"f"=>i(f, "id")}).await?;
            fo.insert("error_checks".into(), Value::Array(errs.iter().map(|e| Value::Object(pick(e, ERROR_KEYS))).collect()));
            fields.push(Value::Object(fo));
        }
        o.insert("fields".into(), Value::Array(fields));
        let emails = db.rows("select * from portal_tracker_email where tracker_id=:t order by id", &row! {"t"=>t.id}).await?;
        let mut statuses = vec![];
        for st_ in &t.statuses {
            let mut so = pick(st_, STATUS_KEYS);
            so.insert("compositeStatuses".into(), st_.get("composite_statuses").cloned().unwrap_or(Value::Null));
            so.insert("emailonupdate".into(), i(st_, "emailonupdate_id").and_then(|x| emails.iter().find(|e| i(e, "id") == Some(x))).map(|e| json!(s(e, "name"))).unwrap_or(Value::Null));
            so.insert("runonupdate".into(), page_slug(db, i(st_, "runonupdate_id")).await);
            statuses.push(Value::Object(so));
        }
        o.insert("statuses".into(), Value::Array(statuses));
        o.insert("roles".into(), Value::Array(t.roles.iter().map(|r| Value::Object(pick(r, ROLE_KEYS))).collect()));
        let mut transitions = vec![];
        for tn in &t.transitions {
            let mut to = pick(&tn.row, TRANS_KEYS);
            to.insert("postprocess".into(), page_slug(db, i(&tn.row, "postprocess_id")).await);
            to.insert("prev_status".into(), json!(tn.prev.iter().map(|x| t.status_name(Some(*x))).collect::<Vec<_>>()));
            to.insert("next_status".into(), tn.next.map(|x| json!(t.status_name(Some(x)))).unwrap_or(Value::Null));
            to.insert("roles".into(), json!(tn.roles));
            let mut el = vec![];
            for e in emails.iter().filter(|e| i(e, "transition_id") == Some(tn.id)) {
                el.push(json!({"name": s(e, "name"), "emailto": e.get("emailto"), "emailcc": e.get("emailcc"), "body": page_slug(db, i(e, "body_id")).await}));
            }
            to.insert("emails".into(), Value::Array(el));
            transitions.push(Value::Object(to));
        }
        o.insert("transitions".into(), Value::Array(transitions));
        let mut other_emails = vec![];
        for e in emails.iter().filter(|e| i(e, "transition_id").is_none()) {
            other_emails.push(json!({"name": s(e, "name"), "emailto": e.get("emailto"), "emailcc": e.get("emailcc"), "body": page_slug(db, i(e, "body_id")).await}));
        }
        o.insert("emails".into(), Value::Array(other_emails));
        let flows = db.rows("select name, fields, transitions from portal_tracker_flow where tracker_id=:t order by id", &row! {"t"=>t.id}).await?;
        o.insert("flows".into(), Value::Array(flows.into_iter().map(Value::Object).collect()));
        let idx = db.rows("select name, fields from portal_tracker_index where tracker_id=:t order by id", &row! {"t"=>t.id}).await?;
        o.insert("indexes".into(), Value::Array(idx.into_iter().map(Value::Object).collect()));
        tlist.push(Value::Object(o));
    }
    if !tlist.is_empty() {
        write_json(&dir.join("trackerlist.json"), &Value::Array(tlist))?;
    } else {
        let _ = std::fs::remove_file(dir.join("trackerlist.json"));
    }
    let settings = db.rows("select * from portal_setting where module=:m order by name", &row! {"m"=>module}).await?;
    if !settings.is_empty() {
        write_json(&dir.join("settinglist.json"), &Value::Array(settings.iter().map(|x| Value::Object(pick(x, SETTING_KEYS))).collect()))?;
    } else {
        let _ = std::fs::remove_file(dir.join("settinglist.json"));
    }
    let scheds = db.rows("select * from portal_scheduler where module=:m order by name", &row! {"m"=>module}).await?;
    if !scheds.is_empty() {
        let sarr: Vec<Value> = scheds
            .iter()
            .map(|x| {
                let mut o = pick(x, SCHED_KEYS);
                let hr = sched_hour(x);
                o.insert("hour".into(), json!(hr));
                o.insert("hour_of_day".into(), json!(hr));
                Value::Object(o)
            })
            .collect();
        write_json(&dir.join("schedulerlist.json"), &Value::Array(sarr))?;
    } else {
        let _ = std::fs::remove_file(dir.join("schedulerlist.json"));
    }
    let eps = db.rows("select * from portal_endpoint where module=:m order by slug", &row! {"m"=>module}).await?;
    if !eps.is_empty() {
        let earr: Vec<Value> = eps
            .iter()
            .map(|x| {
                let mut o = pick(x, ENDPOINT_KEYS);
                o.insert("had_auth_token".into(), json!(!s(x, "auth_token").is_empty()));
                Value::Object(o)
            })
            .collect();
        write_json(&dir.join("endpointlist.json"), &Value::Array(earr))?;
    } else {
        let _ = std::fs::remove_file(dir.join("endpointlist.json"));
    }
    if opts.trees {
        let trees = db.rows("select * from portal_tree where module=:m order by name", &row! {"m"=>module}).await?;
        if !trees.is_empty() {
            let mut tl = vec![];
            for t in trees {
                tl.push(crate::tree::export_tree(db, i(&t, "id").unwrap_or(0), opts.roles).await?);
            }
            write_json(&dir.join("treelist.json"), &Value::Array(tl))?;
        } else {
            let _ = std::fs::remove_file(dir.join("treelist.json"));
        }
    } else {
        let _ = std::fs::remove_file(dir.join("treelist.json"));
    }
    if opts.files {
        export_filelinks(db, module, dir).await?;
    } else {
        let _ = std::fs::remove_file(dir.join("filelinklist.json"));
    }
    if opts.menus {
        export_menu(db, module, dir).await?;
    } else {
        let _ = std::fs::remove_file(dir.join("menulist.json"));
    }
    if opts.roles {
        let roles = db
            .rows("select u.userid as user, u.userid as staffid, u.userid, r.module, r.role from user_role r join portal_user u on u.id=r.user_id where r.module=:m order by u.userid, r.role", &row! {"m"=>module})
            .await?;
        if !roles.is_empty() {
            write_json(&dir.join("userrolelist.json"), &Value::Array(roles.into_iter().map(Value::Object).collect()))?;
        } else {
            let _ = std::fs::remove_file(dir.join("userrolelist.json"));
        }
        let _ = std::fs::remove_file(dir.join("userroles.json"));
    } else {
        let _ = std::fs::remove_file(dir.join("userrolelist.json"));
        let _ = std::fs::remove_file(dir.join("userroles.json"));
    }
    Ok(())
}

/// Page source and whether it is a Grails (`.gsp`: GSP template or Groovy script) file.
fn read_page_source(dir: &Path, prefix: &str, slug: &str) -> Option<(String, bool)> {
    for ext in ["html", "rhai", "jinja", "gsp"] {
        if let Ok(bytes) = std::fs::read(dir.join("pages").join(format!("{}_{}.{}", prefix, slug, ext))) {
            return Some((String::from_utf8_lossy(&bytes).to_string(), ext == "gsp"));
        }
    }
    None
}

fn vals(r: &Row, keys: &[&str]) -> Row {
    let mut o = Row::new();
    for k in keys {
        if let Some(v) = r.get(*k) {
            o.insert(k.to_string(), match v { Value::Array(_) | Value::Object(_) => Value::Null, v => v.clone() });
        }
    }
    o
}

async fn upsert(db: &Db, table: &str, find: &str, fp: &Row, data: &Row) -> Result<i64> {
    match db.first(&format!("select id from {} where {} order by id limit 1", table, find), fp).await? {
        Some(r) => {
            let id = i(&r, "id").unwrap_or(0);
            db.update(table, id, data).await?;
            Ok(id)
        }
        None => db.insert(table, data).await,
    }
}

async fn page_id(db: &Db, module: &str, v: Option<&Value>) -> Value {
    match v.map(db::vs).filter(|x| !x.is_empty()) {
        Some(slug) => crate::page::find_page(db, module, &slug).await.and_then(|p| i(&p, "id")).map(|x| json!(x)).unwrap_or(Value::Null),
        None => Value::Null,
    }
}

pub async fn import(st: &St, module: &str, with_roles: bool) -> Result<(String, String)> {
    import_from(st, module, &folder(st, module), with_roles).await
}

/// Import a module folder. Folders exported by the Grails portal (`.gsp` page
/// sources) are accepted too: template pages are translated to MiniJinja,
/// Groovy scripts are kept as comments for porting. Returns (summary, details).
pub async fn import_from(st: &St, module: &str, dir: &Path, with_roles: bool) -> Result<(String, String)> {
    let opts = ImportOpts {
        files: true,
        roles: with_roles,
        trees: true,
        menus: true,
        endpoints: true,
        setting_choices: HashMap::new(),
        scheduler_choices: HashMap::new(),
    };
    import_from_opts(st, module, dir, &opts).await
}

async fn ensure_menu_tree(db: &Db, name: &str, label: &str, module: &str, icon: Option<&str>) -> Result<(i64, i64)> {
    let tid = match db.first("select id, root_id from portal_tree where module='portal' and name=:n limit 1", &row! {"n"=>name}).await? {
        Some(t) => {
            let tid = i(&t, "id").unwrap_or(0);
            if let Some(rid) = i(&t, "root_id") {
                return Ok((tid, rid));
            }
            tid
        }
        None => db.insert("portal_tree", &row! {"module"=>"portal","name"=>name}).await?,
    };
    let root_id = match db
        .first("select id from portal_tree_node where tree_id=:t and parent_id is null order by lft, id limit 1", &row! {"t"=>tid})
        .await?
    {
        Some(r) => i(&r, "id").unwrap_or(0),
        None => {
            let data = if name == "main_menu" { String::new() } else { menu_stamp(module, icon, "") };
            db.insert("portal_tree_node", &row! {"tree_id"=>tid,"name"=>label,"data"=>data,"lft"=>1,"rgt"=>2,"disabled"=>false}).await?
        }
    };
    let _ = db.update("portal_tree", tid, &row! {"root_id"=>root_id}).await;
    Ok((tid, root_id))
}

#[derive(Clone)]
struct MenuNodeSpec {
    name: String,
    slug: Option<String>,
    icon: Option<String>,
    mainrole: Option<String>,
    hiderole: Option<String>,
    nodes: Vec<MenuNodeSpec>,
}

fn delete_menu_node<'a>(db: &'a Db, node_id: i64) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        for ch in db.rows("select id from portal_tree_node where parent_id=:p", &row! {"p"=>node_id}).await.unwrap_or_default() {
            if let Some(cid) = i(&ch, "id") {
                delete_menu_node(db, cid).await;
            }
        }
        let _ = db.delete("portal_tree_node", node_id).await;
    })
}

fn sync_menu_nodes<'a>(
    db: &'a Db,
    tree_id: i64,
    parent_id: i64,
    wanted: &'a [MenuNodeSpec],
    module: &'a str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let siblings = db.rows("select * from portal_tree_node where parent_id=:p order by lft, id", &row! {"p"=>parent_id}).await.unwrap_or_default();
        let mut spare: Vec<Row> = siblings.iter().filter(|n| owns_menu_node(n, module)).cloned().collect();
        let may_reorder = siblings.len() == spare.len();
        for (pos, w) in wanted.iter().enumerate() {
            let key = w.slug.as_deref().filter(|x| !x.is_empty()).unwrap_or(&w.name);
            let matched = spare.iter().position(|n| {
                let nk = { let sl = s(n, "slug"); if sl.is_empty() { s(n, "name") } else { sl } };
                nk == key
            });
            let (nid, existing_data) = if let Some(idx) = matched {
                let row = spare.remove(idx);
                (Some(i(&row, "id").unwrap_or(0)), s(&row, "data"))
            } else {
                (None, String::new())
            };
            let stamp = menu_stamp(module, w.icon.as_deref(), &existing_data);
            let mut rec = row! {
                "tree_id"=>tree_id,
                "parent_id"=>parent_id,
                "name"=>w.name.clone(),
                "slug"=>w.slug.clone(),
                "data"=>stamp,
                "mainrole"=>w.mainrole.clone(),
                "hiderole"=>w.hiderole.clone(),
                "disabled"=>false
            };
            if may_reorder {
                rec.insert("lft".into(), json!(pos as i64));
            }
            let saved_id = match nid {
                Some(id) => {
                    let _ = db.update("portal_tree_node", id, &rec).await;
                    id
                }
                None => db.insert("portal_tree_node", &rec).await.unwrap_or(0),
            };
            if saved_id > 0 {
                sync_menu_nodes(db, tree_id, saved_id, &w.nodes, module).await;
            }
        }
        for stale in spare {
            if let Some(sid) = i(&stale, "id") {
                delete_menu_node(db, sid).await;
            }
        }
    })
}

async fn import_menu(db: &Db, module: &str, dir: &Path) -> Result<()> {
    let path = dir.join("menulist.json");
    if !path.is_file() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&path)?;
    let val: Value = serde_json::from_str(&text)?;
    let label = val.get("label").and_then(|v| v.as_str()).filter(|x| !x.is_empty()).unwrap_or(module).to_string();
    let icon = val.get("icon").and_then(|v| v.as_str()).filter(|x| !x.is_empty()).map(String::from);
    let items = val.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default();

    let mut ungrouped = vec![];
    let mut group_order: Vec<String> = vec![];
    let mut group_map: HashMap<String, Vec<MenuNodeSpec>> = HashMap::new();
    for it in &items {
        let spec = MenuNodeSpec {
            name: it.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            slug: it.get("link").and_then(|v| v.as_str()).filter(|x| !x.is_empty()).map(String::from),
            icon: it.get("icon").and_then(|v| v.as_str()).filter(|x| !x.is_empty()).map(String::from),
            mainrole: it.get("mainrole").and_then(|v| v.as_str()).filter(|x| !x.is_empty()).map(String::from),
            hiderole: it.get("hiderole").and_then(|v| v.as_str()).filter(|x| !x.is_empty()).map(String::from),
            nodes: vec![],
        };
        if let Some(g) = it.get("group").and_then(|v| v.as_str()).filter(|x| !x.is_empty()) {
            if !group_map.contains_key(g) {
                group_order.push(g.to_string());
            }
            group_map.entry(g.to_string()).or_default().push(spec);
        } else {
            ungrouped.push(spec);
        }
    }
    let groups: Vec<MenuNodeSpec> = group_order
        .into_iter()
        .map(|g| MenuNodeSpec {
            name: g.clone(),
            slug: None,
            icon: None,
            mainrole: None,
            hiderole: None,
            nodes: group_map.remove(&g).unwrap_or_default(),
        })
        .collect();

    // portal/main_menu
    if let Ok((main_tid, main_root)) = ensure_menu_tree(db, "main_menu", "Main Menu", module, icon.as_deref()).await {
        let mut mgroups = vec![];
        if !ungrouped.is_empty() {
            mgroups.push(MenuNodeSpec {
                name: label.clone(),
                slug: None,
                icon: None,
                mainrole: None,
                hiderole: None,
                nodes: ungrouped.clone(),
            });
        }
        mgroups.extend(groups.clone());
        let wanted = if mgroups.is_empty() {
            vec![]
        } else {
            vec![MenuNodeSpec {
                name: label.clone(),
                slug: None,
                icon: icon.clone(),
                mainrole: None,
                hiderole: None,
                nodes: mgroups,
            }]
        };
        sync_menu_nodes(db, main_tid, main_root, &wanted, module).await;
        let _ = crate::tree::rebuild(db, main_tid).await;
    }

    // portal/<module>_menu
    if let Ok((side_tid, side_root)) = ensure_menu_tree(db, &format!("{}_menu", module), &label, module, icon.as_deref()).await {
        let mut side_wanted = ungrouped;
        side_wanted.extend(groups);
        sync_menu_nodes(db, side_tid, side_root, &side_wanted, module).await;
        let _ = crate::tree::rebuild(db, side_tid).await;
    }
    Ok(())
}

pub async fn import_from_opts(st: &St, module: &str, dir: &Path, opts: &ImportOpts) -> Result<(String, String)> {
    let db = &st.db;
    let dir = dir.to_path_buf();
    let mut detail: Vec<String> = vec![];
    let (mut translated, mut flagged, mut scripts) = (0, 0, 0);
    if !dir.is_dir() {
        return Err(anyhow!("Module folder {} does not exist", dir.display()));
    }
    crate::admin::ensure_module(db, module).await;
    let mut log = vec![];
    // pages first: trackers refer to them
    let mut legacy = 0;
    if let Some(pages) = read_json(&dir.join("pagelist.json")) {
        for p in &pages {
            let slug = s(p, "slug");
            if slug.is_empty() {
                continue;
            }
            let mut d = vals(p, PAGE_KEYS);
            d.insert("module".into(), json!(module));
            d.insert("last_updated".into(), json!(now_str()));
            let runable = b(p, "runable");
            match read_page_source(&dir, "content", &slug) {
                Some((c, true)) if runable => {
                    scripts += 1;
                    detail.push(format!("{}: runable page is Groovy - not ported, it stops with a message when run", slug));
                    d.insert("content".into(), json!(crate::gsp::stub_script(&c, "runable page", true)));
                }
                Some((c, true)) => {
                    let t = crate::gsp::translate(&c);
                    let mut warns = t.warnings;
                    if let Err(e) = minijinja::Environment::new().template_from_str(&t.text) {
                        warns.push(format!("the translated template does not compile: {:#}", e));
                    }
                    translated += 1;
                    if !warns.is_empty() {
                        flagged += 1;
                        for w in &warns {
                            detail.push(format!("{}: {}", slug, w));
                        }
                    }
                    d.insert("content".into(), json!(if b(p, "fullpage") { t.text } else { crate::gsp::strip_layout(&t.text) }));
                }
                Some((c, false)) => {
                    if c.contains("<g:") || c.contains("<%") || c.contains("g5portal.") {
                        legacy += 1;
                    }
                    d.insert("content".into(), json!(c));
                }
                None => {}
            }
            d.insert("preprocess".into(), match read_page_source(&dir, "pp", &slug) {
                Some((c, true)) if !c.trim().is_empty() => {
                    scripts += 1;
                    detail.push(format!("{}: preprocess script is Groovy - not ported, the page renders without its `pp` values", slug));
                    json!(crate::gsp::stub_script(&c, "preprocess script", false))
                }
                Some((c, false)) => json!(c),
                _ => Value::Null,
            });
            let pid = upsert(db, db::page_t(), "module=:m and slug=:s", &row! {"m"=>module,"s"=>slug.clone()}, &d).await?;
            db.exec(&format!("delete from {} where page_id=:p", db::page_data_t()), &row! {"p"=>pid}).await?;
            if let Some(Value::Array(ds)) = p.get("datasources") {
                for x in ds.iter().filter_map(|v| v.as_object()) {
                    let mut q = s(x, "query");
                    if dir.join("pages").join(format!("content_{}.gsp", slug)).is_file() && !q.contains("{{") {
                        // Grails data source queries are Groovy expressions that build the SQL
                        let t = crate::gsp::translate_expr(&q);
                        for w in &t.warnings {
                            detail.push(format!("{} data source {}: {}", slug, s(x, "name"), w));
                        }
                        q = t.text;
                    }
                    db.insert(db::page_data_t(), &row! {"page_id"=>pid,"name"=>s(x, "name"),"query"=>q,"return_one"=>b(x, "return_one")}).await?;
                }
            }
        }
        log.push(format!("{} pages", pages.len()));
    }
    if let Some(trackers) = read_json(&dir.join("trackerlist.json")) {
        for t in &trackers {
            let slug = s(t, "slug");
            if !db::ident_ok(&slug) || module.is_empty() || !module.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                log.push(format!("skipped tracker '{}' (invalid name)", slug));
                continue;
            }
            let mut d = vals(t, TRACKER_KEYS);
            d.insert("module".into(), json!(module));
            d.insert("postprocess_id".into(), page_id(db, module, t.get("postprocess")).await);
            let tid = upsert(db, "portal_tracker", "module=:m and slug=:s", &row! {"m"=>module,"s"=>slug.clone()}, &d).await?;
            let tp = row! {"t"=>tid};
            // fields
            let mut keep = vec![];
            if let Some(Value::Array(fs)) = t.get("fields") {
                for f in fs.iter().filter_map(|v| v.as_object()) {
                    let name = s(f, "name").trim().to_string();
                    if !db::ident_ok(&name) {
                        continue;
                    }
                    let mut fd = vals(f, FIELD_KEYS);
                    fd.insert("tracker_id".into(), json!(tid));
                    fd.insert("name".into(), json!(name.clone()));
                    let fid = upsert(db, "portal_tracker_field", "tracker_id=:t and name=:n", &row! {"t"=>tid,"n"=>name}, &fd).await?;
                    keep.push(fid);
                    db.exec("delete from portal_tracker_error where field_id=:f", &row! {"f"=>fid}).await?;
                    if let Some(Value::Array(es)) = f.get("error_checks") {
                        for e in es.iter().filter_map(|v| v.as_object()) {
                            let mut ed = vals(e, ERROR_KEYS);
                            ed.insert("field_id".into(), json!(fid));
                            db.insert("portal_tracker_error", &ed).await?;
                        }
                    }
                }
            }
            db.exec(&format!("delete from portal_tracker_error where field_id in (select id from portal_tracker_field where tracker_id=:t and not (id in {}))", db::lit(&json!(keep))), &tp).await?;
            db.exec(&format!("delete from portal_tracker_field where tracker_id=:t and not (id in {})", db::lit(&json!(keep))), &tp).await?;
            // roles
            let mut role_ids = std::collections::HashMap::new();
            let mut keep = vec![];
            if let Some(Value::Array(rs)) = t.get("roles") {
                for r in rs.iter().filter_map(|v| v.as_object()) {
                    let mut rd = vals(r, ROLE_KEYS);
                    rd.insert("tracker_id".into(), json!(tid));
                    rd.insert("last_updated".into(), json!(now_str()));
                    let rid = upsert(db, "portal_tracker_role", "tracker_id=:t and name=:n", &row! {"t"=>tid,"n"=>s(r, "name")}, &rd).await?;
                    role_ids.insert(s(r, "name"), rid);
                    keep.push(rid);
                }
            }
            db.exec(&format!("delete from portal_tracker_role where tracker_id=:t and not (id in {})", db::lit(&json!(keep))), &tp).await?;
            // statuses
            let mut status_ids = std::collections::HashMap::new();
            let mut keep = vec![];
            if let Some(Value::Array(ss)) = t.get("statuses") {
                for x in ss.iter().filter_map(|v| v.as_object()) {
                    let mut sd = vals(x, STATUS_KEYS);
                    sd.insert("tracker_id".into(), json!(tid));
                    sd.insert("composite_statuses".into(), x.get("compositeStatuses").or(x.get("composite_statuses")).cloned().unwrap_or(Value::Null));
                    sd.insert("runonupdate_id".into(), page_id(db, module, x.get("runonupdate")).await);
                    let sid = upsert(db, "portal_tracker_status", "tracker_id=:t and name=:n", &row! {"t"=>tid,"n"=>s(x, "name")}, &sd).await?;
                    status_ids.insert(s(x, "name"), sid);
                    keep.push(sid);
                }
            }
            db.exec(&format!("delete from portal_tracker_status where tracker_id=:t and not (id in {})", db::lit(&json!(keep))), &tp).await?;
            // transitions (rebuilt) and their e-mails
            db.exec("delete from portal_tracker_email where tracker_id=:t", &tp).await?;
            db.exec("delete from portal_tracker_transition_portal_tracker_role where portal_tracker_transition_roles_id in (select id from portal_tracker_transition where tracker_id=:t)", &tp).await?;
            db.exec("delete from portal_tracker_transition_portal_tracker_status where portal_tracker_transition_prev_status_id in (select id from portal_tracker_transition where tracker_id=:t)", &tp).await?;
            db.exec("delete from portal_tracker_transition where tracker_id=:t", &tp).await?;
            if let Some(Value::Array(ts)) = t.get("transitions") {
                for x in ts.iter().filter_map(|v| v.as_object()) {
                    let mut td = vals(x, TRANS_KEYS);
                    td.insert("tracker_id".into(), json!(tid));
                    td.insert("postprocess_id".into(), page_id(db, module, x.get("postprocess")).await);
                    td.insert("next_status_id".into(), status_ids.get(&s(x, "next_status")).map(|v| json!(v)).unwrap_or(Value::Null));
                    let trid = db.insert("portal_tracker_transition", &td).await?;
                    if let Some(Value::Array(ps)) = x.get("prev_status") {
                        for p in ps {
                            if let Some(sid) = status_ids.get(&db::vs(p)) {
                                db.exec("insert into portal_tracker_transition_portal_tracker_status (portal_tracker_transition_prev_status_id,portal_tracker_status_id) values (:t,:s)", &row! {"t"=>trid,"s"=>sid}).await?;
                            }
                        }
                    }
                    if let Some(Value::Array(rs)) = x.get("roles") {
                        for r in rs {
                            if let Some(rid) = role_ids.get(&db::vs(r)) {
                                db.exec("insert into portal_tracker_transition_portal_tracker_role (portal_tracker_transition_roles_id,portal_tracker_role_id) values (:t,:r)", &row! {"t"=>trid,"r"=>rid}).await?;
                            }
                        }
                    }
                    if let Some(Value::Array(es)) = x.get("emails") {
                        for e in es.iter().filter_map(|v| v.as_object()) {
                            db.insert("portal_tracker_email", &row! {"tracker_id"=>tid,"transition_id"=>trid,"name"=>s(e, "name"),"emailto"=>e.get("emailto"),"emailcc"=>e.get("emailcc"),"body_id"=>page_id(db, module, e.get("body")).await}).await?;
                        }
                    }
                }
            }
            if let Some(Value::Array(es)) = t.get("emails") {
                for e in es.iter().filter_map(|v| v.as_object()) {
                    db.insert("portal_tracker_email", &row! {"tracker_id"=>tid,"name"=>s(e, "name"),"emailto"=>e.get("emailto"),"emailcc"=>e.get("emailcc"),"body_id"=>page_id(db, module, e.get("body")).await}).await?;
                }
            }
            // status e-mails, flows, indexes, tracker-level references
            if let Some(Value::Array(ss)) = t.get("statuses") {
                for x in ss.iter().filter_map(|v| v.as_object()) {
                    let en = s(x, "emailonupdate");
                    if !en.is_empty() {
                        if let (Some(sid), Ok(Some(e))) = (status_ids.get(&s(x, "name")), db.first("select id from portal_tracker_email where tracker_id=:t and name=:n", &row! {"t"=>tid,"n"=>en}).await) {
                            db.update("portal_tracker_status", *sid, &row! {"emailonupdate_id"=>i(&e, "id")}).await?;
                        }
                    }
                }
            }
            db.exec("delete from portal_tracker_flow where tracker_id=:t", &tp).await?;
            if let Some(Value::Array(fl)) = t.get("flows") {
                for x in fl.iter().filter_map(|v| v.as_object()) {
                    db.insert("portal_tracker_flow", &row! {"tracker_id"=>tid,"name"=>s(x, "name"),"fields"=>x.get("fields"),"transitions"=>x.get("transitions")}).await?;
                }
            }
            db.exec("delete from portal_tracker_index where tracker_id=:t", &tp).await?;
            if let Some(Value::Array(ix)) = t.get("indexes") {
                for x in ix.iter().filter_map(|v| v.as_object()) {
                    db.insert("portal_tracker_index", &row! {"tracker_id"=>tid,"name"=>s(x, "name"),"fields"=>x.get("fields")}).await?;
                }
            }
            let dfid = db.first("select id from portal_tracker_field where tracker_id=:t and name=:n", &row! {"t"=>tid,"n"=>s(t, "defaultfield")}).await?.and_then(|r| i(&r, "id"));
            db.update("portal_tracker", tid, &row! {"initial_status_id"=>status_ids.get(&s(t, "initial_status")).copied(),"defaultfield_id"=>dfid}).await?;
            if let Some(tk) = Tracker::by_id(db, tid).await {
                tk.updatedb(db).await?;
                let _ = tk.create_index(db).await;
            }
        }
        log.push(format!("{} trackers", trackers.len()));
    }
    // settings and schedulers: respect per-row choices from import preview
    if let Some(list) = read_json(&dir.join("settinglist.json")) {
        let mut added = 0;
        let mut updated = 0;
        for x in &list {
            let name = s(x, "name");
            let smod = { let m = s(x, "module"); if m.is_empty() { module.to_string() } else { m } };
            let key = setting_key(&smod, &name);
            let existing = db.first("select id from portal_setting where module=:m and name=:n", &row! {"m"=>smod.clone(),"n"=>name.clone()}).await?;
            if let Some(ex) = existing {
                if opts.setting_choices.get(&key).map(|v| v.as_str()) == Some("import") {
                    let mut d = vals(x, SETTING_KEYS);
                    d.insert("module".into(), json!(smod));
                    if let Some(eid) = i(&ex, "id") {
                        db.update("portal_setting", eid, &d).await?;
                        updated += 1;
                    }
                }
            } else {
                let mut d = vals(x, SETTING_KEYS);
                d.insert("module".into(), json!(smod));
                db.insert("portal_setting", &d).await?;
                added += 1;
            }
        }
        if updated > 0 {
            log.push(format!("{} new / {} updated settings", added, updated));
        } else {
            log.push(format!("{} new settings", added));
        }
    }
    if let Some(list) = read_json(&dir.join("schedulerlist.json")) {
        for x in &list {
            let sname = s(x, "name");
            let smod = { let m = s(x, "module"); if m.is_empty() { module.to_string() } else { m } };
            let key = scheduler_key(&smod, &sname);
            let existing = db.first("select id from portal_scheduler where module=:m and name=:n", &row! {"m"=>smod.clone(),"n"=>sname.clone()}).await?;
            let mut d = vals(x, SCHED_KEYS);
            d.insert("module".into(), json!(smod));
            let hr = sched_hour(x);
            if !hr.is_empty() {
                d.insert("hour".into(), json!(hr));
            }
            if let Some(ex) = existing {
                if opts.scheduler_choices.get(&key).map(|v| v.as_str()) == Some("import") {
                    if let Some(eid) = i(&ex, "id") {
                        db.update("portal_scheduler", eid, &d).await?;
                    }
                }
            } else {
                db.insert("portal_scheduler", &d).await?;
            }
        }
    }
    if opts.endpoints {
        if let Some(list) = read_json(&dir.join("endpointlist.json")) {
            for x in &list {
                let mut d = vals(x, ENDPOINT_KEYS);
                d.insert("module".into(), json!(module));
                d.insert("last_updated".into(), json!(now_str()));
                upsert(db, "portal_endpoint", "module=:m and slug=:s", &row! {"m"=>module,"s"=>s(x, "slug")}, &d).await?;
            }
        }
    }
    if opts.trees {
        if let Some(list) = read_json(&dir.join("treelist.json")) {
            let mut stats = crate::tree::ImportStats::default();
            for x in &list {
                if let Err(e) = crate::tree::import_tree(db, x, Some(module), opts.roles, &mut stats).await {
                    detail.push(format!("tree {}: {}", s(x, "name"), e));
                }
            }
            for u in &stats.users_skipped {
                detail.push(format!("tree role holder skipped, no such staff here: {}", u));
            }
            log.push(format!("{} trees ({})", list.len(), stats.summary()));
        }
    }
    if opts.menus && dir.join("menulist.json").is_file() {
        if let Err(e) = import_menu(db, module, &dir).await {
            detail.push(format!("menu: {}", e));
        } else {
            log.push("menu".into());
        }
    }
    if opts.files {
        if let Some(list) = read_json(&dir.join("filelinklist.json")) {
            let (mut done, mut missing) = (0, 0);
            let target_dir = PathBuf::from(&st.cfg.upload_dir).join(module).join("filelinks");
            for x in &list {
                let slug = s(x, "slug");
                let name = s(x, "name");
                if slug.is_empty() {
                    continue;
                }
                let src = dir.join("files").join(format!("fl_{}_{}", s(x, "id"), name.replace(' ', "_")));
                let mut d = vals(x, &["name", "slug", "allowedroles", "filegroup", "sortnum"]);
                d.insert("module".into(), json!(module));
                d.insert("last_updated".into(), json!(now_str()));
                if src.is_file() {
                    std::fs::create_dir_all(&target_dir)?;
                    let fname = crate::files::sanitize_filename(&format!("{}_{}", slug, name));
                    let target = target_dir.join(if fname.is_empty() { slug.clone() } else { fname });
                    std::fs::copy(&src, &target)?;
                    d.insert("path".into(), json!(target.to_string_lossy().to_string()));
                    d.insert("size".into(), json!(std::fs::metadata(&target).map(|m| m.len() as i64).unwrap_or(0)));
                    done += 1;
                } else {
                    missing += 1;
                    detail.push(format!("file link {}: {} is not in the zip - the link is created without a file", slug, src.strip_prefix(&dir).unwrap_or(&src).display()));
                }
                upsert(db, "file_link", "module=:m and slug=:s", &row! {"m"=>module,"s"=>slug}, &d).await?;
            }
            log.push(if missing > 0 { format!("{} file links ({} without their file)", done + missing, missing) } else { format!("{} file links", done) });
        }
    }
    if opts.roles {
        if let Some(list) = read_json(&dir.join("userroles.json")).or_else(|| read_json(&dir.join("userrolelist.json"))) {
            for x in &list {
                let staffid = ["userid", "staffid", "user"].iter().map(|k| s(x, k)).find(|v| !v.is_empty()).unwrap_or_default();
                if let Some(u) = web::find_user_by_staffid(db, &staffid).await {
                    let p = row! {"u"=>i(&u, "id"),"m"=>module,"r"=>s(x, "role")};
                    if db.count("select count(*) from user_role where user_id=:u and module=:m and role=:r", &p).await? == 0 {
                        db.insert("user_role", &row! {"user_id"=>i(&u, "id"),"module"=>module,"role"=>s(x, "role")}).await?;
                    }
                }
            }
        }
    }
    let mut msg = format!("Module imported: {}", log.join(", "));
    if legacy > 0 {
        msg.push_str(&format!(". {} page(s) still contain Groovy/GSP source and must be ported to Rhai/MiniJinja before they will run", legacy));
    }
    if translated + scripts > 0 {
        msg.push_str(&format!(". From Grails: {} template page(s) translated to MiniJinja ({} need a manual check), {} Groovy script(s) left to port - see the import log for the list", translated, flagged, scripts));
    }
    Ok((msg, detail.join("\n")))
}

pub async fn preview_settings(db: &Db, module: &str, dir: &Path) -> Vec<SettingPreview> {
    let Some(list) = read_json(&dir.join("settinglist.json")) else { return vec![] };
    let mut out = vec![];
    for x in &list {
        let sname = s(x, "name");
        let smod = { let m = s(x, "module"); if m.is_empty() { module.to_string() } else { m } };
        let itype = s(x, "type");
        let itext = s(x, "text");
        let inum = s(x, "number");
        let idate = s(x, "date_value");
        let idatum = s(x, "datum_type");
        let inc_canon = setting_canon(&itype, &itext, &inum, &idate, &idatum);
        let cur = db
            .first("select * from portal_setting where module=:m and name=:n limit 1", &row! {"m"=>smod.clone(),"n"=>sname.clone()})
            .await
            .ok()
            .flatten();
        let (status, cur_disp) = match cur {
            None => ("new", String::new()),
            Some(c) => {
                let ct = s(&c, "type");
                let ctx = s(&c, "text");
                let cn = s(&c, "number");
                let cd = s(&c, "date_value").replace(' ', "T").chars().take(19).collect::<String>();
                let cdt = s(&c, "datum_type");
                let cur_canon = setting_canon(&ct, &ctx, &cn, &cd, &cdt);
                let st = if cur_canon == inc_canon { "same" } else { "changed" };
                (st, setting_display(&ct, &ctx, &cn, &cd))
            }
        };
        out.push(SettingPreview {
            key: setting_key(&smod, &sname),
            module: smod,
            name: sname,
            status,
            current: cur_disp,
            incoming: setting_display(&itype, &itext, &inum, &idate),
        });
    }
    let rank = |st: &str| match st { "changed" => 0, "new" => 1, _ => 2 };
    out.sort_by(|a, b| rank(a.status).cmp(&rank(b.status)).then(a.name.cmp(&b.name)));
    out
}

pub async fn preview_schedulers(db: &Db, module: &str, dir: &Path) -> Vec<SchedulerPreview> {
    let Some(list) = read_json(&dir.join("schedulerlist.json")) else { return vec![] };
    let mut out = vec![];
    for x in &list {
        let sname = s(x, "name");
        let smod = { let m = s(x, "module"); if m.is_empty() { module.to_string() } else { m } };
        let islugs = s(x, "slugs");
        let ihour = sched_hour(x);
        let idow = s(x, "day_of_week");
        let idom = s(x, "day_of_month");
        let ien = b(x, "enabled");
        let icu = b(x, "catch_up");
        let inc_canon = sched_canon(&islugs, &ihour, &idow, &idom, ien, icu);
        let cur = db
            .first("select * from portal_scheduler where module=:m and name=:n limit 1", &row! {"m"=>smod.clone(),"n"=>sname.clone()})
            .await
            .ok()
            .flatten();
        let (status, cur_disp, lastrun) = match cur {
            None => ("new", String::new(), String::new()),
            Some(c) => {
                let cs = s(&c, "slugs");
                let ch = sched_hour(&c);
                let cdw = s(&c, "day_of_week");
                let cdm = s(&c, "day_of_month");
                let cen = b(&c, "enabled");
                let ccu = b(&c, "catch_up");
                let cur_canon = sched_canon(&cs, &ch, &cdw, &cdm, cen, ccu);
                let st = if cur_canon == inc_canon { "same" } else { "changed" };
                (st, sched_display(&cs, &ch, &cdw, &cdm, cen, ccu), s(&c, "lastrun").chars().take(16).collect())
            }
        };
        out.push(SchedulerPreview {
            key: scheduler_key(&smod, &sname),
            module: smod,
            name: sname,
            status,
            current: cur_disp,
            incoming: sched_display(&islugs, &ihour, &idow, &idom, ien, icu),
            lastrun,
        });
    }
    let rank = |st: &str| match st { "changed" => 0, "new" => 1, _ => 2 };
    out.sort_by(|a, b| rank(a.status).cmp(&rank(b.status)).then(a.name.cmp(&b.name)));
    out
}

fn walk_rel_files(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(rel) = p.strip_prefix(root) {
                out.insert(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out
}

fn unified_diff_lines(a: &[&str], b: &[&str]) -> String {
    // Simple LCS diff for moderate files; falls back to full hunk for very large files
    let (n, m) = (a.len(), b.len());
    if n * m > 2_000_000 {
        let mut out = format!("@@ -1,{} +1,{} @@\n", n, m);
        for l in a.iter().take(200) {
            out.push('-');
            out.push_str(l);
            out.push('\n');
        }
        if n > 200 {
            out.push_str(&format!("-... ({} more lines)\n", n - 200));
        }
        for l in b.iter().take(200) {
            out.push('+');
            out.push_str(l);
            out.push('\n');
        }
        if m > 200 {
            out.push_str(&format!("+... ({} more lines)\n", m - 200));
        }
        return out;
    }
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] { dp[i + 1][j + 1] + 1 } else { dp[i + 1][j].max(dp[i][j + 1]) };
        }
    }
    let mut ops: Vec<(char, &str)> = vec![];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(('-', a[i]));
            i += 1;
        } else {
            ops.push(('+', b[j]));
            j += 1;
        }
    }
    while i < n {
        ops.push(('-', a[i]));
        i += 1;
    }
    while j < m {
        ops.push(('+', b[j]));
        j += 1;
    }
    // Emit hunks with 3 lines of context
    let ctx = 3usize;
    let mut keep = vec![false; ops.len()];
    for (idx, (tag, _)) in ops.iter().enumerate() {
        if *tag != ' ' {
            let lo = idx.saturating_sub(ctx);
            let hi = (idx + ctx + 1).min(ops.len());
            for k in lo..hi {
                keep[k] = true;
            }
        }
    }
    let mut out = String::new();
    let mut k = 0;
    let (mut la, mut lb) = (1usize, 1usize);
    while k < ops.len() {
        if !keep[k] {
            match ops[k].0 {
                ' ' => { la += 1; lb += 1; }
                '-' => la += 1,
                '+' => lb += 1,
                _ => {}
            }
            k += 1;
            continue;
        }
        let start_a = la;
        let start_b = lb;
        let mut hunk = String::new();
        let (mut ca, mut cb) = (0usize, 0usize);
        while k < ops.len() && keep[k] {
            let (tag, line) = ops[k];
            hunk.push(tag);
            hunk.push_str(line);
            hunk.push('\n');
            match tag {
                ' ' => { la += 1; lb += 1; ca += 1; cb += 1; }
                '-' => { la += 1; ca += 1; }
                '+' => { lb += 1; cb += 1; }
                _ => {}
            }
            k += 1;
        }
        out.push_str(&format!("@@ -{},{} +{},{} @@\n{}", start_a, ca, start_b, cb, hunk));
    }
    out
}

pub fn diff_module_folders(old_dir: &Path, new_dir: &Path) -> String {
    let mut all = walk_rel_files(old_dir);
    all.extend(walk_rel_files(new_dir));
    let mut out = String::new();
    for rel in all {
        let pa = old_dir.join(&rel);
        let pb = new_dir.join(&rel);
        let ba = std::fs::read(&pa).ok();
        let bb = std::fs::read(&pb).ok();
        if ba == bb {
            continue;
        }
        let sa = ba.as_deref().map(|b| std::str::from_utf8(b).ok()).flatten();
        let sb = bb.as_deref().map(|b| std::str::from_utf8(b).ok()).flatten();
        if (ba.is_some() && sa.is_none()) || (bb.is_some() && sb.is_none()) {
            out.push_str(&format!("Binary file {} differs\n", rel));
            continue;
        }
        let la: Vec<&str> = sa.map(|t| t.lines().collect()).unwrap_or_default();
        let lb: Vec<&str> = sb.map(|t| t.lines().collect()).unwrap_or_default();
        out.push_str(&format!("--- current/{}\n+++ incoming/{}\n", rel, rel));
        out.push_str(&unified_diff_lines(&la, &lb));
    }
    out
}

pub fn zip_dir(dir: &Path) -> Result<Vec<u8>> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let base = dir.file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d)? {
                let p = e?.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let rel = p.strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
                    zw.start_file(format!("{}/{}", base, rel), opts)?;
                    zw.write_all(&std::fs::read(&p)?)?;
                }
            }
        }
        zw.finish()?;
    }
    Ok(buf.into_inner())
}

/// The module name a Grails portal zip gets: its file name, minus `.zip` and
/// the " (1)" / "-1" a browser adds to repeated downloads.
fn module_from_filename(filename: &str) -> String {
    let base = filename.rsplit(['/', '\\']).next().unwrap_or("");
    let mut n = base.strip_suffix(".zip").or_else(|| base.strip_suffix(".ZIP")).unwrap_or(base).trim().to_string();
    if let Some(i) = n.rfind(" (") {
        if n.ends_with(')') && n[i + 2..n.len() - 1].chars().all(|c| c.is_ascii_digit()) {
            n.truncate(i);
        }
    }
    n.trim().replace(' ', "_")
}

/// Unpack an uploaded module zip into the module folder. Returns the module
/// name and whether the zip came from the Grails portal. Two layouts are accepted:
/// - this portal's: one folder named after the module, holding the definitions;
/// - the Grails portal's: `pagelist.json`, `trackerlist.json` and `pages/` at the
///   top of the zip, the module named after the zip file.
/// The module folder is replaced as a whole, so page sources from an earlier
/// export (which would win over the uploaded `.gsp` ones) do not linger.
pub fn unzip_module(module_dir: &Path, filename: &str, bytes: &[u8]) -> Result<(String, bool)> {
    let mut za = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let defs = ["pagelist.json", "trackerlist.json", "settinglist.json", "treelist.json", "schedulerlist.json", "endpointlist.json", "filelinklist.json"];
    // (path parts, index) of every file worth keeping
    let mut entries: Vec<(Vec<String>, usize)> = vec![];
    for n in 0..za.len() {
        let f = za.by_index(n)?;
        if f.is_dir() {
            continue;
        }
        // zips made on Windows (as the Grails portal's often are) use "pages\x.gsp"
        let raw = f.name().replace('\\', "/");
        let parts: Vec<String> = raw.split('/').filter(|p| !p.is_empty() && *p != ".").map(String::from).collect();
        if parts.is_empty() || parts.iter().any(|p| p == ".." || p.contains(':')) || raw.starts_with('/') {
            continue;
        }
        // editor/OS clutter such as .claude/, .git/, __MACOSX/
        if parts.iter().any(|p| p.starts_with('.') || p == "__MACOSX") {
            continue;
        }
        entries.push((parts, n));
    }
    let at_root = entries.iter().any(|(p, _)| p.len() == 1 && defs.contains(&p[0].as_str()));
    let (module, strip) = if at_root {
        (module_from_filename(filename), 0)
    } else {
        let mut tops: Vec<&String> = entries.iter().filter(|(p, _)| p.len() >= 2).map(|(p, _)| &p[0]).collect();
        tops.sort();
        tops.dedup();
        match tops.as_slice() {
            [one] if entries.iter().any(|(p, _)| p.len() == 2 && &p[0] == *one && defs.contains(&p[1].as_str())) => ((*one).clone(), 1),
            _ => return Err(anyhow!("This does not look like a module zip: no pagelist.json, trackerlist.json or other module list at its top, nor in a single module folder")),
        }
    };
    if module.is_empty() || !module.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(anyhow!("'{}' is not a valid module name (letters, numbers and _ only) - rename the zip file to the module name", module));
    }
    let base = module_dir.to_path_buf();
    std::fs::create_dir_all(&base)?;
    let tmp = base.join(format!(".upload-{}-{}", module, std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let res = (|| -> Result<()> {
        for (parts, n) in &entries {
            if parts.len() <= strip {
                continue;
            }
            let target = parts[strip..].iter().fold(tmp.clone(), |p, x| p.join(x));
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut data = vec![];
            za.by_index(*n)?.read_to_end(&mut data)?;
            std::fs::write(target, data)?;
        }
        let dest = base.join(&module);
        if dest.exists() {
            std::fs::remove_dir_all(&dest)?;
        }
        std::fs::rename(&tmp, &dest)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    res.map(|_| (module, at_root))
}

async fn module_of(ctx: &Ctx, id: i64) -> Option<String> {
    ctx.db().get("portal_module", id).await.ok().flatten().map(|m| s(&m, "name")).filter(|m| !m.is_empty())
}

pub async fn action(mut ctx: Ctx, act: &str, id: Option<i64>) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    match act {
        "update_module_list" => {
            if !ctx.is_admin() {
                return ctx.status(StatusCode::FORBIDDEN, "Only administrators can do that").await;
            }
            let mut names: Vec<String> = vec![];
            let pq = format!("select distinct module from {}", db::page_t());
            for q in [pq.as_str(), "select distinct module from portal_tracker", "select distinct module from portal_setting", "select distinct module from user_role"] {
                names.extend(ctx.db().rows(q, &db::no_params()).await.unwrap_or_default().iter().map(|r| s(r, "module")));
            }
            if let Ok(rd) = std::fs::read_dir(&ctx.st.cfg.module_dir) {
                for e in rd.flatten() {
                    if e.path().is_dir() {
                        names.push(e.file_name().to_string_lossy().to_string());
                    }
                }
            }
            names.sort();
            names.dedup();
            for n in names.iter().filter(|n| !n.is_empty()) {
                crate::admin::ensure_module(ctx.db(), n).await;
            }
            ctx.flash("Module list updated");
            ctx.redirect("/portalModule/index").await
        }
        "upload" => {
            if !ctx.is_admin() {
                return ctx.status(StatusCode::FORBIDDEN, "Only administrators can do that").await;
            }
            if ctx.method == "POST" {
                let Some(up) = ctx.files.remove("file") else {
                    ctx.flash("No file selected");
                    return ctx.redirect("/portalModule/upload").await;
                };
                let module_dir = PathBuf::from(&ctx.st.cfg.module_dir);
                let filename = up.filename.clone();
                let bytes = up.read_all().await.unwrap_or_default();
                let with_roles = ctx.params.contains_key("staff_on");
                let res = match tokio::task::spawn_blocking(move || unzip_module(&module_dir, &filename, &bytes)).await {
                    Ok(Ok((m, grails))) => {
                        let r = import(&ctx.st, &m, with_roles).await;
                        r.map(|(msg, detail)| (m, if grails { format!("Grails portal zip upload: {}", msg) } else { format!("zip upload: {}", msg) }, detail))
                    }
                    Ok(Err(e)) => Err(e),
                    Err(e) => Err(anyhow!("{}", e)),
                };
                match res {
                    Ok((m, msg, detail)) => {
                        let _ = ctx.db().insert("portal_module_import_log", &row! {"module"=>m,"staffid"=>ctx.user.as_ref().map(|u| s(u, "userid")),"staffname"=>ctx.user.as_ref().map(|u| s(u, "name")),"remarks"=>msg.clone(),"diff"=>detail,"date_created"=>now_str()}).await;
                        ctx.flash(&msg);
                        return match crate::admin::module_url(ctx.db(), &m).await {
                            Some(u) => ctx.redirect(&u).await,
                            None => ctx.redirect("/portalModule/index").await,
                        };
                    }
                    Err(e) => ctx.flash(&format!("Import failed: {}", e)),
                }
                return ctx.redirect("/portalModule/upload").await;
            }
            let h = format!("<div class='container'><h1>Upload module zip</h1><p>Upload a module zip exported from this portal or from the Grails portal. Grails portal zips are recognised automatically: the module is named after the zip file, template pages are translated to MiniJinja, and Groovy scripts are kept as comments for porting by hand. The module's import log lists every page that needs a look. An existing module with the same name is overwritten.</p><form method='post' enctype='multipart/form-data' action='/portalModule/upload'>{}<input type='file' name='file' accept='.zip' class='form-control mb-2' required/><label class='d-block mb-2'><input type='checkbox' name='staff_on'/> Also import user roles</label><button class='btn btn-primary'>Upload and import</button></form></div>", ctx.csrf_field());
            ctx.html("Upload module", h).await
        }
        "export" | "import" | "download" => {
            let Some(m) = (match id {
                Some(x) => module_of(&ctx, x).await,
                None => None,
            }) else {
                return ctx.status(StatusCode::NOT_FOUND, "Module not found").await;
            };
            let allowed = ctx.is_admin() || ctx.user.as_ref().map(|_| true).unwrap_or(false) && web::developer_list(ctx.db(), ctx.user.as_ref().unwrap()).await.contains(&m);
            if !allowed || (act != "download" && ctx.method != "POST") {
                return ctx.status(StatusCode::FORBIDDEN, "Only developers of this module can do that").await;
            }
            let back = format!("/portalModule/show/{}", id.unwrap_or(0));
            if act == "import" {
                // real administrators only, never a switched session
                if ctx.sess.contains_key("adminlink") {
                    ctx.flash("Restore your own account before importing a module");
                    return ctx.redirect(&back).await;
                }
                match import(&ctx.st, &m, ctx.params.contains_key("staff_on")).await {
                    Ok((msg, detail)) => {
                        let _ = ctx.db().insert("portal_module_import_log", &row! {"module"=>m,"staffid"=>ctx.user.as_ref().map(|u| s(u, "userid")),"staffname"=>ctx.user.as_ref().map(|u| s(u, "name")),"remarks"=>msg.clone(),"diff"=>detail,"date_created"=>now_str()}).await;
                        ctx.flash(&msg);
                    }
                    Err(e) => ctx.flash(&format!("Import failed: {}", e)),
                }
                return ctx.redirect(&back).await;
            }
            match export(&ctx.st, &m, true).await {
                Ok(dir) => {
                    if act == "download" {
                        return match tokio::task::spawn_blocking(move || zip_dir(&dir)).await {
                            Ok(Ok(bytes)) => ctx.bytes("application/zip", Some(&format!("{}.zip", m)), bytes, false).await,
                            _ => ctx.status(StatusCode::INTERNAL_SERVER_ERROR, "Could not build the zip").await,
                        };
                    }
                    ctx.flash(&format!("Module exported to {}", dir.display()));
                }
                Err(e) => ctx.flash(&format!("Export failed: {}", esc(&e.to_string()))),
            }
            ctx.redirect(&back).await
        }
        _ => ctx.status(StatusCode::NOT_FOUND, "Unknown action").await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_zip(files: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            for (n, c) in files {
                zw.start_file(*n, zip::write::SimpleFileOptions::default()).unwrap();
                zw.write_all(c.as_bytes()).unwrap();
            }
            zw.finish().unwrap();
        }
        buf.into_inner()
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rp-unzip-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn filename_to_module() {
        assert_eq!(module_from_filename("akpk.zip"), "akpk");
        assert_eq!(module_from_filename("C:\\fakepath\\akpk (2).zip"), "akpk");
        assert_eq!(module_from_filename("My Module.zip"), "My_Module");
    }

    #[test]
    fn grails_layout_named_after_file_and_replaces_old_folder() {
        let base = tmpdir("g5");
        std::fs::create_dir_all(base.join("akpk/pages")).unwrap();
        std::fs::write(base.join("akpk/pages/content_home.html"), "stale").unwrap();
        let z = make_zip(&[(".claude/settings.local.json", "{}"), ("pagelist.json", "[]"), ("trackerlist.json", "[]"), ("pages/content_home.gsp", "<g:if/>")]);
        let (m, grails) = unzip_module(&base, "akpk (1).zip", &z).unwrap();
        let w = make_zip(&[("pagelist.json", "[]"), ("pages\\content_x.gsp", "a"), ("..\\evil.txt", "b")]);
        assert_eq!(unzip_module(&base, "win.zip", &w).unwrap().0, "win");
        assert!(base.join("win/pages/content_x.gsp").is_file());
        assert!(!base.join("evil.txt").exists());
        assert_eq!((m.as_str(), grails), ("akpk", true));
        assert!(base.join("akpk/pagelist.json").is_file());
        assert!(base.join("akpk/pages/content_home.gsp").is_file());
        assert!(!base.join("akpk/pages/content_home.html").exists());
        assert!(!base.join("akpk/.claude").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn own_layout_uses_folder_name() {
        let base = tmpdir("own");
        let z = make_zip(&[("crm/pagelist.json", "[]"), ("crm/pages/content_a.html", "x")]);
        assert_eq!(unzip_module(&base, "whatever.zip", &z).unwrap(), ("crm".to_string(), false));
        assert!(base.join("crm/pages/content_a.html").is_file());
        let bad = make_zip(&[("readme.txt", "x"), ("a/b.txt", "y")]);
        assert!(unzip_module(&base, "x.zip", &bad).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn menu_stamp_preserves_foreign_keys() {
        assert_eq!(menu_stamp("ahli", Some("bi-people"), "adminonly=true;icon=old;module=other"), "module=ahli;icon=bi-people;adminonly=true");
        assert_eq!(menu_stamp("ahli", None, ""), "module=ahli");
    }

    #[test]
    fn diff_folders_detects_changes() {
        let a = tmpdir("diff_a");
        let b = tmpdir("diff_b");
        std::fs::create_dir_all(a.join("pages")).unwrap();
        std::fs::create_dir_all(b.join("pages")).unwrap();
        std::fs::write(a.join("pages/content_home.html"), "<h1>Old</h1>\n<p>same</p>\n").unwrap();
        std::fs::write(b.join("pages/content_home.html"), "<h1>New</h1>\n<p>same</p>\n").unwrap();
        let d = diff_module_folders(&a, &b);
        assert!(d.contains("--- current/pages/content_home.html"));
        assert!(d.contains("+++ incoming/pages/content_home.html"));
        assert!(d.contains("-<h1>Old</h1>"));
        assert!(d.contains("+<h1>New</h1>"));
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn real_grails_zips() {
        let dir = Path::new("/home/abdza/data/cimb/g5portal/modulemigration");
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let base = tmpdir("real");
        let (mut ok, mut bad) = (0, vec![]);
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if !n.ends_with(".zip") { continue; }
            match unzip_module(&base, &n, &std::fs::read(e.path()).unwrap()) {
                Ok((m, true)) if std::fs::read_dir(base.join(&m)).map(|d| d.flatten().all(|e| !e.file_name().to_string_lossy().contains('\\'))).unwrap_or(false) => ok += 1,
                other => bad.push(format!("{}: {:?}", n, other.map(|x| x.0))),
            }
        }
        eprintln!("real grails zips: {} ok, failures: {:?}", ok, bad);
        let _ = std::fs::remove_dir_all(&base);
        assert!(bad.is_empty());
    }
}
