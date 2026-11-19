//! Dynamic page language: Rhai scripts (preprocess, runable pages,
//! postprocess, field defaults/options, conditions) and MiniJinja templates
//! (page content, e-mail bodies, role rules). Both get the same portal API.

use crate::db::{self, i, s, vs, Db, Row};
use crate::row;
use crate::tracker::Tracker;
use crate::web::{self, St, Upload};
use anyhow::{anyhow, Result};
use minijinja::value::{Kwargs, Rest, Value as MV};
use rhai::{Dynamic, Engine, EvalAltResult, Scope};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub struct Inner {
    pub st: St,
    pub user: Option<Row>,
    pub params: Row,
    pub csrf: String,
    pub path: String,
    cache: Mutex<HashMap<String, Value>>,
    /// tracker context for tags used without arguments: (module, slug, record id, transition)
    pub cur: Mutex<Option<(String, String, Option<i64>, Option<String>)>>,
    pub files: Mutex<HashMap<String, Upload>>,
    depth: AtomicUsize,
    /// the visitor's http session (minus the portal's own keys), readable and writable by
    /// scripts through session_get / session_set; written back after the request
    pub sess: Mutex<Row>,
    /// scheme://host[:port] the request came in on (empty outside a request)
    pub base: Mutex<String>,
}

/// Session keys the portal itself owns; scripts can neither read nor change them.
pub const SESSION_RESERVED: &[&str] = &["userid", "csrf", "flash"];


pub struct Sc {
    pub inner: Arc<Inner>,
    engine: Engine,
}

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(x) => !x.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn d2j(d: &Dynamic) -> Value {
    rhai::serde::from_dynamic::<Value>(d).unwrap_or(Value::Null)
}
fn j2d(v: &Value) -> Dynamic {
    rhai::serde::to_dynamic(v).unwrap_or(Dynamic::UNIT)
}
type RR = std::result::Result<Dynamic, Box<EvalAltResult>>;
fn rr(r: Result<Value>) -> RR {
    match r {
        Ok(v) => Ok(j2d(&v)),
        Err(e) => Err(e.to_string().into()),
    }
}
/// Query parameters as Groovy's Sql takes them: a map for `:name` placeholders,
/// or a list for `?` ones (turned into `:_p1`, `:_p2`... outside quoted text).
fn sql_params(q: &str, p: Value) -> (String, Row) {
    let Value::Array(list) = p else { return (q.to_string(), obj(p)) };
    let mut out = String::with_capacity(q.len() + 8);
    let mut params = Row::new();
    let (mut n, mut quote) = (0usize, None::<char>);
    for c in q.chars() {
        match quote {
            Some(qc) => {
                if c == qc {
                    quote = None;
                }
                out.push(c);
            }
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                out.push(c);
            }
            None if c == '?' => {
                n += 1;
                out.push_str(&format!(":_p{}", n));
                params.insert(format!("_p{}", n), list.get(n - 1).cloned().unwrap_or(Value::Null));
            }
            None => out.push(c),
        }
    }
    (out, params)
}

fn obj(v: Value) -> Row {
    match v {
        Value::Object(o) => o,
        _ => Row::new(),
    }
}

pub fn parse_dt(x: &str) -> Option<chrono::NaiveDateTime> {
    let x = x.trim().replace('T', " ");
    for f in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(&x, f) {
            return Some(d);
        }
    }
    chrono::NaiveDate::parse_from_str(&x, "%Y-%m-%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0))
}

pub fn fmt_number(v: &Value, decimals: usize) -> String {
    let f = match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(x) => match x.trim().parse::<f64>() {
            Ok(f) => f,
            Err(_) => return x.clone(),
        },
        _ => return String::new(),
    };
    let raw = format!("{:.*}", decimals, f.abs());
    let (int, frac) = raw.split_once('.').map(|(a, b)| (a.to_string(), Some(b.to_string()))).unwrap_or((raw.clone(), None));
    let mut out = String::new();
    for (n, c) in int.chars().enumerate() {
        if n > 0 && (int.len() - n) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if let Some(fr) = frac {
        out.push('.');
        out.push_str(&fr);
    }
    if f < 0.0 { format!("-{}", out) } else { out }
}

/// Field options are scripts, but a plain list is by far the common case;
/// accept `["A","B"]`, Groovy-style `['A','B']` / `['k':'v']` and `A,B`.
pub fn lenient_options(code: &str) -> Value {
    let c = code.trim();
    if let Ok(v) = serde_json::from_str::<Value>(c) {
        if v.is_array() || v.is_object() {
            return v;
        }
    }
    let inner = if c.starts_with('[') && c.ends_with(']') { &c[1..c.len() - 1] } else { c };
    let mut items: Vec<String> = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in inner.chars() {
        match quote {
            Some(q) if ch == q => {
                quote = None;
                cur.push(ch);
            }
            Some(_) => cur.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                cur.push(ch);
            }
            None if ch == ',' || ch == '\n' => {
                items.push(cur.clone());
                cur.clear();
            }
            None => cur.push(ch),
        }
    }
    items.push(cur);
    let unq = |x: &str| x.trim().trim_matches(|c| c == '\'' || c == '"').to_string();
    let items: Vec<String> = items.into_iter().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
    let is_map = c.starts_with('[') && !items.is_empty() && items.iter().all(|x| (x.starts_with('\'') || x.starts_with('"')) && (x.contains("':") || x.contains("\":")));
    if is_map {
        let mut m = Row::new();
        for it in items {
            if let Some((k, v)) = it.split_once(':') {
                m.insert(unq(k), Value::String(unq(v)));
            }
        }
        return Value::Object(m);
    }
    Value::Array(items.iter().map(|x| Value::String(unq(x))).collect())
}

/// Normalise option data into (value, label) pairs.
pub fn option_pairs(v: &Value) -> Vec<(String, String)> {
    match v {
        Value::Array(a) => a
            .iter()
            .map(|x| match x {
                Value::Object(o) => {
                    let val = ["id", "value", "key"].iter().find_map(|k| o.get(*k)).map(vs).unwrap_or_default();
                    let lab = ["name", "label", "text", "title"].iter().find_map(|k| o.get(*k)).map(vs).unwrap_or_else(|| val.clone());
                    (val, lab)
                }
                Value::Array(p) if p.len() >= 2 => (vs(&p[0]), vs(&p[1])),
                x => (vs(x), vs(x)),
            })
            .collect(),
        Value::Object(o) => o.iter().map(|(k, v)| (k.clone(), vs(v))).collect(),
        Value::String(x) => db::csv(x).into_iter().map(|x| (x.clone(), x)).collect(),
        _ => vec![],
    }
}

impl Inner {
    pub fn block<F: Future>(&self, f: F) -> F::Output {
        tokio::task::block_in_place(|| self.st.rt.block_on(f))
    }
    fn db(&self) -> &Db {
        &self.st.db
    }
    fn uid(&self) -> Option<i64> {
        self.user.as_ref().and_then(|u| i(u, "id"))
    }
}

// ---------------- the portal API shared by scripts and templates ----------------

async fn tracker_of(inner: &Arc<Inner>, module: &str, slug: &str) -> Result<Tracker> {
    Tracker::load(inner.db(), module, slug).await.ok_or_else(|| anyhow!("Tracker {}:{} not found", module, slug))
}

async fn api_rows(inner: &Arc<Inner>, module: &str, slug: &str, q: Value, order: &str, offset: Option<i64>, limit: Option<i64>) -> Result<Value> {
    let t = tracker_of(inner, module, slug).await?;
    let q = obj(q);
    let rows = t.rows(inner.db(), Some(&q), Some(order), offset, limit).await?;
    Ok(Value::Array(rows.into_iter().map(Value::Object).collect()))
}

async fn api_first(inner: &Arc<Inner>, module: &str, slug: &str, q: Value, order: &str) -> Result<Value> {
    let t = tracker_of(inner, module, slug).await?;
    let q = obj(q);
    Ok(t.first_row(inner.db(), Some(&q), Some(order)).await?.map(Value::Object).unwrap_or(Value::Null))
}

async fn api_get(inner: &Arc<Inner>, module: &str, slug: &str, id: i64) -> Result<Value> {
    let t = tracker_of(inner, module, slug).await?;
    Ok(t.get(inner.db(), id).await.map(Value::Object).unwrap_or(Value::Null))
}

async fn api_save(inner: &Arc<Inner>, module: &str, slug: &str, datas: Value) -> Result<Value> {
    let t = tracker_of(inner, module, slug).await?;
    Ok(json!(t.save(inner.db(), &obj(datas)).await?))
}

