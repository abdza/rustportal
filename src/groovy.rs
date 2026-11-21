//! Render-time support for templates translated from GSP (see `gsp.rs`):
//! the Groovy/Java methods that pages call on strings, numbers, lists and
//! maps, Java number/date patterns, and Grails URL building. Everything here
//! is lenient: a call on null gives null instead of an error, as `?.` would.

use crate::db::{vs, Row};
use crate::web::{enc, esc};
use serde_json::{json, Value};

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().replace(',', "").parse::<f64>().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// A float as JSON, as an integer when it has no fraction.
fn jnum(f: f64) -> Value {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 9e15 { json!(f as i64) } else { json!(f) }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

pub fn compare(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (Value::Null, Value::Null) => Equal,
        (Value::Null, _) => Less,
        (_, Value::Null) => Greater,
        (Value::Number(_), _) | (_, Value::Number(_)) => match (num(a), num(b)) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Equal),
            _ => vs(a).cmp(&vs(b)),
        },
        _ => vs(a).cmp(&vs(b)),
    }
}

fn round_half_up(f: f64, places: i32) -> f64 {
    let m = 10f64.powi(places);
    let x = f * m;
    // nudge so that 2.675 style values round the way decimal arithmetic would
    (x + x.signum() * 1e-9).round() / m
}

/// Java regex replacement syntax (`$1`) to the regex crate's (`${1}`).
fn java_repl(r: &str) -> String {
    let mut o = String::new();
    let b: Vec<char> = r.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i] == '\\' && i + 1 < b.len() {
            if b[i + 1] == '$' {
                o.push_str("$$");
            } else {
                o.push(b[i + 1]);
            }
            i += 2;
        } else if b[i] == '$' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
            o.push_str(&format!("${{{}}}", b[i + 1]));
            i += 2;
        } else {
            o.push(b[i]);
            i += 1;
        }
    }
    o
}

fn char_slice(s: &str, from: usize, to: usize) -> String {
    s.chars().skip(from).take(to.saturating_sub(from)).collect()
}

