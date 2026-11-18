//! Database layer: dynamic rows as JSON maps and named-parameter SQL.
//!
//! Tracker data tables are created at runtime with user-defined columns, so
//! nothing here is statically typed. Named parameters (`:name`) are rendered
//! as escaped SQL literals; Postgres then infers each literal's type from the
//! column it is compared with, exactly like the JDBC `stringtype=unspecified`
//! setting the Grails portal relied on.

use anyhow::{anyhow, Result};
use serde_json::{Map, Value};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{Column, Row as _, TypeInfo};

pub type Row = Map<String, Value>;

#[derive(Clone)]
pub struct Db {
    pub pool: PgPool,
}

/// Only plain identifiers may be spliced into SQL as table/column names.
pub fn ident_ok(s: &str) -> bool {
    let mut ch = s.chars();
    match ch.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    ch.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn ident(s: &str) -> Result<String> {
    let s = s.trim();
    if ident_ok(s) {
        Ok(format!("\"{}\"", s))
    } else {
        Err(anyhow!("Invalid database identifier: {}", s))
    }
}

/// Quote a column for a query. Unusual names are passed through untouched
/// (callers that accept user input must use `ident`).
pub fn qcol(s: &str) -> String {
    let s = s.trim();
    if ident_ok(s) {
        format!("\"{}\"", s)
    } else {
        s.to_string()
    }
}

/// Render a JSON value as a SQL literal.
pub fn lit(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "TRUE".into() } else { "FALSE".into() },
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("'{}'", s.replace('\0', "").replace('\'', "''")),
        Value::Array(a) => {
            if a.is_empty() {
                "(NULL)".into()
            } else {
                format!("({})", a.iter().map(lit).collect::<Vec<_>>().join(","))
            }
        }
        Value::Object(_) => format!("'{}'", v.to_string().replace('\'', "''")),
    }
}

