//! GSP -> MiniJinja translator, used when importing a module exported by the
//! Grails portal. Template pages (`content_*.gsp` of non-runable pages) are
//! rewritten tag by tag and expression by expression; anything that has no
//! equivalent is left as a `{# GSP-TODO ... #}` comment and reported.
//!
//! Groovy method calls (`x.toInteger()`, `s.trim()`, `list.size()` ...) are
//! kept as they are: `script.rs` installs a method fallback that implements
//! them at render time, null-safe like `?.`.

use std::collections::{HashMap, HashSet};

pub struct Translated {
    pub text: String,
    pub warnings: Vec<String>,
}

/// Does this page source look like GSP/Groovy rather than a MiniJinja template?
pub fn looks_like_gsp(src: &str) -> bool {
    src.contains("<g:") || src.contains("<%") || src.contains("${") || src.contains("<asset:")
}

pub fn translate(src: &str) -> Translated {
    let src = &resolve_class_aliases(src);
    // pass 1 finds the variables that are changed inside a loop (they need a namespace)
    let mut a = Tr::new(HashSet::new());
    a.run(src);
    let ns = a.loop_assigned();
    let mut b = Tr::new(ns);
    b.run(src);
    let mut text = b.out;
    if !b.ns.is_empty() {
        text = format!("{{% set ns = namespace() %}}{}", text);
    }
    let mut seen = HashSet::new();
    b.warn.retain(|w| seen.insert(w.clone()));
    Translated { text, warnings: b.warn }
}

/// `def TH = g6portal.PortalTracker` and then `TH.raw_rows(...)`: drop the alias
/// line and write the class name where the alias was used, so the calls are
/// recognised like any other.
fn resolve_class_aliases(src: &str) -> String {
    let mut aliases: Vec<(String, String)> = vec![];
    let mut kept = String::with_capacity(src.len());
    for line in src.split_inclusive('\n') {
        let t = line.trim();
        let alias = t.strip_prefix("def ").and_then(|r| r.split_once('=')).and_then(|(name, val)| {
            let (name, val) = (name.trim(), val.trim().trim_end_matches(';').trim());
            let class = val.strip_prefix("g6portal.").or_else(|| val.strip_prefix("g5portal."))?;
            let ok = |x: &str| !x.is_empty() && x.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            (ok(name) && ok(class) && class.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)).then(|| (name.to_string(), class.to_string()))
        });
        match alias {
            Some(a) => aliases.push(a),
            None => kept.push_str(line),
        }
    }
    if aliases.is_empty() {
        return src.to_string();
    }
    let mut out = kept;
    for (name, class) in aliases {
        let mut res = String::with_capacity(out.len());
        let pat = format!("{}.", name);
        let bytes: Vec<char> = out.chars().collect();
        let mut i = 0;
        while i < bytes.len() {
            let rest: String = bytes[i..(i + pat.len()).min(bytes.len())].iter().collect();
            let before_ok = i == 0 || !(bytes[i - 1].is_alphanumeric() || bytes[i - 1] == '_' || bytes[i - 1] == '.');
            if rest == pat && before_ok {
                res.push_str(&class);
                res.push('.');
                i += pat.len();
            } else {
                res.push(bytes[i]);
                i += 1;
            }
        }
        out = res;
    }
    out
}

/// Translate one Groovy expression (a page data source query) into a MiniJinja template producing the same text.
pub fn translate_expr(src: &str) -> Translated {
    let mut t = Tr::new(HashSet::new());
    let x = t.src_expr(src, false);
    Translated { text: format!("{{{{ {} }}}}", x), warnings: t.warn }
}

/// Grails pages carry their own `<html><head><body>` for the layout engine; here
/// the page is placed inside the portal layout, so only the contents are kept.
pub fn strip_layout(html: &str) -> String {
    let mut t = html.to_string();
    for pat in [r"(?is)<!DOCTYPE[^>]*>\s*", r"(?is)<meta\s+name=.layout.[^>]*>\s*", r"(?is)<title>.*?</title>\s*", r"(?is)</?html[^>]*>\s*", r"(?is)</?head>\s*", r"(?is)</?body[^>]*>\s*"] {
        t = regex::Regex::new(pat).unwrap().replace_all(&t, "").to_string();
    }
    t
}

/// Comment out a Groovy script so the page still loads; `fail` makes it stop with a clear message when run.
pub fn stub_script(src: &str, what: &str, fail: bool) -> String {
    let mut o = format!("// NOT PORTED: this {} is still Groovy from the Grails portal. Rewrite it in Rhai below.\n", what);
    if fail {
        o.push_str(&format!("throw \"This {} has not been ported from Groovy to Rhai yet\";\n", what));
    }
    o.push_str("// ---- original Groovy ----\n");
    for l in src.lines() {
        o.push_str("// ");
        o.push_str(l);
        o.push('\n');
    }
    o
}

// ====================================================================
// Groovy lexer
// ====================================================================

#[derive(Clone, Debug, PartialEq)]
enum T {
    Id(String),
    Num(String),
    Str(String),
    GStr(String),
    Op(&'static str),
    Eof,
}

#[derive(Clone, Debug)]
struct Tok {
    t: T,
    nl: bool,
    pos: usize,
}

const OPS: &[&str] = &[
    "<=>", "==~", "===", "!==", "..<", "?.", "*.", "?:", "?[", "=~", "==", "!=", "<=", ">=", "&&", "||", "++", "--", "+=", "-=", "*=", "/=", "->", "<<", "..",
    "**", "+", "-", "*", "/", "%", "<", ">", "=", "!", "?", ":", ".", ",", "(", ")", "[", "]", "{", "}", ";",
];

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = vec![];
    let mut nl = false;
    while i < b.len() {
        let c = b[i];
        if c == '\n' {
            nl = true;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < b.len() && b[i + 1] == '/' {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < b.len() && b[i + 1] == '*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        let pos = i;
        if c.is_alphabetic() || c == '_' {
            let mut s = String::new();
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                s.push(b[i]);
                i += 1;
            }
            out.push(Tok { t: T::Id(s), nl, pos });
            nl = false;
            continue;
        }
        if c.is_ascii_digit() {
            let mut s = String::new();
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == '_' || (b[i] == '.' && i + 1 < b.len() && b[i + 1].is_ascii_digit())) {
                if b[i] != '_' {
                    s.push(b[i]);
                }
                i += 1;
            }
            // type suffixes: 1L, 2.5G, 0.0d
            if i < b.len() && "LlGgDdFf".contains(b[i]) && !(i + 1 < b.len() && (b[i + 1].is_alphanumeric() || b[i + 1] == '_')) {
                i += 1;
            }
            out.push(Tok { t: T::Num(s), nl, pos });
            nl = false;
            continue;
        }
        if c == '\'' || c == '"' {
            let triple = i + 2 < b.len() && b[i + 1] == c && b[i + 2] == c;
            let q = if triple { 3 } else { 1 };
            i += q;
            let mut s = String::new();
            let mut closed = false;
            while i < b.len() {
                if b[i] == '\\' && i + 1 < b.len() {
                    if c == '"' {
                        // keep escapes for the interpolation pass
                        s.push('\\');
                        s.push(b[i + 1]);
                    } else {
                        s.push(unescape(b[i + 1]));
                    }
                    i += 2;
                    continue;
                }
                if b[i] == c && (!triple || (i + 2 < b.len() && b[i + 1] == c && b[i + 2] == c)) {
                    i += q;
                    closed = true;
                    break;
                }
                if c == '"' && b[i] == '$' && i + 1 < b.len() && b[i + 1] == '{' {
                    // copy a balanced ${...} verbatim (it may contain quotes)
                    let mut d = 0;
                    while i < b.len() {
                        if b[i] == '{' {
                            d += 1;
                        } else if b[i] == '}' {
                            d -= 1;
                            if d == 0 {
                                s.push('}');
                                i += 1;
                                break;
                            }
                        }
                        s.push(b[i]);
                        i += 1;
                    }
                    continue;
                }
                s.push(b[i]);
                i += 1;
            }
            if !closed {
                return Err("unterminated string".into());
            }
            out.push(Tok { t: if c == '"' { T::GStr(s) } else { T::Str(s) }, nl, pos });
            nl = false;
            continue;
        }
        if c == '/' {
            // a slashy string (regex) where an operand is expected, a division otherwise
            let operand = match out.last().map(|t: &Tok| &t.t) {
                None => true,
                Some(T::Op(o)) => !matches!(*o, ")" | "]" | "}"),
                Some(T::Id(w)) => matches!(w.as_str(), "return" | "in"),
                _ => false,
            };
            if operand {
                let mut j = i + 1;
                let mut s = String::new();
                let mut closed = false;
                while j < b.len() && b[j] != '\n' {
                    if b[j] == '\\' && j + 1 < b.len() && b[j + 1] == '/' {
                        s.push('/');
                        j += 2;
                        continue;
                    }
                    if b[j] == '/' {
                        closed = true;
                        break;
                    }
                    s.push(b[j]);
                    j += 1;
                }
                if closed && !s.is_empty() {
                    out.push(Tok { t: T::Str(s), nl, pos });
                    nl = false;
                    i = j + 1;
                    continue;
                }
            }
        }
        let rest: String = b[i..(i + 3).min(b.len())].iter().collect();
        match OPS.iter().find(|o| rest.starts_with(**o)) {
            Some(o) => {
                out.push(Tok { t: T::Op(o), nl, pos });
                nl = false;
                i += o.chars().count();
            }
            None => return Err(format!("unexpected character '{}'", c)),
        }
    }
    out.push(Tok { t: T::Eof, nl: true, pos: b.len() });
    Ok(out)
}

fn unescape(c: char) -> char {
    match c {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        x => x,
    }
}

// ====================================================================
// Groovy expression AST + parser
// ====================================================================