async fn api_trail(inner: &Arc<Inner>, module: &str, slug: &str, id: i64, desc: &str, status: Option<&str>) -> Result<Value> {
    let t = tracker_of(inner, module, slug).await?;
    let sc = Sc::from_inner(inner.clone());
    Ok(json!(t.save_trail(&sc, id, desc, status, inner.uid()).await))
}

async fn api_setting(inner: &Arc<Inner>, name: &str, default: Value) -> Result<Value> {
    Ok(web::setting_or(inner.db(), name, default).await)
}

async fn api_user(inner: &Arc<Inner>, id: i64) -> Result<Value> {
    Ok(alias_user(web::load_user(inner.db(), id).await.map(Value::Object).unwrap_or(Value::Null)))
}

/// Users also answer to the Grails property names (`name`, `staffID`, `isAdmin`).
pub fn alias_user(mut u: Value) -> Value {
    if let Value::Object(m) = &mut u {
        for (alias, col) in [("name", "emp_name"), ("staffID", "staffid"), ("isAdmin", "is_admin"), ("isActive", "isactive")] {
            if !m.contains_key(alias) {
                if let Some(v) = m.get(col).cloned() {
                    m.insert(alias.into(), v);
                }
            }
        }
    }
    u
}

/// `xs.findAll { it.a == b }` and friends from translated GSP pages: `expr` is
/// evaluated for every item with the closure parameters and captured variables.
fn g_closure(state: &minijinja::State, a: &[MV]) -> std::result::Result<MV, minijinja::Error> {
    let recv = a.first().cloned().unwrap_or(MV::UNDEFINED);
    let (method, params, src) = (arg(a, 1), arg(a, 2), arg(a, 3));
    let params: Vec<&str> = params.split(',').collect();
    let expr = state.env().compile_expression(&src)?;
    let mut base: std::collections::BTreeMap<String, MV> = std::collections::BTreeMap::new();
    if let Some(caps) = a.get(4) {
        if let Ok(keys) = caps.try_iter() {
            for k in keys {
                if let Ok(v) = caps.get_item(&k) {
                    base.insert(k.to_string(), v);
                }
            }
        }
    }
    // items: list elements, or map entries
    let is_map = recv.kind() == minijinja::value::ValueKind::Map;
    let mut items: Vec<(MV, MV)> = vec![];
    if !(recv.is_undefined() || recv.is_none()) {
        if let Ok(it) = recv.try_iter() {
            for (n, x) in it.enumerate() {
                if is_map {
                    let v = recv.get_item(&x).unwrap_or(MV::UNDEFINED);
                    items.push((x, v));
                } else {
                    items.push((MV::from(n), x));
                }
            }
        }
    }
    let call = |k: &MV, v: &MV, extra: Option<&MV>| -> std::result::Result<MV, minijinja::Error> {
        let mut ctx = base.clone();
        if let Some(b) = extra {
            // comparator: (a, b)
            ctx.insert(params[0].to_string(), v.clone());
            ctx.insert(params.get(1).copied().unwrap_or("b").to_string(), b.clone());
        } else if is_map && params.len() >= 2 {
            ctx.insert(params[0].to_string(), k.clone());
            ctx.insert(params[1].to_string(), v.clone());
        } else if is_map {
            ctx.insert(params[0].to_string(), MV::from_serialize(json!({"key": m2j(k), "value": m2j(v)})));
        } else {
            ctx.insert(params[0].to_string(), v.clone());
            if params.len() >= 2 {
                ctx.insert(params[1].to_string(), k.clone());
            }
        }
        expr.eval(ctx)
    };
    let rebuild = |kept: Vec<&(MV, MV)>| -> MV {
        if is_map {
            let mut m = Row::new();
            for (k, v) in kept {
                m.insert(k.to_string(), m2j(v));
            }
            j2m(&Value::Object(m))
        } else {
            MV::from(kept.into_iter().map(|x| x.1.clone()).collect::<Vec<MV>>())
        }
    };
    let mut results = vec![];
    if !(matches!(method.as_str(), "sort" | "sortBy") && params.len() >= 2 && !is_map) {
        for (k, v) in &items {
            results.push(call(k, v, None)?);
        }
    }
    let truth: Vec<bool> = results.iter().map(|r| r.is_true()).collect();
    Ok(match method.as_str() {
        "findAll" | "grep" => rebuild(items.iter().zip(&truth).filter(|x| *x.1).map(|x| x.0).collect()),
        "find" => items.iter().zip(&truth).find(|x| *x.1).map(|x| if is_map { MV::from_serialize(json!({"key": m2j(&x.0 .0), "value": m2j(&x.0 .1)})) } else { x.0 .1.clone() }).unwrap_or(MV::from(())),
        "findIndexOf" => MV::from(truth.iter().position(|x| *x).map(|x| x as i64).unwrap_or(-1)),
        "any" => MV::from(truth.iter().any(|x| *x)),
        "every" => MV::from(truth.iter().all(|x| *x)),
        "count" => MV::from(truth.iter().filter(|x| **x).count()),
        "takeWhile" => rebuild(items.iter().zip(&truth).take_while(|x| *x.1).map(|x| x.0).collect()),
        "dropWhile" => rebuild(items.iter().zip(&truth).skip_while(|x| *x.1).map(|x| x.0).collect()),
        "collect" => MV::from(results),
        "findResults" => MV::from(results.into_iter().filter(|r| !(r.is_none() || r.is_undefined())).collect::<Vec<MV>>()),
        "collectMany" => {
            let mut o = vec![];
            for r in results {
                match r.try_iter() {
                    Ok(it) if r.kind() == minijinja::value::ValueKind::Seq => o.extend(it),
                    _ => o.push(r),
                }
            }
            MV::from(o)
        }
        "sum" => {
            let js: Vec<Value> = results.iter().map(m2j).collect();
            j2m(&crate::groovy::method(&Value::Array(js), "sum", &[]))
        }
        "join" => MV::from(results.iter().map(|r| r.to_string()).collect::<Vec<_>>().join("")),
        "collectEntries" => {
            let mut m = Row::new();
            for r in &results {
                match m2j(r) {
                    Value::Array(p) if p.len() >= 2 => {
                        m.insert(vs(&p[0]), p[1].clone());
                    }
                    Value::Object(o) => m.extend(o),
                    _ => {}
                }
            }
            j2m(&Value::Object(m))
        }
        "groupBy" | "countBy" => {
            let mut m = Row::new();
            for ((_, v), r) in items.iter().zip(&results) {
                let key = if r.is_none() || r.is_undefined() { String::new() } else { r.to_string() };
                let e = m.entry(key).or_insert_with(|| if method == "countBy" { json!(0) } else { json!([]) });
                match e {
                    Value::Array(l) => l.push(m2j(v)),
                    x => *x = json!(x.as_i64().unwrap_or(0) + 1),
                }
            }
            j2m(&Value::Object(m))
        }
        "sort" | "sortBy" | "max" | "min" | "unique" => {
            if matches!(method.as_str(), "sort" | "sortBy") && params.len() >= 2 && !is_map {
                // comparator closure
                let mut vals: Vec<MV> = items.iter().map(|x| x.1.clone()).collect();
                let mut err = None;
                vals.sort_by(|x, y| match call(&MV::UNDEFINED, x, Some(y)) {
                    Ok(r) => i64::try_from(r).unwrap_or(0).cmp(&0),
                    Err(e) => {
                        err = Some(e);
                        std::cmp::Ordering::Equal
                    }
                });
                if let Some(e) = err {
                    return Err(e);
                }
                return Ok(MV::from(vals));
            }
            let keys: Vec<Value> = results.iter().map(m2j).collect();
            let mut idx: Vec<usize> = (0..items.len()).collect();
            match method.as_str() {
                "max" => return Ok(idx.into_iter().max_by(|x, y| crate::groovy::compare(&keys[*x], &keys[*y])).map(|n| items[n].1.clone()).unwrap_or(MV::from(()))),
                "min" => return Ok(idx.into_iter().min_by(|x, y| crate::groovy::compare(&keys[*x], &keys[*y])).map(|n| items[n].1.clone()).unwrap_or(MV::from(()))),
                "unique" => {
                    let mut seen: Vec<&Value> = vec![];
                    idx.retain(|n| {
                        if seen.contains(&&keys[*n]) {
                            false
                        } else {
                            seen.push(&keys[*n]);
                            true
                        }
                    });
                }
                _ => idx.sort_by(|x, y| crate::groovy::compare(&keys[*x], &keys[*y])),
            }
            rebuild(idx.into_iter().map(|n| &items[n]).collect())
        }
        "inject" => MV::UNDEFINED,
        _ => MV::UNDEFINED,
    })
}