/// Replace `:name` placeholders with literals. Quoted strings, quoted
/// identifiers and `::casts` are left alone. Unknown names are left as-is.
pub fn bind(sql: &str, p: &Row) -> String {
    let b: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len() + 64);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == '\'' || c == '"' {
            out.push(c);
            i += 1;
            while i < b.len() {
                out.push(b[i]);
                if b[i] == c {
                    if i + 1 < b.len() && b[i + 1] == c {
                        out.push(c);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c == ':' {
            if i + 1 < b.len() && b[i + 1] == ':' {
                out.push_str("::");
                i += 2;
                continue;
            }
            if i + 1 < b.len() && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == '_') {
                let mut j = i + 1;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == '_') {
                    j += 1;
                }
                let name: String = b[i + 1..j].iter().collect();
                if let Some(v) = p.get(&name) {
                    out.push_str(&lit(v));
                    i = j;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

fn decode(row: &PgRow) -> Row {
    use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, NaiveTime, Utc};
    let mut m = Row::new();
    for (i, col) in row.columns().iter().enumerate() {
        let t = col.type_info().name().to_uppercase();
        let v: Value = match t.as_str() {
            "BOOL" => row.try_get::<Option<bool>, _>(i).ok().flatten().map(Value::Bool).unwrap_or(Value::Null),
            "INT2" => row.try_get::<Option<i16>, _>(i).ok().flatten().map(|x| Value::from(x as i64)).unwrap_or(Value::Null),
            "INT4" => row.try_get::<Option<i32>, _>(i).ok().flatten().map(|x| Value::from(x as i64)).unwrap_or(Value::Null),
            "INT8" => row.try_get::<Option<i64>, _>(i).ok().flatten().map(Value::from).unwrap_or(Value::Null),
            "FLOAT4" => row.try_get::<Option<f32>, _>(i).ok().flatten().map(|x| Value::from(x as f64)).unwrap_or(Value::Null),
            "FLOAT8" => row.try_get::<Option<f64>, _>(i).ok().flatten().map(Value::from).unwrap_or(Value::Null),
            "NUMERIC" => match row.try_get::<Option<rust_decimal::Decimal>, _>(i).ok().flatten() {
                Some(d) => {
                    use rust_decimal::prelude::ToPrimitive;
                    if d.fract().is_zero() {
                        d.to_i64().map(Value::from).unwrap_or_else(|| Value::String(d.to_string()))
                    } else {
                        d.to_f64().map(Value::from).unwrap_or(Value::Null)
                    }
                }
                None => Value::Null,
            },
            "DATE" => row.try_get::<Option<NaiveDate>, _>(i).ok().flatten()
                .map(|d| Value::String(d.format("%Y-%m-%d").to_string())).unwrap_or(Value::Null),
            // fractions of a second are kept when there are any (as java.sql.Timestamp shows them)
            "TIMESTAMP" => row.try_get::<Option<NaiveDateTime>, _>(i).ok().flatten()
                .map(|d| Value::String(ts_text(d))).unwrap_or(Value::Null),
            "TIMESTAMPTZ" => row.try_get::<Option<DateTime<Utc>>, _>(i).ok().flatten()
                .map(|d| Value::String(ts_text(d.with_timezone(&Local).naive_local()))).unwrap_or(Value::Null),
            "TIME" => row.try_get::<Option<NaiveTime>, _>(i).ok().flatten()
                .map(|d| Value::String(d.format("%H:%M:%S").to_string())).unwrap_or(Value::Null),
            "JSON" | "JSONB" => row.try_get::<Option<Value>, _>(i).ok().flatten().unwrap_or(Value::Null),
            "TEXT[]" | "VARCHAR[]" => row.try_get::<Option<Vec<String>>, _>(i).ok().flatten()
                .map(|v| Value::Array(v.into_iter().map(Value::String).collect())).unwrap_or(Value::Null),
            "BYTEA" => Value::Null,
            _ => row.try_get::<Option<String>, _>(i).ok().flatten().map(Value::String)
                .or_else(|| row.try_get_unchecked::<Option<String>, _>(i).ok().flatten().map(Value::String))
                .unwrap_or(Value::Null),
        };
        m.insert(col.name().to_string(), v);
    }
    m
}

pub fn no_params() -> Row {
    Row::new()
}

impl Db {
    pub async fn connect(url: &str, max: u32) -> Result<Db> {
        let pool = sqlx::postgres::PgPoolOptions::new().max_connections(max).connect(url).await?;
        Ok(Db { pool })
    }
}

/// Every statement passes here before it runs: optional logging, and writes
/// clear the in-memory caches built from the tables they change (cache.rs).
fn note_write(sql: &str) {
    // RUSTPORTAL_LOG_SQL=1 prints every statement to stderr (for finding slow pages)
    static LOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *LOG.get_or_init(|| std::env::var("RUSTPORTAL_LOG_SQL").map(|v| v == "1").unwrap_or(false)) {
        eprintln!("SQL {}", sql.split_whitespace().collect::<Vec<_>>().join(" "));
    }
    crate::cache::note_sql(sql);
}

/// A large query read a chunk at a time through a server-side cursor, so the
/// whole result never sits in memory (used by the Excel export).
pub struct Cursor {
    tx: sqlx::Transaction<'static, sqlx::Postgres>,
    chunk: usize,
}

impl Cursor {
    /// The next chunk of rows; empty when the query is exhausted.
    pub async fn next(&mut self) -> Result<Vec<Row>> {
        let q = format!("fetch {} from rp_cursor", self.chunk);
        let rows = sqlx::query(&q).persistent(false).fetch_all(&mut *self.tx).await?;
        Ok(rows.iter().map(decode).collect())
    }
}

impl Db {
    pub async fn cursor(&self, sql: &str, chunk: usize) -> Result<Cursor> {
        note_write(sql);
        let mut tx = self.pool.begin().await?;
        let declare = format!("declare rp_cursor no scroll cursor for {}", sql);
        if let Err(e) = sqlx::query(&declare).persistent(false).execute(&mut *tx).await {
            return Err(anyhow!("{} -- query: {}", e, sql));
        }
        Ok(Cursor { tx, chunk })
    }

    pub async fn rows(&self, sql: &str, p: &Row) -> Result<Vec<Row>> {
        let q = bind(sql, p);
        note_write(&q);
        let rows = sqlx::query(&q).persistent(false).fetch_all(&self.pool).await
            .map_err(|e| anyhow!("{} -- query: {}", e, q))?;
        Ok(rows.iter().map(decode).collect())
    }

    pub async fn first(&self, sql: &str, p: &Row) -> Result<Option<Row>> {
        let q = bind(sql, p);
        note_write(&q);
        let row = sqlx::query(&q).persistent(false).fetch_optional(&self.pool).await
            .map_err(|e| anyhow!("{} -- query: {}", e, q))?;
        Ok(row.as_ref().map(decode))
    }

    pub async fn exec(&self, sql: &str, p: &Row) -> Result<u64> {
        let q = bind(sql, p);
        note_write(&q);
        let r = sqlx::query(&q).persistent(false).execute(&self.pool).await
            .map_err(|e| anyhow!("{} -- query: {}", e, q))?;
        Ok(r.rows_affected())
    }

    /// Run several statements that carry no parameters (DDL).
    pub async fn ddl(&self, sql: &str) -> Result<()> {
        note_write(sql);
        sqlx::raw_sql(sql).execute(&self.pool).await.map_err(|e| anyhow!("{} -- ddl: {}", e, sql))?;
        Ok(())
    }

    pub async fn count(&self, sql: &str, p: &Row) -> Result<i64> {
        Ok(self.first(sql, p).await?.and_then(|r| r.values().next().and_then(|v| v.as_i64())).unwrap_or(0))
    }

    pub async fn get(&self, table: &str, id: i64) -> Result<Option<Row>> {
        self.first(&format!("select * from {} where id={}", ident(table)?, id), &no_params()).await
    }

    pub async fn insert(&self, table: &str, data: &Row) -> Result<i64> {
        let mut cols = vec![];
        let mut vals = vec![];
        for (k, v) in data {
            if k == "id" {
                continue;
            }
            cols.push(ident(k)?);
            vals.push(lit(v));
        }
        let sql = if cols.is_empty() {
            format!("insert into {} default values returning id", ident(table)?)
        } else {
            format!("insert into {} ({}) values ({}) returning id", ident(table)?, cols.join(","), vals.join(","))
        };
        let r = self.first(&sql, &no_params()).await?;
        r.and_then(|r| r.get("id").and_then(|v| v.as_i64())).ok_or_else(|| anyhow!("insert returned no id"))
    }

    pub async fn update(&self, table: &str, id: i64, data: &Row) -> Result<u64> {
        let mut sets = vec![];
        for (k, v) in data {
            if k == "id" {
                continue;
            }
            sets.push(format!("{}={}", ident(k)?, lit(v)));
        }
        if sets.is_empty() {
            return Ok(0);
        }
        self.exec(&format!("update {} set {} where id={}", ident(table)?, sets.join(","), id), &no_params()).await
    }

    pub async fn delete(&self, table: &str, id: i64) -> Result<u64> {
        self.exec(&format!("delete from {} where id={}", ident(table)?, id), &no_params()).await
    }

    pub async fn table_exists(&self, table: &str) -> bool {
        let mut p = Row::new();
        p.insert("t".into(), Value::String(table.to_string()));
        self.count("select count(*) from information_schema.tables where table_schema='public' and table_name=:t", &p).await.unwrap_or(0) > 0
    }

    pub async fn column_exists(&self, table: &str, col: &str) -> bool {
        let mut p = Row::new();
        p.insert("t".into(), Value::String(table.to_string()));
        p.insert("c".into(), Value::String(col.to_string()));
        self.count("select count(*) from information_schema.columns where table_schema='public' and table_name=:t and column_name=:c", &p).await.unwrap_or(0) > 0
    }
}

// ---- small JSON helpers used across the code base ----

pub fn s(r: &Row, k: &str) -> String {
    match r.get(k) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Null) | None => String::new(),
        Some(Value::Array(a)) => a.iter().map(vs).collect::<Vec<_>>().join(","),
        Some(v) => v.to_string(),
    }
}

pub fn vs(v: &Value) -> String {
    match v {
        Value::String(x) => x.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

pub fn i(r: &Row, k: &str) -> Option<i64> {
    match r.get(k) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(x)) => x.trim().parse().ok(),
        Some(Value::Array(a)) => a.first().and_then(|v| match v {
            Value::String(x) => x.trim().parse().ok(),
            Value::Number(n) => n.as_i64(),
            _ => None,
        }),
        _ => None,
    }
}

pub fn b(r: &Row, k: &str) -> bool {
    match r.get(k) {
        Some(Value::Bool(x)) => *x,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        Some(Value::String(x)) => matches!(x.as_str(), "true" | "on" | "1" | "yes"),
        _ => false,
    }
}

pub fn has(r: &Row, k: &str) -> bool {
    !s(r, k).trim().is_empty()
}

/// Split a comma separated list, trimming and dropping blanks.
pub fn csv(x: &str) -> Vec<String> {
    x.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect()
}

#[macro_export]
macro_rules! row {
    ($($k:expr => $v:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut m = $crate::db::Row::new();
        $( m.insert($k.to_string(), serde_json::json!($v)); )*
        m
    }};
}


static PAGE_TABLES: std::sync::OnceLock<(&'static str, &'static str)> = std::sync::OnceLock::new();

/// Set once at start-up from config.toml (`page_table`, `page_data_table`).
pub fn set_page_tables(page: &str, data: &str) {
    let ok = |t: &str| !t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let page = if ok(page) { page } else { "portal_page" };
    let data = if ok(data) { data } else { "portal_page_data" };
    let _ = PAGE_TABLES.set((Box::leak(page.to_string().into_boxed_str()), Box::leak(data.to_string().into_boxed_str())));
}

/// The table pages are kept in (`portal_page` unless configured otherwise).
pub fn page_t() -> &'static str {
    PAGE_TABLES.get().map(|x| x.0).unwrap_or("portal_page")
}

/// The table page data sources are kept in (`portal_page_data` unless configured otherwise).
pub fn page_data_t() -> &'static str {
    PAGE_TABLES.get().map(|x| x.1).unwrap_or("portal_page_data")
}

static G6: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set once at start-up from config.toml (`g6_schema`).
pub fn set_g6(on: bool) {
    G6.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Running on a g6 Grails portal's database (see `entity::g6_views`).
pub fn g6() -> bool {
    G6.load(std::sync::atomic::Ordering::Relaxed)
}

/// A timestamp as text: seconds, plus the fraction of a second when there is one
/// (".5", ".544"), trailing zeros dropped as java.sql.Timestamp does.
pub fn ts_text(d: chrono::NaiveDateTime) -> String {
    let base = d.format("%Y-%m-%d %H:%M:%S").to_string();
    let frac = d.format("%.f").to_string();
    let frac = frac.trim_end_matches('0');
    if frac.len() > 1 { format!("{}{}", base, frac) } else { base }
}