#[derive(Clone, Debug)]
enum E {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    GStr(String),
    Id(String),
    List(Vec<E>),
    Map(Vec<(E, E)>),
    Prop(Box<E>, String),
    Spread(Box<E>, String),
    Index(Box<E>, Box<E>),
    Call(Box<E>, Vec<Arg>),
    Un(&'static str, Box<E>),
    Bin(&'static str, Box<E>, Box<E>),
    Tern(Box<E>, Box<E>, Box<E>),
    Elvis(Box<E>, Box<E>),
    As(Box<E>, String),
    New(String, Vec<Arg>),
    Range(Box<E>, Box<E>, bool),
    Closure(Vec<String>, Option<Box<E>>),
    /// `xs.each { a, b ->` opened in a scriptlet: (receiver, params, with index)
    EachOpen(Box<E>, Vec<String>, bool),
}

#[derive(Clone, Debug)]
struct Arg {
    name: Option<String>,
    val: E,
}

struct P {
    t: Vec<Tok>,
    i: usize,
    depth: usize,
    stmt: bool,
    stop: bool,
}

type PR<X> = Result<X, String>;

impl P {
    fn new(t: Vec<Tok>) -> P {
        P { t, i: 0, depth: 0, stmt: false, stop: false }
    }
    fn peek(&self) -> &T {
        &self.t[self.i.min(self.t.len() - 1)].t
    }
    fn peek_at(&self, n: usize) -> &T {
        &self.t[(self.i + n).min(self.t.len() - 1)].t
    }
    fn nl(&self) -> bool {
        self.t[self.i.min(self.t.len() - 1)].nl
    }
    fn next(&mut self) -> T {
        let t = self.peek().clone();
        if self.i < self.t.len() - 1 {
            self.i += 1;
        }
        t
    }
    fn is(&self, op: &str) -> bool {
        matches!(self.peek(), T::Op(o) if *o == op)
    }
    fn is_id(&self, id: &str) -> bool {
        matches!(self.peek(), T::Id(o) if o == id)
    }
    fn eat(&mut self, op: &str) -> bool {
        if self.is(op) {
            self.next();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, op: &str) -> PR<()> {
        if self.eat(op) { Ok(()) } else { Err(format!("expected '{}' but found {}", op, show(self.peek()))) }
    }
    fn eof(&self) -> bool {
        matches!(self.peek(), T::Eof)
    }

    fn expr(&mut self) -> PR<E> {
        let c = self.binary(0)?;
        if self.stop {
            return Ok(c);
        }
        if self.eat("?:") {
            let b = self.expr()?;
            return Ok(E::Elvis(Box::new(c), Box::new(b)));
        }
        if self.is("?") {
            self.next();
            self.depth += 1;
            let a = self.expr();
            self.depth -= 1;
            let a = a?;
            self.expect(":")?;
            let b = self.expr()?;
            return Ok(E::Tern(Box::new(c), Box::new(a), Box::new(b)));
        }
        Ok(c)
    }

    fn binop(&self, level: usize) -> Option<&'static str> {
        let ops: &[&'static str] = match level {
            0 => &["||"],
            1 => &["&&"],
            2 => &["==", "!=", "<=>", "=~", "==~", "===", "!=="],
            3 => &["<", ">", "<=", ">="],
            4 => &["<<"],
            5 => &["..", "..<"],
            6 => &["+", "-"],
            7 => &["*", "/", "%"],
            _ => &["**"],
        };
        match self.peek() {
            T::Op(o) => ops.iter().find(|x| *x == o).copied(),
            T::Id(w) if level == 3 => match w.as_str() {
                "in" => Some("in"),
                "instanceof" => Some("instanceof"),
                "as" => Some("as"),
                _ => None,
            },
            _ => None,
        }
    }

    fn binary(&mut self, level: usize) -> PR<E> {
        if level > 8 {
            return self.unary();
        }
        let mut l = self.binary(level + 1)?;
        loop {
            if self.stop {
                return Ok(l);
            }
            // `x !in y`
            if level == 3 && self.is("!") && matches!(self.peek_at(1), T::Id(w) if w == "in") {
                self.next();
                self.next();
                let r = self.binary(level + 1)?;
                l = E::Un("!", Box::new(E::Bin("in", Box::new(l), Box::new(r))));
                continue;
            }
            let Some(op) = self.binop(level) else { return Ok(l) };
            self.next();
            if op == "as" || op == "instanceof" {
                let mut ty = String::new();
                while let T::Id(x) = self.peek().clone() {
                    self.next();
                    ty = x;
                    if !self.eat(".") {
                        break;
                    }
                }
                if self.is("<") && matches!(ty.as_str(), "List" | "Map" | "Set" | "ArrayList" | "HashMap" | "Collection") {
                    // generics: skip to the matching '>'
                    while !self.eof() && !self.eat(">") {
                        self.next();
                    }
                }
                if self.eat("[") {
                    self.expect("]")?;
                    ty.push_str("[]");
                }
                l = if op == "as" { E::As(Box::new(l), ty) } else { E::Bin("instanceof", Box::new(l), Box::new(E::Id(ty))) };
                continue;
            }
            let r = self.binary(level + 1)?;
            l = match op {
                ".." => E::Range(Box::new(l), Box::new(r), true),
                "..<" => E::Range(Box::new(l), Box::new(r), false),
                _ => E::Bin(op, Box::new(l), Box::new(r)),
            };
        }
    }

    fn unary(&mut self) -> PR<E> {
        if self.eat("!") {
            return Ok(E::Un("!", Box::new(self.unary()?)));
        }
        if self.eat("-") {
            return Ok(E::Un("-", Box::new(self.unary()?)));
        }
        if self.eat("+") {
            return self.unary();
        }
        self.postfix()
    }

    fn args(&mut self) -> PR<Vec<Arg>> {
        // after '('
        let mut a = vec![];
        self.depth += 1;
        let r = (|| {
            while !self.is(")") {
                let named = match (self.peek().clone(), self.peek_at(1)) {
                    (T::Id(n), T::Op(":")) | (T::Str(n), T::Op(":")) => Some(n),
                    _ => None,
                };
                if let Some(n) = named {
                    self.next();
                    self.next();
                    a.push(Arg { name: Some(n), val: self.expr()? });
                } else {
                    a.push(Arg { name: None, val: self.expr()? });
                }
                if !self.eat(",") {
                    break;
                }
            }
            self.expect(")")
        })();
        self.depth -= 1;
        r?;
        Ok(a)
    }

    /// `{ a, b -> expr }` (after the `{`).
    fn closure(&mut self) -> PR<E> {
        let params = self.closure_params();
        let (d, st) = (self.depth, self.stmt);
        self.depth = 1;
        self.stmt = false;
        let start = self.i;
        let body = match self.expr() {
            Ok(e) if self.is("}") => Some(Box::new(e)),
            _ => {
                // not a single expression: skip to the matching brace
                self.i = start;
                let mut n = 1;
                while !self.eof() {
                    if self.is("{") {
                        n += 1;
                    } else if self.is("}") {
                        n -= 1;
                        if n == 0 {
                            break;
                        }
                    }
                    self.next();
                }
                None
            }
        };
        self.depth = d;
        self.stmt = st;
        self.expect("}")?;
        Ok(E::Closure(params, body))
    }

    fn closure_params(&mut self) -> Vec<String> {
        let mut n = 0;
        let mut names = vec![];
        loop {
            match (self.peek_at(n).clone(), self.peek_at(n + 1).clone()) {
                (T::Id(a), T::Op("->")) => {
                    names.push(a);
                    self.i += n + 2;
                    return names;
                }
                (T::Id(a), T::Op(",")) => {
                    names.push(a);
                    n += 2;
                }
                // typed parameter: `String a ->`
                (T::Id(_), T::Id(_)) => n += 1,
                (T::Op("->"), _) if n == 0 => {
                    self.i += 1;
                    return vec![];
                }
                _ => return vec![],
            }
        }
    }

    fn postfix(&mut self) -> PR<E> {
        let mut e = self.primary()?;
        loop {
            if self.stop {
                return Ok(e);
            }
            let nl0 = self.nl() && self.depth == 0;
            if self.is(".") || self.is("?.") {
                self.next();
                let name = match self.next() {
                    T::Id(n) | T::Str(n) => n,
                    t => return Err(format!("expected a property name, found {}", show(&t))),
                };
                e = E::Prop(Box::new(e), name);
            } else if self.is("*.") {
                self.next();
                let name = match self.next() {
                    T::Id(n) => n,
                    t => return Err(format!("expected a property name, found {}", show(&t))),
                };
                e = E::Spread(Box::new(e), name);
            } else if self.is("(") && !nl0 {
                self.next();
                let mut a = self.args()?;
                if self.is("{") && !self.nl() {
                    if let Some(open) = self.each_open(&e) {
                        return Ok(open);
                    }
                    self.next();
                    a.push(Arg { name: None, val: self.closure()? });
                }
                e = E::Call(Box::new(e), a);
            } else if self.is("{") && !nl0 && matches!(e, E::Prop(..)) {
                if let Some(open) = self.each_open(&e) {
                    return Ok(open);
                }
                self.next();
                let c = self.closure()?;
                e = E::Call(Box::new(e), vec![Arg { name: None, val: c }]);
            } else if (self.is("[") && !nl0) || self.is("?[") {
                self.next();
                self.depth += 1;
                let ix = self.expr();
                self.depth -= 1;
                let ix = ix?;
                self.expect("]")?;
                e = E::Index(Box::new(e), Box::new(ix));
            } else {
                return Ok(e);
            }
        }
    }

    /// In a scriptlet, `xs.each { x ->` opens a loop whose body is template text.
    fn each_open(&mut self, callee: &E) -> Option<E> {
        if !self.stmt {
            return None;
        }
        let E::Prop(recv, name) = callee else { return None };
        if !matches!(name.as_str(), "each" | "eachWithIndex") {
            return None;
        }
        self.next(); // {
        let params = self.closure_params();
        self.stop = true;
        Some(E::EachOpen(recv.clone(), params, name == "eachWithIndex"))
    }

    fn primary(&mut self) -> PR<E> {
        match self.next() {
            T::Num(n) => Ok(E::Num(n)),
            T::Str(s) => Ok(E::Str(s)),
            T::GStr(s) => Ok(E::GStr(s)),
            T::Id(w) => match w.as_str() {
                "null" => Ok(E::Null),
                "true" => Ok(E::Bool(true)),
                "false" => Ok(E::Bool(false)),
                "new" => {
                    let mut ty = String::new();
                    while let T::Id(x) = self.peek().clone() {
                        self.next();
                        ty = x;
                        if !self.eat(".") {
                            break;
                        }
                    }
                    let a = if self.eat("(") { self.args()? } else { vec![] };
                    Ok(E::New(ty, a))
                }
                _ => Ok(E::Id(w)),
            },
            T::Op("(") => {
                self.depth += 1;
                let e = self.expr();
                self.depth -= 1;
                let e = e?;
                self.expect(")")?;
                Ok(e)
            }
            T::Op("[") => {
                self.depth += 1;
                let r = self.list_or_map();
                self.depth -= 1;
                r
            }
            T::Op("{") => self.closure(),
            t => Err(format!("unexpected {}", show(&t))),
        }
    }

    fn list_or_map(&mut self) -> PR<E> {
        if self.eat(":") {
            self.expect("]")?;
            return Ok(E::Map(vec![]));
        }
        let mut list = vec![];
        let mut map = vec![];
        while !self.is("]") {
            // map key forms: name:, 'name':, (expr):
            let key = match (self.peek().clone(), self.peek_at(1)) {
                (T::Id(n), T::Op(":")) => {
                    self.next();
                    Some(E::Str(n))
                }
                (T::Num(n), T::Op(":")) => {
                    self.next();
                    Some(E::Num(n))
                }
                _ => None,
            };
            let first = match key {
                Some(k) => k,
                None => self.expr()?,
            };
            if self.eat(":") {
                let v = self.expr()?;
                map.push((first, v));
            } else {
                list.push(first);
            }
            if !self.eat(",") {
                break;
            }
        }
        self.expect("]")?;
        if !map.is_empty() && list.is_empty() { Ok(E::Map(map)) } else if map.is_empty() { Ok(E::List(list)) } else { Err("mixed list and map literal".into()) }
    }
}

fn show(t: &T) -> String {
    match t {
        T::Id(x) | T::Num(x) => format!("'{}'", x),
        T::Str(_) | T::GStr(_) => "a string".into(),
        T::Op(o) => format!("'{}'", o),
        T::Eof => "the end".into(),
    }
}

fn parse_expr(src: &str) -> PR<E> {
    let mut p = P::new(lex(src)?);
    let e = p.expr()?;
    if !p.eof() {
        return Err(format!("unexpected {}", show(p.peek())));
    }
    Ok(e)
}

// ====================================================================
// Emitter (Groovy AST -> MiniJinja expression)
// ====================================================================

/// A string literal in MiniJinja syntax.
pub fn jstr(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Methods implemented by the render-time fallback in script.rs (`groovy_method`).
pub const KNOWN_METHODS: &[&str] = &[
    "toString", "toInteger", "toLong", "intValue", "longValue", "toBigInteger", "toBigDecimal", "toDouble", "toFloat", "doubleValue", "floatValue", "isNumber",
    "isInteger", "isLong", "isBigDecimal", "isDouble", "size", "length", "isEmpty", "equals", "trim", "toLowerCase", "toUpperCase", "capitalize", "contains", "startsWith",
    "endsWith", "equalsIgnoreCase", "replace", "replaceAll", "replaceFirst", "split", "tokenize", "substring", "take", "drop", "indexOf", "lastIndexOf",
    "padLeft", "padRight", "reverse", "format", "encodeAsURL", "encodeAsHTML", "encodeAsRaw", "encodeAsJSON", "encodeAsJavaScript", "getAt", "toList", "readLines", "matches", "plus", "minus",
    "multiply", "div", "clearTime", "round", "ceil", "floor", "abs", "setScale", "intdiv", "toPlainString", "stripTrailingZeros", "max", "min", "join",
    "first", "last", "sum", "unique", "sort", "get", "containsKey", "keySet", "keys", "values", "entrySet", "items", "subMap", "int", "long", "head", "tail",
    "flatten", "count", "toSet", "toUpperCase", "compareTo", "getTime", "getYear", "getMonth", "getDate", "isNaN", "toBoolean", "asBoolean", "normalize",
];

fn atomic(e: &E) -> bool {
    matches!(e, E::Null | E::Bool(_) | E::Num(_) | E::Str(_) | E::Id(_) | E::List(_) | E::Map(_) | E::Prop(..) | E::Index(..) | E::Call(..))
}

fn path_of(e: &E) -> Option<String> {
    match e {
        E::Id(n) => Some(n.clone()),
        E::Prop(r, n) => Some(format!("{}.{}", path_of(r)?, n)),
        _ => None,
    }
}

/// Variables a closure body takes from outside (not its own parameters, not class names).
fn free_ids(e: &E, bound: &mut Vec<String>, out: &mut std::collections::BTreeSet<String>) {
    let mut each = |x: &E, bound: &mut Vec<String>| free_ids(x, bound, out);
    match e {
        E::Id(n) => {
            let skip = n.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) || matches!(n.as_str(), "g5portal" | "portalService" | "g" | "request" | "session" | "flash");
            if !bound.contains(n) && !skip {
                out.insert(n.clone());
            }
        }
        E::Null | E::Bool(_) | E::Num(_) | E::Str(_) => {}
        E::GStr(s) => {
            // interpolated names: take every identifier that follows a '$'
            let b: Vec<char> = s.chars().collect();
            let mut i = 0;
            while i < b.len() {
                if b[i] == '$' {
                    let mut j = i + 1;
                    let end = if b.get(j) == Some(&'{') { match_brace(&b, j).unwrap_or(b.len()) } else { b.len() };
                    if b.get(j) == Some(&'{') {
                        let inner: String = b[j + 1..end.min(b.len())].iter().collect();
                        if let Ok(x) = parse_expr(&inner) {
                            free_ids(&x, bound, out);
                        }
                        i = end;
                    } else {
                        let mut w = String::new();
                        while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_') {
                            w.push(b[j]);
                            j += 1;
                        }
                        if !w.is_empty() && !bound.contains(&w) {
                            out.insert(w);
                        }
                        i = j;
                        continue;
                    }
                }
                i += 1;
            }
        }
        E::List(l) => l.iter().for_each(|x| each(x, bound)),
        E::Map(m) => m.iter().for_each(|(k, v)| {
            each(k, bound);
            each(v, bound)
        }),
        E::Prop(r, _) | E::Spread(r, _) | E::Un(_, r) | E::As(r, _) => each(r, bound),
        E::Index(a, b) | E::Bin(_, a, b) | E::Elvis(a, b) | E::Range(a, b, _) => {
            each(a, bound);
            each(b, bound)
        }
        E::Call(c, a) => {
            if !matches!(**c, E::Id(_)) {
                each(c, bound);
            }
            a.iter().for_each(|x| each(&x.val, bound))
        }
        E::Tern(a, b, c) => {
            each(a, bound);
            each(b, bound);
            each(c, bound)
        }
        E::New(_, a) => a.iter().for_each(|x| each(&x.val, bound)),
        E::Closure(params, body) => {
            if let Some(b) = body {
                let n = bound.len();
                bound.extend(params.iter().cloned());
                if params.is_empty() {
                    bound.push("it".into());
                }
                each(b, bound);
                bound.truncate(n);
            }
        }
        E::EachOpen(..) => {}
    }
}

/// Replace identifiers by expressions (used to expand a page-defined closure at its call site).
fn subst(e: &E, m: &HashMap<String, E>) -> E {
    let b = |x: &E| Box::new(subst(x, m));
    let args = |a: &[Arg]| a.iter().map(|x| Arg { name: x.name.clone(), val: subst(&x.val, m) }).collect::<Vec<_>>();
    match e {
        E::Id(n) => m.get(n).cloned().unwrap_or_else(|| e.clone()),
        E::Null | E::Bool(_) | E::Num(_) | E::Str(_) | E::GStr(_) | E::EachOpen(..) => e.clone(),
        E::List(l) => E::List(l.iter().map(|x| subst(x, m)).collect()),
        E::Map(l) => E::Map(l.iter().map(|(k, v)| (subst(k, m), subst(v, m))).collect()),
        E::Prop(r, n) => E::Prop(b(r), n.clone()),
        E::Spread(r, n) => E::Spread(b(r), n.clone()),
        E::Index(x, y) => E::Index(b(x), b(y)),
        E::Call(c, a) => E::Call(b(c), args(a)),
        E::Un(o, x) => E::Un(o, b(x)),
        E::Bin(o, x, y) => E::Bin(o, b(x), b(y)),
        E::Tern(x, y, z) => E::Tern(b(x), b(y), b(z)),
        E::Elvis(x, y) => E::Elvis(b(x), b(y)),
        E::As(x, t) => E::As(b(x), t.clone()),
        E::New(t, a) => E::New(t.clone(), args(a)),
        E::Range(x, y, i) => E::Range(b(x), b(y), *i),
        E::Closure(p, body) => {
            let mut m2 = m.clone();
            for x in p {
                m2.remove(x);
            }
            E::Closure(p.clone(), body.as_ref().map(|x| Box::new(subst(x, &m2))))
        }
    }
}

fn uses(e: &E, name: &str) -> bool {
    match e {
        E::Id(n) => n == name,
        E::Null | E::Bool(_) | E::Num(_) | E::Str(_) => false,
        E::GStr(s) => s.contains(name),
        E::List(l) => l.iter().any(|x| uses(x, name)),
        E::Map(m) => m.iter().any(|(k, v)| uses(k, name) || uses(v, name)),
        E::Prop(r, _) | E::Spread(r, _) | E::Un(_, r) | E::As(r, _) => uses(r, name),
        E::Index(a, b) | E::Bin(_, a, b) | E::Elvis(a, b) | E::Range(a, b, _) => uses(a, name) || uses(b, name),
        E::Call(c, a) => uses(c, name) || a.iter().any(|x| uses(&x.val, name)),
        E::Tern(a, b, c) => uses(a, name) || uses(b, name) || uses(c, name),
        E::New(_, a) => a.iter().any(|x| uses(&x.val, name)),
        E::Closure(..) | E::EachOpen(..) => true,
    }
}

const GRAILS_MESSAGES: &[(&str, &str)] = &[
    ("default.home.label", "Home"), ("default.list.label", "{0} List"), ("default.new.label", "New {0}"), ("default.create.label", "Create {0}"),
    ("default.show.label", "Show {0}"), ("default.edit.label", "Edit {0}"), ("default.add.label", "Add {0}"), ("default.button.create.label", "Create"),
    ("default.button.edit.label", "Edit"), ("default.button.update.label", "Update"), ("default.button.delete.label", "Delete"),
    ("default.button.delete.confirm.message", "Are you sure?"), ("default.link.skip.label", "Skip to content…"), ("default.paginate.prev", "Previous"),
    ("default.paginate.next", "Next"), ("default.created.message", "{0} {1} created"), ("default.updated.message", "{0} {1} updated"),
    ("default.deleted.message", "{0} {1} deleted"), ("default.not.found.message", "{0} not found with id {1}"),
];

#[derive(Clone, Copy, PartialEq)]
enum Blk {
    BraceIf,
    BraceFor,
    GIf,
    GUnless,
    GEach,
    GSet,
    Close,
}

struct Tr {
    closures: HashMap<String, (Vec<String>, E)>,
    out: String,
    warn: Vec<String>,
    ns: HashSet<String>,
    stack: Vec<(Blk, String)>,
    /// loop nesting of every assignment seen, per variable
    assigns: HashMap<String, Vec<Vec<usize>>>,
    loops: Vec<usize>,
    loop_seq: usize,
    /// loop variables in scope (they shadow namespace variables)
    locals: Vec<String>,
}

impl Tr {
    fn new(ns: HashSet<String>) -> Tr {
        Tr { closures: HashMap::new(), out: String::new(), warn: vec![], ns, stack: vec![], assigns: HashMap::new(), loops: vec![], loop_seq: 0, locals: vec![] }
    }

