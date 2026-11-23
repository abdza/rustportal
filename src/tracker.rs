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
    sql_type_for(crate::db::Dialect::Postgres, field_type)
}

pub fn sql_type_for(dialect: crate::db::Dialect, field_type: &str) -> Option<&'static str> {
    if dialect == crate::db::Dialect::Mssql {
        return Some(match field_type {
            "Text Area" => "nvarchar(max)",
            "Date" => "date",
            "DateTime" => "datetime2",
            "Checkbox" => "bit",
            "Integer" => "numeric(24,0)",
            "Number" => "decimal(24,6)",
            "Branch" | "User" | "File" | "BelongsTo" | "TreeNode" => "numeric(19,0)",
            "HasMany" | "FieldGroup" => return None,
            _ => "nvarchar(256)",
        });
    }
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
        r"(?i)\b(information_schema|sqlite_master|sqlite_schema)\b",
        r"(?i)\b(benchmark|sleep|waitfor|pg_sleep|pragma)\b",
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
        let dt_name = self.data_table();
        let dt = ident(&dt_name)?;
        if db.is_mssql() {
            if !db.table_exists(&dt_name).await {
                db.ddl(&format!("create table {} (\"id\" int not null identity(1,1) primary key, \"dataupdate_id\" numeric(19,0) null)", dt)).await?;
            }
            db.add_column_if_missing(&dt_name, "dataupdate_id", "numeric(19,0)").await?;
            if self.tracker_type != "DataStore" {
                db.add_column_if_missing(&dt_name, "record_status", "nvarchar(255)").await?;
            }
            if self.tracker_type == "Tracker" {
                let tt_name = self.trail_table();
                let tt = ident(&tt_name)?;
                if !db.table_exists(&tt_name).await {
                    db.ddl(&format!(
                        "create table {} (\"id\" int not null identity(1,1) primary key, \"attachment_id\" numeric(19,0), \"description\" nvarchar(max), \"record_id\" numeric(19,0), \"update_date\" datetime2, \"updater_id\" numeric(19,0), \"status\" nvarchar(255), \"changes\" nvarchar(max), \"allowedroles\" nvarchar(255))",
                        tt
                    ))
                    .await?;
                }
                let ix = format!("ix_{}_record_id", tt_name);
                db.ddl(&format!(
                    "if not exists (select 1 from sys.indexes where name='{ix}' and object_id=object_id('{tt_name}')) create nonclustered index \"{ix}\" on {tt} (\"record_id\")"
                ))
                .await?;
            }
        } else {
            let id_col = if db.is_sqlite() { "id integer primary key autoincrement" } else { "id serial primary key" };
            db.ddl(&format!("create table if not exists {} ({}, dataupdate_id numeric(19,0) null)", dt, id_col)).await?;
            db.add_column_if_missing(&dt_name, "dataupdate_id", "numeric(19,0)").await?;
            if self.tracker_type != "DataStore" {
                db.add_column_if_missing(&dt_name, "record_status", "varchar(255)").await?;
            }
            if self.tracker_type == "Tracker" {
                let tt = ident(&self.trail_table())?;
                db.ddl(&format!(
                    "create table if not exists {} ({}, attachment_id numeric(19,0), description text, record_id numeric(19,0), update_date timestamp, updater_id numeric(19,0), status varchar(255), changes text, allowedroles varchar(255))",
                    tt, id_col
                ))
                .await?;
                db.ddl(&format!("create index if not exists \"ix_{}_record_id\" on {} (record_id)", self.trail_table(), tt)).await?;
            }
        }
        for f in &self.fields {
            self.update_field_db(db, f).await?;
        }
        Ok(())
    }

    pub async fn update_field_db(&self, db: &Db, f: &Row) -> Result<()> {
        let ft = s(f, "field_type");
        let name = s(f, "name").trim().to_string();
        let Some(t) = sql_type_for(db.dialect(), &ft) else { return Ok(()) };
        if name.is_empty() || name == "id" {
            return Ok(());
        }
        let dt_name = self.data_table();
        let dt = ident(&dt_name)?;
        db.add_column_if_missing(&dt_name, &name, t).await?;
        if matches!(ft.as_str(), "Branch" | "User" | "File" | "BelongsTo" | "TreeNode") {
            let ix = format!("ix_{}_{}", dt_name, name);
            if db.is_mssql() {
                db.ddl(&format!(
                    "if not exists (select 1 from sys.indexes where name='{ix}' and object_id=object_id('{dt_name}')) create nonclustered index \"{ix}\" on {dt} ({})",
                    ident(&name)?
                ))
                .await?;
            } else {
                db.ddl(&format!("create index if not exists \"{ix}\" on {dt} ({})", ident(&name)?)).await?;
            }
        }
        Ok(())
    }

    pub async fn create_index(&self, db: &Db) -> Result<()> {
        let dt_name = self.data_table();
        let dt = ident(&dt_name)?;
        let mut names = csv(&s(&self.row, "searchfields"));
        names.extend(csv(&s(&self.row, "filterfields")));
        names.dedup();
        for n in names {
            if let Some(f) = self.field(&n) {
                if !matches!(s(f, "field_type").as_str(), "Text Area" | "HasMany" | "FieldGroup") {
                    let ix = format!("ix_{}_{}", dt_name, n);
                    if db.is_mssql() {
                        db.ddl(&format!(
                            "if not exists (select 1 from sys.indexes where name='{ix}' and object_id=object_id('{dt_name}')) create nonclustered index \"{ix}\" on {dt} ({})",
                            ident(&n)?
                        ))
                        .await?;
                    } else {
                        db.ddl(&format!("create index if not exists \"{ix}\" on {dt} ({})", ident(&n)?)).await?;
                    }
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
                let ix_name = format!("ix_{}_{}", dt_name, safe);
                if db.is_mssql() {
                    db.ddl(&format!(
                        "if exists (select 1 from sys.indexes where name='{ix_name}' and object_id=object_id('{dt_name}')) drop index \"{ix_name}\" on {dt}"
                    ))
                    .await?;
                    db.ddl(&format!("create nonclustered index \"{ix_name}\" on {dt} ({})", cols.join(","))).await?;
                } else {
                    db.ddl(&format!("drop index if exists \"{ix_name}\"")).await?;
                    db.ddl(&format!("create index \"{ix_name}\" on {dt} ({})", cols.join(","))).await?;
                }
            }
        }
        Ok(())
    }

    /// Create field definitions from the columns of an existing data table.
    pub async fn from_table(&self, db: &Db) -> Result<usize> {
        let cols = db.table_columns(&self.data_table()).await?;
        let mut n = 0;
        for (name, udt) in cols {
            if matches!(name.as_str(), "id" | "dataupdate_id" | "record_status") || self.field(&name).is_some() {
                continue;
            }
            let base = udt.to_ascii_lowercase();
            let base = base.split('(').next().unwrap_or(&base).trim();
            let ft = match base {
                "int2" | "int4" | "int8" | "int" | "integer" | "bigint" | "smallint" | "tinyint" => "Integer",
                "numeric" | "decimal" | "float4" | "float8" | "real" | "float" | "double" | "double precision" | "money" | "smallmoney" => "Number",
                "date" => "Date",
                "timestamp" | "timestamptz" | "datetime" | "datetime2" | "smalldatetime" | "datetimeoffset" => "DateTime",
                "bool" | "boolean" | "bit" => "Checkbox",
                "text" | "ntext" => "Text Area",
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
        } else if db.is_mssql() {
            let sql = if cols.is_empty() {
                format!("set nocount on; insert into {} default values; select cast(scope_identity() as bigint) as id; set nocount off", dt)
            } else {
                format!(
                    "set nocount on; insert into {} ({}) values ({}); select cast(scope_identity() as bigint) as id; set nocount off",
                    dt,
                    cols.iter().map(|(k, _)| qcol(k)).collect::<Vec<_>>().join(","),
                    cols.iter().map(|(_, v)| lit(v)).collect::<Vec<_>>().join(",")
                )
            };
            db.first(&sql, &db::no_params()).await?.and_then(|r| i(&r, "id")).ok_or_else(|| anyhow!("insert failed"))
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
        let stmt = if db.is_sqlite() { "delete from" } else { "truncate table" };
        db.ddl(&format!("{} {}", stmt, ident(&self.data_table())?)).await?;
        if self.tracker_type == "Tracker" {
            db.ddl(&format!("{} {}", stmt, ident(&self.trail_table())?)).await?;
        }
        Ok(())
    }

    pub async fn save_trail(&self, sc: &Sc, record_id: i64, description: &str, status: Option<&str>, user_id: Option<i64>) -> Option<i64> {
        let mut p = row! {"id"=>record_id,"statusUpdateDesc"=>description};
        if let Some(st) = status {
            p.insert("record_status".into(), json!(st));
        } else if crate::db::g6() {
            // a g6 portal's savetrail without a status writes none
            p.insert("__no_status".into(), json!(true));
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
            "status"=>if b(params, "__no_status") { None } else { params.get("record_status").map(vs).filter(|x| !x.is_empty()).or_else(|| data.as_ref().map(|d| s(d, "record_status"))) },
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
            } else if name == "Authenticated" && crate::db::g6() {
                // g6 portals grant it to everyone logged in (g5 only does in module_roles)
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
        if crate::db::g6() && rule.contains("${") {
            return self.eval_role_gstring(sc, role, &rule, datas).await;
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

    /// A g6 portal's role rule is a GSP template: text with ${groovy} pieces, given
    /// curuser, datas and the session (whose 'profile' is the user's member record).
    /// Each piece is translated to Rhai; null prints as "null", and any error makes the
    /// whole rule empty, as the Grails portal does.
    async fn eval_role_gstring(&self, sc: &Sc, role: &Row, rule: &str, datas: Option<&Row>) -> String {
        let mut vars = row! {"datas"=>datas.cloned().map(Value::Object).unwrap_or(Value::Null)};
        if rule.contains("profile") {
            vars.insert("session_profile".into(), user_profile(sc).await);
        }
        let mut out = String::new();
        let mut rest = rule;
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let body = &rest[start + 2..];
            let mut depth = 1;
            let mut end = None;
            for (i, c) in body.char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let Some(end) = end else { break };
            let code = groovy_rule_expr(&body[..end]);
            match sc.eval(&code, &vars) {
                Ok(Value::Null) => out.push_str("null"),
                Ok(v) => out.push_str(&vs(&v)),
                Err(e) => {
                    web::log_error(sc.db(), "trackerRole", "evalrole", &format!("role {}: {}", s(role, "name"), e), Some(&self.module), Some(&self.slug), None).await;
                    return String::new();
                }
            }
            rest = &body[end + 1..];
        }
        out.push_str(rest);
        out
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
        let code = if crate::db::g6() || looks_groovy_snippet(&cond) { groovy_tracker_script(&cond) } else { cond };
        let vars = row! {"datas"=>datas.cloned().map(Value::Object).unwrap_or(Value::Null)};
        match sc.eval(&code, &vars) {
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
                                    ors.push(format!("{} in (select id from {} where \"{}\"::text ilike {} escape '\\')", col, qcol(&ot.data_table()), fmt, like));
                                }
                            }
                        }
                        "User" => ors.push(format!("{} in (select id from portal_user where name ilike {l} escape '\\' or userid ilike {l} escape '\\' or email ilike {l} escape '\\')", col, l = like)),
                        "TreeNode" | "Branch" => ors.push(format!("{} in (select id from portal_tree_node where name ilike {} escape '\\')", col, like)),
                        "HasMany" | "FieldGroup" | "File" => {}
                        _ => ors.push(format!("{}::text ilike {} escape '\\'", col, like)),
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
            let sql = if db.is_mssql() {
                format!("set nocount on; insert into {} (record_status) values ('sys_draft'); select cast(scope_identity() as bigint) as id; set nocount off", ident(&self.data_table())?)
            } else {
                format!("insert into {} (record_status) values ('sys_draft') returning id", ident(&self.data_table())?)
            };
            let r = db.first(&sql, &db::no_params()).await?;
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
            if has(f, "field_default") && !defaults.contains(&fname) {
                // the default wins when it gives a value; otherwise the posted value
                // stays, as in the Grails portals
                defaults.push(fname.clone());
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
            let code = s(f, "field_default");
            let result = match groovy_default(&code) {
                GroovyDefault::Value(v) => Ok(v),
                GroovyDefault::Code(c) => sc.eval(&c, &vars),
            };
            match result {
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
                let sql = if db.is_mssql() {
                    format!(
                        "set nocount on; insert into {} ({}) values ({}); select cast(scope_identity() as bigint) as id; set nocount off",
                        dt,
                        set.iter().map(|(k, _)| qcol(k)).collect::<Vec<_>>().join(","),
                        set.iter().map(|(_, v)| lit(v)).collect::<Vec<_>>().join(",")
                    )
                } else {
                    format!(
                        "insert into {} ({}) values ({}) returning id",
                        dt,
                        set.iter().map(|(k, _)| qcol(k)).collect::<Vec<_>>().join(","),
                        set.iter().map(|(_, v)| lit(v)).collect::<Vec<_>>().join(",")
                    )
                };
                let r = db.first(&sql, &db::no_params()).await?;
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
        let eval_code = if crate::db::g6() || looks_groovy_snippet(&code) { groovy_tracker_script(&code) } else { code.clone() };
        match sc.eval(&eval_code, &row! {"datas"=>Value::Object(datas.clone())}) {
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

pub enum GroovyDefault {
    Value(Value),
    Code(String),
}

/// A field default written for a Grails portal, as a value or as Rhai code:
/// a quoted string, `new SimpleDateFormat(p).format(new Date())`, and
/// `session.curuser.userID` (the Grails session's user) are turned into what
/// Rhai or this portal understand; anything else is run as written.
pub fn groovy_default(code: &str) -> GroovyDefault {
    use std::sync::OnceLock;
    static DATE: OnceLock<regex::Regex> = OnceLock::new();
    static STR: OnceLock<regex::Regex> = OnceLock::new();
    let c = code.trim();
    let date = DATE.get_or_init(|| {
        regex::Regex::new(r#"^new\s+(?:java\.text\.)?SimpleDateFormat\(\s*(?:'([^']*)'|"([^"]*)")\s*\)\.format\(\s*new\s+(?:java\.util\.)?Date\(\s*\)\s*\)$"#).unwrap()
    });
    if let Some(m) = date.captures(c) {
        let pat = m.get(1).or(m.get(2)).map(|x| x.as_str()).unwrap_or("");
        return GroovyDefault::Value(Value::String(crate::groovy::java_date(&crate::web::now_str(), pat)));
    }
    let lit = STR.get_or_init(|| regex::Regex::new(r#"^'((?:[^'\\]|\\.)*)'$"#).unwrap());
    if let Some(m) = lit.captures(c) {
        return GroovyDefault::Value(Value::String(m[1].replace("\\'", "'")));
    }
    let mut r = c.replace("session?.curuser?.", "curuser?.").replace("session.curuser.", "curuser.");
    r = r.replace("curuser?.userID", "curuser?.userid").replace("curuser.userID", "curuser.userid");
    GroovyDefault::Code(r)
}

pub fn looks_groovy_snippet(code: &str) -> bool {
    code.contains("g6portal.")
        || code.contains("def ")
        || code.contains("session?.curuser")
        || code.contains("return null")
        || code.contains("replaceAll(")
        || code.contains("?:")
}

/// Convert single-quoted Groovy string literals to double-quoted Rhai strings (outside double-quoted strings).
fn single_to_double_quotes(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + 8);
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            out.push('"');
            i += 1;
            while i < chars.len() {
                let d = chars[i];
                out.push(d);
                i += 1;
                if d == '\\' && i < chars.len() {
                    out.push(chars[i]);
                    i += 1;
                } else if d == '"' {
                    break;
                }
            }
        } else if c == '\'' {
            out.push('"');
            i += 1;
            while i < chars.len() {
                let d = chars[i];
                if d == '\\' && i + 1 < chars.len() {
                    let nx = chars[i + 1];
                    if nx == '\'' {
                        out.push('\'');
                    } else {
                        out.push('\\');
                        out.push(nx);
                    }
                    i += 2;
                } else if d == '\'' {
                    out.push('"');
                    i += 1;
                    break;
                } else if d == '"' {
                    out.push_str("\\\"");
                    i += 1;
                } else {
                    out.push(d);
                    i += 1;
                }
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Translate short Groovy snippets from g6 tracker definitions (`enabledcondition`,
/// `updatetrails`, `portal_tracker_error.error_function`) into Rhai so shared g6 databases
/// work without mutating the Grails tracker definitions.
pub fn groovy_tracker_script(code: &str) -> String {
    use std::sync::OnceLock;
    let mut s = single_to_double_quotes(code.trim());

    // Elvis operator for session?.curuser?.name ?: "fallback"
    static ELVIS_NAME: OnceLock<regex::Regex> = OnceLock::new();
    let elvis_re = ELVIS_NAME.get_or_init(|| {
        regex::Regex::new(r#"\(\s*(?:session\??\.)?curuser\??\.name\s*\?:\s*"([^"]*)"\s*\)"#).unwrap()
    });
    s = elvis_re
        .replace_all(&s, r#"(if curuser != () && curuser.name != "" { curuser.name } else { "$1" })"#)
        .into_owned();

    // g6portal.User.findAllWhere(profile_id: X as Integer).findAll { it.id != curuser?.id }
    static FIND_PROF: OnceLock<regex::Regex> = OnceLock::new();
    let find_prof = FIND_PROF.get_or_init(|| {
        regex::Regex::new(r#"g6portal\.User\.findAllWhere\(\s*profile_id\s*:\s*([^)]+?)\s*(?:as\s+(?:Integer|Long))?\s*\)\.findAll\s*\{\s*it\.id\s*!=\s*curuser\??\.id\s*\}"#).unwrap()
    });
    s = find_prof
        .replace_all(&s, "users_by_profile_id(to_int($1)).filter(|u| curuser == () || u.id != curuser.id)")
        .into_owned();

    // !others.any { it.id == (X as Long) }
    static NOT_ANY_ID: OnceLock<regex::Regex> = OnceLock::new();
    let not_any = NOT_ANY_ID.get_or_init(|| {
        regex::Regex::new(r#"!(\w+)\.any\s*\{\s*it\.id\s*==\s*\(([^)]+?)\s*(?:as\s+(?:Integer|Long))?\)\s*\}"#).unwrap()
    });
    s = not_any
        .replace_all(&s, "!$1.some(|u| u.id == to_int($2))")
        .into_owned();

    // others*.userID.join(", ")
    static SPREAD_JOIN: OnceLock<regex::Regex> = OnceLock::new();
    let spread_join = SPREAD_JOIN.get_or_init(|| {
        regex::Regex::new(r#"(\w+)\*\.userID\.join\(([^)]+)\)"#).unwrap()
    });
    s = spread_join
        .replace_all(&s, "$1.map(|u| u.userid).join($2)")
        .into_owned();

    // g6portal.User.get(X as Long) -> user(to_int(X))
    static USER_GET: OnceLock<regex::Regex> = OnceLock::new();
    let user_get = USER_GET.get_or_init(|| {
        regex::Regex::new(r#"g6portal\.User\.get\(\s*([^)]+?)\s*(?:as\s+(?:Integer|Long))?\s*\)"#).unwrap()
    });
    s = user_get.replace_all(&s, "user(to_int($1))").into_owned();

    // x.toString().replaceAll("a","b") or x.replaceAll("a","b") -> regex_replace("a", to_string(x), "b")
    static REP_ALL: OnceLock<regex::Regex> = OnceLock::new();
    let rep_all = REP_ALL.get_or_init(|| {
        regex::Regex::new(r#"([A-Za-z_][A-Za-z0-9_\[\]"'.]*?)(?:\.toString\(\))?\.replaceAll\(\s*("[^"]*")\s*,\s*("[^"]*")\s*\)"#).unwrap()
    });
    s = rep_all.replace_all(&s, "regex_replace($2, to_string($1), $3)").into_owned();

    // g6portal.PortalTracker.load_datas("m","s",["k":v]) -> first_row("m","s",#{"k":v})
    static LOAD_DATAS: OnceLock<regex::Regex> = OnceLock::new();
    let load_datas = LOAD_DATAS.get_or_init(|| {
        regex::Regex::new(r#"g6portal\.PortalTracker\.load_datas\(\s*("[^"]*")\s*,\s*("[^"]*")\s*,\s*\["([^"]*)"\s*:\s*([^\n]+)\]\s*\)"#).unwrap()
    });
    s = load_datas.replace_all(&s, r#"first_row($1, $2, #{$3: $4})"#).into_owned();

    // Common Groovy -> Rhai token adjustments
    s = s.replace("session?.curuser?.", "curuser?.");
    s = s.replace("session.curuser.", "curuser.");
    s = s.replace("curuser?.id", "(if curuser != () { curuser.id } else { -1 })");
    s = s.replace(" as Long", "");
    s = s.replace(" as Integer", "");
    s = s.replace(".size()", ".len()");
    s = s.replace("return null", "return ()");

    // Line-by-line fixes: `def x = ...`, `arr << x`, and braceless single-line `if(...) stmt`
    static DEF_RE: OnceLock<regex::Regex> = OnceLock::new();
    static PUSH_RE: OnceLock<regex::Regex> = OnceLock::new();
    let def_re = DEF_RE.get_or_init(|| regex::Regex::new(r#"^(\s*)def\s+"#).unwrap());
    let push_re = PUSH_RE.get_or_init(|| regex::Regex::new(r#"^(\s*)(\w+)\s*<<\s*(.+)$"#).unwrap());

    fn split_if_line(line: &str) -> Option<(&str, &str, &str)> {
        let trimmed = line.trim_start();
        let ind = &line[..line.len() - trimmed.len()];
        let after_if = trimmed.strip_prefix("if")?;
        if after_if.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
            return None;
        }
        let rest = after_if.trim_start();
        if !rest.starts_with('(') {
            return None;
        }
        let mut depth = 0i32;
        let mut in_str = false;
        let mut esc = false;
        for (idx, ch) in rest.char_indices() {
            if in_str {
                if esc { esc = false; } else if ch == '\\' { esc = true; } else if ch == '"' { in_str = false; }
                continue;
            }
            match ch {
                '"' => in_str = true,
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some((ind, &rest[1..idx], rest[idx + 1..].trim()));
                    }
                }
                _ => {}
            }
        }
        None
    }

    let wrap_cond = |cond: &str| -> String {
        let mut c = cond.trim().to_string();
        // Replace bare `!ident` or `!ident["k"]` (not `!fn(...)` and not `!arr.some(...)`) with `!truthy(...)`
        static NEG_TERM: OnceLock<regex::Regex> = OnceLock::new();
        let neg_re = NEG_TERM.get_or_init(|| {
            regex::Regex::new(r#"!([A-Za-z_][A-Za-z0-9_]*(?:\["[^"]*"\])?)([.(]?)"#).unwrap()
        });
        c = neg_re
            .replace_all(&c, |caps: &regex::Captures| {
                let term = &caps[1];
                let tail = &caps[2];
                if !tail.is_empty() {
                    format!("!{}{}", term, tail)
                } else {
                    format!("!truthy({})", term)
                }
            })
            .into_owned();
        if !c.contains("==") && !c.contains("!=") && !c.contains('<') && !c.contains('>') && !c.contains("&&") && !c.contains("||") && !c.starts_with('!') {
            c = format!("truthy({})", c);
        } else if c.contains("&&") {
            static LEAD_TERM: OnceLock<regex::Regex> = OnceLock::new();
            let lead = LEAD_TERM.get_or_init(|| regex::Regex::new(r#"^([A-Za-z_][A-Za-z0-9_]*(?:\["[^"]*"\])?)\s*&&"#).unwrap());
            c = lead.replace(&c, "truthy($1) &&").into_owned();
        }
        c
    };

    let mut lines = vec![];
    for raw in s.lines() {
        let mut line = def_re.replace(raw, "${1}let ").into_owned();
        if let Some((ind, cond_raw, tail)) = split_if_line(&line) {
            let cond = wrap_cond(cond_raw);
            if tail == "{" {
                line = format!("{}if {} {{", ind, cond);
            } else if !tail.is_empty() {
                let mut body = tail.to_string();
                if let Some(pc) = push_re.captures(&body) {
                    body = format!("{}.push({});", &pc[2], &pc[3]);
                }
                line = format!("{}if {} {{ {} }}", ind, cond, body);
            }
        } else if let Some(pc) = push_re.captures(&line) {
            line = format!("{}{}.push({});", &pc[1], &pc[2], &pc[3]);
        }
        let trimmed = line.trim_end();
        if !trimmed.is_empty() && !trimmed.ends_with('{') && !trimmed.ends_with('}') && !trimmed.ends_with(';') && !trimmed.trim_start().starts_with("//") {
            line = format!("{};", trimmed);
        }
        lines.push(line);
    }
    lines.join("\n")
}

#[cfg(test)]
mod default_tests {
    use super::*;
    #[test]
    fn groovy_defaults() {
        match groovy_default("new java.text.SimpleDateFormat('yyyy-MM-dd HH:mm:ss').format(new java.util.Date())") {
            GroovyDefault::Value(Value::String(v)) => assert!(regex::Regex::new(r"^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d$").unwrap().is_match(&v), "{}", v),
            _ => panic!("date"),
        }
        assert!(matches!(groovy_default("'#1a237e'"), GroovyDefault::Value(Value::String(v)) if v == "#1a237e"));
        assert!(matches!(groovy_default("session?.curuser?.userID"), GroovyDefault::Code(c) if c == "curuser?.userid"));
        assert!(matches!(groovy_default("params.event_id_default"), GroovyDefault::Code(c) if c == "params.event_id_default"));
    }

    #[test]
    fn groovy_tracker_snippets_compile_in_rhai() {
        let eng = rhai::Engine::new();
        let samples = [
            "return 'Profil dikemaskini oleh ' + (session?.curuser?.name ?: 'pengguna')",
            "if(!datas || !datas['ic_no']) return false\ndef m = g6portal.PortalTracker.load_datas('ahli','ahli',['no_kp':datas['ic_no'].toString().replaceAll('[^0-9]','')])\nif(!m) return false\nreturn true",
            "if(!val) return null\ndef clean = val.replaceAll('[^0-9]','')\nif(clean.size()!=12) return null\ndef m = g6portal.PortalTracker.load_datas('ahli','ahli',['no_kp':clean])\nif(!m) return 'Tiada rekod'\nreturn null",
            "if(!val) return null\ndef clean = val.replaceAll('[^0-9]','')\nif(clean.size()!=12) return null\ndef m = g6portal.PortalTracker.load_datas('ahli','ahli',['no_kp':clean])\nif(!m) return null\ndef others = g6portal.User.findAllWhere(profile_id: m['id'] as Integer).findAll { it.id != curuser?.id }\nif(m['user_id'] && (m['user_id'] as Long) != curuser?.id && !others.any { it.id == (m['user_id'] as Long) }) {\n    def pu = g6portal.User.get(m['user_id'] as Long)\n    if(pu) others << pu\n}\nif(others) return 'Ada (' + others*.userID.join(', ') + ')'\nreturn null",
        ];
        for sample in samples {
            let rhai_code = groovy_tracker_script(sample);
            eng.compile(&rhai_code).unwrap_or_else(|e| panic!("failed to compile:\n{}\nerror: {}", rhai_code, e));
        }
    }
}

/// A Groovy expression from a role rule, as Rhai: g6portal helpers become this
/// portal's functions, ?: becomes ??, and session['profile'] the member record.
pub fn groovy_rule_expr(code: &str) -> String {
    let mut c = code.trim().to_string();
    for (a, b) in [
        ("g6portal.PortalSetting.namedefault(", "setting("),
        ("g6portal.PortalTracker.array2sql(", "array2sql("),
        ("session['profile']", "session_profile"),
        ("session[\"profile\"]", "session_profile"),
        ("?:", "??"),
    ] {
        c = c.replace(a, b);
    }
    static IDX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = IDX.get_or_init(|| regex::Regex::new(r#"\?\[\s*'([A-Za-z_][A-Za-z0-9_]*)'\s*\]"#).unwrap());
    c = re.replace_all(&c, "?.$1").into_owned();
    c.replace('\'', "\"")
}

/// The signed-in user's member record (config user_profile, e.g. "ahli.ahli": the row
/// of that tracker whose id is the user's profile_id), as a g6 portal keeps in its session.
pub async fn user_profile(sc: &Sc) -> Value {
    let Some(u) = &sc.inner.user else { return Value::Null };
    let Some(pid) = i(u, "profile_id").filter(|x| *x != 0) else { return Value::Null };
    let spec = sc.inner.st.cfg.user_profile.clone().unwrap_or_default();
    let Some((m, sl)) = spec.split_once('.') else { return Value::Null };
    match Tracker::load(sc.db(), m, sl).await {
        Some(t) => t.get(sc.db(), pid).await.map(Value::Object).unwrap_or(Value::Null),
        None => Value::Null,
    }
}

#[cfg(test)]
mod rule_tests {
    use super::*;
    #[test]
    fn rule_exprs() {
        assert_eq!(groovy_rule_expr("curuser?.profile_id ?: 0"), "curuser?.profile_id ?? 0");
        assert_eq!(groovy_rule_expr("g6portal.PortalTracker.array2sql(g6portal.PortalSetting.namedefault('marhalah')[session['profile']?['prkt']])"),
                   "array2sql(setting(\"marhalah\")[session_profile?.prkt])");
    }
}
