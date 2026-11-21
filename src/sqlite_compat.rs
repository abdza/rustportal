//! PostgreSQL-to-SQLite SQL compatibility shim.
//!
//! Translates common PostgreSQL syntax used by rustportal and imported portal
//! modules (`ILIKE`, `::type` casts, `now()`, `interval '...'`, `EXTRACT(...)`,
//! `DATE_TRUNC(...)`, `TO_CHAR(...)`, `STRING_AGG(...)`, `TRUNCATE TABLE`) into
//! equivalent SQLite expressions while leaving string literals, quoted
//! identifiers, and SQL comments untouched.

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    /// Single-quoted string literal (includes quotes)
    Str(String),
    /// Double-quoted identifier (includes quotes)
    Ident(String),
    /// Comment (`-- ...` or `/* ... */`)
    Comment(String),
    /// Whitespace run
    Space(String),
    /// Plain identifier or keyword
    Word(String),
    /// Numeric literal
    Num(String),
    /// `::` cast operator
    DoubleColon,
    /// Single punctuation / operator char
    Punct(char),
}

impl Tok {
    fn as_str(&self) -> String {
        match self {
            Tok::Str(s) | Tok::Ident(s) | Tok::Comment(s) | Tok::Space(s) | Tok::Word(s) | Tok::Num(s) => s.clone(),
            Tok::DoubleColon => "::".into(),
            Tok::Punct(c) => c.to_string(),
        }
    }
    fn is_trivia(&self) -> bool {
        matches!(self, Tok::Space(_) | Tok::Comment(_))
    }
    fn is_word(&self, kw: &str) -> bool {
        matches!(self, Tok::Word(w) if w.eq_ignore_ascii_case(kw))
    }
}

fn tokenize(sql: &str) -> Vec<Tok> {
    let b: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            let start = i;
            while i < b.len() && b[i].is_whitespace() {
                i += 1;
            }
            out.push(Tok::Space(b[start..i].iter().collect()));
            continue;
        }
        // Line comment --
        if c == '-' && i + 1 < b.len() && b[i + 1] == '-' {
            let start = i;
            i += 2;
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            out.push(Tok::Comment(b[start..i].iter().collect()));
            continue;
        }
        // Block comment /* ... */
        if c == '/' && i + 1 < b.len() && b[i + 1] == '*' {
            let start = i;
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            out.push(Tok::Comment(b[start..i].iter().collect()));
            continue;
        }
        // Quoted string or identifier
        if c == '\'' || c == '"' {
            let start = i;
            i += 1;
            while i < b.len() {
                if b[i] == c {
                    if i + 1 < b.len() && b[i + 1] == c {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            let s: String = b[start..i].iter().collect();
            if c == '\'' {
                out.push(Tok::Str(s));
            } else {
                out.push(Tok::Ident(s));
            }
            continue;
        }
        // :: cast operator
        if c == ':' && i + 1 < b.len() && b[i + 1] == ':' {
            out.push(Tok::DoubleColon);
            i += 2;
            continue;
        }
        // Word (identifier / keyword)
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == '_') {
                i += 1;
            }
            out.push(Tok::Word(b[start..i].iter().collect()));
            continue;
        }
        // Number
        if c.is_ascii_digit() {
            let start = i;
            i += 1;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == '.') {
                i += 1;
            }
            out.push(Tok::Num(b[start..i].iter().collect()));
            continue;
        }
        out.push(Tok::Punct(c));
        i += 1;
    }
    out
}

fn render(toks: &[Tok]) -> String {
    let mut s = String::new();
    for t in toks {
        s.push_str(&t.as_str());
    }
    s
}

fn unquote_str(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        s[1..s.len() - 1].replace("''", "'")
    } else {
        s.to_string()
    }
}