async fn api_user_staff(inner: &Arc<Inner>, staffid: &str) -> Result<Value> {
    Ok(match web::find_user_by_staffid(inner.db(), staffid).await {
        Some(mut u) => {
            web::clean_user(&mut u);
            Value::Object(u)
        }
        None => Value::Null,
    })
}

async fn api_users_with_role(inner: &Arc<Inner>, module: &str, role: &str) -> Result<Value> {
    let mut rows = inner
        .db()
        .rows(
            "select u.* from iap_user u where u.isactive is not false and u.id in (select user_id from user_role where module=:m and role=:r) order by u.emp_name",
            &row! {"m"=>module,"r"=>role},
        )
        .await?;
    for r in rows.iter_mut() {
        web::clean_user(r);
    }
    Ok(Value::Array(rows.into_iter().map(Value::Object).collect()))
}

async fn api_module_roles(inner: &Arc<Inner>, module: &str) -> Result<Value> {
    Ok(match &inner.user {
        Some(u) => json!(web::module_roles(inner.db(), u, module).await),
        None => json!([]),
    })
}

async fn api_tracker_roles(inner: &Arc<Inner>, module: &str, slug: &str, datas: Value) -> Result<Value> {
    let t = tracker_of(inner, module, slug).await?;
    let sc = Sc::from_inner(inner.clone());
    let d = obj(datas);
    Ok(json!(t.user_roles(&sc, if d.is_empty() { None } else { Some(&d) }).await))
}

async fn api_email(inner: &Arc<Inner>, m: Value) -> Result<Value> {
    let m = obj(m);
    let list = |k: &str| match m.get(k) {
        Some(Value::Array(a)) => a.iter().map(vs).collect::<Vec<_>>().join(","),
        Some(v) => vs(v),
        None => String::new(),
    };
    let id = crate::mail::send(&inner.st, &list("to"), &list("cc"), &s(&m, "subject"), &s(&m, "body"), &s(&m, "module")).await?;
    Ok(json!(id))
}

/// Run a runable page (script) and return its result.
pub async fn run_page(inner: &Arc<Inner>, module: &str, slug: &str, vars: Row) -> Result<Value> {
    if inner.depth.fetch_add(1, Ordering::SeqCst) > 20 {
        inner.depth.fetch_sub(1, Ordering::SeqCst);
        return Err(anyhow!("run_page nesting too deep"));
    }
    let page = inner.db().first(&format!("select * from {} where module=:m and slug=:s order by id limit 1", crate::db::page_t()), &row! {"m"=>module,"s"=>slug}).await?;
    let out = match page {
        Some(p) => Sc::from_inner(inner.clone()).eval(&s(&p, "content"), &vars),
        None => Err(anyhow!("Page {}:{} not found", module, slug)),
    };
    inner.depth.fetch_sub(1, Ordering::SeqCst);
    out
}

/// Render another page's content template (no layout) and return the HTML.
pub async fn include_page(inner: &Arc<Inner>, module: &str, slug: &str, mut vars: Row) -> Result<String> {
    if inner.depth.fetch_add(1, Ordering::SeqCst) > 20 {
        inner.depth.fetch_sub(1, Ordering::SeqCst);
        return Err(anyhow!("include_page nesting too deep"));
    }
    let page = inner.db().first(&format!("select * from {} where module=:m and slug=:s order by id limit 1", crate::db::page_t()), &row! {"m"=>module,"s"=>slug}).await?;
    let out = match page {
        Some(p) => {
            let sc = Sc::from_inner(inner.clone());
            let pre = s(&p, "preprocess");
            if !pre.trim().is_empty() {
                let pp = sc.eval(&pre, &vars)?;
                vars.insert("pp".into(), pp);
            }
            sc.render(&s(&p, "content"), &vars, true)
        }
        None => Err(anyhow!("Page {}:{} not found", module, slug)),
    };
    inner.depth.fetch_sub(1, Ordering::SeqCst);
    out
}

async fn api_read_excel(inner: &Arc<Inner>, file_id: i64, sheet: usize) -> Result<Value> {
    let f = inner.db().get("file_link", file_id).await?.ok_or_else(|| anyhow!("File {} not found", file_id))?;
    let rows = crate::upload::read_sheet(&s(&f, "path"), sheet)?;
    Ok(Value::Array(rows.into_iter().map(Value::Array).collect()))
}

async fn api_save_upload(inner: &Arc<Inner>, field: &str, module: &str) -> Result<Value> {
    let up = inner.files.lock().unwrap().remove(field);
    match up {
        Some(up) => {
            let dir = format!("{}/{}", inner.st.cfg.upload_dir, module);
            let slug = format!("{}_{}", field, chrono::Local::now().format("%Y%m%d%H%M%S%3f"));
            Ok(json!(crate::files::store_upload(inner.db(), &up, &dir, module, &slug, None, None).await?))
        }
        None => Ok(Value::Null),
    }
}

/// save_upload_as(field, #{dir, filename, module, slug, tracker_id, tracker_data_id}):
/// write the upload to <upload_dir>/<dir>/<filename> exactly (as a Grails page
/// would place it) and register a FileLink with the given fields. Returns its id.
async fn api_save_upload_as(inner: &Arc<Inner>, field: &str, o: Row) -> Result<Value> {
    let Some(up) = inner.files.lock().unwrap().remove(field) else { return Ok(Value::Null) };
    crate::files::validate_upload(&up)?;
    let dir = s(&o, "dir");
    let filename = s(&o, "filename");
    let safe = |p: &str| !p.is_empty() && !p.starts_with('/') && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..");
    if !safe(&dir) || !safe(&filename) || filename.contains('/') {
        return Err(anyhow!("save_upload_as: dir and filename must be plain relative names"));
    }
    let folder = std::path::Path::new(&inner.st.cfg.upload_dir).join(&dir);
    tokio::fs::create_dir_all(&folder).await?;
    let target = folder.join(&filename);
    up.save_to(&target).await?;
    let mut row = row! {"name"=>if o.contains_key("name") { s(&o, "name") } else { filename.clone() },
        "path"=>target.to_string_lossy().to_string(),"module"=>s(&o, "module"),"slug"=>s(&o, "slug"),"size"=>up.size as i64,"last_updated"=>web::now_str()};
    for k in ["tracker_id", "tracker_data_id", "tracker_slug", "allowedroles"] {
        if let Some(v) = o.get(k).filter(|v| !v.is_null()) {
            row.insert(k.into(), v.clone());
        }
    }
    Ok(json!(inner.db().insert("file_link", &row).await?))
}