    fn loop_assigned(&self) -> HashSet<String> {
        let mut r = HashSet::new();
        for (name, list) in &self.assigns {
            let hit = list.iter().any(|a| list.iter().any(|b| b.len() > a.len() && b[..a.len()] == a[..]));
            if hit {
                r.insert(name.clone());
            }
        }
        r
    }

    fn w(&mut self, msg: impl Into<String>) {
        self.warn.push(msg.into());
    }

    fn todo(&mut self, what: &str, code: &str) {
        let short: String = code.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect();
        self.w(format!("{}: {}", what, short));
        self.out.push_str(&format!("{{# GSP-TODO {}: {} #}}", what, code.replace("#}", "# }").replace("{#", "{ #")));
    }

    // ---------------- expressions ----------------

    fn var(&self, n: &str) -> String {
        if self.ns.contains(n) && !self.locals.iter().any(|l| l == n) { format!("ns.{}", n) } else { n.to_string() }
    }

    fn p(&mut self, e: &E) -> String {
        let s = self.e(e);
        let _ = atomic(e);
        paren(&s)
    }

    fn args(&mut self, a: &[Arg]) -> String {
        let mut o = vec![];
        for x in a {
            let v = self.e(&x.val);
            o.push(match &x.name {
                Some(n) => format!("{}={}", n, v),
                None => v,
            });
        }
        o.join(", ")
    }

