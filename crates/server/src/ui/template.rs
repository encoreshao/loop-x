//! Page templates extracted from the Python f-strings, and the small expression
//! evaluator that fills their slots.
//!
//! `tools/extract_ui_assets.py` turns an f-string such as `_render_shell`'s into
//! `assets/templates/<name>.tpl`: the literal text verbatim, every `{expr}`
//! replaced by U+27E6 `expr` U+27E7. [`Template::render`] asks a [`Scope`] for the
//! value of each distinct expression. A scope answers from, in order:
//!
//! 1. `exprs`: exact expression text -> value, for anything a page computes itself
//!    (`scope.set("_sidebar_html(active_page)", html)`);
//! 2. a mini evaluator for the Python subset translations need: string/int
//!    literals, variables (`scope.var("title", ..)` or `dashboard_server.<NAME>`
//!    constants by bare name), `_t('..', k=v)`, `i18n.t`, `json.dumps`,
//!    `html.escape(x[, quote=False])`, `str.upper()`, `a if b else c`, `in`,
//!    `==`, `!=`, `not`, `and`, `or`.
//!
//! So `{json.dumps(html.escape(_t('Cancel')))}` needs no Rust at all.

use super::i18n::{self, Lang};
use super::{assets, consts, pyfmt};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

const OPEN: char = '\u{27e6}';
const CLOSE: char = '\u{27e7}';

#[derive(Debug)]
enum Part {
    Lit(&'static str),
    Slot(&'static str),
}

#[derive(Debug)]
pub struct Template {
    name: String,
    parts: Vec<Part>,
}

impl Template {
    pub fn parse(name: &str, src: &'static str) -> Template {
        let mut parts = Vec::new();
        let mut rest = src;
        while let Some(open) = rest.find(OPEN) {
            if open > 0 {
                parts.push(Part::Lit(&rest[..open]));
            }
            let after = &rest[open + OPEN.len_utf8()..];
            let close = after.find(CLOSE).expect("unterminated template slot");
            parts.push(Part::Slot(&after[..close]));
            rest = &after[close + CLOSE.len_utf8()..];
        }
        if !rest.is_empty() {
            parts.push(Part::Lit(rest));
        }
        Template {
            name: name.to_string(),
            parts,
        }
    }

    /// The template `assets/templates/<name>.tpl` (parsed once).
    pub fn named(name: &str) -> &'static Template {
        static CACHE: OnceLock<Mutex<HashMap<String, &'static Template>>> = OnceLock::new();
        let mut cache = CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("template cache");
        if let Some(t) = cache.get(name) {
            return t;
        }
        let src = assets::text(&format!("templates/{name}.tpl"))
            .unwrap_or_else(|| panic!("template {name} missing: run tools/extract_ui_assets.py"));
        let leaked: &'static Template = Box::leak(Box::new(Template::parse(name, src)));
        cache.insert(name.to_string(), leaked);
        leaked
    }

    /// Every distinct slot expression, in first-use order.
    pub fn slots(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        for p in &self.parts {
            if let Part::Slot(s) = p {
                if !out.contains(s) {
                    out.push(s);
                }
            }
        }
        out
    }

    pub fn render(&self, scope: &Scope) -> Result<String, String> {
        let mut out = String::with_capacity(
            self.parts
                .iter()
                .map(|p| if let Part::Lit(l) = p { l.len() } else { 64 })
                .sum(),
        );
        let mut cache: HashMap<&str, String> = HashMap::new();
        for part in &self.parts {
            match part {
                Part::Lit(l) => out.push_str(l),
                Part::Slot(expr) => {
                    if let Some(v) = cache.get(expr) {
                        out.push_str(v);
                        continue;
                    }
                    let v = scope
                        .eval(expr)
                        .map_err(|e| format!("template {}: slot `{expr}`: {e}", self.name))?;
                    out.push_str(&v);
                    cache.insert(expr, v);
                }
            }
        }
        Ok(out)
    }
}

/// The values a template's slots resolve against.
pub struct Scope {
    pub lang: Lang,
    vars: HashMap<String, String>,
    exprs: HashMap<String, String>,
}

impl Scope {
    pub fn new(lang: Lang) -> Self {
        Scope {
            lang,
            vars: HashMap::new(),
            exprs: HashMap::new(),
        }
    }

    /// Define a variable the expressions may name.
    pub fn var(&mut self, name: &str, value: impl Into<String>) -> &mut Self {
        self.vars.insert(name.to_string(), value.into());
        self
    }

    /// Answer an exact expression text directly.
    pub fn set(&mut self, expr: &str, value: impl Into<String>) -> &mut Self {
        self.exprs.insert(expr.to_string(), value.into());
        self
    }