fn build_engine(inner: &Arc<Inner>) -> Engine {
    let mut e = Engine::new();
    e.set_max_expr_depths(256, 128);
    e.set_max_operations(200_000_000);
    e.on_print(|x| println!("[script] {}", x));
    e.on_debug(|x, _, pos| println!("[script debug {}] {}", pos, x));

    macro_rules! reg {
        ($name:expr, |$inn:ident $(, $a:ident : $t:ty)*| $body:expr) => {{
            let $inn = inner.clone();
            e.register_fn($name, move |$($a: $t),*| -> RR { let $inn = &$inn; rr($inn.block($body)) });
        }};
    }
    // tracker data
    reg!("rows", |x, m: &str, sl: &str| api_rows(x, m, sl, Value::Null, "", None, None));
    reg!("rows", |x, m: &str, sl: &str, q: Dynamic| api_rows(x, m, sl, d2j(&q), "", None, None));
    reg!("rows", |x, m: &str, sl: &str, q: Dynamic, o: &str| api_rows(x, m, sl, d2j(&q), o, None, None));
    reg!("rows", |x, m: &str, sl: &str, q: Dynamic, o: &str, off: i64, lim: i64| api_rows(x, m, sl, d2j(&q), o, Some(off), Some(lim)));
    reg!("load_rows", |x, m: &str, sl: &str| api_rows(x, m, sl, Value::Null, "", None, None));
    reg!("load_rows", |x, m: &str, sl: &str, q: Dynamic| api_rows(x, m, sl, d2j(&q), "", None, None));
    reg!("load_rows", |x, m: &str, sl: &str, q: Dynamic, o: &str| api_rows(x, m, sl, d2j(&q), o, None, None));
    reg!("first_row", |x, m: &str, sl: &str| api_first(x, m, sl, Value::Null, ""));
    reg!("first_row", |x, m: &str, sl: &str, q: Dynamic| api_first(x, m, sl, d2j(&q), ""));
    reg!("first_row", |x, m: &str, sl: &str, q: Dynamic, o: &str| api_first(x, m, sl, d2j(&q), o));
    reg!("load_datas", |x, m: &str, sl: &str, q: Dynamic| api_first(x, m, sl, d2j(&q), ""));
    reg!("load_datas", |x, m: &str, sl: &str, q: Dynamic, o: &str| api_first(x, m, sl, d2j(&q), o));
    reg!("get_data", |x, m: &str, sl: &str, id: i64| api_get(x, m, sl, id));
    reg!("save_data", |x, m: &str, sl: &str, d: Dynamic| api_save(x, m, sl, d2j(&d)));
    reg!("savedatas", |x, m: &str, sl: &str, d: Dynamic| api_save(x, m, sl, d2j(&d)));
    reg!("save_trail", |x, m: &str, sl: &str, id: i64, desc: &str| api_trail(x, m, sl, id, desc, None));
    reg!("save_trail", |x, m: &str, sl: &str, id: i64, desc: &str, st: &str| api_trail(x, m, sl, id, desc, Some(st)));
    reg!("update_trail_desc", |x, m: &str, sl: &str, id: i64, desc: &str| async move {
        tracker_of(x, m, sl).await?.update_trail_desc(x.db(), id, desc).await?;
        Ok(Value::Null)
    });
    reg!("delete_data", |x, m: &str, sl: &str, id: i64| async move {
        tracker_of(x, m, sl).await?.delete(x.db(), id).await?;
        Ok(Value::Null)
    });
    reg!("count_rows", |x, m: &str, sl: &str, q: Dynamic| async move {
        let t = tracker_of(x, m, sl).await?;
        Ok(json!(t.count(x.db(), Some(&obj(d2j(&q)))).await?))
    });
    reg!("tracker_roles", |x, m: &str, sl: &str| api_tracker_roles(x, m, sl, Value::Null));
    reg!("tracker_roles", |x, m: &str, sl: &str, d: Dynamic| api_tracker_roles(x, m, sl, d2j(&d)));
    // raw SQL
    reg!("sql_rows", |x, q: &str| async move { Ok(Value::Array(x.db().rows(q, &Row::new()).await?.into_iter().map(Value::Object).collect())) });
    reg!("sql_rows", |x, q: &str, p: Dynamic| async move { let (q, p) = sql_params(q, d2j(&p)); Ok(Value::Array(x.db().rows(&q, &p).await?.into_iter().map(Value::Object).collect())) });
    reg!("sql_first", |x, q: &str| async move { Ok(x.db().first(q, &Row::new()).await?.map(Value::Object).unwrap_or(Value::Null)) });
    reg!("sql_first", |x, q: &str, p: Dynamic| async move { let (q, p) = sql_params(q, d2j(&p)); Ok(x.db().first(&q, &p).await?.map(Value::Object).unwrap_or(Value::Null)) });
    reg!("sql_exec", |x, q: &str| async move { Ok(json!(x.db().exec(q, &Row::new()).await? as i64)) });
    reg!("sql_exec", |x, q: &str, p: Dynamic| async move { let (q, p) = sql_params(q, d2j(&p)); Ok(json!(x.db().exec(&q, &p).await? as i64)) });
    // settings, users, roles
    reg!("setting", |x, n: &str| api_setting(x, n, Value::Null));
    reg!("setting", |x, n: &str, d: Dynamic| api_setting(x, n, d2j(&d)));
    reg!("user", |x, id: i64| api_user(x, id));
    reg!("user_by_staffid", |x, sid: &str| api_user_staff(x, sid));
    reg!("users_with_role", |x, m: &str, r: &str| api_users_with_role(x, m, r));
    reg!("module_roles", |x, m: &str| api_module_roles(x, m));
    reg!("has_role", |x, m: &str, r: &str| async move { Ok(json!(api_module_roles(x, m).await?.as_array().map(|a| a.iter().any(|v| vs(v) == r)).unwrap_or(false))) });
    reg!("user_module_roles", |x, uid: i64, m: &str| async move {
        Ok(match web::load_user(x.db(), uid).await {
            Some(u) => json!(web::module_roles(x.db(), &u, m).await),
            None => json!([]),
        })
    });
    // email, pages, files
    reg!("send_email", |x, m: Dynamic| api_email(x, d2j(&m)));
    reg!("run_page", |x, m: &str, sl: &str| run_page(x, m, sl, Row::new()));
    reg!("run_page", |x, m: &str, sl: &str, v: Dynamic| run_page(x, m, sl, obj(d2j(&v))));
    reg!("render_page", |x, m: &str, sl: &str| async move { Ok(json!(include_page(x, m, sl, Row::new()).await?)) });
    reg!("render_page", |x, m: &str, sl: &str, v: Dynamic| async move { Ok(json!(include_page(x, m, sl, obj(d2j(&v))).await?)) });
    reg!("render_text", |x, tpl: &str, v: Dynamic| async move { Sc::from_inner(x.clone()).render(tpl, &obj(d2j(&v)), true).map(|h| json!(h)) });
    reg!("file_info", |x, id: i64| async move { Ok(x.db().get("file_link", id).await?.map(Value::Object).unwrap_or(Value::Null)) });
    reg!("read_excel", |x, id: i64| api_read_excel(x, id, 0));
    reg!("read_excel", |x, id: i64, sheet: i64| api_read_excel(x, id, sheet.max(0) as usize));
    reg!("save_upload", |x, field: &str, module: &str| api_save_upload(x, field, module));
    // trees (menus): tree_ensure(module, name) -> #{id, root_id, created}, tree_add(parent, name [, slug [, data]]) -> id,
    // tree_children(node) (in order), tree_find_node(tree_id, name), tree_delete_node(id), tree_clear(tree_id), tree_count(tree_id)
    reg!("tree_ensure", |x, m: &str, n: &str| async move {
        let (t, created) = crate::tree::ensure_tree(x.db(), m, n).await?;
        Ok(json!({"id": t.get("id"), "root_id": t.get("root_id"), "name": t.get("name"), "module": t.get("module"), "created": created}))
    });
    reg!("tree_add", |x, parent: i64, n: &str| async move { Ok(json!(crate::tree::add_node(x.db(), parent, n, None, None).await?)) });
    reg!("tree_add", |x, parent: i64, n: &str, slug: Dynamic| async move {
        let sl = if slug.is_unit() { None } else { Some(slug.to_string()) };
        Ok(json!(crate::tree::add_node(x.db(), parent, n, sl.as_deref(), None).await?))
    });
    reg!("tree_add", |x, parent: i64, n: &str, slug: Dynamic, data: Dynamic| async move {
        let sl = if slug.is_unit() { None } else { Some(slug.to_string()) };
        let da = if data.is_unit() { None } else { Some(data.to_string()) };
        Ok(json!(crate::tree::add_node(x.db(), parent, n, sl.as_deref(), da.as_deref()).await?))
    });
    reg!("tree_children", |x, node: i64| async move {
        Ok(Value::Array(x.db().rows("select * from tree_node where parent_id=:p order by lft, id", &row! {"p"=>node}).await?.into_iter().map(Value::Object).collect()))
    });
    reg!("tree_find_node", |x, tid: i64, n: &str| async move {
        Ok(x.db().first("select * from tree_node where tree_id=:t and name=:n order by lft limit 1", &row! {"t"=>tid,"n"=>n}).await?.map(Value::Object).unwrap_or(Value::Null))
    });
    reg!("tree_delete_node", |x, id: i64| async move {
        if let Some(n) = x.db().first("select tree_id from tree_node where id=:i", &row! {"i"=>id}).await? {
            crate::tree::delete_node_deep(x.db(), i(&n, "tree_id").unwrap_or(0), id).await?;
        }
        Ok(Value::Null)
    });
    reg!("tree_clear", |x, tid: i64| async move {
        let t = x.db().first("select root_id from tree where id=:t", &row! {"t"=>tid}).await?.unwrap_or_default();
        if let Some(root) = i(&t, "root_id") {
            for c in x.db().rows("select id from tree_node where parent_id=:p", &row! {"p"=>root}).await? {
                crate::tree::delete_node_deep(x.db(), tid, i(&c, "id").unwrap_or(0)).await?;
            }
        }
        Ok(Value::Null)
    });
    reg!("tree_count", |x, tid: i64| async move { Ok(json!(x.db().count("select count(*) from tree_node where tree_id=:t", &row! {"t"=>tid}).await?)) });
    // delete_file(id): remove a stored file, its FileLink, and its folder when that is left empty
    reg!("delete_file", |x, id: i64| async move {
        if let Some(f) = x.db().get("file_link", id).await? {
            let path = s(&f, "path");
            crate::files::delete_file_link(x.db(), id).await;
            if let Some(dir) = std::path::Path::new(&path).parent() {
                let _ = tokio::fs::remove_dir(dir).await; // only succeeds when empty
            }
        }
        Ok(Value::Null)
    });
    reg!("save_upload_as", |x, field: &str, opts: Dynamic| api_save_upload_as(x, field, obj(d2j(&opts))));
    {
        let x = inner.clone();
        e.register_fn("upload_info", move |field: &str| -> Dynamic {
            match x.files.lock().unwrap().get(field) {
                Some(up) => j2d(&json!({"filename": up.filename, "size": up.size, "content_type": up.content_type})),
                None => Dynamic::UNIT,
            }
        });
    }
    // an uploaded file's text (UTF-8, at most 2 MB), without storing it: #{filename, size, text}
    reg!("upload_text", |x, field: &str| async move {
        let info = x.files.lock().unwrap().get(field).map(|u| (u.filename.clone(), u.size, u.temp.clone(), u.bytes.clone()));
        let Some((name, size, temp, bytes)) = info else { return Ok(Value::Null) };
        if size > 2 * 1024 * 1024 {
            return Ok(json!({"filename": name, "size": size, "text": Value::Null, "error": "too large"}));
        }
        let data = match temp {
            Some(t) => tokio::fs::read(t).await?,
            None => bytes,
        };
        Ok(json!({"filename": name, "size": size, "text": String::from_utf8_lossy(&data)}))
    });
    reg!("log_error", |x, msg: &str| async move {
        web::log_error(x.db(), "script", "log_error", msg, None, None, x.uid()).await;
        Ok(Value::Null)
    });
    // the visitor's session (keys the portal owns are not visible)
    {
        let x = inner.clone();
        e.register_fn("session_get", move |k: &str| -> Dynamic {
            if SESSION_RESERVED.contains(&k) {
                return Dynamic::UNIT;
            }
            x.sess.lock().unwrap().get(k).map(j2d).unwrap_or(Dynamic::UNIT)
        });
        let x = inner.clone();
        e.register_fn("session_set", move |k: &str, v: Dynamic| {
            if !SESSION_RESERVED.contains(&k) {
                let v = d2j(&v);
                let mut m = x.sess.lock().unwrap();
                if v.is_null() { m.remove(k); } else { m.insert(k.to_string(), v); }
            }
        });
    }
    // Java's java.util.Random + Collections.shuffle, so an order a Grails page shuffled
    // with a seed comes out the same here (e.g. the question order of a past session)
    e.register_fn("shuffle_seeded", |a: rhai::Array, seed: i64| java_shuffle(a, seed));
    e.register_fn("shuffle", |a: rhai::Array| java_shuffle(a, (uuid::Uuid::new_v4().as_u128() & 0xffff_ffff_ffff) as i64));
    // milliseconds since 1970 (local time), for elapsed-time arithmetic
    e.register_fn("now_ms", || chrono::Local::now().timestamp_millis());
    e.register_fn("epoch_ms", |d: Dynamic| -> Dynamic {
        match parse_dt(&vs(&d2j(&d))).and_then(|x| x.and_local_timezone(chrono::Local).earliest()) {
            Some(t) => Dynamic::from(t.timestamp_millis()),
            None => Dynamic::UNIT,
        }
    });
    // Markdown (with tables and strikethrough) to HTML; raw HTML in the source passes through
    e.register_fn("markdown_to_html", |md: &str| -> String {
        let opts = pulldown_cmark::Options::ENABLE_TABLES | pulldown_cmark::Options::ENABLE_STRIKETHROUGH;
        let mut out = String::new();
        pulldown_cmark::html::push_html(&mut out, pulldown_cmark::Parser::new_ext(md, opts));
        out
    });
    e.register_fn("json_pretty", |v: Dynamic| serde_json::to_string_pretty(&d2j(&v)).unwrap_or_default());
    {
        let x = inner.clone();
        e.register_fn("request_base", move || x.base.lock().unwrap().clone());
        // the session's form token, for forms a script writes out itself
        let x = inner.clone();
        e.register_fn("csrf_token", move || x.csrf.clone());
        let x = inner.clone();
        e.register_fn("csrf_field", move || format!("<input type='hidden' name='csrf_token' value='{}'/>", x.csrf));
    }
    // plain helpers
    e.register_fn("uuid", || uuid::Uuid::new_v4().to_string());
    // Groovy list helpers used by g6 role rules: [a, b].join(sep) and array2sql -> ('a','b')
    e.register_fn("join", |a: rhai::Array, sep: &str| -> String { a.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(sep) });
    e.register_fn("array2sql", |a: rhai::Array| -> String { format!("('{}')", a.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("','")) });
    // Unicode NFD (java.text.Normalizer.Form.NFD): accents become separate marks
    e.register_fn("nfd", |x: &str| -> String { unicode_normalization::UnicodeNormalization::nfd(x).collect() });
    // edit distance between two strings, counted in characters
    e.register_fn("levenshtein", |a: &str, b: &str| -> i64 {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        for i in 1..=a.len() {
            let mut cur = vec![i];
            for j in 1..=b.len() {
                let v = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + if a[i - 1] == b[j - 1] { 0 } else { 1 });
                cur.push(v);
            }
            prev = cur;
        }
        prev[b.len()] as i64
    });
    // a random whole number from 0 up to (not including) n
    e.register_fn("random_int", |n: i64| -> i64 { if n <= 0 { 0 } else { (uuid::Uuid::new_v4().as_u128() % n as u128) as i64 } });
    e.register_fn("now", || web::now_str());
    e.register_fn("today", || chrono::Local::now().format("%Y-%m-%d").to_string());
    e.register_fn("date_add", |d: &str, days: i64| -> String {
        match parse_dt(d) {
            Some(dt) => {
                let n = dt + chrono::Duration::days(days);
                if d.trim().len() <= 10 { n.format("%Y-%m-%d").to_string() } else { n.format("%Y-%m-%d %H:%M:%S").to_string() }
            }
            None => String::new(),
        }
    });
    e.register_fn("date_format", |d: &str, f: &str| -> String { parse_dt(d).map(|x| x.format(f).to_string()).unwrap_or_default() });
    e.register_fn("days_between", |a: &str, b: &str| -> i64 {
        match (parse_dt(a), parse_dt(b)) {
            (Some(x), Some(y)) => (y.date() - x.date()).num_days(),
            _ => 0,
        }
    });
    e.register_fn("format_number", |v: Dynamic, d: i64| fmt_number(&d2j(&v), d.max(0) as usize));
    e.register_fn("json_string", |v: Dynamic| d2j(&v).to_string());
    e.register_fn("json_parse", |x: &str| j2d(&serde_json::from_str::<Value>(x).unwrap_or(Value::Null)));
    e.register_fn("url_encode", |x: &str| web::enc(x));
    e.register_fn("html_escape", |x: &str| web::esc(x));
    e.register_fn("sql_quote", |v: Dynamic| db::lit(&d2j(&v)));
    e.register_fn("to_string", |v: Dynamic| vs(&d2j(&v)));
    e.register_fn("regex_match", |pat: &str, x: &str| regex::Regex::new(pat).map(|r| r.is_match(x)).unwrap_or(false));
    // every match of a pattern, each as [whole match, group 1, group 2, ...]
    e.register_fn("regex_captures", |pat: &str, x: &str| -> rhai::Array {
        match regex::Regex::new(pat) {
            Ok(r) => r.captures_iter(x).map(|c| Dynamic::from_array(c.iter().map(|g| g.map(|m| Dynamic::from(m.as_str().to_string())).unwrap_or(Dynamic::UNIT)).collect())).collect(),
            Err(_) => vec![],
        }
    });
    e.register_fn("regex_replace", |pat: &str, x: &str, rep: &str| regex::Regex::new(pat).map(|r| r.replace_all(x, rep).into_owned()).unwrap_or_else(|_| x.to_string()));
    e
}

