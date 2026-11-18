//! In-memory caches for definitions that pages read on every request: tracker
//! definitions, pages, settings, menus (trees) and each user's account and roles.
//!
//! Every statement this process runs goes through `db.rs`, which calls
//! `note_sql`: a write to a table that a cache is built from clears that cache.
//! The time limit covers changes made by another server or directly in the database.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(60);

pub struct Cache<T: Clone> {
    /// tables the cached values are read from
    tables: &'static [&'static str],
    inner: Mutex<(u64, BTreeMap<String, (Instant, T)>)>,
}

impl<T: Clone> Cache<T> {
    pub const fn new(tables: &'static [&'static str]) -> Self {
        Cache { tables, inner: Mutex::new((0, BTreeMap::new())) }
    }
    fn map(&self) -> std::sync::MutexGuard<'_, (u64, BTreeMap<String, (Instant, T)>)> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn get(&self, key: &str) -> Option<T> {
        self.map().1.get(key).filter(|(at, _)| at.elapsed() < TTL).map(|(_, v)| v.clone())
    }
    /// Take this before reading the database, pass it to `put`: a value read while
    /// the cache was being cleared may already be out of date and is not stored.
    pub fn generation(&self) -> u64 {
        self.map().0
    }
    pub fn put(&self, gen: u64, key: String, v: T) {
        let mut m = self.map();
        if m.0 == gen {
            if m.1.len() > 10_000 {
                m.1.clear();
            }
            m.1.insert(key, (Instant::now(), v));
        }
    }
    pub fn clear(&self) {
        let mut m = self.map();
        m.0 += 1;
        m.1.clear();
    }
    /// Read through the cache.
    pub async fn get_or<F: std::future::Future<Output = T>>(&self, key: String, load: F) -> T {
        if let Some(v) = self.get(&key) {
            return v;
        }
        let gen = self.generation();
        let v = load.await;
        self.put(gen, key, v.clone());
        v
    }
}

trait Clear: Sync {
    fn tables(&self) -> &'static [&'static str];
    fn clear_all(&self);
}
impl<T: Clone + Send> Clear for Cache<T> {
    fn tables(&self) -> &'static [&'static str] {
        self.tables
    }
    fn clear_all(&self) {
        self.clear()
    }
}

pub static SETTINGS: Cache<Option<crate::db::Row>> = Cache::new(&["portal_setting"]);
pub static MENUS: Cache<Option<(crate::db::Row, Vec<crate::db::Row>)>> = Cache::new(&["tree", "tree_node"]);
pub static PAGES: Cache<Option<crate::db::Row>> = Cache::new(&["portal_page"]);
pub static PAGE_DATA: Cache<Vec<crate::db::Row>> = Cache::new(&["portal_page_data", "portal_page"]);
/// per user: the account row, module role lists and organisational role
pub static USERS: Cache<serde_json::Value> = Cache::new(&["iap_user", "user_role", "tree_node_user", "tree_node", "portal_setting"]);

fn all() -> [&'static dyn Clear; 5] {
    [&SETTINGS, &MENUS, &PAGES, &PAGE_DATA, &USERS]
}

fn clear_everything() {
    for c in all() {
        c.clear_all();
    }
    crate::tracker::definitions_changed();
}

/// Called for every statement before it runs.
pub fn note_sql(sql: &str) {
    let head = sql.trim_start();
    let word: String = head.chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_lowercase();
    let several = several_statements(head);
    let target = match word.as_str() {
        "select" | "show" | "explain" | "" if !several => return,
        "insert" | "update" | "delete" | "truncate" | "alter" if !several => written_table(head),
        // CTEs, several statements, DDL, DO blocks...: cannot tell what changes
        _ => None,
    };
    match target {
        Some(t) => {
            // the page tables may be configured under other names
            let t = if t == crate::db::page_t() { "portal_page".to_string() } else if t == crate::db::page_data_t() { "portal_page_data".to_string() } else { t };
            if t.starts_with("portal_tracker") {
                crate::tracker::definitions_changed();
            }
            for c in all() {
                if c.tables().contains(&t.as_str()) {
                    c.clear_all();
                }
            }
        }
        None => clear_everything(),
    }
}

/// A `;` followed by more SQL, outside quotes ('...', "...", $tag$...$tag$).
fn several_statements(sql: &str) -> bool {
    if !sql.contains(';') {
        return false;
    }
    let b = sql.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
            }
            b'$' => {
                // dollar quoting: $$...$$ or $tag$...$tag$
                let mut j = i + 1;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    j += 1;
                }
                if j < b.len() && b[j] == b'$' {
                    let tag = &sql[i..=j];
                    match sql[j + 1..].find(tag) {
                        Some(end) => i = j + 1 + end + tag.len() - 1,
                        None => return true,
                    }
                }
            }
            b';' => {
                if !sql[i + 1..].trim().is_empty() {
                    return true;
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// The table an INSERT / UPDATE / DELETE / TRUNCATE / ALTER TABLE writes to.
fn written_table(head: &str) -> Option<String> {
    let lower = head.to_ascii_lowercase();
    let mut words = lower.split_whitespace();
    let first = words.next()?;
    let mut name = match first {
        "insert" => {
            words.next().filter(|w| *w == "into")?;
            words.next()?
        }
        "delete" => {
            words.next().filter(|w| *w == "from")?;
            words.next()?
        }
        "update" => words.next()?,
        "truncate" | "alter" => {
            let mut w = words.next()?;
            if w == "table" {
                w = words.next()?;
            }
            while matches!(w, "only" | "if" | "exists") {
                w = words.next()?;
            }
            w
        }
        _ => return None,
    };
    if name == "only" {
        name = words.next()?;
    }
    // strip the column list "(", schema and quotes
    let name = name.split('(').next()?.rsplit('.').next()?.trim_matches('"').to_string();
    if name.is_empty() { None } else { Some(name) }
}

#[cfg(test)]
mod tests {
    use super::written_table;
    #[test]
    fn tables() {
        assert_eq!(written_table("insert into \"portal_setting\" (a) values (1)").as_deref(), Some("portal_setting"));
        assert_eq!(written_table("insert into tree_node(a) values (1)").as_deref(), Some("tree_node"));
        assert_eq!(written_table("UPDATE public.iap_user set x=1").as_deref(), Some("iap_user"));
        assert_eq!(written_table("delete from user_role where id=3").as_deref(), Some("user_role"));
        assert_eq!(written_table("alter table if exists portal_page add column x int").as_deref(), Some("portal_page"));
    }
    #[test]
    fn statements() {
        use super::several_statements as m;
        assert!(!m("insert into portal_audit_trail (ua) values ('Mozilla/5.0 (X11; Linux x86_64)')"));
        assert!(!m("select 1;"));
        assert!(!m("select $$a;b$$"));
        assert!(m("update a set x=1; update portal_setting set y=2"));
        assert!(m("select 1; delete from tree"));
    }
}