    pub fn eval(&self, expr: &str) -> Result<String, String> {
        if let Some(v) = self.exprs.get(expr) {
            return Ok(v.clone());
        }
        let ast = Parser::new(expr)?.parse_all()?;
        Ok(self.eval_ast(&ast)?.into_text())
    }

    fn eval_ast(&self, e: &Expr) -> Result<Val, String> {
        match e {
            Expr::Str(s) => Ok(Val::Str(s.clone())),
            Expr::Int(i) => Ok(Val::Int(*i)),
            Expr::Path(p) => self.resolve_path(p),
            Expr::Not(x) => Ok(Val::Bool(!self.eval_ast(x)?.truthy())),
            Expr::And(a, b) => {
                let a = self.eval_ast(a)?;
                if a.truthy() {
                    self.eval_ast(b)
                } else {
                    Ok(a)
                }
            }
            Expr::Or(a, b) => {
                let a = self.eval_ast(a)?;
                if a.truthy() {
                    Ok(a)
                } else {
                    self.eval_ast(b)
                }
            }
            Expr::IfElse(body, cond, orelse) => {
                if self.eval_ast(cond)?.truthy() {
                    self.eval_ast(body)
                } else {
                    self.eval_ast(orelse)
                }
            }
            Expr::Cmp(a, op, b) => {
                let (a, b) = (self.eval_ast(a)?, self.eval_ast(b)?);
                let r = match op {
                    CmpOp::Eq => a.text() == b.text(),
                    CmpOp::Ne => a.text() != b.text(),
                    CmpOp::In => b.text().contains(&a.text()),
                    CmpOp::NotIn => !b.text().contains(&a.text()),
                };
                Ok(Val::Bool(r))
            }
            Expr::Call(path, args, kwargs) => self.call(path, args, kwargs),
        }
    }

    fn resolve_path(&self, path: &[String]) -> Result<Val, String> {
        let full = path.join(".");
        match full.as_str() {
            "True" => return Ok(Val::Bool(true)),
            "False" => return Ok(Val::Bool(false)),
            "None" => return Ok(Val::Bool(false)),
            _ => {}
        }
        if let Some(v) = self.vars.get(&full) {
            return Ok(Val::Str(v.clone()));
        }
        if path.len() == 1 {
            if let Some(s) = consts::get_str("dashboard_server", &path[0]) {
                return Ok(Val::Str(s.to_string()));
            }
        }
        if path.len() == 2 {
            if let Some(s) = consts::get_str(&path[0], &path[1]) {
                return Ok(Val::Str(s.to_string()));
            }
        }
        Err(format!(
            "unknown name `{full}` (set it with Scope::var or Scope::set)"
        ))
    }

    fn call(
        &self,
        path: &[String],
        args: &[Expr],
        kwargs: &[(String, Expr)],
    ) -> Result<Val, String> {
        let full = path.join(".");
        let arg = |i: usize| -> Result<Val, String> {
            self.eval_ast(
                args.get(i)
                    .ok_or_else(|| format!("{full}: missing argument"))?,
            )
        };
        match full.as_str() {
            "_t" | "i18n.t" => {
                let text = arg(0)?.text();
                let mut pairs = Vec::new();
                for (k, v) in kwargs {
                    pairs.push((k.clone(), self.eval_ast(v)?.text()));
                }
                let refs: Vec<(&str, &str)> = pairs
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                Ok(Val::Str(i18n::t(self.lang, &text, &refs)))
            }
            "json.dumps" => Ok(Val::Str(pyfmt::json_str(&arg(0)?.text()))),
            "html.escape" => {
                let quote = match kwargs.iter().find(|(k, _)| k == "quote") {
                    Some((_, v)) => self.eval_ast(v)?.truthy(),
                    None => match args.get(1) {
                        Some(v) => self.eval_ast(v)?.truthy(),
                        None => true,
                    },
                };
                Ok(Val::Str(pyfmt::escape_with(&arg(0)?.text(), quote)))
            }
            "i18n.html_lang" => {
                let code = arg(0)?.text();
                Ok(Val::Str(
                    Lang::from_code(&code).map_or(code.clone(), |l| l.html_lang().to_string()),
                ))
            }
            "str" => Ok(Val::Str(arg(0)?.text())),
            _ => {
                if let Some((method, base)) = path.split_last() {
                    if !base.is_empty() {
                        if let Ok(v) = self.resolve_path(base) {
                            let s = v.text();
                            return match method.as_str() {
                                "upper" => Ok(Val::Str(s.to_uppercase())),
                                "lower" => Ok(Val::Str(s.to_lowercase())),
                                "strip" => Ok(Val::Str(pyfmt::strip(&s).to_string())),
                                other => Err(format!("unsupported method `{other}`")),
                            };
                        }
                    }
                }
                Err(format!(
                    "unsupported call `{full}(..)`: compute it in Rust and Scope::set the whole expression"
                ))
            }
        }
    }
}

#[derive(Debug, Clone)]
enum Val {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl Val {
    fn text(&self) -> String {
        match self {
            Val::Str(s) => s.clone(),
            Val::Int(i) => i.to_string(),
            Val::Bool(true) => "True".to_string(),
            Val::Bool(false) => "False".to_string(),
        }
    }