/// java.util.Random (48-bit LCG) driving Collections.shuffle(list, rnd).
pub fn java_shuffle(mut a: rhai::Array, seed: i64) -> rhai::Array {
    const MUL: i64 = 0x5DEECE66D;
    const MASK: i64 = (1 << 48) - 1;
    let mut st = (seed ^ MUL) & MASK;
    let mut next = |bits: u32| -> i32 {
        st = st.wrapping_mul(MUL).wrapping_add(0xB) & MASK;
        (st >> (48 - bits)) as i32
    };
    for i in (2..=a.len()).rev() {
        let bound = i as i32;
        let j = if bound & bound.wrapping_neg() == bound {
            ((bound as i64 * next(31) as i64) >> 31) as i32
        } else {
            loop {
                let bits = next(31);
                let val = bits % bound;
                if bits.wrapping_sub(val).wrapping_add(bound - 1) >= 0 {
                    break val;
                }
            }
        };
        a.swap(i - 1, j as usize);
    }
    a
}

fn mj_err(e: impl ToString) -> minijinja::Error {
    minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, e.to_string())
}
fn m2j(v: &MV) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}
fn j2m(v: &Value) -> MV {
    MV::from_serialize(v)
}
fn arg(args: &[MV], n: usize) -> String {
    args.get(n).map(|v| if v.is_undefined() || v.is_none() { String::new() } else { v.to_string() }).unwrap_or_default()
}
fn kw_row(kw: &Kwargs) -> Row {
    let mut r = Row::new();
    for k in kw.args() {
        if let Ok(v) = kw.get::<MV>(k) {
            r.insert(k.to_string(), m2j(&v));
        }
    }
    r
}

