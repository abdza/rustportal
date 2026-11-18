//! Tracker engine: dynamic data tables, the query-parameter DSL, record
//! updates, workflow transitions, role resolution and audit trails.

use crate::db::{self, b, csv, has, i, ident, lit, qcol, s, vs, Db, Row};
use crate::row;
use crate::script::Sc;
use crate::web::{self, now_str, Upload};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Clone)]
pub struct Trans {
    pub row: Row,
    pub id: i64,
    pub name: String,
    pub roles: Vec<String>,
    pub prev: Vec<i64>,
    pub next: Option<i64>,
}

impl Trans {
    pub fn label(&self) -> String {
        let d = s(&self.row, "display_name");
        if d.is_empty() { self.name.clone() } else { d }
    }
    pub fn url_name(&self) -> String {
        self.name.replace(' ', "_").to_lowercase()
    }
}

#[derive(Clone)]
pub struct Tracker {
    pub row: Row,
    pub id: i64,
    pub module: String,
    pub slug: String,
    pub name: String,
    pub tracker_type: String,
    pub fields: Vec<Row>,
    pub statuses: Vec<Row>,
    pub roles: Vec<Row>,
    pub transitions: Vec<Trans>,
}

pub fn sql_type(field_type: &str) -> Option<&'static str> {
    Some(match field_type {
        "Text Area" => "text",
        "Date" => "date",
        "DateTime" => "timestamp",
        "Checkbox" => "boolean",
        "Integer" => "numeric(24,0)",
        "Number" => "decimal(24,6)",
        "Branch" | "User" | "File" | "BelongsTo" | "TreeNode" => "numeric(19,0)",
        "HasMany" | "FieldGroup" => return None,
        _ => "varchar(256)",
    })
}

fn is_date(x: &str) -> bool {
    chrono::NaiveDate::parse_from_str(x, "%Y-%m-%d").is_ok()
}

/// Convert a submitted/scripted value to what the column expects.
pub fn coerce(field: &Row, v: &Value) -> Value {
    let ft = s(field, "field_type");
    let txt = vs(v);
    match ft.as_str() {
        "Integer" | "User" | "Branch" | "File" | "TreeNode" | "BelongsTo" => {
            if txt.trim().is_empty() {
                Value::Null
            } else if let Ok(n) = txt.trim().parse::<i64>() {
                json!(n)
            } else if let Ok(f) = txt.trim().parse::<f64>() {
                json!(f as i64)
            } else {
                Value::Null
            }
        }
        "Number" => {
            if txt.trim().is_empty() {
                Value::Null
            } else {
                txt.trim().replace(',', "").parse::<f64>().map(|f| json!(f)).unwrap_or(Value::Null)
            }
        }
        "Checkbox" => match v {
            Value::Bool(x) => json!(*x),
            Value::Null => Value::Null,
            _ => json!(matches!(txt.as_str(), "on" | "1" | "true" | "yes")),
        },
        "Date" => {
            if txt.trim().is_empty() { Value::Null } else { Value::String(txt.trim().chars().take(10).collect()) }
        }
        "DateTime" => {
            if txt.trim().is_empty() { Value::Null } else { Value::String(txt.trim().replace('T', " ")) }
        }
        "MultiSelect" => match v {
            Value::Array(a) => Value::String(a.iter().map(vs).filter(|x| !x.is_empty()).collect::<Vec<_>>().join(",")),
            _ => Value::String(txt),
        },
        _ => match v {
            Value::Null => Value::Null,
            Value::Array(a) => Value::String(a.iter().map(vs).collect::<Vec<_>>().join(",")),
            _ => Value::String(txt),
        },
    }
}

/// Reject obviously dangerous fragments in an admin-supplied WHERE clause.
pub fn safe_condition(cond: &str) -> Option<String> {
    let c = cond.trim();
    if c.is_empty() || c.len() > 2000 {
        return None;
    }
    let pats = [
        r"(?i)\b(drop|delete|truncate|alter|create|insert|update)\s",
        r"(?i)\b(exec|execute|sp_|xp_|fn_)\b",
        r"(--|/\*|\*/)",
        r"(?i)\bunion\s+select\b",
        r"(?i);\s*select\b",
        r"(?i)\b(into\s+outfile|load_file|load\s+data)\b",
        r"(?i)\binformation_schema\b",
        r"(?i)\b(benchmark|sleep|waitfor|pg_sleep)\b",
    ];
    for p in pats {
        if regex::Regex::new(p).map(|r| r.is_match(c)).unwrap_or(false) {
            return None;
        }
    }
    Some(c.to_string())
}

fn cmp_part(col: &str, val: &Value) -> String {
    match val {
        Value::Null => format!("{} is null", col),
        Value::String(x) => {
            if x.contains('%') {
                format!("{} ilike {}", col, lit(val))
            } else if x.starts_with(">=") || x.starts_with("<=") {
                format!("{} {} {}", col, &x[..2], lit(&json!(x[2..].trim())))
            } else if x.starts_with('>') || x.starts_with('<') {
                format!("{} {} {}", col, &x[..1], lit(&json!(x[1..].trim())))
            } else if let Some(rest) = x.strip_prefix('!') {
                format!("{} != {}", col, lit(&json!(rest)))
            } else {
                format!("{} = {}", col, lit(val))
            }
        }
        v => format!("{} = {}", col, lit(v)),
    }
}

/// The qparams DSL used by `rows`/`first_row`:
/// `{field: value}` with `%like%`, `>`, `<`, `>=`, `<=`, `!value`, lists
/// (any of), null, and nested `and` / `or` / `not` groups.
pub fn parse_qparams(q: &Row, joiner: &str) -> String {
    let mut parts: Vec<String> = vec![];
    for (k, v) in q {
        if k == "and" || k == "or" {
            if let Value::Object(o) = v {
                parts.push(parse_qparams(o, k));
            }
        } else if k == "not" {
            if let Value::Object(o) = v {
                parts.push(format!("not {}", parse_qparams(o, "and")));
            }
        } else {
            let col = qcol(k);
            match v {
                Value::Array(a) => {
                    if a.is_empty() {
                        parts.push("1!=1".into());
                    } else {
                        parts.push(format!("({})", a.iter().map(|x| cmp_part(&col, x)).collect::<Vec<_>>().join(" or ")));
                    }
                }
                Value::Object(o) => {
                    for (ik, iv) in o {
                        if ik == "and" || ik == "or" {
                            match iv {
                                Value::Array(a) => parts.push(format!("({})", a.iter().map(|x| cmp_part(&col, x)).collect::<Vec<_>>().join(&format!(" {} ", ik)))),
                                Value::Object(io) => parts.push(parse_qparams(io, ik)),
                                _ => {}
                            }
                        }
                    }
                }
                v => parts.push(cmp_part(&col, v)),
            }
        }
    }
    if parts.is_empty() {
        return "(1=1)".into();
    }
    format!("({})", parts.join(&format!(" {} ", joiner)))
}