    fn into_text(self) -> String {
        self.text()
    }

    fn truthy(&self) -> bool {
        match self {
            Val::Str(s) => !s.is_empty(),
            Val::Int(i) => *i != 0,
            Val::Bool(b) => *b,
        }
    }
}

#[derive(Debug)]
enum CmpOp {
    Eq,
    Ne,
    In,
    NotIn,
}

#[derive(Debug)]
enum Expr {
    Str(String),
    Int(i64),
    Path(Vec<String>),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    IfElse(Box<Expr>, Box<Expr>, Box<Expr>),
    Cmp(Box<Expr>, CmpOp, Box<Expr>),
    Call(Vec<String>, Vec<Expr>, Vec<(String, Expr)>),
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Str(String),
    Int(i64),
    Ident(String),
    Sym(&'static str),
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '\'' || c == '"' {
            let quote = c;
            i += 1;
            let mut s = String::new();
            loop {
                let ch = *chars.get(i).ok_or("unterminated string")?;
                i += 1;
                if ch == quote {
                    break;
                }
                if ch != '\\' {
                    s.push(ch);
                    continue;
                }
                let esc = *chars.get(i).ok_or("bad escape")?;
                i += 1;
                match esc {
                    'n' => s.push('\n'),
                    't' => s.push('\t'),
                    'r' => s.push('\r'),
                    '\\' => s.push('\\'),
                    '\'' => s.push('\''),
                    '"' => s.push('"'),
                    'x' | 'u' | 'U' => {
                        let len = match esc {
                            'x' => 2,
                            'u' => 4,
                            _ => 8,
                        };
                        let hex: String = chars[i..(i + len).min(chars.len())].iter().collect();
                        i += len;
                        let code = u32::from_str_radix(&hex, 16).map_err(|_| "bad escape")?;
                        s.push(char::from_u32(code).ok_or("bad escape")?);
                    }
                    other => {
                        s.push('\\');
                        s.push(other);
                    }
                }
            }
            out.push(Tok::Str(s));
        } else if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let n: String = chars[start..i].iter().collect();
            out.push(Tok::Int(n.parse().map_err(|_| "bad number")?));
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(Tok::Ident(chars[start..i].iter().collect()));
        } else {
            let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
            let sym = match two.as_str() {
                "==" => Some("=="),
                "!=" => Some("!="),
                _ => None,
            };
            if let Some(s) = sym {
                out.push(Tok::Sym(s));
                i += 2;
                continue;
            }
            let s = match c {
                '(' => "(",
                ')' => ")",
                ',' => ",",
                '.' => ".",
                '=' => "=",
                _ => return Err(format!("unsupported character `{c}`")),
            };
            out.push(Tok::Sym(s));
            i += 1;
        }
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn new(src: &str) -> Result<Self, String> {
        Ok(Parser {
            toks: lex(src)?,
            pos: 0,
        })
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn eat_sym(&mut self, s: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Sym(x)) if *x == s) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Ident(x)) if x == kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn parse_all(mut self) -> Result<Expr, String> {
        let e = self.expr()?;
        if self.pos != self.toks.len() {
            return Err(format!("unexpected token {:?}", self.toks[self.pos]));
        }
        Ok(e)
    }