/// Call a Groovy-style method. Unknown methods and calls on null give null.
pub fn method(recv: &Value, name: &str, a: &[Value]) -> Value {
    let a0 = a.first().cloned().unwrap_or(Value::Null);
    let a1 = a.get(1).cloned().unwrap_or(Value::Null);
    let ai = |n: usize| a.get(n).and_then(num).map(|f| f as i64).unwrap_or(0);
    // conversions and tests that also make sense on null
    match name {
        "toString" => return json!(vs(recv)),
        "isEmpty" => return json!(!truthy(recv)),
        "asBoolean" | "toBoolean" => return json!(match recv { Value::String(s) => matches!(s.trim().to_lowercase().as_str(), "true" | "y" | "1" | "yes"), v => truthy(v) }),
        "equals" => return json!(recv == &a0 || (!recv.is_null() && !a0.is_null() && vs(recv) == vs(&a0))),
        "isNumber" | "isBigDecimal" | "isDouble" | "isFloat" => return json!(num(recv).is_some() && !recv.is_boolean()),
        "isInteger" | "isLong" => return json!(match recv { Value::String(s) => s.trim().parse::<i64>().is_ok(), Value::Number(n) => n.is_i64() || n.is_u64(), _ => false }),
        "encodeAsHTML" => return json!(esc(&vs(recv))),
        "encodeAsURL" => return json!(enc(&vs(recv))),
        "encodeAsRaw" => return recv.clone(),
        "encodeAsJSON" => return json!(serde_json::to_string(recv).unwrap_or_default()),
        "encodeAsJavaScript" => return json!(vs(recv).replace('\\', "\\\\").replace('\'', "\\'").replace('"', "\\\"").replace('\n', "\\n").replace('\r', "")),
        _ => {}
    }
    if recv.is_null() {
        return match name {
            "size" | "length" | "count" => json!(0),
            _ => Value::Null,
        };
    }
    // numeric conversions on anything
    match name {
        "toInteger" | "toLong" | "intValue" | "longValue" | "toBigInteger" => return num(recv).map(|f| json!(f.trunc() as i64)).unwrap_or(Value::Null),
        "toBigDecimal" | "toDouble" | "toFloat" | "doubleValue" | "floatValue" => return num(recv).map(jnum).unwrap_or(Value::Null),
        "compareTo" => return json!(compare(recv, &a0) as i32),
        _ => {}
    }
    match recv {
        Value::Array(l) => match name {
            "size" | "length" => json!(l.len()),
            "count" => json!(l.iter().filter(|x| **x == a0).count()),
            "contains" => json!(l.iter().any(|x| *x == a0 || vs(x) == vs(&a0))),
            "join" => json!(l.iter().map(vs).collect::<Vec<_>>().join(&vs(&a0))),
            "first" | "head" => l.first().cloned().unwrap_or(Value::Null),
            "last" => l.last().cloned().unwrap_or(Value::Null),
            "tail" => json!(l.iter().skip(1).collect::<Vec<_>>()),
            "take" => json!(l.iter().take(ai(0).max(0) as usize).collect::<Vec<_>>()),
            "drop" => json!(l.iter().skip(ai(0).max(0) as usize).collect::<Vec<_>>()),
            "get" | "getAt" => {
                let n = ai(0);
                let ix = if n < 0 { l.len() as i64 + n } else { n };
                l.get(ix.max(0) as usize).cloned().unwrap_or(Value::Null)
            }
            "sum" => {
                if l.iter().all(|x| num(x).is_some()) { jnum(l.iter().filter_map(num).sum()) } else { json!(l.iter().map(vs).collect::<String>()) }
            }
            "max" => l.iter().max_by(|x, y| compare(x, y)).cloned().unwrap_or(Value::Null),
            "min" => l.iter().min_by(|x, y| compare(x, y)).cloned().unwrap_or(Value::Null),
            "unique" | "toSet" => {
                let mut o: Vec<Value> = vec![];
                for x in l {
                    if !o.contains(x) {
                        o.push(x.clone());
                    }
                }
                json!(o)
            }
            "sort" => {
                let mut o = l.clone();
                o.sort_by(compare);
                json!(o)
            }
            "reverse" => json!(l.iter().rev().collect::<Vec<_>>()),
            "indexOf" => json!(l.iter().position(|x| *x == a0).map(|x| x as i64).unwrap_or(-1)),
            "toList" => recv.clone(),
            "plus" => match &a0 {
                Value::Array(o) => json!(l.iter().chain(o.iter()).collect::<Vec<_>>()),
                x => json!(l.iter().chain(std::iter::once(x)).collect::<Vec<_>>()),
            },
            "minus" => match &a0 {
                Value::Array(o) => json!(l.iter().filter(|x| !o.contains(x)).collect::<Vec<_>>()),
                x => json!(l.iter().filter(|y| *y != x).collect::<Vec<_>>()),
            },
            "flatten" => {
                fn flat(l: &[Value], o: &mut Vec<Value>) {
                    for x in l {
                        match x {
                            Value::Array(i) => flat(i, o),
                            x => o.push(x.clone()),
                        }
                    }
                }
                let mut o = vec![];
                flat(l, &mut o);
                json!(o)
            }
            _ => Value::Null,
        },
        Value::Object(m) => match name {
            "size" | "length" => json!(m.len()),
            "get" | "getAt" => m.get(&vs(&a0)).cloned().filter(|v| !v.is_null()).unwrap_or(a1),
            "containsKey" => json!(m.contains_key(&vs(&a0))),
            "containsValue" => json!(m.values().any(|v| *v == a0)),
            "keySet" | "keys" => json!(m.keys().collect::<Vec<_>>()),
            "values" => json!(m.values().collect::<Vec<_>>()),
            "entrySet" | "items" => json!(m.iter().map(|(k, v)| json!({"key": k, "value": v})).collect::<Vec<_>>()),
            "subMap" => {
                let keys: Vec<String> = match &a0 {
                    Value::Array(k) => k.iter().map(vs).collect(),
                    _ => a.iter().map(vs).collect(),
                };
                let mut o = Row::new();
                for k in keys {
                    if let Some(v) = m.get(&k) {
                        o.insert(k, v.clone());
                    }
                }
                Value::Object(o)
            }
            "plus" => {
                let mut o = m.clone();
                if let Value::Object(x) = &a0 {
                    for (k, v) in x {
                        o.insert(k.clone(), v.clone());
                    }
                }
                Value::Object(o)
            }
            "int" | "long" => m.get(&vs(&a0)).and_then(num).map(|f| json!(f as i64)).unwrap_or(a1),
            "float" | "double" => m.get(&vs(&a0)).and_then(num).map(jnum).unwrap_or(a1),
            "boolean" => json!(m.get(&vs(&a0)).map(truthy).unwrap_or(false)),
            "list" => match m.get(&vs(&a0)) {
                Some(Value::Array(l)) => json!(l),
                Some(Value::Null) | None => json!([]),
                Some(x) => json!([x]),
            },
            // a property that happens to be called like a method (user.name())
            _ => m.get(name).cloned().unwrap_or(Value::Null),
        },
        Value::Number(_) | Value::Bool(_) if !matches!(name, "format" | "padLeft" | "padRight" | "size" | "length" | "contains" | "trim" | "take" | "replace" | "replaceAll" | "startsWith" | "endsWith" | "toLowerCase" | "toUpperCase" | "substring" | "split" | "tokenize") => {
            let f = num(recv).unwrap_or(0.0);
            match name {
                "round" => {
                    if a.is_empty() { json!(round_half_up(f, 0) as i64) } else { jnum(round_half_up(f, ai(0) as i32)) }
                }
                "setScale" => jnum(round_half_up(f, ai(0) as i32)),
                "ceil" => json!(f.ceil() as i64),
                "floor" => json!(f.floor() as i64),
                "abs" => jnum(f.abs()),
                "intdiv" => num(&a0).filter(|d| *d != 0.0).map(|d| json!((f / d).trunc() as i64)).unwrap_or(Value::Null),
                "plus" => num(&a0).map(|d| jnum(f + d)).unwrap_or(Value::Null),
                "minus" => num(&a0).map(|d| jnum(f - d)).unwrap_or(Value::Null),
                "multiply" => num(&a0).map(|d| jnum(f * d)).unwrap_or(Value::Null),
                "div" => num(&a0).filter(|d| *d != 0.0).map(|d| jnum(f / d)).unwrap_or(Value::Null),
                "max" => num(&a0).map(|d| jnum(f.max(d))).unwrap_or(recv.clone()),
                "min" => num(&a0).map(|d| jnum(f.min(d))).unwrap_or(recv.clone()),
                "toPlainString" | "stripTrailingZeros" => {
                    let s = format!("{}", f);
                    if name == "toPlainString" { json!(s) } else { jnum(f) }
                }
                "scale" => json!(vs(recv).split_once('.').map(|(_, d)| d.len()).unwrap_or(0)),
                "isNaN" => json!(f.is_nan()),
                _ => Value::Null,
            }
        }
        _ => {
            let s = vs(recv);
            match name {
                "size" | "length" => json!(s.chars().count()),
                "trim" | "strip" => json!(s.trim()),
                "toLowerCase" => json!(s.to_lowercase()),
                "toUpperCase" => json!(s.to_uppercase()),
                "capitalize" => {
                    let mut c = s.chars();
                    json!(c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default())
                }
                "contains" => json!(s.contains(&vs(&a0))),
                "startsWith" => json!(s.starts_with(&vs(&a0))),
                "endsWith" => json!(s.ends_with(&vs(&a0))),
                "equalsIgnoreCase" => json!(s.to_lowercase() == vs(&a0).to_lowercase()),
                "replace" => json!(s.replace(&vs(&a0), &vs(&a1))),
                "replaceAll" | "replaceFirst" => match regex::Regex::new(&vs(&a0)) {
                    Ok(re) => {
                        let r = java_repl(&vs(&a1));
                        json!(if name == "replaceAll" { re.replace_all(&s, r.as_str()).to_string() } else { re.replace(&s, r.as_str()).to_string() })
                    }
                    Err(_) => json!(s),
                },
                "matches" => json!(regex::Regex::new(&format!("^(?:{})$", vs(&a0))).map(|re| re.is_match(&s)).unwrap_or(false)),
                "split" => match regex::Regex::new(&vs(&a0)) {
                    Ok(re) => json!(re.split(&s).collect::<Vec<_>>()),
                    Err(_) => json!(s.split(&vs(&a0)).collect::<Vec<_>>()),
                },
                "tokenize" => {
                    let d = if a.is_empty() { " \t\n\r".to_string() } else { vs(&a0) };
                    json!(s.split(|c| d.contains(c)).filter(|x| !x.is_empty()).collect::<Vec<_>>())
                }
                "readLines" => json!(s.lines().collect::<Vec<_>>()),
                "toList" => json!(s.chars().map(|c| c.to_string()).collect::<Vec<_>>()),
                "substring" => {
                    let n = s.chars().count();
                    json!(char_slice(&s, ai(0).max(0) as usize, if a.len() > 1 { (ai(1).max(0) as usize).min(n) } else { n }))
                }
                "take" => json!(char_slice(&s, 0, ai(0).max(0) as usize)),
                "drop" => json!(s.chars().skip(ai(0).max(0) as usize).collect::<String>()),
                "getAt" => {
                    let n = ai(0);
                    let len = s.chars().count() as i64;
                    let ix = if n < 0 { len + n } else { n };
                    json!(s.chars().nth(ix.max(0) as usize).map(|c| c.to_string()).unwrap_or_default())
                }
                "indexOf" => json!(s.find(&vs(&a0)).map(|b| s[..b].chars().count() as i64).unwrap_or(-1)),
                "lastIndexOf" => json!(s.rfind(&vs(&a0)).map(|b| s[..b].chars().count() as i64).unwrap_or(-1)),
                "padLeft" | "padRight" => {
                    let w = ai(0).max(0) as usize;
                    let pad = if a.len() > 1 { vs(&a1) } else { " ".into() };
                    let n = s.chars().count();
                    let fill: String = pad.chars().cycle().take(w.saturating_sub(n)).collect();
                    json!(if name == "padLeft" { format!("{}{}", fill, s) } else { format!("{}{}", s, fill) })
                }
                "reverse" => json!(s.chars().rev().collect::<String>()),
                "plus" => json!(format!("{}{}", s, vs(&a0))),
                "normalize" => json!(s),
                "count" => json!(if vs(&a0).is_empty() { 0 } else { s.matches(&vs(&a0)).count() }),
                // dates are text here: "2026-01-31 10:00:00"
                "format" => json!(java_date(&s, &vs(&a0))),
                "clearTime" => json!(match crate::script::parse_dt(&s) { Some(d) => d.date().format("%Y-%m-%d 00:00:00").to_string(), None => s }),
                "getTime" => crate::script::parse_dt(&s)
                    .and_then(|d| d.and_local_timezone(chrono::Local).earliest())
                    .map(|d| json!(d.timestamp_millis()))
                    .unwrap_or(Value::Null),
                "getYear" => crate::script::parse_dt(&s).map(|d| json!(d.format("%Y").to_string().parse::<i64>().unwrap_or(0) - 1900)).unwrap_or(Value::Null),
                "getMonth" => crate::script::parse_dt(&s).map(|d| json!(d.format("%m").to_string().parse::<i64>().unwrap_or(1) - 1)).unwrap_or(Value::Null),
                "getDate" => crate::script::parse_dt(&s).map(|d| json!(d.format("%d").to_string().parse::<i64>().unwrap_or(0))).unwrap_or(Value::Null),
                // numeric methods on numeric text
                "round" | "abs" | "ceil" | "floor" | "setScale" | "intdiv" | "multiply" | "minus" | "div" | "max" | "min" => match num(recv) {
                    Some(f) => method(&jnum(f), name, a),
                    None => Value::Null,
                },
                _ => Value::Null,
            }
        }
    }
}

