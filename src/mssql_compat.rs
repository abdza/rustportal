//! PostgreSQL-to-MSSQL (T-SQL) SQL compatibility shim.
//!
//! Translates common PostgreSQL syntax used by rustportal and imported portal
//! modules (`ILIKE`, `::type` casts, `now()`, `current_date`, `interval '...'`,
//! `EXTRACT(...)`, `DATE_TRUNC(...)`, `TO_CHAR(...)`, `STRING_AGG(...)`,
//! `IS [NOT] TRUE/FALSE`, `TRUE`/`FALSE` literals, `NULLS LAST`/`NULLS FIRST`,
//! `LIMIT ... OFFSET ...`, `INSERT ... RETURNING ...`, and non-ASCII Unicode
//! string literals) into equivalent SQL Server T-SQL while leaving string
//! literals, quoted/bracketed identifiers, and SQL comments untouched.

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    /// Single-quoted string literal (includes quotes, and optional leading `N`)
    Str(String),
    /// Double-quoted or bracketed identifier (includes delimiters)
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
        // Unicode N'...' string literal
        if (c == 'N' || c == 'n') && i + 1 < b.len() && b[i + 1] == '\'' {
            let start = i;
            i += 2;
            while i < b.len() {
                if b[i] == '\'' {
                    if i + 1 < b.len() && b[i + 1] == '\'' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(Tok::Str(b[start..i].iter().collect()));
            continue;
        }
        // Bracketed T-SQL identifier [...]
        if c == '[' {
            let start = i;
            i += 1;
            while i < b.len() {
                if b[i] == ']' {
                    if i + 1 < b.len() && b[i + 1] == ']' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(Tok::Ident(b[start..i].iter().collect()));
            continue;
        }
        // Quoted string or double-quoted identifier
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
    let body = if (s.starts_with("N'") || s.starts_with("n'")) && s.ends_with('\'') && s.len() >= 3 {
        &s[2..s.len() - 1]
    } else if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        &s[1..s.len() - 1]
    } else {
        return s.to_string();
    };
    body.replace("''", "'")
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

fn now_fn(utc: bool) -> &'static str {
    if utc { "GETUTCDATE()" } else { "GETDATE()" }
}

/// Parse a PostgreSQL interval string like `'2 days'`, `'48 hours'`, `'1 week'`,
/// `'1 day 2 hours'` into `(mssql_unit, signed_amount)` pairs.
fn interval_parts(spec: &str, sign: i64) -> Option<Vec<(&'static str, i64)>> {
    let parts: Vec<&str> = spec.split_whitespace().collect();
    if parts.is_empty() || parts.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::new();
    for pair in parts.chunks(2) {
        let mut n: i64 = pair[0].parse().ok()?;
        n *= sign;
        let unit = pair[1].to_ascii_lowercase();
        let (val, ms_unit) = match unit.as_str() {
            "year" | "years" | "yr" | "yrs" | "y" => (n, "year"),
            "month" | "months" | "mon" | "mons" => (n, "month"),
            "week" | "weeks" | "wk" | "wks" | "w" => (n * 7, "day"),
            "day" | "days" | "d" => (n, "day"),
            "hour" | "hours" | "hr" | "hrs" | "h" => (n, "hour"),
            "minute" | "minutes" | "min" | "mins" | "m" => (n, "minute"),
            "second" | "seconds" | "sec" | "secs" | "s" => (n, "second"),
            _ => return None,
        };
        out.push((ms_unit, val));
    }
    Some(out)
}

/// Rewrite `EXTRACT(field FROM expr)`, `DATE_TRUNC('unit', expr)`, `TO_CHAR(expr, 'fmt')`, `STRING_AGG(expr, sep)`.
fn pass_functions(toks: Vec<Tok>, utc: bool) -> Vec<Tok> {
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
                                            let expr_sql = rewrite(render(&inner[from_idx + 1..]).trim(), utc);
                                            let rep = match field.to_ascii_uppercase().as_str() {
                                                "YEAR" => Some(format!("DATEPART(year, {})", expr_sql)),
                                                "MONTH" => Some(format!("DATEPART(month, {})", expr_sql)),
                                                "DAY" => Some(format!("DATEPART(day, {})", expr_sql)),
                                                "HOUR" => Some(format!("DATEPART(hour, {})", expr_sql)),
                                                "MINUTE" => Some(format!("DATEPART(minute, {})", expr_sql)),
                                                "SECOND" => Some(format!("DATEPART(second, {})", expr_sql)),
                                                "DOW" => Some(format!("(DATEPART(weekday, {}) - 1)", expr_sql)),
                                                "DOY" => Some(format!("DATEPART(dayofyear, {})", expr_sql)),
                                                "WEEK" => Some(format!("DATEPART(week, {})", expr_sql)),
                                                "EPOCH" => Some(format!("DATEDIFF(second, '1970-01-01', {})", expr_sql)),
                                                _ => None,
                                            };
                                            if let Some(rep) = rep {
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
                                        let expr_sql = rewrite(render(args[1]).trim(), utc);
                                        let rep = match unquote_str(u_lit).to_ascii_lowercase().as_str() {
                                            "year" => Some(format!("DATETIME2FROMPARTS(YEAR({e}), 1, 1, 0, 0, 0, 0, 0)", e = expr_sql)),
                                            "month" => Some(format!("DATETIME2FROMPARTS(YEAR({e}), MONTH({e}), 1, 0, 0, 0, 0, 0)", e = expr_sql)),
                                            "day" => Some(format!("CAST(CAST({e} AS DATE) AS DATETIME2)", e = expr_sql)),
                                            "hour" => Some(format!("DATEADD(hour, DATEDIFF(hour, 0, {e}), 0)", e = expr_sql)),
                                            "minute" => Some(format!("DATEADD(minute, DATEDIFF(minute, 0, {e}), 0)", e = expr_sql)),
                                            _ => None,
                                        };
                                        if let Some(rep) = rep {
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
                                            let ms_fmt = pg_fmt
                                                .replace("YYYY", "yyyy")
                                                .replace("DD", "dd")
                                                .replace("HH24", "HH")
                                                .replace("MI", "mm")
                                                .replace("SS", "ss");
                                            let expr_sql = rewrite(render(args[0]).trim(), utc);
                                            out.extend(tokenize(&format!(
                                                "FORMAT(CAST({} AS DATETIME2), '{}')",
                                                expr_sql,
                                                ms_fmt.replace('\'', "''")
                                            )));
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
            // STRING_AGG(expr, sep) -> ensure NVARCHAR(MAX) to avoid 8000-byte overflow
            if toks[i].is_word("string_agg") {
                if let Some(open) = next_sig(&toks, i + 1) {
                    if matches!(toks[open], Tok::Punct('(')) {
                        if let Some(close) = match_paren(&toks, open) {
                            let args = split_args(&toks[open + 1..close]);
                            if args.len() == 2 {
                                let a0 = rewrite(render(args[0]).trim(), utc);
                                let a1 = rewrite(render(args[1]).trim(), utc);
                                if !a0.to_ascii_uppercase().starts_with("CAST(") {
                                    out.extend(tokenize(&format!("STRING_AGG(CAST({} AS NVARCHAR(MAX)), {})", a0, a1)));
                                    i = close + 1;
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

/// Rewrite `<expr> +/- INTERVAL '...'` into T-SQL `DATEADD(...)`.
fn pass_intervals(toks: Vec<Tok>, utc: bool) -> Vec<Tok> {
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
                            if let Some(parts) = interval_parts(&spec, sign) {
                                if let Some(lhs_start) = expr_start_back(&out, out.len().saturating_sub(1)) {
                                    let lhs_toks: Vec<Tok> = out.drain(lhs_start..).collect();
                                    let lhs_str = render(&lhs_toks).trim().to_string();
                                    let lhs_low = lhs_str.to_ascii_lowercase().replace(' ', "");
                                    let mut expr = if lhs_low == "now()" || lhs_low == "current_timestamp" {
                                        now_fn(utc).to_string()
                                    } else if lhs_low == "current_date" {
                                        format!("CAST({} AS DATE)", now_fn(utc))
                                    } else {
                                        lhs_str
                                    };
                                    for (unit, val) in parts {
                                        expr = format!("DATEADD({}, {}, {})", unit, val, expr);
                                    }
                                    out.extend(tokenize(&expr));
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

/// Rewrite `<expr> IS [NOT] TRUE/FALSE` into T-SQL `BIT` comparisons.
fn pass_is_bool(toks: Vec<Tok>) -> Vec<Tok> {
    let mut out = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if toks[i].is_word("is") {
            if let Some(n1) = next_sig(&toks, i + 1) {
                let (neg, b_idx) = if toks[n1].is_word("not") {
                    if let Some(n2) = next_sig(&toks, n1 + 1) {
                        (true, n2)
                    } else {
                        out.push(toks[i].clone());
                        i += 1;
                        continue;
                    }
                } else {
                    (false, n1)
                };
                let is_t = toks[b_idx].is_word("true");
                let is_f = toks[b_idx].is_word("false");
                if is_t || is_f {
                    if let Some(lhs_start) = expr_start_back(&out, out.len().saturating_sub(1)) {
                        let lhs_toks: Vec<Tok> = out.drain(lhs_start..).collect();
                        let lhs = render(&lhs_toks).trim().to_string();
                        let rep = match (neg, is_t) {
                            (false, true) => format!("({} = 1)", lhs),
                            (true, true) => format!("({l} IS NULL OR {l} = 0)", l = lhs),
                            (false, false) => format!("({} = 0)", lhs),
                            (true, false) => format!("({l} IS NULL OR {l} = 1)", l = lhs),
                        };
                        out.extend(tokenize(&rep));
                        i = b_idx + 1;
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

/// Rewrite standalone `now()`, `current_timestamp`, `current_date`, `ILIKE`, `TRUE`, `FALSE`.
fn pass_keywords(toks: Vec<Tok>, utc: bool) -> Vec<Tok> {
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
                                out.extend(tokenize(now_fn(utc)));
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
                    out.extend(tokenize(now_fn(utc)));
                    i += 1;
                    continue;
                }
            }
            // current_date (not followed by `(`)
            if toks[i].is_word("current_date") {
                let followed_by_paren = next_sig(&toks, i + 1).map(|n| matches!(toks[n], Tok::Punct('('))).unwrap_or(false);
                if !followed_by_paren {
                    out.extend(tokenize(&format!("CAST({} AS DATE)", now_fn(utc))));
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
            // Standalone TRUE / FALSE boolean literals -> 1 / 0
            if toks[i].is_word("true") || toks[i].is_word("false") {
                let followed_by_paren = next_sig(&toks, i + 1).map(|n| matches!(toks[n], Tok::Punct('('))).unwrap_or(false);
                let preceded_by_as = prev_sig(&out, out.len()).map(|p| out[p].is_word("as")).unwrap_or(false);
                if !followed_by_paren && !preceded_by_as {
                    out.push(Tok::Num(if toks[i].is_word("true") { "1".into() } else { "0".into() }));
                    i += 1;
                    continue;
                }
            }
        }
        out.push(toks[i].clone());
        i += 1;
    }
    out
}

/// Rewrite PostgreSQL `expr::type` casts to T-SQL `CAST(expr AS ...)`.
fn pass_casts(toks: Vec<Tok>) -> Vec<Tok> {
    let mut out = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if matches!(toks[i], Tok::DoubleColon) {
            if let Some(t_start) = next_sig(&toks, i + 1) {
                if let Tok::Word(t1) = &toks[t_start] {
                    let mut end = t_start;
                    let mut tname = t1.to_ascii_lowercase();
                    if let Some(t2_idx) = next_sig(&toks, end + 1) {
                        if let Tok::Word(t2) = &toks[t2_idx] {
                            let t2l = t2.to_ascii_lowercase();
                            if (tname == "double" && t2l == "precision")
                                || (tname == "character" && t2l == "varying")
                            {
                                tname = format!("{} {}", tname, t2l);
                                end = t2_idx;
                            } else if (tname == "timestamp" || tname == "time") && (t2l == "with" || t2l == "without") {
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
                    let mut prec = String::new();
                    if let Some(open) = next_sig(&toks, end + 1) {
                        if matches!(toks[open], Tok::Punct('(')) {
                            if let Some(close) = match_paren(&toks, open) {
                                prec = render(&toks[open..=close]);
                                end = close;
                            }
                        }
                    }
                    if let Some(lhs_start) = expr_start_back(&out, out.len().saturating_sub(1)) {
                        let lhs_toks: Vec<Tok> = out.drain(lhs_start..).collect();
                        let lhs = render(&lhs_toks).trim().to_string();
                        let ms_type = match tname.as_str() {
                            "date" => "DATE".to_string(),
                            "time" => "TIME".to_string(),
                            "timestamp" | "timestamptz" => "DATETIME2".to_string(),
                            "int2" | "smallint" => "SMALLINT".to_string(),
                            "int" | "int4" | "integer" => "INT".to_string(),
                            "int8" | "bigint" => "BIGINT".to_string(),
                            "bool" | "boolean" => "BIT".to_string(),
                            "numeric" | "decimal" => {
                                if prec.is_empty() { "DECIMAL(24,6)".to_string() } else { format!("DECIMAL{}", prec) }
                            }
                            "real" | "float4" => "REAL".to_string(),
                            "float" | "float8" | "double precision" => "FLOAT".to_string(),
                            _ => "NVARCHAR(MAX)".to_string(),
                        };
                        out.extend(tokenize(&format!("CAST({} AS {})", lhs, ms_type)));
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

/// Rewrite `<expr> [ASC|DESC] NULLS LAST|FIRST` into T-SQL `CASE WHEN <expr> IS NULL ...`.
fn pass_nulls_order(toks: Vec<Tok>) -> Vec<Tok> {
    let mut out = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if toks[i].is_word("nulls") {
            if let Some(next_idx) = next_sig(&toks, i + 1) {
                let is_last = toks[next_idx].is_word("last");
                let is_first = toks[next_idx].is_word("first");
                if is_last || is_first {
                    // Check if immediately preceded by ASC or DESC
                    let mut dir = "ASC";
                    let mut cut_end = out.len();
                    if let Some(p) = prev_sig(&out, out.len()) {
                        if out[p].is_word("desc") {
                            dir = "DESC";
                            cut_end = p;
                        } else if out[p].is_word("asc") {
                            dir = "ASC";
                            cut_end = p;
                        }
                    }
                    if cut_end > 0 {
                        if let Some(expr_start) = expr_start_back(&out, cut_end - 1) {
                            let expr_toks: Vec<Tok> = out.drain(expr_start..).collect();
                            // Strip trailing ASC/DESC from expr_toks if we truncated after it
                            let expr_len = cut_end - expr_start;
                            let expr = render(&expr_toks[..expr_len]).trim().to_string();
                            let rep = if is_last {
                                format!("CASE WHEN {e} IS NULL THEN 1 ELSE 0 END, {e} {d}", e = expr, d = dir)
                            } else {
                                format!("CASE WHEN {e} IS NULL THEN 0 ELSE 1 END, {e} {d}", e = expr, d = dir)
                            };
                            out.extend(tokenize(&rep));
                            i = next_idx + 1;
                            continue;
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

/// Rewrite `LIMIT n [OFFSET m]` / `OFFSET m LIMIT n` for a single SELECT statement (recursing into parenthesized subqueries).
fn rewrite_limit_in_segment(seg: Vec<Tok>) -> Vec<Tok> {
    // First recursively rewrite any parenthesized subqueries inside `seg`
    let mut toks = Vec::with_capacity(seg.len());
    let mut i = 0;
    while i < seg.len() {
        if matches!(seg[i], Tok::Punct('(')) {
            if let Some(close) = match_paren(&seg, i) {
                toks.push(seg[i].clone());
                let inner = rewrite_limit_in_segment(seg[i + 1..close].to_vec());
                toks.extend(inner);
                toks.push(seg[close].clone());
                i = close + 1;
                continue;
            }
        }
        toks.push(seg[i].clone());
        i += 1;
    }

    // Scan top-level (depth 0) tokens of this segment
    let mut depth = 0usize;
    let mut first_select: Option<usize> = None;
    let mut has_order_by = false;
    let mut limit_idx: Option<usize> = None;
    let mut offset_idx: Option<usize> = None;

    for (idx, t) in toks.iter().enumerate() {
        match t {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                if first_select.is_none() && t.is_word("select") {
                    first_select = Some(idx);
                }
                if t.is_word("order") {
                    if let Some(n) = next_sig(&toks, idx + 1) {
                        if toks[n].is_word("by") {
                            has_order_by = true;
                        }
                    }
                }
                if t.is_word("limit") {
                    limit_idx = Some(idx);
                }
                if t.is_word("offset") {
                    // Skip if already T-SQL `OFFSET n ROWS`
                    let is_tsql = next_sig(&toks, idx + 1)
                        .and_then(|v| next_sig(&toks, v + 1))
                        .map(|r| toks[r].is_word("rows") || toks[r].is_word("row"))
                        .unwrap_or(false);
                    if !is_tsql {
                        offset_idx = Some(idx);
                    }
                }
            }
            _ => {}
        }
    }

    if limit_idx.is_none() && offset_idx.is_none() {
        return toks;
    }

    // Parse limit value and offset value (numeric literal or simple expression)
    let lim_val = limit_idx.and_then(|li| next_sig(&toks, li + 1)).map(|vi| (vi, toks[vi].as_str()));
    let off_val = offset_idx.and_then(|oi| next_sig(&toks, oi + 1)).map(|vi| (vi, toks[vi].as_str()));

    let cut_start = match (limit_idx, offset_idx) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return toks,
    };
    let cut_end = match (&lim_val, &off_val) {
        (Some((a, _)), Some((b, _))) => (*a).max(*b),
        (Some((a, _)), None) => *a,
        (None, Some((b, _))) => *b,
        (None, None) => return toks,
    };

    let lim_str = lim_val.map(|(_, s)| s);
    let off_str = off_val.map(|(_, s)| s);
    let off_is_zero = off_str.as_deref().map(|s| s == "0").unwrap_or(true);

    // Strip the LIMIT / OFFSET clause from the tail
    let mut prefix = toks[..cut_start].to_vec();
    let suffix = toks[cut_end + 1..].to_vec();

    // Trim trailing spaces from prefix cleanly
    while matches!(prefix.last(), Some(Tok::Space(_))) {
        prefix.pop();
    }

    if has_order_by {
        let off = off_str.unwrap_or_else(|| "0".into());
        let clause = match lim_str {
            Some(l) => format!(" OFFSET {} ROWS FETCH NEXT {} ROWS ONLY", off, l),
            None => format!(" OFFSET {} ROWS", off),
        };
        prefix.extend(tokenize(&clause));
        prefix.extend(suffix);
        return prefix;
    }

    // No ORDER BY at top level:
    if let (Some(sel_i), Some(l), true) = (first_select, &lim_str, off_is_zero) {
        // Inject `TOP (n)` right after SELECT [DISTINCT|ALL]
        let mut insert_at = sel_i + 1;
        if let Some(nxt) = next_sig(&prefix, insert_at) {
            if prefix[nxt].is_word("distinct") || prefix[nxt].is_word("all") {
                insert_at = nxt + 1;
            }
        }
        let top_toks = tokenize(&format!(" TOP ({})", l));
        let mut out = Vec::with_capacity(prefix.len() + top_toks.len() + suffix.len());
        out.extend_from_slice(&prefix[..insert_at]);
        out.extend(top_toks);
        out.extend_from_slice(&prefix[insert_at..]);
        out.extend(suffix);
        return out;
    }

    // Has non-zero OFFSET without ORDER BY -> use dummy ORDER BY (SELECT NULL)
    let off = off_str.unwrap_or_else(|| "0".into());
    let clause = match lim_str {
        Some(l) => format!(" ORDER BY (SELECT NULL) OFFSET {} ROWS FETCH NEXT {} ROWS ONLY", off, l),
        None => format!(" ORDER BY (SELECT NULL) OFFSET {} ROWS", off),
    };
    prefix.extend(tokenize(&clause));
    prefix.extend(suffix);
    prefix
}

/// Split by top-level `;` so each statement's LIMIT/OFFSET is rewritten independently.
fn pass_limit_offset(toks: Vec<Tok>) -> Vec<Tok> {
    let mut out = Vec::with_capacity(toks.len());
    let mut start = 0usize;
    let mut depth = 0usize;
    for (i, t) in toks.iter().enumerate() {
        match t {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => depth = depth.saturating_sub(1),
            Tok::Punct(';') if depth == 0 => {
                out.extend(rewrite_limit_in_segment(toks[start..i].to_vec()));
                out.push(t.clone());
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < toks.len() {
        out.extend(rewrite_limit_in_segment(toks[start..].to_vec()));
    }
    out
}

/// Rewrite `INSERT INTO t [(cols)] VALUES (...) RETURNING cols` or `INSERT INTO t DEFAULT VALUES RETURNING cols`
/// into T-SQL `OUTPUT INSERTED.<col>` form.
fn pass_returning(toks: Vec<Tok>) -> Vec<Tok> {
    let mut out = Vec::with_capacity(toks.len());
    let mut start = 0usize;
    let mut depth = 0usize;
    for (i, t) in toks.iter().enumerate() {
        match t {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => depth = depth.saturating_sub(1),
            Tok::Punct(';') if depth == 0 => {
                out.extend(rewrite_returning_stmt(toks[start..i].to_vec()));
                out.push(t.clone());
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < toks.len() {
        out.extend(rewrite_returning_stmt(toks[start..].to_vec()));
    }
    out
}

fn rewrite_returning_stmt(toks: Vec<Tok>) -> Vec<Tok> {
    let Some(first) = next_sig(&toks, 0) else { return toks };
    if !toks[first].is_word("insert") {
        return toks;
    }
    let mut depth = 0usize;
    let mut values_idx: Option<usize> = None;
    let mut default_idx: Option<usize> = None;
    let mut ret_idx: Option<usize> = None;
    for (i, t) in toks.iter().enumerate() {
        match t {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                if values_idx.is_none() && t.is_word("values") {
                    values_idx = Some(i);
                } else if default_idx.is_none() && t.is_word("default") {
                    if let Some(n) = next_sig(&toks, i + 1) {
                        if toks[n].is_word("values") {
                            default_idx = Some(i);
                        }
                    }
                } else if t.is_word("returning") {
                    ret_idx = Some(i);
                }
            }
            _ => {}
        }
    }
    let Some(r_idx) = ret_idx else { return toks };
    let insert_at = default_idx.or(values_idx);
    let Some(ins_at) = insert_at else { return toks };
    let ret_expr = render(&toks[r_idx + 1..]).trim().to_string();
    if ret_expr.is_empty() {
        return toks;
    }
    let cols: Vec<String> = ret_expr
        .split(',')
        .map(|c| {
            let c = c.trim();
            if c == "*" {
                "INSERTED.*".to_string()
            } else if c.to_ascii_lowercase().starts_with("inserted.") {
                c.to_string()
            } else {
                format!("INSERTED.{}", c)
            }
        })
        .collect();
    let output_clause = format!("OUTPUT {} ", cols.join(", "));
    let mut prefix = toks[..ins_at].to_vec();
    let middle = toks[ins_at..r_idx].to_vec();
    prefix.extend(tokenize(&output_clause));
    prefix.extend(middle);
    while matches!(prefix.last(), Some(Tok::Space(_))) {
        prefix.pop();
    }
    prefix
}

/// Prefix single-quoted literals containing non-ASCII characters with `N` so
/// SQL Server stores Unicode text without codepage loss.
fn pass_unicode_strings(toks: Vec<Tok>) -> Vec<Tok> {
    toks.into_iter()
        .map(|t| match t {
            Tok::Str(s) if s.starts_with('\'') && !s.is_ascii() => Tok::Str(format!("N{}", s)),
            other => other,
        })
        .collect()
}

/// Rewrite a SQL string from PostgreSQL syntax to SQL Server (T-SQL) compatible SQL.
pub fn rewrite(sql: &str, utc: bool) -> String {
    let toks = tokenize(sql);
    let toks = pass_functions(toks, utc);
    let toks = pass_intervals(toks, utc);
    let toks = pass_is_bool(toks);
    let toks = pass_keywords(toks, utc);
    let toks = pass_casts(toks);
    let toks = pass_nulls_order(toks);
    let toks = pass_limit_offset(toks);
    let toks = pass_returning(toks);
    let toks = pass_unicode_strings(toks);
    render(&toks)
}

#[cfg(test)]
mod tests {
    use super::rewrite;

    #[test]
    fn ilike_casts_and_booleans() {
        assert_eq!(
            rewrite("select id, \"name\"::text as d from portal_user where is_active is not false and name ilike '%ali%'", false),
            "select id, CAST(\"name\" AS NVARCHAR(MAX)) as d from portal_user where (is_active IS NULL OR is_active = 1) and name LIKE '%ali%'"
        );
        assert_eq!(
            rewrite("select * from portal_email where email_sent is not true and active = true", false),
            "select * from portal_email where (email_sent IS NULL OR email_sent = 0) and active = 1"
        );
        assert_eq!(
            rewrite("select count(*) from portal_user where is_admin is true", false),
            "select count(*) from portal_user where (is_admin = 1)"
        );
        assert_eq!(
            rewrite("select * from t where u.id::text = d.v::text", false),
            "select * from t where CAST(u.id AS NVARCHAR(MAX)) = CAST(d.v AS NVARCHAR(MAX))"
        );
        assert_eq!(
            rewrite("select coalesce(a, 0)::numeric(10,2)::text, 'x::int true' as lit", false),
            "select CAST(CAST(coalesce(a, 0) AS DECIMAL(10,2)) AS NVARCHAR(MAX)), 'x::int true' as lit"
        );
    }

    #[test]
    fn now_and_intervals() {
        assert_eq!(
            rewrite("delete from portal_session where updated < now() - interval '12 hours'", false),
            "delete from portal_session where updated < DATEADD(hour, -12, GETDATE())"
        );
        assert_eq!(
            rewrite("select * from e where delivery_time <= now() and delivery_time > now() - interval '2 days'", true),
            "select * from e where delivery_time <= GETUTCDATE() and delivery_time > DATEADD(day, -2, GETUTCDATE())"
        );
        assert_eq!(
            rewrite("select current_date - interval '1 week'", false),
            "select DATEADD(day, -7, CAST(GETDATE() AS DATE))"
        );
        assert_eq!(
            rewrite("update portal_user set lastlogin=now() where id=1", false),
            "update portal_user set lastlogin=GETDATE() where id=1"
        );
    }

    #[test]
    fn limit_offset_and_nulls_last() {
        assert_eq!(
            rewrite("select * from portal_user order by name asc limit 10 offset 20", false),
            "select * from portal_user order by name asc OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY"
        );
        assert_eq!(
            rewrite("select * from portal_user where userid='a' order by id limit 1", false),
            "select * from portal_user where userid='a' order by id OFFSET 0 ROWS FETCH NEXT 1 ROWS ONLY"
        );
        assert_eq!(
            rewrite("select id from job_status where status='RUNNING' limit 1", false),
            "select TOP (1) id from job_status where status='RUNNING'"
        );
        assert_eq!(
            rewrite("select distinct role from user_role limit 5", false),
            "select distinct TOP (5) role from user_role"
        );
        assert_eq!(
            rewrite("select * from portal_tracker_field where tracker_id=1 order by field_order nulls last, id", false),
            "select * from portal_tracker_field where tracker_id=1 order by CASE WHEN field_order IS NULL THEN 1 ELSE 0 END, field_order ASC, id"
        );
        assert_eq!(
            rewrite("select id from portal_tree_node where tree_id=1 order by lft desc nulls last, id limit 1", false),
            "select id from portal_tree_node where tree_id=1 order by CASE WHEN lft IS NULL THEN 1 ELSE 0 END, lft DESC, id OFFSET 0 ROWS FETCH NEXT 1 ROWS ONLY"
        );
    }

    #[test]
    fn extract_date_trunc_to_char_string_agg_and_returning() {
        assert_eq!(
            rewrite("select * from t where extract(YEAR from created_at)=2026 and extract(MONTH from created_at)=5", false),
            "select * from t where DATEPART(year, created_at)=2026 and DATEPART(month, created_at)=5"
        );
        assert_eq!(
            rewrite("select date_trunc('month', dt), to_char(dt, 'YYYY-MM-DD'), string_agg(name, ',') from t", false),
            "select DATETIME2FROMPARTS(YEAR(dt), MONTH(dt), 1, 0, 0, 0, 0, 0), FORMAT(CAST(dt AS DATETIME2), 'yyyy-MM-dd'), STRING_AGG(CAST(name AS NVARCHAR(MAX)), ',') from t"
        );
        assert_eq!(
            rewrite("insert into \"t\" (\"a\",\"b\") values ('x',true) returning id", false),
            "insert into \"t\" (\"a\",\"b\") OUTPUT INSERTED.id values ('x',1)"
        );
        assert_eq!(
            rewrite("insert into \"t\" default values returning id", false),
            "insert into \"t\" OUTPUT INSERTED.id default values"
        );
    }

    #[test]
    fn unicode_literals_prefixed_with_n() {
        assert_eq!(
            rewrite("insert into t (a, b) values ('hello', 'Selamat pagi — café')", false),
            "insert into t (a, b) values ('hello', N'Selamat pagi — café')"
        );
        assert_eq!(
            rewrite("select N'already — prefixed' from t", false),
            "select N'already — prefixed' from t"
        );
    }

    #[test]
    fn comments_and_strings_untouched() {
        let q = "select 'now() ilike a::text limit 1' as s -- now() ilike true\n from t where x ilike 'foo'";
        assert_eq!(
            rewrite(q, false),
            "select 'now() ilike a::text limit 1' as s -- now() ilike true\n from t where x LIKE 'foo'"
        );
    }
}