// ---- definition cache ----
//
// A tracker definition is 7 queries (tracker, fields, statuses, roles, transitions and
// their two join tables) and pages look trackers up many times (every BelongsTo cell
// in a list does), so definitions are kept in memory. Any statement this process runs
// that writes to a portal_tracker* table clears the cache (see db::note_write); the
// time limit covers edits made by another server or straight in the database.

const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

struct DefCache {
    gen: u64,
    by_key: HashMap<String, (std::time::Instant, Tracker)>,
}

fn def_cache() -> &'static std::sync::Mutex<DefCache> {
    static C: std::sync::OnceLock<std::sync::Mutex<DefCache>> = std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(DefCache { gen: 0, by_key: HashMap::new() }))
}

/// Forget every cached tracker definition.
pub fn definitions_changed() {
    let mut c = def_cache().lock().unwrap();
    c.gen += 1;
    c.by_key.clear();
}

fn cache_get(key: &str) -> Option<Tracker> {
    let c = def_cache().lock().unwrap();
    c.by_key.get(key).filter(|(at, _)| at.elapsed() < CACHE_TTL).map(|(_, t)| t.clone())
}

fn cache_gen() -> u64 {
    def_cache().lock().unwrap().gen
}

/// Store a definition read while the cache was at generation `gen`; if it was
/// cleared in the meantime the definition may be stale, so it is not kept.
fn cache_put(gen: u64, t: &Tracker) {
    let mut c = def_cache().lock().unwrap();
    if c.gen != gen {
        return;
    }
    let now = std::time::Instant::now();
    c.by_key.insert(format!("id:{}", t.id), (now, t.clone()));
    c.by_key.insert(format!("ms:{}\u{1}{}", t.module, t.slug), (now, t.clone()));
}

impl Tracker {
    pub async fn load(db: &Db, module: &str, slug: &str) -> Option<Tracker> {
        let key = format!("ms:{}\u{1}{}", module, slug);
        if let Some(t) = cache_get(&key) {
            return Some(t);
        }
        let gen = cache_gen();
        let r = db.first("select * from portal_tracker where module=:m and slug=:s order by id limit 1", &row! {"m"=>module,"s"=>slug}).await.ok().flatten()?;
        let t = Tracker::read(db, r).await?;
        // keyed by what was asked for too (module names are matched as the database matches them)
        cache_put(gen, &t);
        if t.module != module || t.slug != slug {
            let mut c = def_cache().lock().unwrap();
            if c.gen == gen {
                c.by_key.insert(key, (std::time::Instant::now(), t.clone()));
            }
        }
        Some(t)
    }

    /// `slug`, `module:slug` or `module:slug:field`.
    pub async fn load_ref(db: &Db, spec: &str, default_module: &str) -> Option<Tracker> {
        let t: Vec<&str> = spec.split(':').map(|x| x.trim()).collect();
        if t.len() > 1 {
            Tracker::load(db, t[0], t[1]).await
        } else {
            match Tracker::load(db, default_module, t[0]).await {
                Some(x) => Some(x),
                None => {
                    let r = db.first("select * from portal_tracker where slug=:s order by id limit 1", &row! {"s"=>t[0]}).await.ok().flatten()?;
                    Tracker::from_row(db, r).await
                }
            }
        }
    }

    pub async fn by_id(db: &Db, id: i64) -> Option<Tracker> {
        if let Some(t) = cache_get(&format!("id:{}", id)) {
            return Some(t);
        }
        let gen = cache_gen();
        let r = db.get("portal_tracker", id).await.ok().flatten()?;
        let t = Tracker::read(db, r).await?;
        cache_put(gen, &t);
        Some(t)
    }

    /// The definition for a portal_tracker row, from the cache when it is there.
    pub async fn from_row(db: &Db, r: Row) -> Option<Tracker> {
        let id = i(&r, "id")?;
        if let Some(t) = cache_get(&format!("id:{}", id)) {
            return Some(t);
        }
        let gen = cache_gen();
        let t = Tracker::read(db, r).await?;
        cache_put(gen, &t);
        Some(t)
    }

    /// Read a definition from the database (no cache).
    async fn read(db: &Db, r: Row) -> Option<Tracker> {
        let id = i(&r, "id")?;
        let p = row! {"t"=>id};
        let fields = db.rows("select * from portal_tracker_field where tracker_id=:t order by field_order nulls last, id", &p).await.ok()?;
        let statuses = db.rows("select * from portal_tracker_status where tracker_id=:t order by flow nulls last, id", &p).await.ok()?;
        let roles = db.rows("select * from portal_tracker_role where tracker_id=:t order by id", &p).await.ok()?;
        let trows = db.rows("select * from portal_tracker_transition where tracker_id=:t order by id", &p).await.ok()?;
        let troles = db
            .rows(
                "select j.portal_tracker_transition_roles_id as tid, r.name from portal_tracker_transition_portal_tracker_role j join portal_tracker_role r on r.id=j.portal_tracker_role_id where r.tracker_id=:t",
                &p,
            )
            .await
            .unwrap_or_default();
        let tprev = db
            .rows(
                "select j.portal_tracker_transition_prev_status_id as tid, j.portal_tracker_status_id as sid from portal_tracker_transition_portal_tracker_status j join portal_tracker_status st on st.id=j.portal_tracker_status_id where st.tracker_id=:t",
                &p,
            )
            .await
            .unwrap_or_default();
        let transitions = trows
            .into_iter()
            .map(|tr| {
                let tid = i(&tr, "id").unwrap_or(0);
                Trans {
                    id: tid,
                    name: s(&tr, "name"),
                    roles: troles.iter().filter(|x| i(x, "tid") == Some(tid)).map(|x| s(x, "name")).collect(),
                    prev: tprev.iter().filter(|x| i(x, "tid") == Some(tid)).filter_map(|x| i(x, "sid")).collect(),
                    next: i(&tr, "next_status_id"),
                    row: tr,
                }
            })
            .collect();
        Some(Tracker {
            id,
            module: s(&r, "module"),
            slug: s(&r, "slug"),
            name: s(&r, "name"),
            tracker_type: { let t = s(&r, "tracker_type"); if t.is_empty() { "Tracker".into() } else { t } },
            fields,
            statuses,
            roles,
            transitions,
            row: r,
        })
    }