impl Sc {
    pub fn new(st: St, user: Option<Row>, params: Row, csrf: String, path: String) -> Sc {
        let inner = Arc::new(Inner { st, user, params, csrf, path, cache: Mutex::new(HashMap::new()), cur: Mutex::new(None), files: Mutex::new(HashMap::new()), depth: AtomicUsize::new(0), sess: Mutex::new(Row::new()), base: Mutex::new(String::new()) });
        Sc::from_inner(inner)
    }
    pub fn from_inner(inner: Arc<Inner>) -> Sc {
        let engine = build_engine(&inner);
        Sc { inner, engine }
    }
    /// A context with no request behind it (scheduler, background jobs).
    pub fn system(st: St) -> Sc {
        Sc::new(st, None, Row::new(), String::new(), String::new())
    }
    pub fn db(&self) -> &Db {
        &self.inner.st.db
    }
    pub fn cache_get(&self, k: &str) -> Option<Value> {
        self.inner.cache.lock().unwrap().get(k).cloned()
    }
    pub fn cache_put(&self, k: &str, v: Value) {
        self.inner.cache.lock().unwrap().insert(k.to_string(), v);
    }
    pub fn set_cur(&self, module: &str, slug: &str, id: Option<i64>, transition: Option<String>) {
        *self.inner.cur.lock().unwrap() = Some((module.to_string(), slug.to_string(), id, transition));
    }

    /// Evaluate a Rhai script. `params` and `curuser` are always in scope.
    pub fn eval(&self, code: &str, vars: &Row) -> Result<Value> {
        let mut scope = Scope::new();
        scope.push_dynamic("params", j2d(&Value::Object(self.inner.params.clone())));
        scope.push_dynamic("curuser", j2d(&self.inner.user.clone().map(Value::Object).unwrap_or(Value::Null)));
        for (k, v) in vars {
            if k != "params" && k != "curuser" {
                scope.push_dynamic(k.as_str(), j2d(v));
            }
        }
        if !vars.contains_key("datas") {
            scope.push_dynamic("datas", Dynamic::UNIT);
        }
        let engine = &self.engine;
        tokio::task::block_in_place(|| match engine.eval_with_scope::<Dynamic>(&mut scope, code) {
            Ok(d) => Ok(d2j(&d)),
            Err(e) => Err(anyhow!("{}", e)),
        })
    }

    pub fn eval_options(&self, code: &str, vars: &Row) -> Value {
        match self.eval(code, vars) {
            Ok(v) if v.is_array() || v.is_object() => v,
            Ok(Value::String(x)) => lenient_options(&x),
            _ => lenient_options(code),
        }
    }

    /// Render a MiniJinja template with the portal functions available.
    /// `html` turns on HTML auto-escaping (off for SQL rules).
    pub fn render(&self, tpl: &str, vars: &Row, html: bool) -> Result<String> {
        let mut env = minijinja::Environment::new();
        if html {
            env.set_auto_escape_callback(|_| minijinja::AutoEscape::Html);
            // escape the way GSP did (& < > " ' only, so URLs and dates survive inside
            // <script>), and print nothing for null
            env.set_formatter(|out, state, value| {
                if value.is_none() || value.is_undefined() {
                    return Ok(());
                }
                if value.is_safe() || !matches!(state.auto_escape(), minijinja::AutoEscape::Html) {
                    return minijinja::escape_formatter(out, state, value);
                }
                let text = match value.as_str() {
                    Some(s) => web::esc(s),
                    None if value.kind() == minijinja::value::ValueKind::Bool => (if value.is_true() { "true" } else { "false" }).to_string(),
                    None => web::esc(&value.to_string()),
                };
                out.write_str(&text).map_err(|_| minijinja::Error::new(minijinja::ErrorKind::WriteFailure, "write failed"))
            });
        } else {
            env.set_auto_escape_callback(|_| minijinja::AutoEscape::None);
        }
        // pages translated from GSP lean on Groovy's null-safety and method names
        env.set_undefined_behavior(minijinja::UndefinedBehavior::Chainable);
        env.set_unknown_method_callback(|_state, value, method, args| {
            let a: Vec<Value> = args.iter().map(m2j).collect();
            let recv = if value.is_undefined() || value.is_none() { Value::Null } else { m2j(value) };
            let out = crate::groovy::method(&recv, method, &a);
            Ok(if out.is_null() { MV::UNDEFINED } else { j2m(&out) })
        });
        self.add_functions(&mut env);
        let mut ctx = vars.clone();
        if !ctx.contains_key("params") {
            ctx.insert("params".into(), Value::Object(self.inner.params.clone()));
        }
        if !ctx.contains_key("curuser") {
            ctx.insert("curuser".into(), alias_user(self.inner.user.clone().map(Value::Object).unwrap_or(Value::Null)));
        }
        if crate::db::g6() && !ctx.contains_key("session") && tpl.contains("session") {
            // a g6 page reads its session directly; 'profile' is the member record the
            // Grails portal puts there at login (config user_profile)
            let mut sess = self.inner.sess.lock().unwrap().clone();
            if tpl.contains("profile") && !sess.contains_key("profile") {
                let p = match self.cache_get("session_profile") {
                    Some(p) => p,
                    None => {
                        let p = self.inner.block(crate::tracker::user_profile(self));
                        self.cache_put("session_profile", p.clone());
                        p
                    }
                };
                sess.insert("profile".into(), p);
            }
            ctx.insert("session".into(), Value::Object(sess));
        }
        tokio::task::block_in_place(|| env.render_str(tpl, Value::Object(ctx)).map_err(|e| anyhow!("{:#}", e)))
    }