    fn expr(&mut self) -> Result<Expr, String> {
        let body = self.or()?;
        if self.eat_kw("if") {
            let cond = self.or()?;
            if !self.eat_kw("else") {
                return Err("expected `else`".into());
            }
            let orelse = self.expr()?;
            return Ok(Expr::IfElse(
                Box::new(body),
                Box::new(cond),
                Box::new(orelse),
            ));
        }
        Ok(body)
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut left = self.and()?;
        while self.eat_kw("or") {
            left = Expr::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut left = self.not()?;
        while self.eat_kw("and") {
            left = Expr::And(Box::new(left), Box::new(self.not()?));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr, String> {
        if self.eat_kw("not") {
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.cmp()
    }

    fn cmp(&mut self) -> Result<Expr, String> {
        let left = self.atom()?;
        let op = if self.eat_sym("==") {
            CmpOp::Eq
        } else if self.eat_sym("!=") {
            CmpOp::Ne
        } else if self.eat_kw("in") {
            CmpOp::In
        } else if matches!(self.peek(), Some(Tok::Ident(x)) if x == "not")
            && matches!(self.toks.get(self.pos + 1), Some(Tok::Ident(x)) if x == "in")
        {
            self.pos += 2;
            CmpOp::NotIn
        } else {
            return Ok(left);
        };
        Ok(Expr::Cmp(Box::new(left), op, Box::new(self.atom()?)))
    }

    fn atom(&mut self) -> Result<Expr, String> {
        match self.peek().cloned() {
            Some(Tok::Str(s)) => {
                self.pos += 1;
                Ok(Expr::Str(s))
            }
            Some(Tok::Int(i)) => {
                self.pos += 1;
                Ok(Expr::Int(i))
            }
            Some(Tok::Sym("(")) => {
                self.pos += 1;
                let e = self.expr()?;
                if !self.eat_sym(")") {
                    return Err("expected `)`".into());
                }
                Ok(e)
            }
            Some(Tok::Ident(first)) => {
                self.pos += 1;
                let mut path = vec![first];
                while self.eat_sym(".") {
                    match self.peek().cloned() {
                        Some(Tok::Ident(n)) => {
                            self.pos += 1;
                            path.push(n);
                        }
                        _ => return Err("expected a name after `.`".into()),
                    }
                }
                if self.eat_sym("(") {
                    let (mut args, mut kwargs) = (Vec::new(), Vec::new());
                    while !self.eat_sym(")") {
                        let named = matches!(
                            (self.toks.get(self.pos), self.toks.get(self.pos + 1)),
                            (Some(Tok::Ident(_)), Some(Tok::Sym("=")))
                        );
                        if named {
                            let Some(Tok::Ident(k)) = self.peek().cloned() else {
                                unreachable!()
                            };
                            self.pos += 2;
                            kwargs.push((k, self.expr()?));
                        } else {
                            args.push(self.expr()?);
                        }
                        if !self.eat_sym(",") && !matches!(self.peek(), Some(Tok::Sym(")"))) {
                            return Err("expected `,` or `)`".into());
                        }
                    }
                    Ok(Expr::Call(path, args, kwargs))
                } else {
                    Ok(Expr::Path(path))
                }
            }
            other => Err(format!("unexpected {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(lang: Lang) -> Scope {
        let mut s = Scope::new(lang);
        s.var("current_lang", "ja")
            .var("status_badge_html", "<x class='md-spinner'>");
        s
    }

    #[test]
    fn translation_expressions_evaluate_like_python() {
        let s = scope(Lang::Fr);
        assert_eq!(s.eval("html.escape(_t('Cancel'))").unwrap(), "Annuler");
        assert_eq!(
            s.eval("json.dumps(_t('auto-refreshes every {interval}', interval='30s'))")
                .unwrap(),
            "\"actualisation auto toutes les 30s\""
        );
        assert_eq!(
            s.eval("json.dumps(html.escape(_t(\"Couldn't load this section.\"), quote=False))")
                .unwrap(),
            serde_json::to_string(&pyfmt::escape_text(&i18n::t(
                Lang::Fr,
                "Couldn't load this section.",
                &[]
            )))
            .unwrap()
            .replace('é', "\\u00e9")
            .replace('è', "\\u00e8")
        );
    }

    #[test]
    fn variables_methods_and_conditionals() {
        let s = scope(Lang::En);
        assert_eq!(s.eval("current_lang.upper()").unwrap(), "JA");
        assert_eq!(
            s.eval("' is-active' if 'md-spinner' in status_badge_html else ''")
                .unwrap(),
            " is-active"
        );
        assert_eq!(s.eval("i18n.COOKIE_NAME").unwrap(), "loop_lang");
        assert_eq!(s.eval("i18n.html_lang(current_lang)").unwrap(), "ja");
        assert!(s.eval("nope").unwrap_err().contains("unknown name"));
    }

    #[test]
    fn exact_expression_overrides_win() {
        let mut s = scope(Lang::En);
        s.set("_sidebar_html(active_page)", "<nav/>");
        assert_eq!(s.eval("_sidebar_html(active_page)").unwrap(), "<nav/>");
    }

    #[test]
    fn templates_split_literals_and_slots() {
        let t = Template::parse(
            "t",
            Box::leak(format!("a{OPEN}x{CLOSE}b{OPEN}x{CLOSE}").into_boxed_str()),
        );
        assert_eq!(t.slots(), vec!["x"]);
        let mut s = Scope::new(Lang::En);
        s.var("x", "1");
        assert_eq!(t.render(&s).unwrap(), "a1b1");
    }
}