    pub fn data_table(&self) -> String {
        let d = s(&self.row, "datatable");
        if !d.trim().is_empty() { d.trim().to_string() } else { format!("trak_{}_{}_data", self.module.to_lowercase(), self.slug.to_lowercase()) }
    }
    pub fn trail_table(&self) -> String {
        let d = s(&self.row, "trailtable");
        if !d.trim().is_empty() { d.trim().to_string() } else { format!("trak_{}_{}_updates", self.module.to_lowercase(), self.slug.to_lowercase()) }
    }
    /// Field names with every FieldGroup's members (named in its field_options) added
    /// after it, as the Grails portal's forms include them.
    pub fn expand_groups(&self, names: &[String]) -> Vec<String> {
        fn walk(t: &Tracker, names: &[String], out: &mut Vec<String>, depth: usize) {
            for n in names {
                if out.contains(n) {
                    continue;
                }
                out.push(n.clone());
                if depth < 4 {
                    if let Some(f) = t.field(n) {
                        if s(f, "field_type") == "FieldGroup" {
                            walk(t, &csv(&s(f, "field_options")), out, depth + 1);
                        }
                    }
                }
            }
        }
        let mut out = vec![];
        walk(self, names, &mut out, 0);
        out
    }

    pub fn field(&self, name: &str) -> Option<&Row> {
        let n = name.trim();
        self.fields.iter().find(|f| s(f, "name").trim() == n)
    }
    pub fn field_by_id(&self, id: i64) -> Option<&Row> {
        self.fields.iter().find(|f| i(f, "id") == Some(id))
    }
    pub fn status(&self, name: &str) -> Option<&Row> {
        self.statuses.iter().find(|f| s(f, "name") == name)
    }
    pub fn status_by_id(&self, id: i64) -> Option<&Row> {
        self.statuses.iter().find(|f| i(f, "id") == Some(id))
    }
    pub fn status_name(&self, id: Option<i64>) -> String {
        id.and_then(|x| self.status_by_id(x)).map(|r| s(r, "name")).unwrap_or_default()
    }
    pub fn initial_status(&self) -> Option<i64> {
        i(&self.row, "initial_status_id")
    }
    pub fn default_field(&self) -> Option<String> {
        if let Some(f) = i(&self.row, "defaultfield_id").and_then(|x| self.field_by_id(x)) {
            return Some(s(f, "name"));
        }
        for n in ["name", "title"] {
            if self.field(n).is_some() {
                return Some(n.to_string());
            }
        }
        None
    }
    /// Member names of a composite status, or the name itself.
    pub fn expand_status(&self, name: &str) -> Vec<String> {
        match self.status(name) {
            Some(st) if has(st, "composite_statuses") => csv(&s(st, "composite_statuses")),
            _ => vec![name.to_string()],
        }
    }
    pub fn url(&self, action: &str) -> String {
        format!("/{}/{}/{}", self.module, self.slug, action)
    }

    // ---------------- schema ----------------

    pub async fn updatedb(&self, db: &Db) -> Result<()> {
        let dt = ident(&self.data_table())?;
        db.ddl(&format!("create table if not exists {} (id serial primary key, dataupdate_id numeric(19,0) null)", dt)).await?;
        db.ddl(&format!("alter table {} add column if not exists dataupdate_id numeric(19,0) null", dt)).await?;
        if self.tracker_type != "DataStore" {
            db.ddl(&format!("alter table {} add column if not exists record_status varchar(255) null", dt)).await?;
        }
        if self.tracker_type == "Tracker" {
            let tt = ident(&self.trail_table())?;
            db.ddl(&format!(
                "create table if not exists {} (id serial primary key, attachment_id numeric(19,0), description text, record_id numeric(19,0), update_date timestamp, updater_id numeric(19,0), status varchar(255), changes text, allowedroles varchar(255))",
                tt
            ))
            .await?;
            db.ddl(&format!("create index if not exists \"ix_{}_record_id\" on {} (record_id)", self.trail_table(), tt)).await?;
        }
        for f in &self.fields {
            self.update_field_db(db, f).await?;
        }
        Ok(())
    }

    pub async fn update_field_db(&self, db: &Db, f: &Row) -> Result<()> {
        let ft = s(f, "field_type");
        let name = s(f, "name").trim().to_string();
        let Some(t) = sql_type(&ft) else { return Ok(()) };
        if name.is_empty() || name == "id" {
            return Ok(());
        }
        let dt = ident(&self.data_table())?;
        db.ddl(&format!("alter table {} add column if not exists {} {} null", dt, ident(&name)?, t)).await?;
        if matches!(ft.as_str(), "Branch" | "User" | "File" | "BelongsTo" | "TreeNode") {
            db.ddl(&format!("create index if not exists \"ix_{}_{}\" on {} ({})", self.data_table(), name, dt, ident(&name)?)).await?;
        }
        Ok(())
    }

    pub async fn create_index(&self, db: &Db) -> Result<()> {
        let dt = ident(&self.data_table())?;
        let mut names = csv(&s(&self.row, "searchfields"));
        names.extend(csv(&s(&self.row, "filterfields")));
        names.dedup();
        for n in names {
            if let Some(f) = self.field(&n) {
                if !matches!(s(f, "field_type").as_str(), "Text Area" | "HasMany" | "FieldGroup") {
                    db.ddl(&format!("create index if not exists \"ix_{}_{}\" on {} ({})", self.data_table(), n, dt, ident(&n)?)).await?;
                }
            }
        }
        let idx = db.rows("select * from portal_tracker_index where tracker_id=:t", &row! {"t"=>self.id}).await?;
        for ix in idx {
            let cols: Vec<String> = csv(&s(&ix, "fields"))
                .into_iter()
                .filter(|c| self.field(c).map(|f| !matches!(s(f, "field_type").as_str(), "Text Area" | "HasMany")).unwrap_or(c == "record_status"))
                .filter_map(|c| ident(&c).ok())
                .collect();
            let safe: String = s(&ix, "name").chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
            if !cols.is_empty() {
                db.ddl(&format!("drop index if exists \"ix_{}_{}\"", self.data_table(), safe)).await?;
                db.ddl(&format!("create index \"ix_{}_{}\" on {} ({})", self.data_table(), safe, dt, cols.join(","))).await?;
            }
        }
        Ok(())
    }