    fn add_functions(&self, env: &mut minijinja::Environment) {
        macro_rules! func {
            ($name:expr, |$inn:ident, $args:ident, $kw:ident| $body:expr) => {{
                let $inn = self.inner.clone();
                env.add_function($name, move |$args: Rest<MV>, $kw: Kwargs| -> std::result::Result<MV, minijinja::Error> {
                    let $inn = &$inn;
                    let _ = &$kw;
                    let r: Result<MV> = $inn.block($body);
                    r.map_err(mj_err)
                });
            }};
        }
        // --- data ---
        func!("rows", |x, a, kw| async move {
            let q = a.get(2).map(m2j).unwrap_or(Value::Null);
            let lim: Option<i64> = kw.get("limit").ok();
            let off: Option<i64> = kw.get("offset").ok();
            Ok(j2m(&api_rows(x, &arg(&a, 0), &arg(&a, 1), q, &arg(&a, 3), off, lim).await?))
        });
        func!("first_row", |x, a, kw| async move { Ok(j2m(&api_first(x, &arg(&a, 0), &arg(&a, 1), a.get(2).map(m2j).unwrap_or(Value::Null), &arg(&a, 3)).await?)) });
        func!("get_data", |x, a, kw| async move { Ok(j2m(&api_get(x, &arg(&a, 0), &arg(&a, 1), arg(&a, 2).parse().unwrap_or(0)).await?)) });
        func!("sql_rows", |x, a, kw| async move {
            let (q, p) = sql_params(&arg(&a, 0), a.get(1).map(m2j).unwrap_or(Value::Null));
            Ok(j2m(&Value::Array(x.db().rows(&q, &p).await?.into_iter().map(Value::Object).collect())))
        });
        func!("sql_first", |x, a, kw| async move {
            let (q, p) = sql_params(&arg(&a, 0), a.get(1).map(m2j).unwrap_or(Value::Null));
            Ok(j2m(&x.db().first(&q, &p).await?.map(Value::Object).unwrap_or(Value::Null)))
        });
        // the modules the current user administers (the Grails portals' curuser.adminlist())
        func!("admin_modules", |x, a, kw| async move {
            let l = match &x.user { Some(u) => web::admin_list(x.db(), u).await, None => vec![] };
            Ok(j2m(&json!(l)))
        });
        func!("setting", |x, a, kw| async move { Ok(j2m(&api_setting(x, &arg(&a, 0), a.get(1).map(m2j).unwrap_or(Value::Null)).await?)) });
        func!("user", |x, a, kw| async move { Ok(j2m(&api_user(x, arg(&a, 0).parse().unwrap_or(0)).await?)) });
        func!("user_name", |x, a, kw| async move { Ok(MV::from(api_user(x, arg(&a, 0).parse().unwrap_or(0)).await?.get("emp_name").map(vs).unwrap_or_default())) });
        func!("users_with_role", |x, a, kw| async move { Ok(j2m(&api_users_with_role(x, &arg(&a, 0), &arg(&a, 1)).await?)) });
        func!("module_roles", |x, a, kw| async move { Ok(j2m(&api_module_roles(x, &arg(&a, 0)).await?)) });
        func!("has_role", |x, a, kw| async move {
            let want = arg(&a, 1);
            Ok(MV::from(api_module_roles(x, &arg(&a, 0)).await?.as_array().map(|l| l.iter().any(|v| vs(v) == want)).unwrap_or(false)))
        });
        func!("tracker_roles", |x, a, kw| async move { Ok(j2m(&api_tracker_roles(x, &arg(&a, 0), &arg(&a, 1), a.get(2).map(m2j).unwrap_or(Value::Null)).await?)) });
        func!("include_page", |x, a, kw| async move { Ok(MV::from_safe_string(include_page(x, &arg(&a, 0), &arg(&a, 1), kw_row(&kw)).await?)) });
        func!("run_page", |x, a, kw| async move { Ok(j2m(&run_page(x, &arg(&a, 0), &arg(&a, 1), kw_row(&kw)).await?)) });
        func!("script", |x, a, kw| async move { Ok(j2m(&Sc::from_inner(x.clone()).eval(&arg(&a, 0), &kw_row(&kw))?)) });
        // --- tracker tags ---
        macro_rules! tag {
            ($name:expr, |$sc:ident, $t:ident, $id:ident, $tr:ident, $kw:ident| $body:expr) => {
                func!($name, |x, a, $kw| async move {
                    let cur = x.cur.lock().unwrap().clone();
                    let (m, sl) = if a.len() >= 2 { (arg(&a, 0), arg(&a, 1)) } else { cur.clone().map(|c| (c.0, c.1)).unwrap_or_default() };
                    // no tracker (e.g. a tracker template opened as a plain page): the
                    // Grails tags render nothing then, rather than failing the page
                    let $t = match tracker_of(x, &m, &sl).await {
                        Ok(t) => t,
                        Err(_) => return Ok(MV::from_safe_string(String::new())),
                    };
                    let $id: Option<i64> = $kw.get::<Option<i64>>("id").ok().flatten().or_else(|| a.get(2).and_then(|v| v.to_string().parse().ok())).or_else(|| cur.as_ref().and_then(|c| c.2));
                    let $tr: Option<String> = $kw.get::<Option<String>>("transition").ok().flatten().or_else(|| cur.as_ref().and_then(|c| c.3.clone()));
                    let _ = (&$id, &$tr);
                    let $sc = Sc::from_inner(x.clone());
                    let html: String = $body;
                    Ok(MV::from_safe_string(html))
                })
            };
        }
        tag!("tracker_list", |sc, t, id, tr, kw| crate::tags::tracker_list(&sc, &t, &kw_row(&kw)).await);
        tag!("tracker_filter", |sc, t, id, tr, kw| crate::tags::tracker_filter(&sc, &t).await);
        tag!("tracker_display", |sc, t, id, tr, kw| crate::tags::tracker_display(&sc, &t, id.unwrap_or(0), &kw_row(&kw)).await);
        tag!("tracker_updates", |sc, t, id, tr, kw| crate::tags::tracker_updates(&sc, &t, id.unwrap_or(0)).await);
        tag!("transition_buttons", |sc, t, id, tr, kw| crate::tags::transition_buttons(&sc, &t, id.unwrap_or(0)).await);
        tag!("tracker_transitions", |sc, t, id, tr, kw| crate::tags::tracker_transitions(&sc, &t, id.unwrap_or(0)).await);
        tag!("tracker_nav", |sc, t, id, tr, kw| crate::tags::tracker_nav(&sc, &t, true).await);
        tag!("tracker_form", |sc, t, id, tr, kw| {
            let name = tr.unwrap_or_else(|| "New".into());
            let data = match id {
                Some(rid) => t.get(sc.db(), rid).await,
                None => None,
            };
            match t.resolve(&sc, &name, data.as_ref()).await {
                Some(tn) => crate::tags::tracker_form(&sc, &t, &tn, id, &kw_row(&kw)).await,
                None => "<div class='alert alert-warning'>You are not authorized to do that</div>".to_string(),
            }
        });
        tag!("tracker_info", |sc, t, id, tr, kw| {
            let name = tr.unwrap_or_else(|| "New".into());
            let data = match id {
                Some(rid) => t.get(sc.db(), rid).await,
                None => None,
            };
            match t.resolve(&sc, &name, data.as_ref()).await {
                Some(tn) => crate::tags::tracker_info(&sc, &t, &tn, id).await,
                None => String::new(),
            }
        });
        func!("display_field", |x, a, kw| async move {
            let t = tracker_of(x, &arg(&a, 0), &arg(&a, 1)).await?;
            let fname = arg(&a, 2);
            let f = t.field(&fname).cloned().ok_or_else(|| anyhow!("field {} not found", fname))?;
            let datas = a.get(4).map(m2j).map(obj).unwrap_or_default();
            let val = a.get(3).map(m2j).unwrap_or_else(|| datas.get(&fname).cloned().unwrap_or(Value::Null));
            let sc = Sc::from_inner(x.clone());
            Ok(MV::from_safe_string(crate::tags::display_field(&sc, &t, &f, &val, &datas, false).await))
        });
        func!("transition_allowed", |x, a, kw| async move {
            let t = tracker_of(x, &arg(&a, 0), &arg(&a, 1)).await?;
            let sc = Sc::from_inner(x.clone());
            let d = a.get(3).map(m2j).map(obj);
            Ok(MV::from(t.transition_allowed(&sc, &arg(&a, 2), d.as_ref()).await))
        });
        // --- helpers used by pages translated from GSP ---
        env.add_function("g_closure", |state: &minijinja::State, a: Rest<MV>| g_closure(state, &a));
        // looping over the entries of something that is not there is an empty loop
        env.add_filter("items", |v: MV| -> MV {
            match m2j(&v) {
                Value::Object(m) => MV::from(m.iter().map(|(k, v)| MV::from(vec![MV::from(k.clone()), j2m(v)])).collect::<Vec<MV>>()),
                _ => MV::from(Vec::<MV>::new()),
            }
        });
        env.add_function("cmp", |a: MV, b: MV| -> i64 { crate::groovy::compare(&m2j(&a), &m2j(&b)) as i64 });
        env.add_function("merge", |a: Rest<MV>| -> MV {
            let mut m = Row::new();
            for x in a.iter() {
                if let Value::Object(o) = m2j(x) {
                    m.extend(o);
                }
            }
            j2m(&Value::Object(m))
        });
        env.add_function("sprintf", |a: Rest<MV>| -> String {
            let rest: Vec<Value> = a.iter().skip(1).map(m2j).collect();
            // String.format(fmt, [a, b]) and String.format(fmt, a, b) are both in use
            let rest = match rest.as_slice() {
                [Value::Array(l)] => l.clone(),
                _ => rest,
            };
            crate::groovy::sprintf(&arg(&a, 0), &rest)
        });
        env.add_function("human_size", |v: MV| -> String { crate::groovy::human_size(f64::try_from(v).unwrap_or(0.0)) });
        env.add_function("grails_url", |kw: Kwargs| -> MV {
            let k = kw_row(&kw);
            let u = crate::groovy::grails_url(&k.get("controller").map(vs).unwrap_or_default(), &k.get("action").map(vs).unwrap_or_default(), &k.get("id").map(vs).unwrap_or_default(), &k.get("params").cloned().map(obj).unwrap_or_default());
            // query=true: the page appends "&name=value" itself
            // already URL-encoded, and used inside <script> as often as in href
            MV::from_safe_string(if k.contains_key("query") && !u.contains('?') { format!("{}?", u) } else { u })
        });
        env.add_function("g_select", |kw: Kwargs| -> MV { MV::from_safe_string(crate::groovy::select_html(&kw_row(&kw))) });
        let path = self.inner.path.clone();
        env.add_function("request_path", move || path.clone());
        let x = self.inner.clone();
        env.add_function("request_base", move || x.base.lock().unwrap().clone());
        let x = self.inner.clone();
        env.add_function("session_get", move |k: String| -> MV {
            if SESSION_RESERVED.contains(&k.as_str()) {
                return MV::from(());
            }
            x.sess.lock().unwrap().get(&k).map(j2m).unwrap_or(MV::from(()))
        });
        env.add_filter("number_format", |v: MV, f: Option<String>| -> String {
            if v.is_undefined() || v.is_none() { String::new() } else { crate::groovy::number_format(&m2j(&v), f.as_deref().unwrap_or("#,##0.###")) }
        });
        env.add_filter("java_date", |v: MV, f: Option<String>| -> String {
            if v.is_undefined() || v.is_none() { String::new() } else { crate::groovy::java_date(&v.to_string(), f.as_deref().unwrap_or("yyyy-MM-dd HH:mm:ss")) }
        });
        func!("user_by_staffid", |x, a, kw| async move { Ok(j2m(&alias_user(api_user_staff(x, &arg(&a, 0)).await?))) });
        func!("tracker_ref", |x, a, kw| async move {
            Ok(match Tracker::load(x.db(), &arg(&a, 0), &arg(&a, 1)).await {
                Some(t) => j2m(&Value::Object(t.row.clone())),
                None => MV::from(()),
            })
        });
        func!("file_link", |x, a, kw| async move {
            let k = kw_row(&kw);
            let id = match k.get("slug") {
                Some(sl) => x.db().first("select id from file_link where module=:m and slug=:s order by id desc limit 1", &row! {"m"=>k.get("module").map(vs).unwrap_or_else(|| "portal".into()),"s"=>vs(sl)}).await?.and_then(|r| i(&r, "id")),
                None => arg(&a, 0).parse::<i64>().ok(),
            };
            Ok(match id {
                Some(id) => MV::from_safe_string(crate::files::link_html(x.db(), id).await),
                None => MV::from(""),
            })
        });
        // --- links, forms, misc ---
        let csrf = self.inner.csrf.clone();
        env.add_function("csrf_field", move || MV::from_safe_string(format!("<input type='hidden' name='csrf_token' value='{}'/>", csrf)));
        let csrf = self.inner.csrf.clone();
        env.add_function("csrf_token", move || csrf.clone());
        env.add_function("page_url", |a: Rest<MV>, kw: Kwargs| -> String {
            let mut u = format!("/view/{}/{}", arg(&a, 0), arg(&a, 1));
            for n in 2..a.len() {
                u.push('/');
                u.push_str(&web::enc(&arg(&a, n)));
            }
            let q = web::qs(&kw_row(&kw), &[]);
            if q.is_empty() { u } else { format!("{}?{}", u, q) }
        });
        env.add_function("run_url", |a: Rest<MV>, kw: Kwargs| -> String {
            let mut u = format!("/run/{}/{}", arg(&a, 0), arg(&a, 1));
            for n in 2..a.len() {
                u.push('/');
                u.push_str(&web::enc(&arg(&a, n)));
            }
            let q = web::qs(&kw_row(&kw), &[]);
            if q.is_empty() { u } else { format!("{}?{}", u, q) }
        });
        env.add_function("list_url", |a: Rest<MV>, kw: Kwargs| -> String {
            let q = web::qs(&kw_row(&kw), &[]);
            format!("/{}/{}/list{}", arg(&a, 0), arg(&a, 1), if q.is_empty() { String::new() } else { format!("?{}", q) })
        });
        env.add_function("create_url", |a: Rest<MV>, kw: Kwargs| -> String {
            let q = web::qs(&kw_row(&kw), &[]);
            format!("/{}/{}/create{}", arg(&a, 0), arg(&a, 1), if q.is_empty() { String::new() } else { format!("?{}", q) })
        });
        env.add_function("display_url", |a: Rest<MV>| -> String { format!("/{}/{}/display/{}", arg(&a, 0), arg(&a, 1), arg(&a, 2)) });
        env.add_function("transition_url", |a: Rest<MV>, kw: Kwargs| -> String {
            let q = web::qs(&kw_row(&kw), &[]);
            let id = arg(&a, 3);
            format!(
                "/{}/{}/{}{}{}",
                arg(&a, 0),
                arg(&a, 1),
                arg(&a, 2).replace(' ', "_").to_lowercase(),
                if id.is_empty() { String::new() } else { format!("/{}", id) },
                if q.is_empty() { String::new() } else { format!("?{}", q) }
            )
        });
        env.add_function("download_url", |a: Rest<MV>| -> String {
            if a.len() >= 2 { format!("/download/{}/{}", arg(&a, 0), arg(&a, 1)) } else { format!("/fileLink/get/{}", arg(&a, 0)) }
        });
        env.add_function("file_url", |a: Rest<MV>| -> String { format!("/fileLink/get/{}", arg(&a, 0)) });
        env.add_function("now", || web::now_str());
        env.add_function("today", || chrono::Local::now().format("%Y-%m-%d").to_string());
        env.add_filter("format_number", |v: MV, d: Option<usize>| -> String { fmt_number(&m2j(&v), d.unwrap_or(2)) });
        env.add_filter("format_date", |v: MV, f: Option<String>| -> String {
            if v.is_none() || v.is_undefined() {
                return String::new();
            }
            parse_dt(&v.to_string()).map(|d| d.format(f.as_deref().unwrap_or("%d/%m/%Y")).to_string()).unwrap_or_else(|| v.to_string())
        });
        env.add_filter("sql", |v: MV| -> MV { MV::from_safe_string(db::lit(&m2j(&v))) });
        env.add_filter("nl2br", |v: MV| -> MV { MV::from_safe_string(web::esc(&v.to_string()).replace('\n', "<br/>")) });
        env.add_filter("slugify", |v: MV| -> String { v.to_string().trim().replace(' ', "_").to_lowercase() });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn shuffle_matches_java() {
        // produced by Collections.shuffle(list, new Random(seed)) on a JVM
        for line in include_str!("testdata_java_shuffle.txt").lines() {
            let mut it = line.split(' ');
            let seed: i64 = it.next().unwrap().parse().unwrap();
            let n: i64 = it.next().unwrap().parse().unwrap();
            let want: Vec<i64> = it.next().unwrap().split(',').filter(|x| !x.is_empty()).map(|x| x.parse().unwrap()).collect();
            let got: Vec<i64> = super::java_shuffle((0..n).map(rhai::Dynamic::from).collect(), seed).into_iter().map(|d| d.as_int().unwrap()).collect();
            assert_eq!(got, want, "seed {} n {}", seed, n);
        }
    }
}

#[cfg(test)]
mod sql_params_tests {
    #[test]
    fn positional_placeholders() {
        let (q, p) = super::sql_params("select * from t where a = ? and b = '?' and c = ?", serde_json::json!([1, "x"]));
        assert_eq!(q, "select * from t where a = :_p1 and b = '?' and c = :_p2");
        assert_eq!(p.get("_p1"), Some(&serde_json::json!(1)));
        assert_eq!(p.get("_p2"), Some(&serde_json::json!("x")));
        let (q2, p2) = super::sql_params("select :a", serde_json::json!({"a": 5}));
        assert_eq!((q2.as_str(), p2.get("a")), ("select :a", Some(&serde_json::json!(5))));
    }
}