    fn named<'a>(&self, a: &'a [Arg], n: &str) -> Option<&'a E> {
        a.iter().find(|x| x.name.as_deref() == Some(n)).map(|x| &x.val)
    }

    /// `createLink(controller:.., action:.., id:.., params:[..])` as a URL expression.
    fn link_expr(&mut self, a: &[Arg]) -> String {
        if let Some(v) = self.named(a, "url").cloned() {
            return self.e(&v);
        }
        let mut parts = vec![];
        for k in ["controller", "action", "id", "params"] {
            if let Some(v) = self.named(a, k).cloned() {
                parts.push(format!("{}={}", k, self.e(&v)));
            }
        }
        format!("grails_url({})", parts.join(", "))
    }

    fn message(&mut self, a: &[Arg]) -> String {
        let code = match self.named(a, "code") {
            Some(E::Str(c)) => c.clone(),
            Some(E::GStr(c)) if !c.contains('$') => c.clone(),
            _ => String::new(),
        };
        let default = self.named(a, "default").cloned();
        let args: Vec<E> = match self.named(a, "args") {
            Some(E::List(l)) => l.clone(),
            _ => vec![],
        };
        let Some(tpl) = GRAILS_MESSAGES.iter().find(|(c, _)| *c == code).map(|(_, t)| t.to_string()) else {
            return match default {
                Some(d) => self.e(&d),
                None => jstr(&code),
            };
        };
        // "{0} List" with args -> "" ~ arg0 ~ " List"
        let mut parts = vec![];
        let mut rest = tpl.as_str();
        while let Some(i) = rest.find('{') {
            let (head, tail) = rest.split_at(i);
            let Some(j) = tail.find('}') else { break };
            if !head.is_empty() {
                parts.push(jstr(head));
            }
            let n: usize = tail[1..j].parse().unwrap_or(0);
            match args.get(n) {
                Some(x) => {
                    let v = self.p(x);
                    parts.push(v)
                }
                None => {}
            }
            rest = &tail[j + 1..];
        }
        if !rest.is_empty() {
            parts.push(jstr(rest));
        }
        if parts.len() == 1 { parts.remove(0) } else { format!("({})", parts.join(" ~ ")) }
    }

    fn gstr(&mut self, raw: &str) -> String {
        let b: Vec<char> = raw.chars().collect();
        let mut parts: Vec<String> = vec![];
        let mut lit = String::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == '\\' && i + 1 < b.len() {
                lit.push(unescape(b[i + 1]));
                i += 2;
            } else if b[i] == '$' && i + 1 < b.len() && b[i + 1] == '{' {
                let mut d = 0;
                let mut j = i + 1;
                while j < b.len() {
                    if b[j] == '{' {
                        d += 1;
                    } else if b[j] == '}' {
                        d -= 1;
                        if d == 0 {
                            break;
                        }
                    }
                    j += 1;
                }
                let inner: String = b[i + 2..j.min(b.len())].iter().collect();
                if !lit.is_empty() {
                    parts.push(jstr(&lit));
                    lit.clear();
                }
                parts.push(self.src_expr(&inner, true));
                i = j + 1;
            } else if b[i] == '$' && i + 1 < b.len() && (b[i + 1].is_alphabetic() || b[i + 1] == '_') {
                let mut j = i + 1;
                while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_' || (b[j] == '.' && j + 1 < b.len() && b[j + 1].is_alphabetic())) {
                    j += 1;
                }
                let inner: String = b[i + 1..j].iter().collect();
                if !lit.is_empty() {
                    parts.push(jstr(&lit));
                    lit.clear();
                }
                parts.push(self.src_expr(&inner, true));
                i = j;
            } else {
                lit.push(b[i]);
                i += 1;
            }
        }
        if !lit.is_empty() || parts.is_empty() {
            parts.push(jstr(&lit));
        }
        if parts.len() == 1 && parts[0].starts_with('"') { parts.remove(0) } else { format!("({})", parts.join(" ~ ")) }
    }

    /// Translate Groovy source to a MiniJinja expression; on failure warn and yield `none`.
    fn src_expr(&mut self, src: &str, paren: bool) -> String {
        match parse_expr(src) {
            Ok(e) => {
                if paren { self.p(&e) } else { self.e(&e) }
            }
            Err(err) => {
                self.w(format!("expression not translated ({}): {}", err, src.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect::<String>()));
                "none".into()
            }
        }
    }

    fn closure_filter(&mut self, recv: &str, method: &str, c: &E) -> Option<String> {
        let E::Closure(params, Some(body)) = c else { return None };
        let p = params.first().cloned().unwrap_or_else(|| "it".into());
        fn attr(e: &E, p: &str) -> Option<String> {
            match e {
                E::Prop(r, n) => match &**r {
                    E::Id(x) if x == p => Some(n.clone()),
                    _ => Some(format!("{}.{}", attr(r, p)?, n)),
                },
                E::Index(r, k) => match (&**r, &**k) {
                    (E::Id(x), E::Str(n)) if x == p => Some(n.clone()),
                    _ => None,
                },
                _ => None,
            }
        }
        match method {
            "collect" => Some(format!("{}|map(attribute={})|list", recv, jstr(&attr(body, &p)?))),
            "sum" => Some(format!("{}|sum(attribute={})", recv, jstr(&attr(body, &p)?))),
            "sort" | "sortBy" => Some(format!("{}|sort(attribute={})", recv, jstr(&attr(body, &p)?))),
            "max" | "min" => None,
            "findAll" | "find" | "any" | "count" | "every" | "grep" => {
                if method == "every" {
                    return None;
                }
                let mut terms = vec![];
                fn flat<'a>(e: &'a E, out: &mut Vec<&'a E>) {
                    match e {
                        E::Bin("&&", a, b) => {
                            flat(a, out);
                            flat(b, out);
                        }
                        x => out.push(x),
                    }
                }
                flat(body, &mut terms);
                let mut chain = recv.to_string();
                for t in terms {
                    let f = match t {
                        E::Un("!", x) => format!("|rejectattr({})", jstr(&attr(x, &p)?)),
                        E::Bin(op, a, b) => {
                            let test = match *op {
                                "==" | "===" => "eq",
                                "!=" | "!==" => "ne",
                                "<" => "lt",
                                ">" => "gt",
                                "<=" => "le",
                                ">=" => "ge",
                                "in" => "in",
                                _ => return None,
                            };
                            if uses(b, &p) {
                                return None;
                            }
                            let rhs = self.e(b);
                            format!("|selectattr({}, {}, {})", jstr(&attr(a, &p)?), jstr(test), rhs)
                        }
                        x => format!("|selectattr({})", jstr(&attr(x, &p)?)),
                    };
                    chain.push_str(&f);
                }
                Some(match method {
                    "find" => format!("({}|first)", chain),
                    "any" => format!("({}|list|length > 0)", chain),
                    "count" => format!("({}|list|length)", chain),
                    _ => format!("({}|list)", chain),
                })
            }
            _ => None,
        }
    }

    /// Any single-expression closure: the body is compiled and evaluated per item at render time.
    fn closure_generic(&mut self, recv: &str, method: &str, c: &E, before: &[Arg]) -> Option<String> {
        const METHODS: &[&str] = &["findAll", "find", "any", "every", "count", "collect", "sum", "sort", "max", "min", "groupBy", "unique", "collectEntries", "findResults", "grep", "inject", "join", "findIndexOf", "takeWhile", "dropWhile", "countBy", "collectMany", "sortBy"];
        let E::Closure(params, Some(body)) = c else { return None };
        if !METHODS.contains(&method) || !before.is_empty() {
            return None;
        }
        let params: Vec<String> = if params.is_empty() { vec!["it".into()] } else { params.clone() };
        let n = params.len();
        for x in &params {
            self.locals.push(x.clone());
        }
        let src = self.e(body);
        for _ in 0..n {
            self.locals.pop();
        }
        let mut free = std::collections::BTreeSet::new();
        free_ids(body, &mut params.clone(), &mut free);
        let mut caps = vec![];
        let mut seen = HashSet::new();
        for f in free {
            let v = self.var(&f);
            let key = if v.starts_with("ns.") { "ns".to_string() } else { f.clone() };
            if seen.insert(key.clone()) {
                caps.push(format!("{}: {}", jstr(&key), key));
            }
        }
        Some(format!("g_closure({}, {}, {}, {}, {{{}}})", recv, jstr(method), jstr(&params.join(",")), jstr(&src), caps.join(", ")))
    }

    fn call(&mut self, callee: &E, a: &[Arg]) -> String {
        // a closure defined earlier in the page with `def f = { x -> expr }` is expanded in place
        if let E::Id(f) = callee {
            if let Some((params, body)) = self.closures.get(f).cloned() {
                let mut map = HashMap::new();
                for (k, p) in params.iter().enumerate() {
                    map.insert(p.clone(), a.get(k).map(|x| x.val.clone()).unwrap_or(E::Null));
                }
                if params.is_empty() {
                    map.insert("it".to_string(), a.first().map(|x| x.val.clone()).unwrap_or(E::Null));
                }
                let x = subst(&body, &map);
                return self.p(&x);
            }
        }
        let path = path_of(callee).map(|p| p.trim_start_matches("g5portal.").trim_start_matches("g6portal.").to_string());
        let pos: Vec<&E> = a.iter().filter(|x| x.name.is_none()).map(|x| &x.val).collect();
        if let Some(p) = &path {
            let simple = |me: &mut Tr, f: &str| format!("{}({})", f, me.args(a));
            match p.as_str() {
                "raw" if pos.len() == 1 => return format!("{}|safe", self.p(pos[0])),
                "message" | "g.message" => return self.message(a),
                "createLink" | "g.createLink" | "createLinkTo" | "g.createHashlink" | "createHashlink" => return self.link_expr(a),
                "resource" | "g.resource" | "assetPath" | "asset.assetPath" => {
                    let f = self.named(a, "file").or(self.named(a, "src")).cloned();
                    let d = self.named(a, "dir").cloned();
                    return match (d, f) {
                        (Some(d), Some(f)) => format!("(\"/static/\" ~ {} ~ \"/\" ~ {})", self.p(&d), self.p(&f)),
                        (None, Some(f)) => format!("(\"/static/\" ~ {})", self.p(&f)),
                        _ => "\"/static\"".into(),
                    };
                }
                "formatNumber" | "g.formatNumber" => {
                    let n = self.named(a, "number").cloned().unwrap_or(E::Null);
                    let f = match self.named(a, "format").cloned() {
                        Some(f) => self.e(&f),
                        None => "\"#,##0.##\"".into(),
                    };
                    return format!("{}|number_format({})", self.p(&n), f);
                }
                "formatDate" | "g.formatDate" => {
                    let n = self.named(a, "date").cloned().unwrap_or(E::Null);
                    let f = match self.named(a, "format").cloned() {
                        Some(f) => self.e(&f),
                        None => "\"yyyy-MM-dd HH:mm:ss\"".into(),
                    };
                    return format!("{}|java_date({})", self.p(&n), f);
                }
                "PortalTracker.load_rows" => return simple(self, "rows"),
                "PortalTracker.load_datas" => return simple(self, "first_row"),
                "PortalTracker.getdatas" if pos.len() == 3 => return simple(self, "get_data"),
                "PortalTracker.raw_rows" | "PortalTracker.raw_firstRow" => {
                    self.w("raw SQL query kept as written: check it for SQL Server syntax");
                    return simple(self, if p.ends_with("rows") { "sql_rows" } else { "sql_first" });
                }
                "sql.rows" | "sql.firstRow" => return simple(self, if p.ends_with("rows") { "sql_rows" } else { "sql_first" }),
                "curuser.adminlist" | "session.curuser.adminlist" if pos.is_empty() => return "admin_modules()".to_string(),
                "PortalTracker.findByModuleAndSlug" => return simple(self, "tracker_ref"),
                "PortalSetting.namedefault" => return simple(self, "setting"),
                "User.get" => return simple(self, "user"),
                "User.findByStaffID" => return simple(self, "user_by_staffid"),
                "FileLink.humanSize" => return simple(self, "human_size"),
                "String.format" => return simple(self, "sprintf"),
                "String.valueOf" if pos.len() == 1 => return format!("{}.toString()", self.p(pos[0])),
                "Integer.parseInt" | "Long.parseLong" | "Integer.valueOf" | "Long.valueOf" if pos.len() == 1 => return format!("{}.toInteger()", self.p(pos[0])),
                "Double.parseDouble" | "Float.parseFloat" | "Double.valueOf" if pos.len() == 1 => return format!("{}.toBigDecimal()", self.p(pos[0])),
                "Math.round" | "Math.ceil" | "Math.floor" | "Math.abs" if pos.len() == 1 => return format!("{}.{}()", self.p(pos[0]), &p[5..]),
                "Math.max" | "Math.min" if pos.len() == 2 => return format!("[{}, {}]|{}", self.e(pos[0]), self.e(pos[1]), &p[5..]),
                "URLEncoder.encode" if !pos.is_empty() => return format!("{}|urlencode", self.p(pos[0])),
                "portalService.includePage" | "portalService.runPage" if pos.len() >= 2 => {
                    let f = if p.ends_with("includePage") { "include_page" } else { "run_page" };
                    let mut parts = vec![self.e(pos[0]), self.e(pos[1])];
                    match pos.get(2) {
                        Some(E::Map(m)) => {
                            for (k, v) in m {
                                match k {
                                    E::Str(k) if k.chars().all(|c| c.is_alphanumeric() || c == '_') => parts.push(format!("{}={}", k, self.e(v))),
                                    _ => self.w("include/run page variable with a computed name was dropped"),
                                }
                            }
                        }
                        Some(_) => self.w("include/run page: variables passed as a map variable were dropped"),
                        None => {}
                    }
                    return format!("{}({})", f, parts.join(", "));
                }
                "curuser.modulerole" | "thisuser.modulerole" | "session.curuser.modulerole" => return simple(self, "module_roles"),
                "tracker.transitionallowed" if !pos.is_empty() => {
                    let t = self.e(pos[0]);
                    let d = pos.get(1).map(|d| format!(", {}", self.e(d))).unwrap_or_default();
                    return format!("transition_allowed(tracker.module, tracker.slug, {}{})", t, d);
                }
                _ => {}
            }
        }
        match callee {
            // xs*.trim()
            E::Spread(recv, m) => {
                let r = self.p(recv);
                let body = E::Call(Box::new(E::Prop(Box::new(E::Id("it".into())), m.clone())), a.to_vec());
                let c = E::Closure(vec![], Some(Box::new(body)));
                self.closure_generic(&r, "collect", &c, &[]).unwrap_or(r)
            }
            E::Prop(recv, m) => {
                let r = self.p(recv);
                // closures: only the simple attribute forms have an equivalent
                if let Some(Arg { val: c @ E::Closure(..), .. }) = a.last() {
                    if let Some(x) = self.closure_filter(&r, m, c) {
                        return x;
                    }
                    if let Some(x) = self.closure_generic(&r, m, c, &a[..a.len() - 1]) {
                        return x;
                    }
                    self.w(format!("closure passed to .{}() was not translated", m));
                    return if matches!(m.as_str(), "find" | "any" | "count" | "sum" | "every") { "none".into() } else { r };
                }
                if !KNOWN_METHODS.contains(&m.as_str()) {
                    self.w(format!("method .{}() has no equivalent yet", m));
                }
                format!("{}.{}({})", r, m, self.args(a))
            }
            E::Id(f) => {
                self.w(format!("function {}() is not defined in templates", f));
                format!("{}({})", f, self.args(a))
            }
            other => {
                let c = self.p(other);
                format!("{}({})", c, self.args(a))
            }
        }
    }

    fn e(&mut self, e: &E) -> String {
        match e {
            E::Null => "none".into(),
            E::Bool(b) => b.to_string(),
            E::Num(n) => n.clone(),
            E::Str(s) => jstr(s),
            E::GStr(s) => self.gstr(s),
            E::Id(n) => self.var(n),
            E::List(l) => format!("[{}]", l.iter().map(|x| self.e(x)).collect::<Vec<_>>().join(", ")),
            E::Map(m) => {
                let mut parts = vec![];
                for (k, v) in m {
                    parts.push(format!("{}: {}", self.e(k), self.e(v)));
                }
                format!("{{{}}}", parts.join(", "))
            }
            E::Prop(r, n) => {
                if let Some(p) = path_of(e) {
                    match p.trim_start_matches("g5portal.").trim_start_matches("g6portal.") {
                        "request.contextPath" => return "\"\"".into(),
                        "session.userid" | "session.userId" => return "curuser.id".into(),
                        "session.curuser" => return "curuser".into(),
                        "flash.message" | "flash.error" => return "flash".into(),
                        "request.forwardURI" | "request.requestURI" => return "request_path()".into(),
                        "Calendar.YEAR" | "Calendar.MONTH" | "Calendar.DAY_OF_MONTH" => {
                            self.w(format!("{} has no equivalent", p));
                            return "none".into();
                        }
                        _ => {}
                    }
                }
                if let E::Id(root) = &**r {
                    if root == "g5portal" {
                        return n.clone();
                    }
                }
                format!("{}.{}", self.p(r), n)
            }
            E::Spread(r, n) => format!("{}|map(attribute={})|list", self.p(r), jstr(n)),
            E::Index(r, k) => format!("{}[{}]", self.p(r), self.e(k)),
            E::Call(c, a) => self.call(c, a),
            E::Un("!", x) => format!("not {}", self.p(x)),
            E::Un(op, x) => format!("{}{}", op, self.p(x)),
            E::Bin(op, a, b) => {
                let stringy = |x: &E| matches!(x, E::Str(_) | E::GStr(_));
                let jop = match *op {
                    "&&" => "and",
                    "||" => "or",
                    "===" => "==",
                    "!==" => "!=",
                    "+" if stringy(a) || stringy(b) => "~",
                    "+" if matches!(**a, E::Map(_)) || matches!(**b, E::Map(_)) => return format!("merge({}, {})", self.e(a), self.e(b)),
                    "+" if matches!(**a, E::Bin("+", ..)) && bin_has_string(a) => "~",
                    "instanceof" => {
                        let test = match &**b {
                            E::Id(ty) => match ty.as_str() {
                                "Collection" | "List" | "ArrayList" | "Set" | "Iterable" | "Object[]" | "String[]" => "sequence",
                                "Map" | "HashMap" | "LinkedHashMap" => "mapping",
                                "String" | "GString" | "CharSequence" | "Date" | "Timestamp" => "string",
                                "Number" | "Integer" | "Long" | "BigDecimal" | "Double" | "Float" | "BigInteger" => "number",
                                "Boolean" => "boolean",
                                _ => "",
                            },
                            _ => "",
                        };
                        if test.is_empty() {
                            self.w("instanceof with this type has no equivalent");
                            return "false".into();
                        }
                        return format!("{} is {}", self.p(a), test);
                    }
                    "==~" => return format!("{}.matches({})", self.p(a), self.e(b)),
                    "<=>" => return format!("cmp({}, {})", self.e(a), self.e(b)),
                    "=~" | "<<" => {
                        self.w(format!("operator {} has no equivalent", op));
                        return "none".into();
                    }
                    o => o,
                };
                format!("{} {} {}", self.p(a), jop, self.p(b))
            }
            E::Tern(c, a, b) => format!("({} if {} else {})", self.p(a), self.p(c), self.p(b)),
            E::Elvis(a, b) => format!("({} or {})", self.p(a), self.p(b)),
            E::As(x, ty) => {
                let m = match ty.as_str() {
                    "int" | "Integer" | "long" | "Long" | "BigInteger" | "short" => "toInteger",
                    "BigDecimal" | "double" | "Double" | "float" | "Float" | "Number" => "toBigDecimal",
                    "String" => "toString",
                    "List" | "ArrayList" | "Set" | "HashSet" | "Map" | "boolean" | "Boolean" => return self.e(x),
                    _ => {
                        self.w(format!("'as {}' has no equivalent", ty));
                        return self.e(x);
                    }
                };
                format!("{}.{}()", self.p(x), m)
            }
            E::New(ty, a) => match (ty.as_str(), a.len()) {
                ("Date", 0) => "now()".into(),
                ("BigDecimal" | "Double" | "Float", 1) => format!("{}.toBigDecimal()", self.p(&a[0].val)),
                ("Integer" | "Long" | "BigInteger", 1) => format!("{}.toInteger()", self.p(&a[0].val)),
                ("String", 1) => format!("{}.toString()", self.p(&a[0].val)),
                ("ArrayList" | "LinkedList" | "HashSet", 0) => "[]".into(),
                ("HashMap" | "LinkedHashMap" | "TreeMap", 0) => "{}".into(),
                _ => {
                    self.w(format!("'new {}(...)' has no equivalent", ty));
                    "none".into()
                }
            },
            E::Range(a, b, incl) => match (&**b, incl) {
                (E::Num(n), true) if n.parse::<i64>().is_ok() => format!("range({}, {})", self.e(a), n.parse::<i64>().unwrap() + 1),
                (_, true) => format!("range({}, {} + 1)", self.e(a), self.p(b)),
                (_, false) => format!("range({}, {})", self.e(a), self.e(b)),
            },
            E::Closure(..) => {
                self.w("closure has no equivalent in a template");
                "none".into()
            }
            E::EachOpen(..) => "none".into(),
        }
    }

    // ---------------- template walking ----------------

    fn set(&mut self, name: &str, value: &str) {
        self.assigns.entry(name.to_string()).or_default().push(self.loops.clone());
        let v = self.var(name);
        self.out.push_str(&format!("{{% set {} = {} %}}", v, value));
    }

    fn open_loop(&mut self, vars: &[String]) {
        self.loop_seq += 1;
        self.loops.push(self.loop_seq);
        for v in vars {
            self.locals.push(v.clone());
        }
    }

    fn close_loop(&mut self, nvars: usize) {
        self.loops.pop();
        for _ in 0..nvars {
            self.locals.pop();
        }
    }

    /// The page appends `&name=value` to a link it just built: Grails links always
    /// had a query string, ours may not, so ask for one.
    fn query_follows(&mut self) {
        if let Some(pos) = self.out.rfind("{{ grails_url(") {
            let tail = &self.out[pos..];
            if tail.ends_with(") }}") && tail.matches("}}").count() == 1 {
                let n = self.out.len() - 4;
                let empty = self.out[..n].ends_with('(');
                self.out.insert_str(n, if empty { "query=true" } else { ", query=true" });
            }
        }
    }

    fn push_text(&mut self, t: &str) {
        self.out.push_str(&t.replace("{{", "{{ \"{{\" }}").replace("{%", "{{ \"{%\" }}").replace("{#", "{{ \"{#\" }}"));
    }

    fn run(&mut self, src: &str) {
        let b: Vec<char> = src.chars().collect();
        let mut i = 0;
        let mut text = String::new();
        macro_rules! flush {
            () => {
                if !text.is_empty() {
                    let t = std::mem::take(&mut text);
                    self.push_text(&t);
                }
            };
        }
        let starts = |i: usize, s: &str| -> bool { s.chars().enumerate().all(|(k, c)| b.get(i + k) == Some(&c)) };
        while i < b.len() {
            if starts(i, "\\${") {
                text.push_str("${");
                i += 3;
            } else if starts(i, "${") {
                let Some(j) = match_brace(&b, i + 1) else {
                    text.push(b[i]);
                    i += 1;
                    continue;
                };
                flush!();
                let inner: String = b[i + 2..j].iter().collect();
                let mut x = self.src_expr(&inner, false);
                // printing these objects showed their name in Grails
                if matches!(inner.trim(), "transition" | "tracker") {
                    x.push_str(".name");
                }
                self.out.push_str(&format!("{{{{ {} }}}}", x));
                i = j + 1;
                if starts(i, "&") {
                    self.query_follows();
                }
            } else if starts(i, "<%--") {
                flush!();
                let j = find(&b, i + 4, "--%>").unwrap_or(b.len());
                let c: String = b[i + 4..j.min(b.len())].iter().collect();
                self.out.push_str(&format!("{{#{}#}}", c.replace("#}", "# }").replace("{#", "{ #")));
                i = (j + 4).min(b.len());
            } else if starts(i, "<%") {
                flush!();
                let j = find(&b, i + 2, "%>").unwrap_or(b.len());
                let c: String = b[i + 2..j.min(b.len())].iter().collect();
                if let Some(x) = c.strip_prefix('=') {
                    let x = self.src_expr(x, false);
                    self.out.push_str(&format!("{{{{ {} }}}}", x));
                } else if c.starts_with('@') {
                    // page directive: nothing to do
                } else {
                    self.scriptlet(&c);
                }
                i = (j + 2).min(b.len());
            } else if starts(i, "<g:") || starts(i, "<asset:") || starts(i, "</g:") || starts(i, "</asset:") {
                flush!();
                match self.tag(&b, i) {
                    Some(j) => {
                        i = j;
                        if starts(i, "&") {
                            self.query_follows();
                        }
                    }
                    None => {
                        text.push(b[i]);
                        i += 1;
                    }
                }
            } else {
                text.push(b[i]);
                i += 1;
            }
        }
        flush!();
        while let Some((k, close)) = self.stack.pop() {
            self.w("a block was left open at the end of the page");
            if k == Blk::GEach || k == Blk::BraceFor {
                self.close_loop(0);
            }
            self.out.push_str(&close);
        }
    }

    // ---------------- scriptlets ----------------

    fn scriptlet(&mut self, code: &str) {
        let toks = match lex(code) {
            Ok(t) => t,
            Err(e) => return self.todo(&format!("scriptlet not translated ({})", e), code),
        };
        let starts: Vec<usize> = toks.iter().map(|t| t.pos).collect();
        let chars: Vec<char> = code.chars().collect();
        let mut p = P::new(toks);
        p.stmt = true;
        loop {
            while p.eat(";") {}
            if p.eof() {
                break;
            }
            let at = p.i;
            if let Err(e) = self.statement(&mut p) {
                // give up on the rest of this scriptlet
                let rest: String = chars[starts[at].min(chars.len())..].iter().collect();
                self.todo(&format!("statement not translated ({})", e), rest.trim());
                break;
            }
        }
    }

    fn close_brace(&mut self) -> PR<()> {
        match self.stack.last().map(|x| x.0) {
            Some(Blk::BraceIf) => {
                let (_, c) = self.stack.pop().unwrap();
                self.out.push_str(&c);
                Ok(())
            }
            Some(Blk::BraceFor) => {
                let (_, c) = self.stack.pop().unwrap();
                let n = c.matches('\u{1}').count();
                self.close_loop(n);
                self.out.push_str(&c.replace('\u{1}', ""));
                Ok(())
            }
            _ => Err("'}' does not close a block opened in a scriptlet".into()),
        }
    }

    fn define_closure(&mut self, p: &mut P, name: &str) -> PR<()> {
        p.next(); // {
        p.stmt = false;
        let c = p.closure();
        p.stmt = true;
        match c? {
            E::Closure(params, Some(body)) => {
                self.closures.insert(name.to_string(), (params, *body));
                Ok(())
            }
            _ => {
                self.w(format!("closure '{}' has more than one statement and was not translated", name));
                Ok(())
            }
        }
    }

    fn statement(&mut self, p: &mut P) -> PR<()> {
        const TYPES: &[&str] = &["def", "int", "Integer", "long", "Long", "String", "double", "Double", "BigDecimal", "boolean", "Boolean", "List", "Map", "float", "Date", "Object", "final", "var"];
        if p.eat("}") {
            if p.is_id("else") {
                if self.stack.last().map(|x| x.0) != Some(Blk::BraceIf) {
                    return Err("'else' without a matching if".into());
                }
                p.next();
                if p.is_id("if") {
                    p.next();
                    p.expect("(")?;
                    p.depth += 1;
                    let c = p.expr();
                    p.depth -= 1;
                    let c = c?;
                    p.expect(")")?;
                    p.expect("{")?;
                    let c = self.e(&c);
                    self.out.push_str(&format!("{{% elif {} %}}", c));
                } else {
                    p.expect("{")?;
                    self.out.push_str("{% else %}");
                }
                return Ok(());
            }
            self.close_brace()?;
            while p.is_id("catch") || p.is_id("finally") {
                p.next();
                let mut d = 0;
                loop {
                    if p.eof() {
                        return Err("unterminated catch block".into());
                    }
                    if p.is("{") {
                        d += 1;
                    } else if p.is("}") {
                        d -= 1;
                        if d == 0 {
                            p.next();
                            break;
                        }
                    }
                    p.next();
                }
            }
            return Ok(());
        }
        let (t0, t1) = (p.peek().clone(), p.peek_at(1).clone());
        match (&t0, &t1) {
            (T::Id(k), _) if k == "if" => {
                p.next();
                p.expect("(")?;
                p.depth += 1;
                let c = p.expr();
                p.depth -= 1;
                let c = c?;
                p.expect(")")?;
                let c = self.e(&c);
                self.out.push_str(&format!("{{% if {} %}}", c));
                if p.eat("{") {
                    self.stack.push((Blk::BraceIf, "{% endif %}".into()));
                } else {
                    self.statement(p)?;
                    while p.eat(";") {}
                    if p.is_id("else") {
                        p.next();
                        self.out.push_str("{% else %}");
                        if p.eat("{") {
                            self.stack.push((Blk::BraceIf, "{% endif %}".into()));
                            return Ok(());
                        }
                        self.statement(p)?;
                    }
                    self.out.push_str("{% endif %}");
                }
                Ok(())
            }
            (T::Id(k), _) if k == "for" => {
                p.next();
                p.expect("(")?;
                while matches!(p.peek(), T::Id(x) if TYPES.contains(&x.as_str())) && matches!(p.peek_at(1), T::Id(_)) {
                    p.next();
                }
                let T::Id(v) = p.next() else { return Err("unsupported for loop".into()) };
                if !(p.is_id("in") || p.is(":")) {
                    return Err("only 'for (x in list)' loops are supported".into());
                }
                p.next();
                p.depth += 1;
                let l = p.expr();
                p.depth -= 1;
                let l = l?;
                p.expect(")")?;
                p.expect("{")?;
                let l = self.e(&l);
                self.out.push_str(&format!("{{% for {} in {} %}}", v, l));
                self.open_loop(&[v]);
                self.stack.push((Blk::BraceFor, "{% endfor %}\u{1}".into()));
                Ok(())
            }
            (T::Id(k), _) if k == "try" && matches!(t1, T::Op("{")) => {
                p.next();
                p.next();
                self.stack.push((Blk::BraceIf, String::new()));
                Ok(())
            }
            (T::Id(k), _) if k == "println" || k == "print" => {
                let _ = p.expr()?;
                Ok(())
            }
            (T::Id(k), T::Op("<<")) if k != "out" => {
                // list << item
                let name = k.clone();
                p.next();
                p.next();
                let v = p.expr()?;
                let v = self.e(&v);
                let cur = self.var(&name);
                self.set(&name, &format!("{} + [{}]", cur, v));
                Ok(())
            }
            (T::Id(k), T::Op("<<")) if k == "out" => {
                p.next();
                while p.eat("<<") {
                    let x = p.binary(5)?;
                    let x = self.e(&x);
                    self.out.push_str(&format!("{{{{ {} }}}}", x));
                }
                Ok(())
            }
            (T::Id(k), T::Id(_)) if TYPES.contains(&k.as_str()) => {
                p.next();
                while matches!(p.peek(), T::Id(x) if TYPES.contains(&x.as_str())) && matches!(p.peek_at(1), T::Id(_)) {
                    p.next();
                }
                loop {
                    let T::Id(name) = p.next() else { return Err("bad declaration".into()) };
                    if p.eat("=") {
                        if p.is("{") {
                            self.define_closure(p, &name)?;
                        } else {
                            let v = p.expr()?;
                            let v = self.e(&v);
                            self.set(&name, &v);
                        }
                    } else {
                        self.set(&name, "none");
                    }
                    if !p.eat(",") {
                        break;
                    }
                }
                Ok(())
            }
            (T::Id(name), T::Op(op)) if matches!(*op, "=" | "+=" | "-=" | "*=" | "/=" | "++" | "--") => {
                let name = name.clone();
                p.next();
                p.next();
                let cur = self.var(&name);
                let v = match *op {
                    "++" => format!("{} + 1", cur),
                    "--" => format!("{} - 1", cur),
                    "=" => {
                        if p.is("{") {
                            return self.define_closure(p, &name);
                        }
                        let v = p.expr()?;
                        self.e(&v)
                    }
                    o => {
                        let v = p.expr()?;
                        let stringy = matches!(v, E::Str(_) | E::GStr(_));
                        let v = self.p(&v);
                        format!("{} {} {}", cur, if stringy && o == "+=" { "~" } else { &o[..1] }, v)
                    }
                };
                self.set(&name, &v);
                Ok(())
            }
            _ => {
                let e = p.expr()?;
                p.stop = false;
                let e_keep = e.clone();
                match e {
                    E::EachOpen(recv, params, with_index) => {
                        let r = self.e(&recv);
                        let mut vars = params.clone();
                        if vars.is_empty() {
                            vars.push("it".into());
                        }
                        if with_index && vars.len() == 2 {
                            self.out.push_str(&format!("{{% for {} in {} %}}{{% set {} = loop.index0 %}}", vars[0], r, vars[1]));
                        } else if vars.len() == 2 {
                            self.out.push_str(&format!("{{% for {}, {} in {}|items %}}", vars[0], vars[1], r));
                        } else {
                            self.out.push_str(&format!("{{% for {} in {} %}}", vars[0], r));
                        }
                        let marks = "\u{1}".repeat(vars.len());
                        self.open_loop(&vars);
                        self.stack.push((Blk::BraceFor, format!("{{% endfor %}}{}", marks)));
                        Ok(())
                    }
                    // map.key = value / map[key] = value
                    E::Prop(m, _) | E::Index(m, _) if p.is("=") && matches!(*m, E::Id(_)) => {
                        let E::Id(name) = &*m else { unreachable!() };
                        let key = match &e_keep {
                            E::Prop(_, k) => jstr(k),
                            E::Index(_, k) => self.e(k),
                            _ => unreachable!(),
                        };
                        p.next();
                        let v = p.expr()?;
                        let v = self.e(&v);
                        let cur = self.var(name);
                        self.set(name, &format!("merge({}, {{{}: {}}})", cur, key, v));
                        Ok(())
                    }
                    // logging has no effect on the page
                    E::Call(c, _) if path_of(&c).map(|x| x.starts_with("System.out.") || x.starts_with("log.")).unwrap_or(false) => Ok(()),
                    _ => Err("only assignments, if/else, loops and 'out <<' can be translated".into()),
                }
            }
        }
    }

    // ---------------- tags ----------------

    /// Attribute value as a MiniJinja expression.
    fn attr_expr(&mut self, v: &str, groovy_literal: bool) -> String {
        let t = v.trim();
        if t.starts_with("${") && t.ends_with('}') {
            let b: Vec<char> = t.chars().collect();
            if match_brace(&b, 1) == Some(b.len() - 1) {
                let inner: String = b[2..b.len() - 1].iter().collect();
                return self.src_expr(&inner, false);
            }
        }
        if v.contains("${") {
            return self.gstr(&v.replace('\\', "\\\\"));
        }
        if groovy_literal && (t.starts_with('[') || t == "true" || t == "false" || t.parse::<f64>().is_ok()) {
            return self.src_expr(t, false);
        }
        jstr(v)
    }

    /// Attribute value as template text (inside an HTML attribute).
    fn attr_text(&mut self, v: &str) -> String {
        let b: Vec<char> = v.chars().collect();
        let mut o = String::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == '$' && b.get(i + 1) == Some(&'{') {
                if let Some(j) = match_brace(&b, i + 1) {
                    let inner: String = b[i + 2..j].iter().collect();
                    let x = self.src_expr(&inner, false);
                    o.push_str(&format!("{{{{ {} }}}}", x));
                    i = j + 1;
                    continue;
                }
            }
            o.push(b[i]);
            i += 1;
        }
        o
    }

    fn html_attrs(&mut self, attrs: &[(String, String)], skip: &[&str]) -> String {
        let mut o = String::new();
        for (k, v) in attrs {
            if skip.contains(&k.as_str()) {
                continue;
            }
            let t = self.attr_text(v);
            o.push_str(&format!(" {}=\"{}\"", k, t.replace('"', "&quot;")));
        }
        o
    }

    fn url_of(&mut self, attrs: &[(String, String)]) -> String {
        let get = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        if let Some(u) = get("url") {
            return format!("{{{{ {} }}}}", self.attr_expr(&u, true));
        }
        if let Some(u) = get("uri") {
            return self.attr_text(&u);
        }
        let mut parts = vec![];
        for k in ["controller", "action", "id", "params"] {
            if let Some(v) = get(k) {
                parts.push(format!("{}={}", k, self.attr_expr(&v, true)));
            }
        }
        format!("{{{{ grails_url({}) }}}}", parts.join(", "))
    }

    /// Arguments for a tracker tag: nothing for the page's own tracker, else (module, slug).
    fn tracker_args(&mut self, attrs: &[(String, String)], with_record: bool, with_transition: bool) -> String {
        let get = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.trim().to_string());
        let mut parts = vec![];
        if let Some(t) = get("tracker") {
            if t != "${tracker}" {
                let x = self.attr_expr(&t, false);
                parts.push(format!("{}.module", x));
                parts.push(format!("{}.slug", x));
            }
        }
        if with_record {
            if let Some(r) = get("record_id") {
                if r != "${params.id}" {
                    parts.push(format!("id={}", self.attr_expr(&r, false)));
                }
            }
        }
        if with_transition {
            if let Some(t) = get("transition") {
                if t != "${transition}" && !t.contains("newtransition()") {
                    let x = self.attr_expr(&t, false);
                    parts.push(format!("transition={}.name", x));
                }
            }
        }
        parts.join(", ")
    }

    fn tag(&mut self, b: &[char], start: usize) -> Option<usize> {
        let closing = b.get(start + 1) == Some(&'/');
        let mut i = start + if closing { 2 } else { 1 };
        let mut name = String::new();
        while i < b.len() && (b[i].is_alphanumeric() || b[i] == ':' || b[i] == '_') {
            name.push(b[i]);
            i += 1;
        }
        if closing {
            while i < b.len() && b[i] != '>' {
                i += 1;
            }
            self.end_tag(&name);
            return Some((i + 1).min(b.len()));
        }
        // attributes
        let mut attrs: Vec<(String, String)> = vec![];
        let mut selfclose = false;
        loop {
            while i < b.len() && b[i].is_whitespace() {
                i += 1;
            }
            if i >= b.len() {
                return None;
            }
            if b[i] == '/' && b.get(i + 1) == Some(&'>') {
                selfclose = true;
                i += 2;
                break;
            }
            if b[i] == '>' {
                i += 1;
                break;
            }
            let mut k = String::new();
            while i < b.len() && !b[i].is_whitespace() && b[i] != '=' && b[i] != '>' && b[i] != '/' {
                k.push(b[i]);
                i += 1;
            }
            if k.is_empty() {
                return None;
            }
            while i < b.len() && b[i].is_whitespace() {
                i += 1;
            }
            if b.get(i) != Some(&'=') {
                attrs.push((k, String::new()));
                continue;
            }
            i += 1;
            while i < b.len() && b[i].is_whitespace() {
                i += 1;
            }
            let q = *b.get(i)?;
            if q != '"' && q != '\'' {
                return None;
            }
            i += 1;
            let mut v = String::new();
            loop {
                let c = *b.get(i)?;
                if c == '$' && b.get(i + 1) == Some(&'{') {
                    let j = match_brace(b, i + 1)?;
                    v.extend(&b[i..=j]);
                    i = j + 1;
                    continue;
                }
                if c == q {
                    i += 1;
                    break;
                }
                v.push(c);
                i += 1;
            }
            attrs.push((k, v));
        }
        self.start_tag(&name, &attrs, selfclose);
        Some(i)
    }

    fn end_tag(&mut self, name: &str) {
        let want = match name {
            "g:if" | "g:elseif" | "g:else" => Blk::GIf,
            "g:unless" => Blk::GUnless,
            "g:each" => Blk::GEach,
            "g:set" => Blk::GSet,
            _ => Blk::Close,
        };
        match self.stack.last() {
            Some((k, _)) if *k == want => {
                let (k, c) = self.stack.pop().unwrap();
                if k == Blk::GEach {
                    let n = c.matches('\u{1}').count();
                    self.close_loop(n);
                }
                self.out.push_str(&c.replace('\u{1}', ""));
            }
            _ => self.w(format!("</{}> does not match an open tag", name)),
        }
    }

    fn start_tag(&mut self, name: &str, attrs: &[(String, String)], selfclose: bool) {
        let get = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        match name {
            "g:if" => {
                let c = self.attr_expr(&get("test").unwrap_or_default(), true);
                self.out.push_str(&format!("{{% if {} %}}", c));
                self.stack.push((Blk::GIf, "{% endif %}".into()));
            }
            "g:elseif" | "g:else" => {
                // re-open the if that the previous </g:if> closed
                if let Some(pos) = self.out.rfind("{% endif %}") {
                    if only_comments(&self.out[pos + 11..]) {
                        self.out.replace_range(pos..pos + 11, "");
                        if name == "g:else" {
                            self.out.push_str("{% else %}");
                        } else {
                            let c = self.attr_expr(&get("test").unwrap_or_default(), true);
                            self.out.push_str(&format!("{{% elif {} %}}", c));
                        }
                        self.stack.push((Blk::GIf, "{% endif %}".into()));
                        if selfclose {
                            self.end_tag("g:if");
                        }
                        return;
                    }
                }
                self.w(format!("<{}> without a preceding <g:if>", name));
                self.out.push_str("{% if false %}");
                self.stack.push((Blk::GIf, "{% endif %}".into()));
            }
            "g:unless" => {
                let c = self.attr_expr(&get("test").unwrap_or_default(), true);
                self.out.push_str(&format!("{{% if not ({}) %}}", c));
                self.stack.push((Blk::GUnless, "{% endif %}".into()));
            }
            "g:each" => {
                let list = self.attr_expr(&get("in").unwrap_or_default(), true);
                let ident = |x: String| -> String { x.chars().filter(|c| c.is_alphanumeric() || *c == '_').collect() };
                let var = get("var").map(ident).filter(|x| !x.is_empty()).unwrap_or_else(|| "it".into());
                self.out.push_str(&format!("{{% for {} in {} %}}", var, list));
                let mut vars = vec![var];
                if let Some(st) = get("status").map(ident).filter(|x| !x.is_empty()) {
                    self.out.push_str(&format!("{{% set {} = loop.index0 %}}", st));
                    vars.push(st);
                }
                let marks = "\u{1}".repeat(vars.len());
                self.open_loop(&vars);
                self.stack.push((Blk::GEach, format!("{{% endfor %}}{}", marks)));
                if selfclose {
                    self.end_tag("g:each");
                }
            }
            "g:set" => {
                let var: String = get("var").unwrap_or_default().chars().filter(|c| c.is_alphanumeric() || *c == '_').collect();
                if selfclose || get("value").is_some() {
                    let v = self.attr_expr(&get("value").unwrap_or_default(), true);
                    self.set(&var, &v);
                    if !selfclose {
                        self.stack.push((Blk::GSet, String::new()));
                    }
                } else {
                    self.assigns.entry(var.clone()).or_default().push(self.loops.clone());
                    let v = self.var(&var);
                    self.out.push_str(&format!("{{% set {} %}}", v));
                    self.stack.push((Blk::GSet, "{% endset %}".into()));
                }
            }
            "g:formatNumber" => {
                let n = self.attr_expr(&get("number").unwrap_or_default(), true);
                let fmt = match (get("format"), get("maxFractionDigits"), get("type")) {
                    (Some(f), _, _) => self.attr_expr(&f, false),
                    (None, Some(d), _) => jstr(&format!("#,##0.{}{}", "0".repeat(get("minFractionDigits").and_then(|x| x.parse().ok()).unwrap_or(0)), "#".repeat(d.parse::<usize>().unwrap_or(2)))),
                    (None, None, Some(t)) if t == "currency" => jstr("#,##0.00"),
                    (None, None, Some(t)) if t == "percent" => jstr("#,##0%"),
                    _ => jstr("#,##0.###"),
                };
                self.out.push_str(&format!("{{{{ {}|number_format({}) }}}}", paren(&n), fmt));
            }
            "g:formatDate" => {
                let d = self.attr_expr(&get("date").unwrap_or_else(|| "${new Date()}".into()), true);
                let f = self.attr_expr(&get("format").unwrap_or_else(|| "yyyy-MM-dd HH:mm:ss".into()), false);
                self.out.push_str(&format!("{{{{ {}|java_date({}) }}}}", paren(&d), f));
            }
            "g:message" => {
                let mut a = vec![];
                for k in ["code", "default", "args"] {
                    if let Some(v) = get(k) {
                        let src = if v.trim().starts_with("${") || (k == "args" && v.trim().starts_with('[')) {
                            v.trim().trim_start_matches("${").trim_end_matches('}').to_string()
                        } else {
                            format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'"))
                        };
                        if let Ok(e) = parse_expr(&src) {
                            a.push(Arg { name: Some(k.to_string()), val: e });
                        }
                    }
                }
                let m = self.message(&a);
                if m.starts_with('"') && !m.contains('\\') {
                    let lit = m[1..m.len() - 1].to_string();
                    self.push_text(&lit);
                } else {
                    self.out.push_str(&format!("{{{{ {} }}}}", m));
                }
            }
            "g:link" | "g:hashlink" => {
                let url = self.url_of(attrs);
                let rest = self.html_attrs(attrs, &["controller", "action", "id", "params", "url", "uri", "attributes", "absolute", "base", "elementId", "mapping", "resource"]);
                let eid = get("elementId").map(|x| format!(" id=\"{}\"", self.attr_text(&x))).unwrap_or_default();
                self.out.push_str(&format!("<a href=\"{}\"{}{}>", url, eid, rest));
                if selfclose {
                    self.out.push_str("</a>");
                } else {
                    self.stack.push((Blk::Close, "</a>".into()));
                }
            }
            "g:createLink" | "g:createHashlink" => {
                let url = self.url_of(attrs);
                self.out.push_str(&url);
            }
            "g:form" => {
                let url = self.url_of(attrs);
                let method = get("method").unwrap_or_else(|| "post".into());
                let rest = self.html_attrs(attrs, &["controller", "action", "id", "params", "url", "uri", "method", "useToken", "resource"]);
                let fid = get("name").filter(|_| get("id").is_none()).map(|x| format!(" id=\"{}\"", self.attr_text(&x))).unwrap_or_default();
                self.out.push_str(&format!("<form action=\"{}\" method=\"{}\"{}{}>", url, method.to_lowercase(), fid, rest));
                if !method.eq_ignore_ascii_case("get") {
                    self.out.push_str("{{ csrf_field() }}");
                }
                if selfclose {
                    self.out.push_str("</form>");
                } else {
                    self.stack.push((Blk::Close, "</form>".into()));
                }
            }
            "g:hiddenField" | "g:textField" | "g:passwordField" => {
                let ty = match name {
                    "g:hiddenField" => "hidden",
                    "g:passwordField" => "password",
                    _ => "text",
                };
                let n = self.attr_text(&get("name").unwrap_or_default());
                let id = get("id").map(|x| self.attr_text(&x)).unwrap_or_else(|| n.clone());
                let v = self.attr_text(&get("value").unwrap_or_default());
                let rest = self.html_attrs(attrs, &["name", "id", "value"]);
                self.out.push_str(&format!("<input type=\"{}\" name=\"{}\" id=\"{}\" value=\"{}\"{}/>", ty, n, id, v, rest));
            }
            "g:textArea" => {
                let n = self.attr_text(&get("name").unwrap_or_default());
                let v = self.attr_text(&get("value").unwrap_or_default());
                let rest = self.html_attrs(attrs, &["name", "value"]);
                self.out.push_str(&format!("<textarea name=\"{}\" id=\"{}\"{}>{}</textarea>", n, n, rest, v));
            }
            "g:submitButton" | "g:actionSubmit" => {
                let n = self.attr_text(&get("name").unwrap_or_default());
                let v = get("value").map(|x| self.attr_text(&x)).unwrap_or_else(|| n.clone());
                let rest = self.html_attrs(attrs, &["name", "value", "action"]);
                self.out.push_str(&format!("<input type=\"submit\" name=\"{}\" id=\"{}\" value=\"{}\"{}/>", n, n, v, rest));
            }
            "g:select" => {
                let mut parts = vec![];
                for (k, to) in [("name", "name"), ("from", "options"), ("value", "value"), ("keys", "keys"), ("optionKey", "optionKey"), ("optionValue", "optionValue"), ("noSelection", "noSelection"), ("multiple", "multiple")] {
                    if let Some(v) = get(k) {
                        parts.push(format!("{}={}", to, self.attr_expr(&v, matches!(k, "from" | "keys" | "noSelection" | "value"))));
                    }
                }
                let mut extra = vec![];
                for (k, v) in attrs {
                    if !["name", "from", "value", "keys", "optionKey", "optionValue", "noSelection", "multiple"].contains(&k.as_str()) {
                        extra.push(format!("{}: {}", jstr(k), self.attr_expr(v, false)));
                    }
                }
                if !extra.is_empty() {
                    parts.push(format!("attrs={{{}}}", extra.join(", ")));
                }
                self.out.push_str(&format!("{{{{ g_select({}) }}}}", parts.join(", ")));
                if !selfclose {
                    self.stack.push((Blk::Close, String::new()));
                }
            }
            "asset:script" => {
                let rest = self.html_attrs(attrs, &[]);
                self.out.push_str(&format!("<script{}>", rest));
                if selfclose {
                    self.out.push_str("</script>");
                } else {
                    self.stack.push((Blk::Close, "</script>".into()));
                }
            }
            "asset:javascript" => {
                let src = self.attr_text(&get("src").unwrap_or_default());
                let src = if src.starts_with("http") || src.starts_with('/') || src.starts_with("{{") { src } else { format!("/assets/{}", src) };
                self.out.push_str(&format!("<script src=\"{}\"></script>", src));
                if !selfclose {
                    self.stack.push((Blk::Close, String::new()));
                }
            }
            "asset:stylesheet" | "asset:link" => {
                let src = self.attr_text(&get("href").or(get("src")).unwrap_or_default());
                let src = if src.starts_with("http") || src.starts_with('/') || src.starts_with("{{") { src } else { format!("/assets/{}", src) };
                self.out.push_str(&format!("<link rel=\"{}\" href=\"{}\"/>", get("rel").unwrap_or_else(|| "stylesheet".into()), src));
                if !selfclose {
                    self.stack.push((Blk::Close, String::new()));
                }
            }
            "asset:image" => {
                let src = self.attr_text(&get("src").unwrap_or_default());
                let rest = self.html_attrs(attrs, &["src"]);
                self.out.push_str(&format!("<img src=\"/assets/{}\"{}/>", src, rest));
            }
            "g:picture_file" | "g:stream_file" => {
                let m = self.attr_expr(&get("module").unwrap_or_else(|| "portal".into()), false);
                let s = self.attr_expr(&get("slug").unwrap_or_default(), false);
                self.out.push_str(&format!("{{{{ download_url({}, {}) }}}}", m, s));
            }
            "g:filelink" | "g:filelink_link" => {
                let x = match (get("id"), get("slug")) {
                    (Some(id), _) => format!("file_link({})", self.attr_expr(&id, true)),
                    (None, Some(sl)) => {
                        let m = self.attr_expr(&get("module").unwrap_or_else(|| "portal".into()), false);
                        format!("file_link(module={}, slug={})", m, self.attr_expr(&sl, false))
                    }
                    _ => "\"\"".into(),
                };
                self.out.push_str(&format!("{{{{ {} }}}}", x));
            }
            "g:trackerList" => {
                let a = self.tracker_args(attrs, false, false);
                self.out.push_str(&format!("{{{{ tracker_list({}) }}}}", a));
            }
            "g:trackerFilter" => {
                let a = self.tracker_args(attrs, false, false);
                self.out.push_str(&format!("{{{{ tracker_filter({}) }}}}", a));
            }
            "g:trackerDisplay" => {
                let a = self.tracker_args(attrs, true, false);
                if get("field_filters").is_some() {
                    self.w("trackerDisplay field_filters attribute was dropped");
                }
                self.out.push_str(&format!("{{{{ tracker_display({}) }}}}", a));
            }
            "g:trackerUpdates" => {
                let a = self.tracker_args(attrs, true, false);
                self.out.push_str(&format!("{{{{ tracker_updates({}) }}}}", a));
            }
            "g:trackerForm" => {
                let a = self.tracker_args(attrs, true, true);
                self.out.push_str(&format!("{{{{ tracker_form({}) }}}}", a));
            }
            "g:trackerInfo" => {
                let a = self.tracker_args(attrs, true, true);
                self.out.push_str(&format!("{{{{ tracker_info({}) }}}}", a));
            }
            "g:trackerErrorHandler" => {
                // required-field checks are built into the form the portal renders
                self.out.push_str("/* trackerErrorHandler: validation is built into tracker_form() */");
            }
            "g:javascript" => {
                self.out.push_str("<script>");
                if selfclose {
                    self.out.push_str("</script>");
                } else {
                    self.stack.push((Blk::Close, "</script>".into()));
                }
            }
            _ => {
                self.w(format!("tag <{}> has no equivalent", name));
                self.out.push_str(&format!("{{# GSP-TODO tag <{}{}> #}}", name, attrs.iter().map(|(k, v)| format!(" {}=\"{}\"", k, v.replace("#}", "# }"))).collect::<String>()));
                if !selfclose {
                    self.stack.push((Blk::Close, format!("{{# GSP-TODO end of <{}> #}}", name)));
                }
            }
        }
    }
}

