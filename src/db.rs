//! Database layer: dynamic rows as JSON maps and named-parameter SQL.
//!
//! Supports PostgreSQL (`postgres://...`) and embedded SQLite (`sqlite://...`
//! or `sqlite::memory:`). Tracker data tables are created at runtime with
//! user-defined columns, so nothing here is statically typed. Named parameters
//! (`:name`) are rendered as escaped SQL literals; on SQLite a compatibility
//! shim translates common PostgreSQL expressions (`ILIKE`, `::casts`, `now()`,
//! `interval '...'`, `EXTRACT(...)`, etc.).

use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteRow, SqliteSynchronous};
use sqlx::{Column, Row as _, TypeInfo, ValueRef};
use std::str::FromStr;
use std::time::Duration;

pub type Row = Map<String, Value>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dialect {
    Postgres,
    Sqlite,
}

#[derive(Clone)]
enum Pool {
    Pg(PgPool),
    Sqlite(SqlitePool),
}

#[derive(Clone)]
pub struct Db {
    pool: Pool,
    dialect: Dialect,
    sqlite_localtime: bool,
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

fn decode_pg(row: &PgRow) -> Row {
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

fn decode_sqlite(row: &SqliteRow) -> Row {
    let mut m = Row::new();
    for (i, col) in row.columns().iter().enumerate() {
        let v: Value = match row.try_get_raw(i) {
            Ok(raw) if raw.is_null() => Value::Null,
            Ok(raw) => {
                let decl = col.type_info().name().to_uppercase();
                let actual = raw.type_info().name().to_uppercase();
                if matches!(decl.as_str(), "BOOLEAN" | "BOOL") && matches!(actual.as_str(), "INTEGER" | "BOOLEAN" | "INT" | "INT4" | "INT8") {
                    row.try_get::<Option<bool>, _>(i).ok().flatten().map(Value::Bool).unwrap_or(Value::Null)
                } else {
                    match actual.as_str() {
                        "BOOLEAN" | "BOOL" => row.try_get::<Option<bool>, _>(i).ok().flatten().map(Value::Bool).unwrap_or(Value::Null),
                        "INTEGER" | "INT" | "INT4" | "INT8" => row.try_get::<Option<i64>, _>(i).ok().flatten().map(Value::from).unwrap_or(Value::Null),
                        "REAL" | "FLOAT" | "DOUBLE" => row.try_get::<Option<f64>, _>(i).ok().flatten().map(Value::from).unwrap_or(Value::Null),
                        "BLOB" => Value::Null,
                        _ => row.try_get::<Option<String>, _>(i).ok().flatten().map(Value::String).unwrap_or(Value::Null),
                    }
                }
            }
            Err(_) => Value::Null,
        };
        m.insert(col.name().to_string(), v);
    }
    m
}

pub fn no_params() -> Row {
    Row::new()
}

fn is_sqlite_url(url: &str) -> bool {
    let u = url.trim();
    u.starts_with("sqlite:") || u == ":memory:" || (!u.contains("://") && (u.ends_with(".db") || u.ends_with(".sqlite") || u.ends_with(".sqlite3")))
}

fn sqlite_connect_url(url: &str) -> String {
    let u = url.trim();
    if u == ":memory:" {
        "sqlite::memory:".into()
    } else if u.starts_with("sqlite:") {
        u.to_string()
    } else {
        format!("sqlite://{}", u)
    }
}

fn is_sqlite_memory(url: &str) -> bool {
    let u = url.trim().to_ascii_lowercase();
    u == ":memory:" || u.starts_with("sqlite::memory:") || u.starts_with("sqlite://:memory:") || u.contains("mode=memory")
}

impl Db {
    #[allow(dead_code)]
    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    pub fn is_sqlite(&self) -> bool {
        self.dialect == Dialect::Sqlite
    }

    /// `timezone`: the session time zone for SQL's now() / current_date (sqlx
    /// would otherwise start every connection in UTC).
    pub async fn connect(url: &str, max: u32, timezone: Option<String>) -> Result<Db> {
        if is_sqlite_url(url) {
            let conn_url = sqlite_connect_url(url);
            let memory = is_sqlite_memory(&conn_url);
            let mut opts = SqliteConnectOptions::from_str(&conn_url)?
                .create_if_missing(true)
                .busy_timeout(Duration::from_secs(10))
                .foreign_keys(false);
            if !memory {
                if let Some(parent) = opts.get_filename().parent() {
                    if !parent.as_os_str().is_empty() {
                        std::fs::create_dir_all(parent)?;
                    }
                }
                opts = opts
                    .journal_mode(SqliteJournalMode::Wal)
                    .synchronous(SqliteSynchronous::Normal);
            }
            // In-memory SQLite creates a separate DB per connection unless limited to 1.
            let max_conns = if memory { 1 } else { max.max(1) };
            let pool = SqlitePoolOptions::new()
                .max_connections(max_conns)
                .connect_with(opts)
                .await?;
            let sqlite_localtime = !matches!(
                timezone.as_deref().map(|z| z.trim().to_ascii_uppercase()).as_deref(),
                Some("UTC" | "ETC/UTC")
            );
            return Ok(Db { pool: Pool::Sqlite(pool), dialect: Dialect::Sqlite, sqlite_localtime });
        }
        let tz = timezone.filter(|z| !z.is_empty() && z.chars().all(|c| c.is_ascii_alphanumeric() || "/_+-:".contains(c)));
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(max)
            .after_connect(move |conn, _meta| {
                let tz = tz.clone();
                Box::pin(async move {
                    if let Some(z) = tz {
                        sqlx::Executor::execute(&mut *conn, format!("SET TIME ZONE '{}'", z).as_str()).await?;
                    }
                    Ok(())
                })
            })
            .connect(url)
            .await?;
        Ok(Db { pool: Pool::Pg(pool), dialect: Dialect::Postgres, sqlite_localtime: false })
    }

    fn prepare(&self, sql: &str) -> String {
        if self.is_sqlite() {
            crate::sqlite_compat::rewrite(sql, self.sqlite_localtime)
        } else {
            sql.to_string()
        }
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

enum CursorInner {
    Pg {
        tx: sqlx::Transaction<'static, sqlx::Postgres>,
        chunk: usize,
    },
    Sqlite {
        rx: tokio::sync::mpsc::Receiver<Result<Vec<Row>>>,
    },
}

/// A large query read a chunk at a time, so the whole result never sits in
/// memory (used by the Excel export).
pub struct Cursor {
    inner: CursorInner,
}

impl Cursor {
    /// The next chunk of rows; empty when the query is exhausted.
    pub async fn next(&mut self) -> Result<Vec<Row>> {
        match &mut self.inner {
            CursorInner::Pg { tx, chunk } => {
                let q = format!("fetch {} from rp_cursor", chunk);
                let rows = sqlx::query(&q).persistent(false).fetch_all(&mut **tx).await?;
                Ok(rows.iter().map(decode_pg).collect())
            }
            CursorInner::Sqlite { rx } => match rx.recv().await {
                Some(res) => res,
                None => Ok(vec![]),
            },
        }
    }
}

impl Db {
    pub async fn cursor(&self, sql: &str, chunk: usize) -> Result<Cursor> {
        let q = self.prepare(sql);
        note_write(&q);
        match &self.pool {
            Pool::Pg(pool) => {
                let mut tx = pool.begin().await?;
                let declare = format!("declare rp_cursor no scroll cursor for {}", q);
                if let Err(e) = sqlx::query(&declare).persistent(false).execute(&mut *tx).await {
                    return Err(anyhow!("{} -- query: {}", e, q));
                }
                Ok(Cursor { inner: CursorInner::Pg { tx, chunk } })
            }
            Pool::Sqlite(pool) => {
                let pool = pool.clone();
                let chunk = chunk.max(1);
                let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<Row>>>(2);
                tokio::spawn(async move {
                    let mut stream = sqlx::query(&q).persistent(false).fetch(&pool);
                    let mut buf = Vec::with_capacity(chunk);
                    while let Some(item) = stream.next().await {
                        match item {
                            Ok(r) => {
                                buf.push(decode_sqlite(&r));
                                if buf.len() >= chunk {
                                    let out = std::mem::replace(&mut buf, Vec::with_capacity(chunk));
                                    if tx.send(Ok(out)).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(Err(anyhow!("{} -- query: {}", e, q))).await;
                                return;
                            }
                        }
                    }
                    if !buf.is_empty() {
                        let _ = tx.send(Ok(buf)).await;
                    }
                });
                Ok(Cursor { inner: CursorInner::Sqlite { rx } })
            }
        }
    }

    pub async fn rows(&self, sql: &str, p: &Row) -> Result<Vec<Row>> {
        let q = self.prepare(&bind(sql, p));
        note_write(&q);
        match &self.pool {
            Pool::Pg(pool) => {
                let rows = sqlx::query(&q).persistent(false).fetch_all(pool).await
                    .map_err(|e| anyhow!("{} -- query: {}", e, q))?;
                Ok(rows.iter().map(decode_pg).collect())
            }
            Pool::Sqlite(pool) => {
                let rows = sqlx::query(&q).persistent(false).fetch_all(pool).await
                    .map_err(|e| anyhow!("{} -- query: {}", e, q))?;
                Ok(rows.iter().map(decode_sqlite).collect())
            }
        }
    }

    pub async fn first(&self, sql: &str, p: &Row) -> Result<Option<Row>> {
        let q = self.prepare(&bind(sql, p));
        note_write(&q);
        match &self.pool {
            Pool::Pg(pool) => {
                let row = sqlx::query(&q).persistent(false).fetch_optional(pool).await
                    .map_err(|e| anyhow!("{} -- query: {}", e, q))?;
                Ok(row.as_ref().map(decode_pg))
            }
            Pool::Sqlite(pool) => {
                let row = sqlx::query(&q).persistent(false).fetch_optional(pool).await
                    .map_err(|e| anyhow!("{} -- query: {}", e, q))?;
                Ok(row.as_ref().map(decode_sqlite))
            }
        }
    }

    pub async fn exec(&self, sql: &str, p: &Row) -> Result<u64> {
        let q = self.prepare(&bind(sql, p));
        note_write(&q);
        match &self.pool {
            Pool::Pg(pool) => {
                let r = sqlx::query(&q).persistent(false).execute(pool).await
                    .map_err(|e| anyhow!("{} -- query: {}", e, q))?;
                Ok(r.rows_affected())
            }
            Pool::Sqlite(pool) => {
                let r = sqlx::query(&q).persistent(false).execute(pool).await
                    .map_err(|e| anyhow!("{} -- query: {}", e, q))?;
                Ok(r.rows_affected())
            }
        }
    }

    /// Run several statements that carry no parameters (DDL).
    pub async fn ddl(&self, sql: &str) -> Result<()> {
        let q = self.prepare(sql);
        note_write(&q);
        match &self.pool {
            Pool::Pg(pool) => {
                sqlx::raw_sql(&q).execute(pool).await.map_err(|e| anyhow!("{} -- ddl: {}", e, q))?;
            }
            Pool::Sqlite(pool) => {
                sqlx::raw_sql(&q).execute(pool).await.map_err(|e| anyhow!("{} -- ddl: {}", e, q))?;
            }
        }
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
        if self.is_sqlite() {
            self.count("select count(*) from sqlite_master where type='table' and name=:t", &p).await.unwrap_or(0) > 0
        } else {
            self.count("select count(*) from information_schema.tables where table_schema='public' and table_name=:t", &p).await.unwrap_or(0) > 0
        }
    }

    pub async fn column_exists(&self, table: &str, col: &str) -> bool {
        let mut p = Row::new();
        p.insert("t".into(), Value::String(table.to_string()));
        p.insert("c".into(), Value::String(col.to_string()));
        if self.is_sqlite() {
            self.count("select count(*) from pragma_table_info(:t) where lower(name)=lower(:c)", &p).await.unwrap_or(0) > 0
        } else {
            self.count("select count(*) from information_schema.columns where table_schema='public' and table_name=:t and column_name=:c", &p).await.unwrap_or(0) > 0
        }
    }

    /// Return `(column_name, type_name)` pairs for a table in column order.
    pub async fn table_columns(&self, table: &str) -> Result<Vec<(String, String)>> {
        let mut p = Row::new();
        p.insert("t".into(), Value::String(table.to_string()));
        if self.is_sqlite() {
            let rows = self.rows("select name as column_name, type as udt_name from pragma_table_info(:t) order by cid", &p).await?;
            Ok(rows.iter().map(|r| (s(r, "column_name"), s(r, "udt_name"))).collect())
        } else {
            let rows = self.rows("select column_name, udt_name from information_schema.columns where table_schema=current_schema() and table_name=:t order by ordinal_position", &p).await?;
            Ok(rows.iter().map(|r| (s(r, "column_name"), s(r, "udt_name"))).collect())
        }
    }

    /// Add a nullable column if it does not already exist on `table`.
    pub async fn add_column_if_missing(&self, table: &str, col: &str, sql_type: &str) -> Result<()> {
        let qt = ident(table)?;
        let qc = ident(col)?;
        if self.is_sqlite() {
            if !self.column_exists(table, col).await {
                self.ddl(&format!("alter table {} add column {} {} null", qt, qc, sql_type)).await?;
            }
        } else {
            self.ddl(&format!("alter table {} add column if not exists {} {} null", qt, qc, sql_type)).await?;
        }
        Ok(())
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

/// Running in g6 Grails portal compatibility mode.
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

/// This machine's time zone name (TZ, else what /etc/localtime points at).
pub fn local_timezone() -> Option<String> {
    if let Ok(t) = std::env::var("TZ") {
        let t = t.trim_start_matches(':').to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    let target = std::fs::read_link("/etc/localtime").ok()?;
    let t = target.to_string_lossy().to_string();
    t.split_once("zoneinfo/").map(|(_, z)| z.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity;
    use crate::tracker::Tracker;

    #[tokio::test]
    async fn sqlite_schema_crud_and_compat_queries() {
        let db = Db::connect("sqlite::memory:", 4, None).await.expect("connect sqlite memory");
        assert!(db.is_sqlite());
        assert_eq!(db.dialect(), Dialect::Sqlite);

        let ents = entity::entities();
        entity::sync_schema(&db, &ents).await.expect("initial sync_schema");
        // Idempotent second run (as on restart)
        entity::sync_schema(&db, &ents).await.expect("second sync_schema");
        assert!(db.table_exists("portal_user").await);
        assert!(db.table_exists("portal_session").await);
        assert!(db.column_exists("portal_user", "userid").await);
        assert!(!db.column_exists("portal_user", "nonexistent_col").await);

        // User insert + defaults + boolean decoding
        let uid = db
            .insert("portal_user", &crate::row! {"userid"=>"alice","name"=>"Alice_Smith"})
            .await
            .expect("insert user");
        let u = db.get("portal_user", uid).await.unwrap().expect("get user");
        assert_eq!(u.get("is_admin"), Some(&Value::Bool(false)));
        assert_eq!(u.get("is_active"), Some(&Value::Bool(true)));
        assert_eq!(u.get("reset_password"), Some(&Value::Bool(false)));
        assert_eq!(s(&u, "email"), "");

        // ILIKE + ESCAPE '\' + ::text cast
        let found = db
            .rows(
                "select id, \"name\"::text as d from portal_user where is_active is not false and name ilike :q escape '\\'",
                &crate::row! {"q"=>"%alice\\_smith%"},
            )
            .await
            .expect("ilike query");
        assert_eq!(found.len(), 1);
        assert_eq!(s(&found[0], "d"), "Alice_Smith");

        // Escape check: underscore shouldn't match arbitrary char when escaped
        let miss = db
            .rows(
                "select id from portal_user where name ilike :q escape '\\'",
                &crate::row! {"q"=>"%alice\\_x%"},
            )
            .await
            .expect("ilike miss");
        assert!(miss.is_empty());

        // Session upsert with now() + ON CONFLICT + interval cleanup
        db.exec(
            "insert into portal_session (id,data,updated) values (:id,:d,now()) on conflict (id) do update set data=excluded.data, updated=now()",
            &crate::row! {"id"=>"s1","d"=>"{\"a\":1}"},
        )
        .await
        .expect("session insert");
        db.exec(
            "insert into portal_session (id,data,updated) values (:id,:d,now()) on conflict (id) do update set data=excluded.data, updated=now()",
            &crate::row! {"id"=>"s1","d"=>"{\"a\":2}"},
        )
        .await
        .expect("session upsert");
        let sess = db.first("select data from portal_session where id='s1'", &no_params()).await.unwrap().unwrap();
        assert_eq!(s(&sess, "data"), "{\"a\":2}");
        db.exec("delete from portal_session where updated < now() - interval '12 hours'", &no_params())
            .await
            .expect("session prune");
        assert_eq!(db.count("select count(*) from portal_session where id='s1'", &no_params()).await.unwrap(), 1);

        // Email due query (now() + interval '2 days' + is not true)
        let eid = db
            .insert("portal_email", &crate::row! {"title"=>"Hi","module"=>"portal","emailto"=>"a@b.c","body"=>"x","delivery_time"=>crate::web::now_str(),"email_sent"=>false})
            .await
            .expect("insert email");
        let due = db
            .rows(
                "select id from portal_email where email_sent is not true and (delivery_time is null or delivery_time <= now()) and (delivery_time is null or delivery_time > now() - interval '2 days') order by id limit 50",
                &no_params(),
            )
            .await
            .expect("email due");
        assert_eq!(due.len(), 1);
        assert_eq!(i(&due[0], "id"), Some(eid));

        // EXTRACT, DATE_TRUNC, TO_CHAR, STRING_AGG, NULLS LAST
        let agg = db
            .first(
                "select extract(YEAR from '2026-05-17 14:30:00') as y, extract(MONTH from '2026-05-17 14:30:00') as m, date_trunc('month', '2026-05-17 14:30:00') as dt, to_char('2026-05-17', 'DD/MM/YYYY') as tc",
                &no_params(),
            )
            .await
            .expect("date funcs")
            .unwrap();
        assert_eq!(i(&agg, "y"), Some(2026));
        assert_eq!(i(&agg, "m"), Some(5));
        assert_eq!(s(&agg, "dt"), "2026-05-01 00:00:00");
        assert_eq!(s(&agg, "tc"), "17/05/2026");

        // Dynamic tracker lifecycle + streaming cursor + cleardb
        let tid = db
            .insert("portal_tracker", &crate::row! {"module"=>"demo","slug"=>"items","name"=>"Items","tracker_type"=>"Tracker","searchfields"=>"title"})
            .await
            .expect("insert tracker");
        for (n, ft) in [("title", "Text"), ("qty", "Integer"), ("price", "Number"), ("active", "Checkbox"), ("created_on", "Date")] {
            db.insert("portal_tracker_field", &crate::row! {"tracker_id"=>tid,"name"=>n,"label"=>n,"field_type"=>ft})
                .await
                .expect("insert field");
        }
        let t = Tracker::by_id(&db, tid).await.expect("load tracker");
        t.updatedb(&db).await.expect("tracker updatedb");
        t.create_index(&db).await.expect("tracker create_index");

        for k in 1..=5 {
            t.save(&db, &crate::row! {"title"=>format!("Item {}", k),"qty"=>k * 10,"price"=>k as f64 + 0.5,"active"=>(k % 2 == 0),"created_on"=>"2026-05-17","record_status"=>"Open"})
                .await
                .expect("save tracker row");
        }
        assert_eq!(t.count(&db, None).await.unwrap(), 5);
        let filtered = t.rows(&db, Some(&crate::row! {"title"=>"%item 3%"}), None, None, None).await.unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(i(&filtered[0], "qty"), Some(30));
        assert_eq!(filtered[0].get("active"), Some(&Value::Bool(false)));

        // Streaming cursor in chunks of 2 over 5 rows => 2, 2, 1, 0
        let mut cur = db.cursor(&format!("select * from {} order by id", qcol(&t.data_table())), 2).await.expect("cursor");
        let mut sizes = vec![];
        loop {
            let chunk = cur.next().await.expect("cursor next");
            if chunk.is_empty() {
                break;
            }
            sizes.push(chunk.len());
        }
        assert_eq!(sizes, vec![2, 2, 1]);

        // from_table detects an added column
        db.ddl(&format!("alter table {} add column extra_note text null", ident(&t.data_table()).unwrap())).await.unwrap();
        let added = t.from_table(&db).await.expect("from_table");
        assert_eq!(added, 1);

        // cleardb empties data table
        t.cleardb(&db).await.expect("cleardb");
        assert_eq!(t.count(&db, None).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn sqlite_file_db_creates_parent_dirs_and_persists() {
        let dir = std::env::temp_dir().join(format!("rustportal_sqlite_test_{}", uuid::Uuid::new_v4()));
        let path = dir.join("nested").join("portal.db");
        let url = format!("sqlite://{}", path.to_string_lossy());
        {
            let db = Db::connect(&url, 4, None).await.expect("connect file sqlite");
            entity::sync_schema(&db, &entity::entities()).await.expect("sync file sqlite");
            db.insert("portal_setting", &crate::row! {"module"=>"portal","name"=>"k","type"=>"Text","text"=>"hello"}).await.unwrap();
        }
        assert!(path.exists());
        {
            let db = Db::connect(&url, 4, None).await.expect("reopen file sqlite");
            entity::sync_schema(&db, &entity::entities()).await.expect("resync file sqlite");
            let r = db.first("select text from portal_setting where name='k'", &no_params()).await.unwrap().unwrap();
            assert_eq!(s(&r, "text"), "hello");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sqlite_imports_livequiz_module() {
        let mod_dir = std::path::Path::new("modulemigration/livequiz");
        if !mod_dir.exists() {
            return;
        }
        let db = Db::connect("sqlite::memory:", 4, None).await.expect("connect sqlite");
        let ents = entity::entities();
        entity::sync_schema(&db, &ents).await.expect("sync_schema");
        let st: crate::web::St = std::sync::Arc::new(crate::web::AppState {
            db: db.clone(),
            cfg: crate::web::Config::default(),
            entities: ents,
            excel_jobs: std::sync::Mutex::new(std::collections::HashMap::new()),
            rt: tokio::runtime::Handle::current(),
        });
        let (summary, _detail) = crate::module::import_from(&st, "livequiz", mod_dir, true).await.expect("import livequiz on sqlite");
        assert!(!summary.is_empty());
        let trackers = db.count("select count(*) from portal_tracker where module='livequiz'", &no_params()).await.unwrap();
        let pages = db.count(&format!("select count(*) from {} where module='livequiz'", page_t()), &no_params()).await.unwrap();
        assert!(trackers > 0, "expected livequiz trackers imported");
        assert!(pages > 0, "expected livequiz pages imported");
        // Verify tracker tables exist and are queryable
        let ev = Tracker::load(&db, "livequiz", "event").await.expect("load livequiz event tracker");
        assert!(db.table_exists(&ev.data_table()).await);
        // Verify livequiz SQL query (with cast(... as decimal(10,2))) executes cleanly
        let avg = db
            .first("select avg(cast(rating_value as decimal(10,2))) as avg from trak_livequiz_answer_data where question_id=:q", &crate::row! {"q"=>1})
            .await
            .expect("livequiz avg query");
        assert!(avg.is_some());
    }
}