fn next_sig(toks: &[Tok], mut i: usize) -> Option<usize> {
    while i < toks.len() {
        if !toks[i].is_trivia() {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn prev_sig(toks: &[Tok], i: usize) -> Option<usize> {
    let mut j = i;
    while j > 0 {
        j -= 1;
        if !toks[j].is_trivia() {
            return Some(j);
        }
    }
    None
}

/// Given index of `(`, find index of matching `)`.
fn match_paren(toks: &[Tok], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, t) in toks.iter().enumerate().skip(open) {
        match t {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Given index of `)`, find index of matching `(`.
fn match_paren_back(toks: &[Tok], close: usize) -> Option<usize> {
    let mut depth = 0usize;
    for i in (0..=close).rev() {
        match toks[i] {
            Tok::Punct(')') => depth += 1,
            Tok::Punct('(') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split a token slice on top-level commas.
fn split_args(toks: &[Tok]) -> Vec<&[Tok]> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, t) in toks.iter().enumerate() {
        match t {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => depth = depth.saturating_sub(1),
            Tok::Punct(',') if depth == 0 => {
                out.push(&toks[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&toks[start..]);
    out
}

/// Find the start index in `out` of the primary SQL expression ending at `end` (inclusive).
fn expr_start_back(out: &[Tok], end: usize) -> Option<usize> {
    let last = prev_sig(out, end + 1)?;
    match &out[last] {
        Tok::Punct(')') => {
            let open = match_paren_back(out, last)?;
            if let Some(p) = prev_sig(out, open) {
                if matches!(out[p], Tok::Word(_) | Tok::Ident(_)) {
                    // Could be schema.func(...)
                    let mut start = p;
                    while let Some(dot) = prev_sig(out, start) {
                        if matches!(out[dot], Tok::Punct('.')) {
                            if let Some(pref) = prev_sig(out, dot) {
                                if matches!(out[pref], Tok::Word(_) | Tok::Ident(_)) {
                                    start = pref;
                                    continue;
                                }
                            }
                        }
                        break;
                    }
                    return Some(start);
                }
            }
            Some(open)
        }
        Tok::Word(_) | Tok::Ident(_) => {
            let mut start = last;
            while let Some(dot) = prev_sig(out, start) {
                if matches!(out[dot], Tok::Punct('.')) {
                    if let Some(pref) = prev_sig(out, dot) {
                        if matches!(out[pref], Tok::Word(_) | Tok::Ident(_)) {
                            start = pref;
                            continue;
                        }
                    }
                }
                break;
            }
            Some(start)
        }
        Tok::Str(_) | Tok::Num(_) => Some(last),
        _ => None,
    }
}

/// Parse a PostgreSQL interval string like `'2 days'`, `'48 hours'`, `'1 week'`,
/// `'1 day 2 hours'` into SQLite date/time modifiers, applying `sign` (`+1` or `-1`).
fn interval_modifiers(spec: &str, sign: i64) -> Option<Vec<String>> {
    let parts: Vec<&str> = spec.split_whitespace().collect();
    if parts.is_empty() || parts.len() % 2 != 0 {
        return None;
    }
    let mut mods = Vec::new();
    for pair in parts.chunks(2) {
        let mut n: i64 = pair[0].parse().ok()?;
        n *= sign;
        let unit = pair[1].to_ascii_lowercase();
        let (val, sqlite_unit) = match unit.as_str() {
            "year" | "years" | "yr" | "yrs" | "y" => (n, "years"),
            "month" | "months" | "mon" | "mons" => (n, "months"),
            "week" | "weeks" | "wk" | "wks" | "w" => (n * 7, "days"),
            "day" | "days" | "d" => (n, "days"),
            "hour" | "hours" | "hr" | "hrs" | "h" => (n, "hours"),
            "minute" | "minutes" | "min" | "mins" | "m" => (n, "minutes"),
            "second" | "seconds" | "sec" | "secs" | "s" => (n, "seconds"),
            _ => return None,
        };
        let prefix = if val >= 0 { "+" } else { "" };
        mods.push(format!("'{}{} {}'", prefix, val, sqlite_unit));
    }
    Some(mods)
}

/// Rewrite `TRUNCATE [TABLE] <name>` to `DELETE FROM <name>`.
fn pass_truncate(toks: Vec<Tok>) -> Vec<Tok> {
    let mut out = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        let at_stmt_start = prev_sig(&out, out.len()).map(|p| matches!(out[p], Tok::Punct(';'))).unwrap_or(true);
        if at_stmt_start && toks[i].is_word("truncate") {
            out.push(Tok::Word("DELETE".into()));
            out.push(Tok::Space(" ".into()));
            out.push(Tok::Word("FROM".into()));
            i += 1;
            if let Some(nxt) = next_sig(&toks, i) {
                if toks[nxt].is_word("table") {
                    i = nxt + 1;
                    continue;
                }
            }
            continue;
        }
        out.push(toks[i].clone());
        i += 1;
    }
    out
}

/// Rewrite `EXTRACT(field FROM expr)`, `DATE_TRUNC('unit', expr)`, `TO_CHAR(expr, 'fmt')`.
fn pass_functions(toks: Vec<Tok>, localtime: bool) -> Vec<Tok> {
    let mut out = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        let preceded_by_dot = prev_sig(&out, out.len()).map(|p| matches!(out[p], Tok::Punct('.'))).unwrap_or(false);
        if !preceded_by_dot {
            // EXTRACT(field FROM expr)
            if toks[i].is_word("extract") {
                if let Some(open) = next_sig(&toks, i + 1) {
                    if matches!(toks[open], Tok::Punct('(')) {
                        if let Some(close) = match_paren(&toks, open) {
                            let inner = &toks[open + 1..close];
                            if let Some(fidx) = next_sig(inner, 0) {
                                if let Tok::Word(field) = &inner[fidx] {
                                    if let Some(from_idx) = next_sig(inner, fidx + 1) {
                                        if inner[from_idx].is_word("from") {
                                            let fmt = match field.to_ascii_uppercase().as_str() {
                                                "YEAR" => Some("%Y"),
                                                "MONTH" => Some("%m"),
                                                "DAY" => Some("%d"),
                                                "HOUR" => Some("%H"),
                                                "MINUTE" => Some("%M"),
                                                "SECOND" => Some("%S"),
                                                "DOW" => Some("%w"),
                                                "DOY" => Some("%j"),
                                                "WEEK" => Some("%W"),
                                                "EPOCH" => Some("%s"),
                                                _ => None,
                                            };
                                            if let Some(fmt) = fmt {
                                                let expr_sql = rewrite(&render(&inner[from_idx + 1..]).trim(), localtime);
                                                let rep = format!("CAST(strftime('{}', {}) AS INTEGER)", fmt, expr_sql);
                                                out.extend(tokenize(&rep));
                                                i = close + 1;
                                                continue;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // DATE_TRUNC('unit', expr)
            if toks[i].is_word("date_trunc") {
                if let Some(open) = next_sig(&toks, i + 1) {
                    if matches!(toks[open], Tok::Punct('(')) {
                        if let Some(close) = match_paren(&toks, open) {
                            let args = split_args(&toks[open + 1..close]);
                            if args.len() == 2 {
                                if let Some(u_idx) = next_sig(args[0], 0) {
                                    if let Tok::Str(u_lit) = &args[0][u_idx] {
                                        let fmt = match unquote_str(u_lit).to_ascii_lowercase().as_str() {
                                            "year" => Some("%Y-01-01 00:00:00"),
                                            "month" => Some("%Y-%m-01 00:00:00"),
                                            "day" => Some("%Y-%m-%d 00:00:00"),
                                            "hour" => Some("%Y-%m-%d %H:00:00"),
                                            "minute" => Some("%Y-%m-%d %H:%M:00"),
                                            _ => None,
                                        };
                                        if let Some(fmt) = fmt {
                                            let expr_sql = rewrite(render(args[1]).trim(), localtime);
                                            out.extend(tokenize(&format!("strftime('{}', {})", fmt, expr_sql)));
                                            i = close + 1;
                                            continue;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // TO_CHAR(expr, 'pg_date_fmt')
            if toks[i].is_word("to_char") {
                if let Some(open) = next_sig(&toks, i + 1) {
                    if matches!(toks[open], Tok::Punct('(')) {
                        if let Some(close) = match_paren(&toks, open) {
                            let args = split_args(&toks[open + 1..close]);
                            if args.len() == 2 {
                                if let Some(f_idx) = next_sig(args[1], 0) {
                                    if let Tok::Str(f_lit) = &args[1][f_idx] {
                                        let pg_fmt = unquote_str(f_lit);
                                        if pg_fmt.contains("YYYY") || pg_fmt.contains("MM") || pg_fmt.contains("DD") || pg_fmt.contains("HH24") {
                                            let sq_fmt = pg_fmt
                                                .replace("YYYY", "%Y")
                                                .replace("MM", "%m")
                                                .replace("DD", "%d")
                                                .replace("HH24", "%H")
                                                .replace("MI", "%M")
                                                .replace("SS", "%S");
                                            let expr_sql = rewrite(render(args[0]).trim(), localtime);
                                            out.extend(tokenize(&format!("strftime('{}', {})", sq_fmt.replace('\'', "''"), expr_sql)));
                                            i = close + 1;
                                            continue;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        out.push(toks[i].clone());
        i += 1;
    }
    out
}

/// Rewrite `<expr> +/- INTERVAL '...'` into SQLite `datetime(...)` / `date(...)`.
fn pass_intervals(toks: Vec<Tok>, localtime: bool) -> Vec<Tok> {
    let mut out = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if matches!(toks[i], Tok::Punct('+') | Tok::Punct('-')) {
            let sign = if matches!(toks[i], Tok::Punct('+')) { 1 } else { -1 };
            if let Some(kw) = next_sig(&toks, i + 1) {
                if toks[kw].is_word("interval") {
                    if let Some(s_idx) = next_sig(&toks, kw + 1) {
                        if let Tok::Str(spec_lit) = &toks[s_idx] {
                            let spec = unquote_str(spec_lit);
                            if let Some(mods) = interval_modifiers(&spec, sign) {
                                if let Some(lhs_start) = expr_start_back(&out, out.len().saturating_sub(1)) {
                                    let lhs_toks: Vec<Tok> = out.drain(lhs_start..).collect();
                                    // Preserve trailing whitespace before +/- only if needed
                                    let lhs_str = render(&lhs_toks).trim().to_string();
                                    let lhs_low = lhs_str.to_ascii_lowercase().replace(' ', "");
                                    let lt_arg = if localtime { ", 'localtime'" } else { "" };
                                    let mods_joined = mods.join(", ");
                                    let rep = if lhs_low == "now()" || lhs_low == "current_timestamp" {
                                        format!("datetime('now'{}, {})", lt_arg, mods_joined)
                                    } else if lhs_low == "current_date" {
                                        format!("date('now'{}, {})", lt_arg, mods_joined)
                                    } else {
                                        format!("datetime({}, {})", lhs_str, mods_joined)
                                    };
                                    out.extend(tokenize(&rep));
                                    i = s_idx + 1;
                                    continue;
                                }
                            }
                        }
                    }
                }
            }
        }
        out.push(toks[i].clone());
        i += 1;
    }
    out
}

/// Rewrite standalone `now()`, `current_timestamp`, `current_date`, `ILIKE`, `STRING_AGG(`.
fn pass_keywords(toks: Vec<Tok>, localtime: bool) -> Vec<Tok> {
    let mut out = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        let preceded_by_dot = prev_sig(&out, out.len()).map(|p| matches!(out[p], Tok::Punct('.'))).unwrap_or(false);
        if !preceded_by_dot {
            // now()
            if toks[i].is_word("now") {
                if let Some(open) = next_sig(&toks, i + 1) {
                    if matches!(toks[open], Tok::Punct('(')) {
                        if let Some(close) = next_sig(&toks, open + 1) {
                            if matches!(toks[close], Tok::Punct(')')) {
                                let rep = if localtime {
                                    "(datetime('now', 'localtime'))"
                                } else {
                                    "(datetime('now'))"
                                };
                                out.extend(tokenize(rep));
                                i = close + 1;
                                continue;
                            }
                        }
                    }
                }
            }
            // current_timestamp (not followed by `(`)
            if toks[i].is_word("current_timestamp") {
                let followed_by_paren = next_sig(&toks, i + 1).map(|n| matches!(toks[n], Tok::Punct('('))).unwrap_or(false);
                if !followed_by_paren {
                    let rep = if localtime {
                        "(datetime('now', 'localtime'))"
                    } else {
                        "(datetime('now'))"
                    };
                    out.extend(tokenize(rep));
                    i += 1;
                    continue;
                }
            }
            // current_date (not followed by `(`)
            if toks[i].is_word("current_date") {
                let followed_by_paren = next_sig(&toks, i + 1).map(|n| matches!(toks[n], Tok::Punct('('))).unwrap_or(false);
                if !followed_by_paren {
                    let rep = if localtime {
                        "(date('now', 'localtime'))"
                    } else {
                        "(date('now'))"
                    };
                    out.extend(tokenize(rep));
                    i += 1;
                    continue;
                }
            }
            // ILIKE -> LIKE
            if toks[i].is_word("ilike") {
                out.push(Tok::Word("LIKE".into()));
                i += 1;
                continue;
            }
            // STRING_AGG( -> group_concat(
            if toks[i].is_word("string_agg") {
                if let Some(open) = next_sig(&toks, i + 1) {
                    if matches!(toks[open], Tok::Punct('(')) {
                        out.push(Tok::Word("group_concat".into()));
                        i += 1;
                        continue;
                    }
                }
            }
        }
        out.push(toks[i].clone());
        i += 1;
    }
    out
}

/// Rewrite PostgreSQL `expr::type` casts to `CAST(expr AS ...)` (or `date(expr)` / `datetime(expr)`).
fn pass_casts(toks: Vec<Tok>) -> Vec<Tok> {
    let mut out = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if matches!(toks[i], Tok::DoubleColon) {
            if let Some(t_start) = next_sig(&toks, i + 1) {
                if let Tok::Word(t1) = &toks[t_start] {
                    let mut end = t_start;
                    let mut tname = t1.to_ascii_lowercase();
                    // Multi-word Postgres type names
                    if let Some(t2_idx) = next_sig(&toks, end + 1) {
                        if let Tok::Word(t2) = &toks[t2_idx] {
                            let t2l = t2.to_ascii_lowercase();
                            if (tname == "double" && t2l == "precision")
                                || (tname == "character" && t2l == "varying")
                            {
                                tname = format!("{} {}", tname, t2l);
                                end = t2_idx;
                            } else if (tname == "timestamp" || tname == "time") && (t2l == "with" || t2l == "without") {
                                // consume `with/without time zone`
                                if let Some(t3) = next_sig(&toks, t2_idx + 1) {
                                    if toks[t3].is_word("time") {
                                        if let Some(t4) = next_sig(&toks, t3 + 1) {
                                            if toks[t4].is_word("zone") {
                                                end = t4;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // Optional type precision like varchar(255) or numeric(10, 2)
                    if let Some(open) = next_sig(&toks, end + 1) {
                        if matches!(toks[open], Tok::Punct('(')) {
                            if let Some(close) = match_paren(&toks, open) {
                                end = close;
                            }
                        }
                    }
                    if let Some(lhs_start) = expr_start_back(&out, out.len().saturating_sub(1)) {
                        let lhs_toks: Vec<Tok> = out.drain(lhs_start..).collect();
                        let lhs = render(&lhs_toks).trim().to_string();
                        let rep = match tname.as_str() {
                            "date" => format!("date({})", lhs),
                            "timestamp" | "timestamptz" => format!("datetime({})", lhs),
                            "text" | "varchar" | "character varying" | "char" | "character" | "bpchar" | "time" | "uuid" | "json" | "jsonb" => {
                                format!("CAST({} AS TEXT)", lhs)
                            }
                            "int" | "int2" | "int4" | "int8" | "integer" | "bigint" | "smallint" | "bool" | "boolean" => {
                                format!("CAST({} AS INTEGER)", lhs)
                            }
                            "numeric" | "decimal" | "real" | "float" | "float4" | "float8" | "double precision" => {
                                format!("CAST({} AS REAL)", lhs)
                            }
                            _ => format!("CAST({} AS TEXT)", lhs),
                        };
                        out.extend(tokenize(&rep));
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        out.push(toks[i].clone());
        i += 1;
    }
    out
}

/// Rewrite a SQL string from PostgreSQL syntax to SQLite-compatible SQL.
pub fn rewrite(sql: &str, localtime: bool) -> String {
    let toks = tokenize(sql);
    let toks = pass_truncate(toks);
    let toks = pass_functions(toks, localtime);
    let toks = pass_intervals(toks, localtime);
    let toks = pass_keywords(toks, localtime);
    let toks = pass_casts(toks);
    render(&toks)
}

#[cfg(test)]
mod tests {
    use super::rewrite;

    #[test]
    fn ilike_and_casts() {
        assert_eq!(
            rewrite("select id, \"name\"::text as d from portal_user where name ilike '%ali%'", true),
            "select id, CAST(\"name\" AS TEXT) as d from portal_user where name LIKE '%ali%'"
        );
        assert_eq!(
            rewrite("select * from t where u.id::text = d.v::text", true),
            "select * from t where CAST(u.id AS TEXT) = CAST(d.v AS TEXT)"
        );
        assert_eq!(
            rewrite("select coalesce(a, 0)::numeric(10,2)::text, 'x::int' as lit", true),
            "select CAST(CAST(coalesce(a, 0) AS REAL) AS TEXT), 'x::int' as lit"
        );
    }

    #[test]
    fn strings_and_comments_untouched() {
        let q = "select 'now() ilike a::text' as s -- now() ilike\n from t where x ilike 'foo'";
        assert_eq!(
            rewrite(q, true),
            "select 'now() ilike a::text' as s -- now() ilike\n from t where x LIKE 'foo'"
        );
    }

    #[test]
    fn now_and_intervals() {
        assert_eq!(
            rewrite("delete from portal_session where updated < now() - interval '12 hours'", true),
            "delete from portal_session where updated < datetime('now', 'localtime', '-12 hours')"
        );
        assert_eq!(
            rewrite("select * from e where delivery_time <= now() and delivery_time > now() - interval '2 days'", true),
            "select * from e where delivery_time <= (datetime('now', 'localtime')) and delivery_time > datetime('now', 'localtime', '-2 days')"
        );
        assert_eq!(
            rewrite("select current_date - interval '1 week'", false),
            "select date('now', '-7 days')"
        );
        assert_eq!(
            rewrite("update portal_user set lastlogin=now() where id=1", true),
            "update portal_user set lastlogin=(datetime('now', 'localtime')) where id=1"
        );
    }

    #[test]
    fn extract_date_trunc_truncate_and_to_char() {
        assert_eq!(
            rewrite("select * from t where extract(YEAR from created_at)=2026 and extract(MONTH from created_at)=5", true),
            "select * from t where CAST(strftime('%Y', created_at) AS INTEGER)=2026 and CAST(strftime('%m', created_at) AS INTEGER)=5"
        );
        assert_eq!(
            rewrite("select date_trunc('month', dt), to_char(dt, 'YYYY-MM-DD'), string_agg(name, ',') from t", true),
            "select strftime('%Y-%m-01 00:00:00', dt), strftime('%Y-%m-%d', dt), group_concat(name, ',') from t"
        );
        assert_eq!(
            rewrite("truncate table trak_hr_leave_data", true),
            "DELETE FROM trak_hr_leave_data"
        );
    }
}