    /// Create field definitions from the columns of an existing data table.
    pub async fn from_table(&self, db: &Db) -> Result<usize> {
        let cols = db
            .rows("select column_name, udt_name from information_schema.columns where table_schema='public' and table_name=:t order by ordinal_position", &row! {"t"=>self.data_table()})
            .await?;
        let mut n = 0;
        for c in cols {
            let name = s(&c, "column_name");
            if matches!(name.as_str(), "id" | "dataupdate_id" | "record_status") || self.field(&name).is_some() {
                continue;
            }
            let ft = match s(&c, "udt_name").as_str() {
                "int2" | "int4" | "int8" => "Integer",
                "numeric" | "float4" | "float8" => "Number",
                "date" => "Date",
                "timestamp" | "timestamptz" => "DateTime",
                "bool" => "Checkbox",
                "text" => "Text Area",
                _ => "Text",
            };
            db.insert("portal_tracker_field", &row! {"tracker_id"=>self.id,"name"=>name.clone(),"label"=>crate::entity::label_of(&name),"field_type"=>ft}).await?;
            n += 1;
        }
        Ok(n)
    }

    // ---------------- reading ----------------

    pub async fn get(&self, db: &Db, id: i64) -> Option<Row> {
        db.first(&format!("select * from {} where id={}", qcol(&self.data_table()), id), &db::no_params()).await.ok().flatten()
    }

    fn expand_q(&self, q: &Row) -> Row {
        let mut q = q.clone();
        if let Some(Value::String(st)) = q.get("record_status").cloned() {
            let members = self.expand_status(&st);
            if members.len() > 1 || members.first() != Some(&st) {
                q.insert("record_status".into(), json!(members));
            }
        }
        q
    }

    pub async fn rows(&self, db: &Db, q: Option<&Row>, order: Option<&str>, offset: Option<i64>, limit: Option<i64>) -> Result<Vec<Row>> {
        let mut sql = format!("select * from {}", qcol(&self.data_table()));
        if let Some(q) = q {
            if !q.is_empty() {
                sql.push_str(" where ");
                sql.push_str(&parse_qparams(&self.expand_q(q), "and"));
            }
        }
        match order {
            Some(o) if !o.trim().is_empty() => sql.push_str(&format!(" order by {}", o)),
            _ => {
                if limit.is_some() {
                    sql.push_str(" order by id");
                }
            }
        }
        if let Some(l) = limit {
            sql.push_str(&format!(" limit {} offset {}", l, offset.unwrap_or(0)));
        }
        db.rows(&sql, &db::no_params()).await
    }

    pub async fn first_row(&self, db: &Db, q: Option<&Row>, order: Option<&str>) -> Result<Option<Row>> {
        let o = order.filter(|x| !x.trim().is_empty());
        Ok(self.rows(db, q, o.or(Some("id")), None, Some(1)).await?.into_iter().next())
    }

    pub async fn count(&self, db: &Db, q: Option<&Row>) -> Result<i64> {
        let mut sql = format!("select count(*) from {}", qcol(&self.data_table()));
        if let Some(q) = q {
            if !q.is_empty() {
                sql.push_str(" where ");
                sql.push_str(&parse_qparams(&self.expand_q(q), "and"));
            }
        }
        db.count(&sql, &db::no_params()).await
    }

    // ---------------- writing ----------------