/// Format with a Java `SimpleDateFormat` pattern. Text that is not a date is returned unchanged.
pub fn java_date(value: &str, pattern: &str) -> String {
    let Some(d) = crate::script::parse_dt(value) else { return value.to_string() };
    let b: Vec<char> = pattern.chars().collect();
    let mut f = String::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == '\'' {
            i += 1;
            if b.get(i) == Some(&'\'') {
                f.push('\'');
                i += 1;
                continue;
            }
            while i < b.len() && b[i] != '\'' {
                if b[i] == '%' {
                    f.push('%');
                }
                f.push(b[i]);
                i += 1;
            }
            i += 1;
            continue;
        }
        if !c.is_ascii_alphabetic() {
            if c == '%' {
                f.push('%');
            }
            f.push(c);
            i += 1;
            continue;
        }
        let mut n = 1;
        while i + n < b.len() && b[i + n] == c {
            n += 1;
        }
        f.push_str(match (c, n) {
            ('y', 2) => "%y",
            ('y', _) | ('Y', _) => "%Y",
            ('M', 1) => "%-m",
            ('M', 2) => "%m",
            ('M', 3) => "%b",
            ('M', _) => "%B",
            ('d', 1) => "%-d",
            ('d', _) => "%d",
            ('H', 1) => "%-H",
            ('H', _) => "%H",
            ('h', 1) => "%-I",
            ('h', _) => "%I",
            ('m', 1) => "%-M",
            ('m', _) => "%M",
            ('s', 1) => "%-S",
            ('s', _) => "%S",
            ('S', _) => "%3f",
            ('a', _) => "%p",
            ('E', 4) => "%A",
            ('E', _) => "%a",
            ('D', _) => "%j",
            ('w', _) | ('W', _) => "%V",
            ('u', _) => "%u",
            ('z', _) | ('Z', _) | ('X', _) => "",
            _ => "",
        });
        i += n;
    }
    d.format(&f).to_string()
}