/// Wrap an emitted expression in parentheses unless it is already a single term.
fn paren(x: &str) -> String {
    // a bare number cannot take `.method()`
    if simple_str(x) && !x.chars().all(|c| c.is_ascii_digit() || c == '.') { x.to_string() } else { format!("({})", x) }
}

/// No space or filter bar outside brackets and quotes.
fn simple_str(s: &str) -> bool {
    if s.starts_with('(') && s.ends_with(')') && balanced_outer(s) {
        return true;
    }
    let mut d = 0i32;
    let mut q: Option<char> = None;
    let mut prev = ' ';
    for c in s.chars() {
        if let Some(qc) = q {
            if c == qc && prev != '\\' {
                q = None;
            }
        } else {
            match c {
                '"' | '\'' => q = Some(c),
                '(' | '[' | '{' => d += 1,
                ')' | ']' | '}' => d -= 1,
                ' ' | '|' if d == 0 => return false,
                _ => {}
            }
        }
        prev = if prev == '\\' && c == '\\' { ' ' } else { c };
    }
    true
}

/// Is the first '(' matched by the last ')'?
fn balanced_outer(s: &str) -> bool {
    let mut d = 0;
    let n = s.chars().count();
    let mut q: Option<char> = None;
    let mut prev = ' ';
    for (i, c) in s.chars().enumerate() {
        if let Some(qc) = q {
            if c == qc && prev != '\\' {
                q = None;
            }
        } else if c == '"' || c == '\'' {
            q = Some(c);
        } else if c == '(' {
            d += 1;
        } else if c == ')' {
            d -= 1;
            if d == 0 && i != n - 1 {
                return false;
            }
        }
        prev = c;
    }
    d == 0
}

