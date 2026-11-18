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
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

fn folder(st: &St, module: &str) -> PathBuf {
    PathBuf::from(&st.cfg.module_dir).join(module)
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
        Some(x) => db.get("portal_page", x).await.ok().flatten().map(|p| json!(s(&p, "slug"))).unwrap_or(Value::Null),
        None => Value::Null,
    }
}

pub async fn export(st: &St, module: &str, with_roles: bool) -> Result<PathBuf> {
    let db = &st.db;
    let dir = folder(st, module);
    std::fs::create_dir_all(dir.join("pages"))?;
    // pages
    let pages = db.rows("select * from portal_page where module=:m order by slug", &row! {"m"=>module}).await?;
    let mut plist = vec![];
    for p in &pages {
        let mut o = pick(p, PAGE_KEYS);
        o.insert("lastUpdated".into(), p.get("last_updated").cloned().unwrap_or(Value::Null));
        let ds = db.rows("select name, query, return_one from portal_page_data where page_id=:p order by id", &row! {"p"=>i(p, "id")}).await?;
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
    write_json(&dir.join("pagelist.json"), &Value::Array(plist))?;
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
    write_json(&dir.join("trackerlist.json"), &Value::Array(tlist))?;
    let settings = db.rows("select * from portal_setting where module=:m order by name", &row! {"m"=>module}).await?;
    write_json(&dir.join("settinglist.json"), &Value::Array(settings.iter().map(|x| Value::Object(pick(x, SETTING_KEYS))).collect()))?;
    let scheds = db.rows("select * from portal_scheduler where module=:m order by name", &row! {"m"=>module}).await?;
    write_json(&dir.join("schedulerlist.json"), &Value::Array(scheds.iter().map(|x| Value::Object(pick(x, SCHED_KEYS))).collect()))?;
    let eps = db.rows("select * from portal_endpoint where module=:m order by slug", &row! {"m"=>module}).await?;
    write_json(&dir.join("endpointlist.json"), &Value::Array(eps.iter().map(|x| Value::Object(pick(x, ENDPOINT_KEYS))).collect()))?;
    let trees = db.rows("select * from tree where module=:m order by name", &row! {"m"=>module}).await?;
    let mut tl = vec![];
    for t in trees {
        let nodes = db.rows("select id, parent_id, name, slug, domain, domainid, mainrole, hiderole, data, disabled from tree_node where tree_id=:t order by lft nulls last, id", &row! {"t"=>i(&t, "id")}).await?;
        tl.push(json!({"name": s(&t, "name"), "module": module, "nodes": nodes}));
    }
    write_json(&dir.join("treelist.json"), &Value::Array(tl))?;
    if with_roles {
        let roles = db.rows("select u.staffid, r.role from user_role r join iap_user u on u.id=r.user_id where r.module=:m order by u.staffid, r.role", &row! {"m"=>module}).await?;
        write_json(&dir.join("userroles.json"), &Value::Array(roles.into_iter().map(Value::Object).collect()))?;
    }
    Ok(dir)
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
            let pid = upsert(db, "portal_page", "module=:m and slug=:s", &row! {"m"=>module,"s"=>slug.clone()}, &d).await?;
            db.exec("delete from portal_page_data where page_id=:p", &row! {"p"=>pid}).await?;
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
                    db.insert("portal_page_data", &row! {"page_id"=>pid,"name"=>s(x, "name"),"query"=>q,"return_one"=>b(x, "return_one")}).await?;
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
    // settings and schedulers: only add what is missing, never overwrite local values
    if let Some(list) = read_json(&dir.join("settinglist.json")) {
        let mut added = 0;
        for x in &list {
            let name = s(x, "name");
            if db.count("select count(*) from portal_setting where module=:m and name=:n", &row! {"m"=>module,"n"=>name}).await? == 0 {
                let mut d = vals(x, SETTING_KEYS);
                d.insert("module".into(), json!(module));
                db.insert("portal_setting", &d).await?;
                added += 1;
            }
        }
        log.push(format!("{} new settings", added));
    }
    if let Some(list) = read_json(&dir.join("schedulerlist.json")) {
        for x in &list {
            if db.count("select count(*) from portal_scheduler where module=:m and name=:n", &row! {"m"=>module,"n"=>s(x, "name")}).await? == 0 {
                let mut d = vals(x, SCHED_KEYS);
                d.insert("module".into(), json!(module));
                db.insert("portal_scheduler", &d).await?;
            }
        }
    }
    if let Some(list) = read_json(&dir.join("endpointlist.json")) {
        for x in &list {
            let mut d = vals(x, ENDPOINT_KEYS);
            d.insert("module".into(), json!(module));
            d.insert("last_updated".into(), json!(now_str()));
            upsert(db, "portal_endpoint", "module=:m and slug=:s", &row! {"m"=>module,"s"=>s(x, "slug")}, &d).await?;
        }
    }
    if let Some(list) = read_json(&dir.join("treelist.json")) {
        for x in &list {
            let name = s(x, "name");
            if db.count("select count(*) from tree where module=:m and name=:n", &row! {"m"=>module,"n"=>name.clone()}).await? > 0 {
                continue; // trees are edited locally; never overwrite
            }
            let tree_id = db.insert("tree", &row! {"module"=>module,"name"=>name}).await?;
            let mut idmap = std::collections::HashMap::new();
            if let Some(Value::Array(nodes)) = x.get("nodes") {
                for n in nodes.iter().filter_map(|v| v.as_object()) {
                    let mut d = vals(n, &["name", "slug", "domain", "domainid", "mainrole", "hiderole", "data", "disabled"]);
                    d.insert("tree_id".into(), json!(tree_id));
                    d.insert("parent_id".into(), i(n, "parent_id").and_then(|p| idmap.get(&p).copied()).map(|v: i64| json!(v)).unwrap_or(Value::Null));
                    let nid = db.insert("tree_node", &d).await?;
                    idmap.insert(i(n, "id").unwrap_or(0), nid);
                }
            }
            crate::tree::rebuild(db, tree_id).await?;
        }
    }
    if with_roles {
        if let Some(list) = read_json(&dir.join("userroles.json")).or_else(|| read_json(&dir.join("userrolelist.json"))) {
            for x in &list {
                let staffid = if s(x, "staffid").is_empty() { s(x, "user") } else { s(x, "staffid") };
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

fn zip_dir(dir: &Path) -> Result<Vec<u8>> {
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

/// Unpack an uploaded module zip into the module folder. Returns the module name.
fn unzip_module(st: &St, bytes: &[u8]) -> Result<String> {
    let mut za = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let mut module = String::new();
    for n in 0..za.len() {
        let mut f = za.by_index(n)?;
        let Some(name) = f.enclosed_name() else { continue };
        let parts: Vec<String> = name.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect();
        if parts.len() < 2 || f.is_dir() {
            continue;
        }
        if module.is_empty() {
            module = parts[0].clone();
        }
        if parts[0] != module || !module.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(anyhow!("The zip must contain exactly one module folder with a simple name"));
        }
        let target = PathBuf::from(&st.cfg.module_dir).join(name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut data = vec![];
        f.read_to_end(&mut data)?;
        std::fs::write(target, data)?;
    }
    if module.is_empty() {
        return Err(anyhow!("No module folder found in the zip"));
    }
    Ok(module)
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
            for q in ["select distinct module from portal_page", "select distinct module from portal_tracker", "select distinct module from portal_setting", "select distinct module from user_role"] {
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
                let st = ctx.st.clone();
                let bytes = up.read_all().await.unwrap_or_default();
                let res = match tokio::task::spawn_blocking(move || unzip_module(&st, &bytes)).await {
                    Ok(Ok(m)) => {
                        let r = import(&ctx.st, &m, false).await;
                        r.map(|(msg, detail)| (m, msg, detail))
                    }
                    Ok(Err(e)) => Err(e),
                    Err(e) => Err(anyhow!("{}", e)),
                };
                match res {
                    Ok((m, msg, detail)) => {
                        let _ = ctx.db().insert("portal_module_import_log", &row! {"module"=>m,"staffid"=>ctx.user.as_ref().map(|u| s(u, "staffid")),"staffname"=>ctx.user.as_ref().map(|u| s(u, "emp_name")),"remarks"=>format!("zip upload: {}", msg),"diff"=>detail,"date_created"=>now_str()}).await;
                        ctx.flash(&msg);
                    }
                    Err(e) => ctx.flash(&format!("Import failed: {}", e)),
                }
                return ctx.redirect("/portalModule/index").await;
            }
            let h = format!("<div class='container'><h1>Upload module zip</h1><p>The zip must contain one folder named after the module, with <code>pagelist.json</code>, <code>trackerlist.json</code> and a <code>pages</code> folder.</p><form method='post' enctype='multipart/form-data' action='/portalModule/upload'>{}<input type='file' name='file' accept='.zip' class='form-control mb-2' required/><button class='btn btn-primary'>Upload and import</button></form></div>", ctx.csrf_field());
            ctx.html("Upload module", h).await
        }
        "g5list" | "g5import" => {
            if !ctx.is_admin() || ctx.sess.contains_key("adminlink") {
                return ctx.status(StatusCode::FORBIDDEN, "Only administrators can do that").await;
            }
            let base = PathBuf::from(&ctx.st.cfg.g5_module_dir);
            if ctx.st.cfg.g5_module_dir.is_empty() || !base.is_dir() {
                let h = "<div class='container'><h1>Import from the Grails portal</h1><p>Set <code>g5_module_dir</code> in <code>config.toml</code> to the Grails portal's <code>modulemigration</code> folder and restart. The folder is only read, never written.</p></div>".to_string();
                return ctx.html("Import from Grails", h).await;
            }
            if act == "g5import" {
                let name = ctx.p("name");
                let dir = base.join(&name);
                if ctx.method != "POST" || name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') || !dir.join("pagelist.json").is_file() && !dir.join("trackerlist.json").is_file() {
                    return ctx.status(StatusCode::BAD_REQUEST, "No such module folder").await;
                }
                let to = match import_from(&ctx.st, &name, &dir, ctx.params.contains_key("staff_on")).await {
                    Ok((msg, detail)) => {
                        let _ = ctx.db().insert("portal_module_import_log", &row! {"module"=>name.clone(),"staffid"=>ctx.user.as_ref().map(|u| s(u, "staffid")),"staffname"=>ctx.user.as_ref().map(|u| s(u, "emp_name")),"remarks"=>format!("from Grails: {}", msg),"diff"=>detail,"date_created"=>now_str()}).await;
                        ctx.flash(&msg);
                        crate::admin::module_url(ctx.db(), &name).await.unwrap_or_else(|| "/portalModule/index".into())
                    }
                    Err(e) => {
                        ctx.flash(&format!("Import failed: {}", e));
                        "/portalModule/g5list".into()
                    }
                };
                return ctx.redirect(&to).await;
            }
            let mut names: Vec<String> = std::fs::read_dir(&base).map(|rd| rd.flatten().filter(|e| e.path().join("pagelist.json").is_file() || e.path().join("trackerlist.json").is_file()).map(|e| e.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default();
            names.sort_by_key(|n| n.to_lowercase());
            let have: Vec<String> = ctx.db().rows("select name from portal_module", &db::no_params()).await.unwrap_or_default().iter().map(|r| s(r, "name")).collect();
            let csrf = ctx.csrf_field();
            let mut h = format!("<div class='container-fluid'><h1>Import from the Grails portal</h1><p>Modules found in <code>{}</code> (read only). Importing creates or overwrites the module's trackers and pages here. Template pages are translated to MiniJinja; runable pages and preprocess scripts stay Groovy, commented out, for porting by hand. The import log lists every page that needs a look.</p><table class='table table-sm table-striped'><thead><tr><th>Module</th><th>Trackers</th><th>Pages</th><th>Here</th><th></th></tr></thead><tbody>", esc(&base.display().to_string()));
            for n in names {
                let count = |f: &str| read_json(&base.join(&n).join(f)).map(|l| l.len()).unwrap_or(0);
                let here = if have.contains(&n) { match crate::admin::module_url(ctx.db(), &n).await { Some(u) => format!("<a href='{}'>imported</a>", u), None => String::new() } } else { String::new() };
                h.push_str(&format!("<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><form method='post' action='/portalModule/g5import' style='margin:0' onsubmit=\"return confirm('Import module {} from the Grails portal? Its trackers and pages here are overwritten.')\">{}<input type='hidden' name='name' value='{}'/><label class='me-2'><input type='checkbox' name='staff_on'/> user roles</label><button class='btn btn-sm btn-primary'>Import</button></form></td></tr>", esc(&n), count("trackerlist.json"), count("pagelist.json"), here, esc(&n), csrf, esc(&n)));
            }
            h.push_str("</tbody></table></div>");
            ctx.html("Import from Grails", h).await
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
                        let _ = ctx.db().insert("portal_module_import_log", &row! {"module"=>m,"staffid"=>ctx.user.as_ref().map(|u| s(u, "staffid")),"staffname"=>ctx.user.as_ref().map(|u| s(u, "emp_name")),"remarks"=>msg.clone(),"diff"=>detail,"date_created"=>now_str()}).await;
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