/// Format with a Java `DecimalFormat` pattern such as `#,##0.00`. Null gives an empty string.
pub fn number_format(v: &Value, pattern: &str) -> String {
    let Some(mut f) = num(v) else { return if v.is_null() { String::new() } else { vs(v) } };
    let is_pat = |c: char| matches!(c, '#' | '0' | ',' | '.');
    let start = pattern.find(is_pat).unwrap_or(pattern.len());
    let end = pattern.rfind(is_pat).map(|x| x + 1).unwrap_or(pattern.len());
    let (prefix, body, suffix) = (&pattern[..start], &pattern[start..end.max(start)], &pattern[end.max(start)..]);
    if suffix.contains('%') {
        f *= 100.0;
    }
    let (ip, fp) = body.split_once('.').unwrap_or((body, ""));
    let min_frac = fp.chars().filter(|c| *c == '0').count();
    let max_frac = fp.chars().filter(|c| matches!(c, '0' | '#')).count();
    let min_int = ip.chars().filter(|c| *c == '0').count();
    let grouping = ip.contains(',');
    let r = round_half_up(f.abs(), max_frac as i32);
    let raw = format!("{:.*}", max_frac, r);
    let (int, mut frac) = raw.split_once('.').map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((raw.clone(), String::new()));
    while frac.len() > min_frac && frac.ends_with('0') {
        frac.pop();
    }
    let mut int = if int == "0" && min_int == 0 && !frac.is_empty() { String::new() } else { int };
    while int.len() < min_int {
        int.insert(0, '0');
    }
    let mut out = String::new();
    for (n, c) in int.chars().enumerate() {
        if grouping && n > 0 && (int.len() - n) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if !frac.is_empty() {
        out.push('.');
        out.push_str(&frac);
    }
    let neg = f < 0.0 && out.chars().any(|c| c.is_ascii_digit() && c != '0');
    format!("{}{}{}{}", if neg { "-" } else { "" }, prefix.replace('\'', ""), out, suffix.replace('\'', ""))
}

/// `String.format` for the common conversions: %s %d %f %.2f %,d %,.2f %05d %x %%.
pub fn sprintf(fmt: &str, args: &[Value]) -> String {
    let b: Vec<char> = fmt.chars().collect();
    let mut o = String::new();
    let (mut i, mut n) = (0, 0);
    while i < b.len() {
        if b[i] != '%' {
            o.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        let (mut comma, mut zero, mut left) = (false, false, false);
        let (mut width, mut prec) = (String::new(), None::<usize>);
        while i < b.len() && matches!(b[i], ',' | '0' | '-' | '+' | ' ') && !(b[i] == '0' && !width.is_empty()) {
            match b[i] {
                ',' => comma = true,
                '0' => zero = true,
                '-' => left = true,
                _ => {}
            }
            i += 1;
        }
        while i < b.len() && b[i].is_ascii_digit() {
            width.push(b[i]);
            i += 1;
        }
        if i < b.len() && b[i] == '.' {
            i += 1;
            let mut p = String::new();
            while i < b.len() && b[i].is_ascii_digit() {
                p.push(b[i]);
                i += 1;
            }
            prec = p.parse().ok();
        }
        let Some(conv) = b.get(i).copied() else { break };
        i += 1;
        if conv == '%' {
            o.push('%');
            continue;
        }
        if conv == 'n' {
            o.push('\n');
            continue;
        }
        let v = args.get(n).cloned().unwrap_or(Value::Null);
        n += 1;
        let mut s = match conv {
            'd' => {
                let x = num(&v).unwrap_or(0.0).trunc();
                if comma { number_format(&json!(x), "#,##0") } else { format!("{}", x as i64) }
            }
            'f' => {
                let p = prec.unwrap_or(6);
                let x = num(&v).unwrap_or(0.0);
                if comma { number_format(&json!(x), &format!("#,##0.{}", "0".repeat(p))) } else { format!("{:.*}", p, round_half_up(x, p as i32)) }
            }
            'x' => format!("{:x}", num(&v).unwrap_or(0.0) as i64),
            'X' => format!("{:X}", num(&v).unwrap_or(0.0) as i64),
            _ => {
                let t = vs(&v);
                match prec {
                    Some(p) => t.chars().take(p).collect(),
                    None => t,
                }
            }
        };
        let w: usize = width.parse().unwrap_or(0);
        while s.chars().count() < w {
            if left {
                s.push(' ');
            } else {
                s.insert(0, if zero { '0' } else { ' ' });
            }
        }
        o.push_str(&s);
    }
    o
}

pub fn human_size(n: f64) -> String {
    let mut v = n;
    for u in ["B", "KB", "MB", "GB"] {
        if v < 1024.0 {
            return if u == "B" { format!("{} B", n as i64) } else { format!("{:.1} {}", v, u) };
        }
        v /= 1024.0;
    }
    format!("{:.1} TB", v)
}

/// The URL this portal uses for a Grails `controller`/`action`/`id`/`params` link.
pub fn grails_url(controller: &str, action: &str, id: &str, params: &Row) -> String {
    let c = controller.to_lowercase();
    let a = action.to_string();
    let mut p = params.clone();
    let mut take = |k: &str| p.remove(k).map(|v| vs(&v)).filter(|v| !v.is_empty());
    let id = if id.is_empty() { None } else { Some(id.to_string()) };
    let path = match (c.as_str(), a.as_str()) {
        ("portalpage", "display" | "runpage" | "index" | "") => {
            let (m, s) = (take("module").unwrap_or_else(|| "portal".into()), take("slug").unwrap_or_default());
            let mut u = format!("/{}/{}/{}", if a == "runpage" { "run" } else { "view" }, enc(&m), enc(&s));
            if let Some(x) = id.clone().or_else(|| take("arg1")) {
                u.push('/');
                u.push_str(&enc(&x));
            }
            u
        }
        ("portaltracker", "list" | "create_data" | "display_data" | "edit_data" | "transition" | "excel" | "delete_data") => {
            let (m, s) = (take("module").unwrap_or_else(|| "portal".into()), take("slug").unwrap_or_default());
            let rid = id.clone().or_else(|| take("id"));
            let base = format!("/{}/{}", enc(&m), enc(&s));
            match a.as_str() {
                "list" => format!("{}/list", base),
                "excel" => {
                    p.insert("excel".into(), json!("1"));
                    format!("{}/list", base)
                }
                "create_data" => format!("{}/create", base),
                "display_data" => format!("{}/display/{}", base, rid.unwrap_or_default()),
                "edit_data" => format!("{}/edit/{}", base, rid.unwrap_or_default()),
                "delete_data" => format!("{}/delete/{}", base, rid.unwrap_or_default()),
                _ => {
                    let t = take("transition").unwrap_or_default().trim().replace(' ', "_").to_lowercase();
                    match rid {
                        Some(r) => format!("{}/{}/{}", base, t, r),
                        None => format!("{}/{}", base, t),
                    }
                }
            }
        }
        ("filelink", "download" | "stream") => match id.clone().or_else(|| take("id")) {
            Some(x) => format!("/fileLink/get/{}", enc(&x)),
            None => {
                let (m, s) = (take("module").unwrap_or_else(|| "portal".into()), take("slug").unwrap_or_default());
                format!("/download/{}/{}", enc(&m), enc(&s))
            }
        },
        _ => {
            let mut u = format!("/{}", if controller.is_empty() { "portalPage".to_string() } else { controller[..1].to_lowercase() + &controller[1..] });
            if !a.is_empty() || id.is_some() {
                u.push('/');
                u.push_str(if a.is_empty() { "index" } else { &a });
            }
            if let Some(x) = id {
                u.push('/');
                u.push_str(&enc(&x));
            }
            u
        }
    };
    let q = crate::web::qs(&p, &[]);
    if q.is_empty() { path } else { format!("{}?{}", path, q) }
}

/// `<g:select>`: options from a list (of scalars or rows) or a map.
pub fn select_html(kw: &Row) -> String {
    let name = kw.get("name").map(vs).unwrap_or_default();
    let cur: Vec<String> = match kw.get("value") {
        Some(Value::Array(a)) => a.iter().map(vs).collect(),
        Some(Value::Null) | None => vec![],
        Some(v) => vec![vs(v)],
    };
    let (okey, oval) = (kw.get("optionKey").map(vs).unwrap_or_default(), kw.get("optionValue").map(vs).unwrap_or_default());
    let mut pairs: Vec<(String, String)> = vec![];
    if let Some(Value::Object(ns)) = kw.get("noSelection") {
        for (k, v) in ns {
            pairs.push((if k == "null" { String::new() } else { k.clone() }, vs(v)));
        }
    }
    let keys: Option<Vec<String>> = match kw.get("keys") {
        Some(Value::Array(k)) => Some(k.iter().map(vs).collect()),
        _ => None,
    };
    match kw.get("options") {
        Some(Value::Array(l)) => {
            for (n, x) in l.iter().enumerate() {
                match x {
                    Value::Object(r) => {
                        let k = if okey.is_empty() { r.get("id").map(vs).unwrap_or_default() } else { r.get(&okey).map(vs).unwrap_or_default() };
                        let v = if oval.is_empty() { r.get("name").map(vs).unwrap_or_else(|| k.clone()) } else { r.get(&oval).map(vs).unwrap_or_default() };
                        pairs.push((k, v));
                    }
                    x => pairs.push((keys.as_ref().and_then(|k| k.get(n).cloned()).unwrap_or_else(|| vs(x)), vs(x))),
                }
            }
        }
        Some(Value::Object(m)) => {
            for (k, v) in m {
                pairs.push((k.clone(), vs(v)));
            }
        }
        _ => {}
    }
    let mut h = format!("<select name=\"{}\"", esc(&name));
    let mut has_id = false;
    if let Some(Value::Object(at)) = kw.get("attrs") {
        for (k, v) in at {
            has_id |= k == "id";
            if matches!(k.as_str(), "required" | "disabled" | "readonly") {
                if truthy(v) && vs(v) != "false" {
                    h.push_str(&format!(" {}", k));
                }
            } else {
                h.push_str(&format!(" {}=\"{}\"", esc(k), esc(&vs(v))));
            }
        }
    }
    if !has_id {
        h.push_str(&format!(" id=\"{}\"", esc(&name)));
    }
    if kw.get("multiple").map(|v| truthy(v) && vs(v) != "false").unwrap_or(false) {
        h.push_str(" multiple");
    }
    h.push('>');
    for (k, v) in pairs {
        h.push_str(&format!("<option value=\"{}\"{}>{}</option>", esc(&k), if cur.contains(&k) { " selected" } else { "" }, esc(&v)));
    }
    h.push_str("</select>");
    h
}

/// A value as Java's toString() writes it (what a Grails page prints for a
/// returned value): lists as [a, b], maps as {k=v}, null as null.
pub fn to_groovy_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::String(s) => s.clone(),
        Value::Array(a) => format!("[{}]", a.iter().map(to_groovy_string).collect::<Vec<_>>().join(", ")),
        Value::Object(m) => format!("{{{}}}", m.iter().map(|(k, x)| format!("{}={}", k, to_groovy_string(x))).collect::<Vec<_>>().join(", ")),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod groovy_string_tests {
    use super::*;
    #[test]
    fn groovy_strings() {
        assert_eq!(to_groovy_string(&serde_json::json!([])), "[]");
        assert_eq!(to_groovy_string(&serde_json::json!({})), "{}");
        assert_eq!(to_groovy_string(&serde_json::json!([{"a": 1, "b": "x"}, null])), "[{a=1, b=x}, null]");
    }
}