/// Whitespace and `{# ... #}` comments only.
fn only_comments(s: &str) -> bool {
    let mut rest = s.trim_start();
    while let Some(r) = rest.strip_prefix("{#") {
        match r.find("#}") {
            Some(i) => rest = r[i + 2..].trim_start(),
            None => return false,
        }
    }
    rest.is_empty()
}

fn bin_has_string(e: &E) -> bool {
    match e {
        E::Str(_) | E::GStr(_) => true,
        E::Bin("+", a, b) => bin_has_string(a) || bin_has_string(b),
        _ => false,
    }
}

fn find(b: &[char], from: usize, pat: &str) -> Option<usize> {
    let p: Vec<char> = pat.chars().collect();
    (from..b.len().saturating_sub(p.len() - 1)).find(|&i| b[i..i + p.len()] == p[..])
}

/// Index of the '}' matching the '{' at `open`, skipping quoted strings.
fn match_brace(b: &[char], open: usize) -> Option<usize> {
    let mut d = 0;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            '\'' | '"' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == '\\' {
                        i += 1;
                    }
                    if q == '"' && b[i] == '$' && b.get(i + 1) == Some(&'{') {
                        i = match_brace(b, i + 1)?;
                    }
                    i += 1;
                }
            }
            '{' => d += 1,
            '}' => {
                d -= 1;
                if d == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}