    /// Insert (no id) or update (id) a record from a map. Unknown keys are ignored.
    pub async fn save(&self, db: &Db, datas: &Row) -> Result<i64> {
        let mut cols: Vec<(String, Value)> = vec![];
        for (k, v) in datas {
            if k == "id" {
                continue;
            }
            if k == "record_status" && (self.tracker_type != "DataStore" || self.field(k).is_none()) {
                // the workflow status column (a DataStore has none, unless it defines a record_status field)
                if self.tracker_type != "DataStore" {
                    cols.push((k.clone(), v.clone()));
                }
            } else if let Some(f) = self.field(k) {
                if sql_type(&s(f, "field_type")).is_some() {
                    cols.push((k.clone(), coerce(f, v)));
                }
            }
        }
        let dt = ident(&self.data_table())?;
        if let Some(id) = i(datas, "id") {
            let sets: Vec<String> = cols.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| format!("{}={}", qcol(k), lit(v))).collect();
            if !sets.is_empty() {
                db.exec(&format!("update {} set {} where id={}", dt, sets.join(","), id), &db::no_params()).await?;
            }
            Ok(id)
        } else {
            let sql = if cols.is_empty() {
                format!("insert into {} default values returning id", dt)
            } else {
                format!(
                    "insert into {} ({}) values ({}) returning id",
                    dt,
                    cols.iter().map(|(k, _)| qcol(k)).collect::<Vec<_>>().join(","),
                    cols.iter().map(|(_, v)| lit(v)).collect::<Vec<_>>().join(",")
                )
            };
            db.first(&sql, &db::no_params()).await?.and_then(|r| i(&r, "id")).ok_or_else(|| anyhow!("insert failed"))
        }
    }

    pub async fn delete(&self, db: &Db, id: i64) -> Result<()> {
        if let Some(data) = self.get(db, id).await {
            for f in &self.fields {
                if s(f, "field_type") == "File" {
                    if let Some(fid) = i(&data, &s(f, "name")) {
                        crate::files::delete_file_link(db, fid).await;
                    }
                }
            }
        }
        if self.tracker_type == "Tracker" {
            let _ = db.exec(&format!("delete from {} where record_id={}", qcol(&self.trail_table()), id), &db::no_params()).await;
        }
        db.exec(&format!("delete from {} where id={}", qcol(&self.data_table()), id), &db::no_params()).await?;
        Ok(())
    }

    pub async fn cleardb(&self, db: &Db) -> Result<()> {
        db.ddl(&format!("truncate table {}", ident(&self.data_table())?)).await?;
        if self.tracker_type == "Tracker" {
            db.ddl(&format!("truncate table {}", ident(&self.trail_table())?)).await?;
        }
        Ok(())
    }

    pub async fn save_trail(&self, sc: &Sc, record_id: i64, description: &str, status: Option<&str>, user_id: Option<i64>) -> Option<i64> {
        let mut p = row! {"id"=>record_id,"statusUpdateDesc"=>description};
        if let Some(st) = status {
            p.insert("record_status".into(), json!(st));
        }
        self.update_trail(sc, &p, user_id, None).await
    }

    pub async fn update_trail_desc(&self, db: &Db, trail_id: i64, description: &str) -> Result<()> {
        db.exec(&format!("update {} set description=:d where id=:id", qcol(&self.trail_table())), &row! {"d"=>description,"id"=>trail_id}).await?;
        Ok(())
    }

    /// Append an audit-trail entry (with optional attachment). Returns the trail id.
    pub async fn update_trail(&self, sc: &Sc, params: &Row, user_id: Option<i64>, upload: Option<&Upload>) -> Option<i64> {
        if self.tracker_type != "Tracker" {
            return None;
        }
        let db = sc.db();
        let rid = i(params, "id")?;
        let data = self.get(db, rid).await;
        let cur_status = data.as_ref().map(|d| s(d, "record_status")).and_then(|n| self.status(&n).cloned());
        let mut attachment: Option<i64> = None;
        if let Some(up) = upload {
            let base = match web::setting_str(db, &format!("{}.{}_attachment_path", self.module, self.slug)).await {
                Some(x) => x,
                None => match web::setting_str(db, &format!("{}_attachment_path", self.slug)).await {
                    Some(x) => x,
                    None => format!("{}/{}", sc.inner.st.cfg.upload_dir, self.slug),
                },
            };
            match crate::files::store_upload(db, up, &format!("{}/{}", base, rid), &self.module, &format!("{}_{}_{}", self.slug, rid, chrono::Local::now().format("%Y%m%d%H%M%S")), Some(&self.slug), Some(rid)).await {
                Ok(fid) => attachment = Some(fid),
                Err(e) => {
                    web::log_error(db, "portalTracker", "attachment_rejected", &e.to_string(), Some(&self.module), Some(&self.slug), user_id).await;
                    return None;
                }
            }
        }
        // "Admin->Admin; RD->RD,Admin": updates made as a role are only visible to the listed roles
        let mut visible: Vec<String> = vec![];
        if let Some(st) = &cur_status {
            let rules = s(st, "updateallowedroles");
            if !rules.is_empty() && rules.contains("->") {
                let uroles = self.user_roles(sc, Some(&row! {"id"=>rid})).await;
                for rule in rules.split(';') {
                    if let Some((who, see)) = rule.split_once("->") {
                        if uroles.iter().any(|u| u.trim() == who.trim()) && see.trim().to_lowercase() != "all" {
                            visible.push(see.trim().to_string());
                        }
                    }
                }
            }
        }
        let desc = s(params, "statusUpdateDesc");
        if desc.is_empty() && attachment.is_none() {
            return None;
        }
        let rec = row! {
            "attachment_id"=>attachment, "description"=>desc, "record_id"=>rid, "update_date"=>now_str(), "updater_id"=>user_id,
            "status"=>params.get("record_status").map(vs).filter(|x| !x.is_empty()).or_else(|| data.as_ref().map(|d| s(d, "record_status"))),
            "allowedroles"=>visible.join(","),
        };
        match db.insert(&self.trail_table(), &rec).await {
            Ok(id) => Some(id),
            Err(e) => {
                web::log_error(db, "tracker", "updatetrail", &e.to_string(), Some(&self.module), Some(&self.slug), user_id).await;
                None
            }
        }
    }

    // ---------------- roles ----------------

    /// Roles the user holds for the module as a whole (no record needed).
    pub async fn module_roles(&self, sc: &Sc) -> Vec<String> {
        let Some(u) = &sc.inner.user else { return vec![] };
        let key = format!("mroles_{}", self.id);
        if let Some(Value::Array(a)) = sc.cache_get(&key) {
            return a.iter().map(vs).collect();
        }
        let mroles = web::module_roles(sc.db(), u, &self.module).await;
        let mut out = vec![];
        for r in &self.roles {
            let name = s(r, "name");
            if name == "Authenticated" || (s(r, "role_type") == "User Role" && mroles.contains(&name)) {
                out.push(name);
            }
        }
        sc.cache_put(&key, json!(out));
        out
    }

    pub async fn is_admin(&self, sc: &Sc) -> bool {
        self.module_roles(sc).await.iter().any(|r| r == "Admin")
    }

    /// Roles the user holds for one record: user roles plus "Data Compare"
    /// roles whose rule (a template producing a SQL condition) matches.
    pub async fn user_roles(&self, sc: &Sc, datas: Option<&Row>) -> Vec<String> {
        let Some(u) = &sc.inner.user else { return vec![] };
        let rid = datas.and_then(|d| i(d, "id"));
        let key = format!("uroles_{}_{}", self.id, rid.unwrap_or(0));
        if let Some(Value::Array(a)) = sc.cache_get(&key) {
            return a.iter().map(vs).collect();
        }
        let db = sc.db();
        let uid = i(u, "id").unwrap_or(0);
        let names = web::granted_roles(db, uid, &self.module).await;
        let cur_role = web::current_role(db, u).await.map(|r| s(&r, "role").trim().to_string()).unwrap_or_default();
        let admin = self.is_admin(sc).await;
        let mut out = vec![];
        let mut compare: Vec<(String, String)> = vec![];
        for r in &self.roles {
            let name = s(r, "name");
            let rt = s(r, "role_type");
            if name == "Admin" && admin {
                out.push(name);
            } else if rt == "User Role" {
                if names.contains(&name) || (!cur_role.is_empty() && cur_role == name.trim()) {
                    out.push(name);
                }
            } else if rt == "Data Compare" && rid.is_some() {
                let cond = self.eval_role(sc, r, datas).await;
                if !cond.trim().is_empty() {
                    compare.push((name, cond));
                }
            }
        }
        if let Some(rid) = rid {
            for (name, cond) in compare {
                let q = format!("select 1 as ok from {} where id={} and ({})", qcol(&self.data_table()), rid, cond);
                match db.first(&q, &db::no_params()).await {
                    Ok(Some(_)) => out.push(name),
                    Ok(None) => {}
                    Err(e) => web::log_error(db, "trackerRole", "evalrole", &e.to_string(), Some(&self.module), Some(&self.slug), Some(uid)).await,
                }
            }
        }
        sc.cache_put(&key, json!(out));
        out
    }

    async fn eval_role(&self, sc: &Sc, role: &Row, datas: Option<&Row>) -> String {
        let rule = s(role, "role_rule");
        if rule.trim().is_empty() {
            return String::new();
        }
        let vars = row! {"datas"=>datas.cloned().map(Value::Object).unwrap_or(Value::Null)};
        match sc.render(&rule, &vars, false) {
            Ok(x) => x,
            Err(e) => {
                web::log_error(sc.db(), "trackerRole", "evalrole", &format!("role {}: {}", s(role, "name"), e), Some(&self.module), Some(&self.slug), None).await;
                String::new()
            }
        }
    }

    /// SQL conditions selecting the records a user can see through Data Compare roles.
    pub async fn role_query(&self, sc: &Sc) -> Vec<String> {
        let mut out = vec![];
        for r in &self.roles {
            if s(r, "role_type") == "Data Compare" {
                let c = self.eval_role(sc, r, None).await;
                if !c.trim().is_empty() {
                    out.push(c.trim().to_string());
                }
            }
        }
        out
    }

    // ---------------- transitions ----------------

    pub fn transitions_named(&self, name: &str) -> Vec<&Trans> {
        let n = name.replace('_', " ").to_lowercase();
        self.transitions.iter().filter(|t| t.name.to_lowercase() == n || t.name.replace('_', " ").to_lowercase() == n).collect()
    }

    pub async fn transition_allowed(&self, sc: &Sc, name: &str, datas: Option<&Row>) -> bool {
        let ts = self.transitions_named(name);
        if ts.is_empty() {
            return false;
        }
        let wanted: Vec<&String> = ts.iter().flat_map(|t| t.roles.iter()).collect();
        if wanted.is_empty() {
            return true;
        }
        let uroles = self.user_roles(sc, datas).await;
        uroles.iter().any(|u| wanted.contains(&u))
    }

    pub async fn test_enabled(&self, sc: &Sc, t: &Trans, datas: Option<&Row>) -> bool {
        if datas.is_some() && t.next.is_some() && self.initial_status() == t.next && t.prev.is_empty() {
            return false;
        }
        if !t.roles.is_empty() {
            let uroles = self.user_roles(sc, datas).await;
            if !uroles.iter().any(|u| t.roles.contains(u)) {
                return false;
            }
        }
        let cond = s(&t.row, "enabledcondition");
        if cond.trim().is_empty() {
            return true;
        }
        let vars = row! {"datas"=>datas.cloned().map(Value::Object).unwrap_or(Value::Null)};
        match sc.eval(&cond, &vars) {
            Ok(v) => crate::script::truthy(&v),
            Err(e) => {
                web::log_error(sc.db(), "trackertransition", "testenabled", &format!("{}: {}", t.name, e), Some(&self.module), Some(&self.slug), None).await;
                false
            }
        }
    }

    pub fn matches_prev(&self, t: &Trans, status: &str) -> bool {
        t.prev.iter().any(|sid| match self.status_by_id(*sid) {
            Some(st) => s(st, "name") == status || csv(&s(st, "composite_statuses")).iter().any(|m| m == status),
            None => false,
        })
    }

    /// Resolve the transition named in a URL to one row, preferring the
    /// variant whose previous status matches the record.
    pub async fn resolve(&self, sc: &Sc, name: &str, datas: Option<&Row>) -> Option<Trans> {
        let mut enabled = vec![];
        for t in self.transitions_named(name) {
            if self.test_enabled(sc, t, datas).await {
                enabled.push(t);
            }
        }
        if enabled.is_empty() {
            return None;
        }
        if let Some(st) = datas.map(|d| s(d, "record_status")).filter(|x| !x.is_empty()) {
            if let Some(m) = enabled.iter().filter(|t| self.matches_prev(t, &st)).last() {
                return Some((*m).clone());
            }
        }
        enabled.last().map(|t| (*t).clone())
    }

    /// Transitions offered for a record in its current status.
    pub async fn available_transitions(&self, sc: &Sc, datas: &Row) -> Vec<Trans> {
        let st = s(datas, "record_status");
        let mut out = vec![];
        for t in &self.transitions {
            if t.prev.is_empty() || !self.matches_prev(t, &st) {
                continue;
            }
            if self.test_enabled(sc, t, Some(datas)).await {
                out.push(t.clone());
            }
        }
        out
    }

    /// The transition that creates new records.
    pub fn new_transition(&self) -> Option<&Trans> {
        if let Some(t) = self.transitions.iter().find(|t| t.name == "New") {
            return Some(t);
        }
        let init = self.initial_status()?;
        let cands: Vec<&Trans> = self.transitions.iter().filter(|t| t.next == Some(init)).collect();
        cands.iter().find(|t| t.prev.is_empty()).copied().or_else(|| cands.first().copied())
    }

    // ---------------- list query ----------------

    /// Build the list SQL from request parameters (search, filters, paging)
    /// plus the visibility rules for the current user.
    pub async fn list_query(&self, sc: &Sc, params: &Row, select: &str, background: bool) -> String {
        let db = sc.db();
        let count = select.trim().to_lowercase().starts_with("select count");
        let mut cond: Vec<String> = vec![];
        for (k, v) in params {
            let val = vs(v);
            if val.is_empty() {
                continue;
            }
            if k == "search" {
                let term = val.trim().replace('\\', "\\\\").replace('_', "\\_");
                let like = lit(&json!(format!("%{}%", term)));
                let mut ors = vec![];
                for n in csv(&s(&self.row, "searchfields")) {
                    let Some(f) = self.field(&n) else { continue };
                    let col = qcol(&n);
                    match s(f, "field_type").as_str() {
                        "BelongsTo" => {
                            if let Some(ot) = self.other_tracker(db, f).await {
                                let ff = other_format(f);
                                let fmt = if ff.is_empty() { ot.default_field().unwrap_or_default() } else { ff };
                                if db::ident_ok(&fmt) {
                                    ors.push(format!("{} in (select id from {} where \"{}\"::text ilike {})", col, qcol(&ot.data_table()), fmt, like));
                                }
                            }
                        }
                        "User" => ors.push(format!("{} in (select id from iap_user where emp_name ilike {l} or staffid ilike {l} or new_staffid ilike {l} or email ilike {l})", col, l = like)),
                        "TreeNode" | "Branch" => ors.push(format!("{} in (select id from tree_node where name ilike {})", col, like)),
                        "HasMany" | "FieldGroup" | "File" => {}
                        _ => ors.push(format!("{}::text ilike {}", col, like)),
                    }
                }
                if !ors.is_empty() {
                    cond.push(format!("({})", ors.join(" or ")));
                }
            } else if k == "record_status" && (self.tracker_type != "DataStore" || self.field(k).is_none()) {
                if self.tracker_type != "DataStore" {
                    let members = self.expand_status(&val);
                    cond.push(format!("record_status in {}", lit(&json!(members))));
                }
            } else if !matches!(k.as_str(), "slug" | "module" | "action" | "controller" | "max" | "offset" | "excel" | "csrf_token") {
                let Some(f) = self.field(k) else { continue };
                let col = qcol(k);
                match s(f, "field_type").as_str() {
                    "Date" | "DateTime" => {
                        let d = val.trim();
                        let ym = regex::Regex::new(r"^(\d{4})-(\d{1,2})$").unwrap();
                        if let Some(c) = ym.captures(d) {
                            cond.push(format!("extract(YEAR from {c})={} and extract(MONTH from {c})={}", &c[1], c[2].parse::<i32>().unwrap_or(0), c = col));
                        } else if let Some(rest) = d.strip_prefix("between_") {
                            let ds: Vec<&str> = rest.split('_').map(|x| x.trim()).collect();
                            if ds.len() == 2 && ds.iter().all(|x| is_date(x)) {
                                cond.push(format!("{} between '{}' and '{} 23:59:59'", col, ds[0], ds[1]));
                            }
                        } else if d.starts_with('<') || d.starts_with('>') {
                            let bound = d[1..].trim();
                            if is_date(bound) {
                                cond.push(format!("{} {} '{}'", col, &d[..1], bound));
                            }
                        } else if is_date(d) {
                            let next = chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").unwrap() + chrono::Duration::days(1);
                            cond.push(format!("{c} >= '{}' and {c} < '{}'", d, next.format("%Y-%m-%d"), c = col));
                        }
                    }
                    "Checkbox" => {
                        if val == "true" || val == "1" {
                            cond.push(format!("{} = true", col));
                        } else if val == "false" || val == "0" {
                            cond.push(format!("({c} = false or {c} is null)", c = col));
                        }
                    }
                    "HasMany" | "FieldGroup" => {}
                    "Text" if val.starts_with("between_") => {
                        let ds: Vec<&str> = val[8..].split('_').map(|x| x.trim()).collect();
                        if ds.len() == 2 {
                            cond.push(format!("{} between {} and {}", col, lit(&json!(ds[0])), lit(&json!(ds[1]))));
                        }
                    }
                    "MultiSelect" => cond.push(format!("{}::text ilike {}", col, lit(&json!(format!("%{}%", val))))),
                    _ => cond.push(format!("{} = {}", col, lit(&coerce(f, v)))),
                }
            }
        }
        // who may see what
        if !background {
            let mut ruled = false;
            if sc.inner.user.is_some() {
                if !self.module_roles(sc).await.is_empty() {
                    ruled = true;
                } else {
                    let rq = self.role_query(sc).await;
                    if !rq.is_empty() {
                        cond.push(format!("({})", rq.iter().map(|q| format!("({})", q)).collect::<Vec<_>>().join(" or ")));
                        ruled = true;
                    }
                }
            }
            if !ruled && !b(&self.row, "anonymous_list") {
                cond.push("1!=1".into());
            }
        }
        if let Some(c) = safe_condition(&s(&self.row, "condition_q")) {
            cond.push(format!("({})", c));
        }
        let mut sql = format!("{} from {}", select, qcol(&self.data_table()));
        if !cond.is_empty() {
            sql.push_str(" where ");
            sql.push_str(&cond.join(" and "));
        }
        if !count {
            let sort = s(&self.row, "defaultsort");
            sql.push_str(&format!(" order by {}", if sort.trim().is_empty() { "id".to_string() } else { sort }));
            if let Some(max) = i(params, "max") {
                sql.push_str(&format!(" limit {} offset {}", max.clamp(1, 100000), i(params, "offset").unwrap_or(0).max(0)));
            }
        }
        sql
    }

    /// Tracker referenced by a BelongsTo/HasMany field (`slug`, `module:slug`, `module:slug:field`).
    pub async fn other_tracker(&self, db: &Db, f: &Row) -> Option<Tracker> {
        let opt = s(f, "field_options");
        if opt.trim().is_empty() {
            return None;
        }
        Tracker::load_ref(db, opt.trim(), &self.module).await
    }

    // ---------------- record update from a form ----------------

    /// Apply a submitted transition form: creates the record when there is
    /// no id, deletes it when the next status is "Delete", otherwise updates
    /// the submitted fields and the status. Returns the saved values.
    pub async fn update_record(
        &self,
        sc: &Sc,
        params: &Row,
        files: &HashMap<String, Upload>,
        allowed: Option<&[String]>,
        default_fields: &[String],
        clear_fields: &[String],
    ) -> Result<(Row, Option<Row>, bool)> {
        let db = sc.db();
        let uid = sc.inner.user.as_ref().and_then(|u| i(u, "id"));
        let mut id = i(params, "id");
        let mut cur: Row = Row::new();
        let mut prev: Option<Row> = None;
        let next_status = i(params, "next_status").and_then(|x| self.status_by_id(x)).map(|r| s(r, "name"));
        if let Some(rid) = id {
            cur = self.get(db, rid).await.ok_or_else(|| anyhow!("Record not found"))?;
            prev = Some(cur.clone());
            if next_status.as_deref().map(|x| x.to_lowercase()) == Some("delete".into()) {
                // remove children that point back at this record
                for f in &self.fields {
                    if s(f, "field_type") == "HasMany" {
                        if let Some(ot) = self.other_tracker(db, f).await {
                            if let Some(link) = ot.fields.iter().find(|x| s(x, "field_type") == "BelongsTo" && s(x, "field_options").contains(&self.slug)) {
                                let q = row! {s(link, "name")=>rid};
                                for child in ot.rows(db, Some(&q), None, None, None).await.unwrap_or_default() {
                                    if let Some(cid) = i(&child, "id") {
                                        let _ = ot.delete(db, cid).await;
                                    }
                                }
                            }
                        }
                    }
                }
                self.delete(db, rid).await?;
                return Ok((cur, prev, true));
            }
        } else if self.tracker_type != "DataStore" {
            let r = db.first(&format!("insert into {} (record_status) values ('sys_draft') returning id", ident(&self.data_table())?), &db::no_params()).await?;
            id = r.and_then(|r| i(&r, "id"));
            cur.insert("id".into(), json!(id));
        }
        let mut set: Vec<(String, Value)> = vec![];
        let mut defaults: Vec<String> = default_fields.to_vec();
        if let Some(ns) = &next_status {
            if self.tracker_type != "DataStore" {
                set.push(("record_status".into(), json!(ns)));
                cur.insert("record_status".into(), json!(ns));
            }
        }
        let mut keys: Vec<String> = params.keys().cloned().collect();
        // an unticked checkbox posts nothing: treat editable checkboxes as submitted
        for cf in clear_fields {
            if !keys.contains(cf) {
                if let Some(f) = self.field(cf) {
                    if s(f, "field_type") == "Checkbox" || s(f, "field_type") == "MultiSelect" {
                        keys.push(cf.clone());
                    }
                }
            }
        }
        for key in keys {
            if matches!(key.as_str(), "next_status" | "record_status" | "id" | "slug" | "module" | "csrf_token") {
                continue;
            }
            let fname = if key == "indata_slug" { "slug".to_string() } else { key.clone() };
            let Some(f) = self.field(&fname) else { continue };
            if let Some(al) = allowed {
                if !al.iter().any(|a| a == &fname) {
                    continue;
                }
            }
            if has(f, "field_default") {
                if !defaults.contains(&fname) {
                    defaults.push(fname.clone());
                }
                continue;
            }
            let ft = s(f, "field_type");
            let raw = params.get(&key).cloned().unwrap_or(Value::Null);
            let txt = vs(&raw);
            let clearable = clear_fields.contains(&fname);
            let value: Option<Value> = match ft.as_str() {
                "HasMany" | "FieldGroup" => None,
                "File" => match files.get(&key) {
                    Some(up) => {
                        let rid = id.unwrap_or(0);
                        let mut base = match web::setting_str(db, &format!("{}.{}_upload_path", self.module, self.slug)).await {
                            Some(x) => format!("{}/{}", x, rid),
                            None => format!("{}/{}/{}", sc.inner.st.cfg.upload_dir, self.slug, rid),
                        };
                        if has(f, "field_options") {
                            if let Ok(v) = sc.eval(&s(f, "field_options"), &row! {"datas"=>Value::Object(cur.clone())}) {
                                let sub = vs(&v);
                                let clean: Vec<&str> = sub.split('/').map(|x| x.trim()).filter(|x| !x.is_empty() && *x != "..").collect();
                                if !clean.is_empty() {
                                    let root = web::setting_str(db, &format!("{}.{}_upload_path", self.module, self.slug)).await.unwrap_or_else(|| format!("{}/{}", sc.inner.st.cfg.upload_dir, self.slug));
                                    base = format!("{}/{}", root, clean.join("/"));
                                }
                            }
                        }
                        let slug = format!("{}_{}_{}", fname, rid, chrono::Local::now().format("%Y%m%d%H%M%S"));
                        match crate::files::store_upload(db, up, &base, &self.module, &slug, Some(&self.slug), Some(rid)).await {
                            Ok(fid) => Some(json!(fid)),
                            Err(e) => {
                                web::log_error(db, "tracker", "filelink_save_failed", &e.to_string(), Some(&self.module), Some(&self.slug), uid).await;
                                None
                            }
                        }
                    }
                    None => None,
                },
                "Number" | "Integer" | "User" | "Branch" | "BelongsTo" | "TreeNode" => {
                    if txt.trim().is_empty() {
                        if clearable { Some(Value::Null) } else { None }
                    } else {
                        let v = coerce(f, &raw);
                        if v.is_null() { None } else { Some(v) }
                    }
                }
                "Checkbox" => Some(json!(matches!(txt.as_str(), "on" | "1" | "true" | "yes"))),
                "Date" | "DateTime" => Some(coerce(f, &raw)),
                "Text" if has(f, "field_options") && !txt.is_empty() => {
                    // a Text field can copy a value from another tracker's record
                    let mut v = coerce(f, &raw);
                    if let (Some(ot), Ok(oid)) = (self.other_tracker(db, f).await, txt.trim().parse::<i64>()) {
                        let ff = other_format(f);
                        if !ff.is_empty() {
                            if let Some(od) = ot.get(db, oid).await {
                                v = od.get(&ff).cloned().unwrap_or(v);
                            }
                        }
                    }
                    Some(v)
                }
                _ => Some(coerce(f, &raw)),
            };
            if let Some(v) = value {
                cur.insert(fname.clone(), v.clone());
                set.push((fname, v));
            }
        }
        for name in defaults {
            let Some(f) = self.field(&name) else { continue };
            let vars = row! {"datas"=>Value::Object(cur.clone())};
            match sc.eval(&s(f, "field_default"), &vars) {
                Ok(v) => {
                    if crate::script::truthy(&v) || v == json!(0) || v == json!(false) {
                        let cv = coerce(f, &v);
                        cur.insert(name.clone(), cv.clone());
                        set.retain(|(k, _)| k != &name);
                        set.push((name, cv));
                    }
                }
                Err(e) => web::log_error(db, "tracker", "field_default", &format!("{}: {}", name, e), Some(&self.module), Some(&self.slug), uid).await,
            }
        }
        if !set.is_empty() {
            let dt = ident(&self.data_table())?;
            if let Some(rid) = id {
                let sets: Vec<String> = set.iter().map(|(k, v)| format!("{}={}", qcol(k), lit(v))).collect();
                db.exec(&format!("update {} set {} where id={}", dt, sets.join(","), rid), &db::no_params()).await?;
            } else {
                let r = db
                    .first(
                        &format!(
                            "insert into {} ({}) values ({}) returning id",
                            dt,
                            set.iter().map(|(k, _)| qcol(k)).collect::<Vec<_>>().join(","),
                            set.iter().map(|(_, v)| lit(v)).collect::<Vec<_>>().join(",")
                        ),
                        &db::no_params(),
                    )
                    .await?;
                id = r.and_then(|r| i(&r, "id"));
            }
        }
        let mut out = Row::new();
        for (k, v) in set {
            out.insert(k, v);
        }
        out.insert("id".into(), json!(id));
        Ok((out, prev, false))
    }

    /// Render a transition's `updatetrails` expression into the trail text.
    pub fn trail_text(&self, sc: &Sc, t: &Trans, datas: &Row) -> Option<String> {
        let code = s(&t.row, "updatetrails");
        if code.trim().is_empty() {
            return None;
        }
        match sc.eval(&code, &row! {"datas"=>Value::Object(datas.clone())}) {
            Ok(v) => Some(vs(&v)).filter(|x| !x.is_empty()),
            Err(_) => Some(code),
        }
    }
}

/// For `module:slug:field` references the third token names the display field.
pub fn other_format(f: &Row) -> String {
    let ff = s(f, "field_format");
    if !ff.trim().is_empty() && db::ident_ok(ff.trim()) {
        return ff.trim().to_string();
    }
    let opt = s(f, "field_options");
    let t: Vec<&str> = opt.split(':').collect();
    if t.len() > 2 { t[2].trim().to_string() } else { String::new() }
}
