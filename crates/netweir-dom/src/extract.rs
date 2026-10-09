//! Declarative extraction: an item's fields and a crawl's link rules,
//! compiled once and run on every page without leaving Rust.

use regex::Regex;

use crate::document::{Node, NodeKind};
use crate::query::{Hit, Query};
use crate::xpath::XPath;

/// A compiled CSS (with `::text`/`::attr()`) or XPath query.
pub enum Selector {
    Css(Query),
    XPath(XPath),
}

impl Selector {
    pub fn css(query: &str) -> Result<Selector, String> {
        Query::new(query)
            .map(Selector::Css)
            .map_err(|e| e.to_string())
    }

    pub fn xpath(query: &str) -> Result<Selector, String> {
        XPath::new(query)
            .map(Selector::XPath)
            .map_err(|e| e.to_string())
    }

    fn hits<'a>(&self, scope: Node<'a>) -> Result<Vec<Hit<'a>>, String> {
        match self {
            Selector::Css(q) => Ok(q.run(scope)),
            Selector::XPath(x) => x.run(scope).map_err(|e| e.to_string()),
        }
    }
}

/// What a field's text is turned into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Convert {
    Text,
    Int,
    Float,
    /// `true`/`false`, `yes`/`no`, `on`/`off` or `1`/`0`, in any case.
    Bool,
}

/// One field's value on one page.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Nothing matched (or the pattern found nothing).
    Missing,
    Text(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    /// With `all`, one value per result.
    List(Vec<Value>),
    /// The text that would not convert, or why the query failed.
    Invalid(String),
}

/// A named query and what to do with what it finds.
pub struct Field {
    name: String,
    selector: Selector,
    re: Option<Regex>,
    all: bool,
    strip: bool,
    convert: Convert,
}

impl Field {
    pub fn new(name: &str, selector: Selector) -> Field {
        Field {
            name: name.to_string(),
            selector,
            re: None,
            all: false,
            strip: false,
            convert: Convert::Text,
        }
    }

    /// Keeps only what `pattern` matches in each result: its first group if
    /// it has groups, otherwise the whole match.
    pub fn re(mut self, pattern: &str) -> Result<Field, String> {
        let re = Regex::new(pattern).map_err(|e| {
            // The regex crate's message spans lines; the last one says why.
            let reason = e.to_string();
            let reason = reason.lines().last().unwrap_or("").trim().to_string();
            format!("bad regular expression {pattern:?}: {reason}")
        })?;
        self.re = Some(re);
        Ok(self)
    }

    /// Every result as a list, instead of the first.
    pub fn all(mut self, all: bool) -> Field {
        self.all = all;
        self
    }

    /// Trims surrounding whitespace from each result.
    pub fn strip(mut self, strip: bool) -> Field {
        self.strip = strip;
        self
    }

    pub fn convert(mut self, convert: Convert) -> Field {
        self.convert = convert;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn texts(&self, scope: Node<'_>) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        for hit in self.selector.hits(scope)? {
            let text = hit.to_string_value();
            match &self.re {
                Some(re) => {
                    for caps in re.captures_iter(&text) {
                        let m = caps.get(1).or_else(|| caps.get(0));
                        out.push(m.map_or("", |m| m.as_str()).to_string());
                        if !self.all {
                            return Ok(out);
                        }
                    }
                }
                None => {
                    out.push(text);
                    if !self.all {
                        return Ok(out);
                    }
                }
            }
        }
        Ok(out)
    }

    fn value(&self, text: String) -> Value {
        let text = if self.strip {
            text.trim().to_string()
        } else {
            text
        };
        let trimmed = text.trim();
        match self.convert {
            Convert::Text => Value::Text(text),
            Convert::Int => trimmed.parse().map_or(Value::Invalid(text), Value::Int),
            Convert::Float => trimmed.parse().map_or(Value::Invalid(text), Value::Float),
            Convert::Bool => match trimmed.to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" | "1" => Value::Bool(true),
                "false" | "no" | "off" | "0" => Value::Bool(false),
                _ => Value::Invalid(text),
            },
        }
    }

    pub fn extract(&self, scope: Node<'_>) -> Value {
        let texts = match self.texts(scope) {
            Ok(t) => t,
            Err(e) => return Value::Invalid(e),
        };
        if self.all {
            Value::List(texts.into_iter().map(|t| self.value(t)).collect())
        } else {
            texts
                .into_iter()
                .next()
                .map_or(Value::Missing, |t| self.value(t))
        }
    }
}

/// An item's fields, in order.
pub struct ItemSpec {
    fields: Vec<Field>,
}

impl ItemSpec {
    pub fn new(fields: Vec<Field>) -> ItemSpec {
        ItemSpec { fields }
    }

    pub fn names(&self) -> Vec<&str> {
        self.fields.iter().map(Field::name).collect()
    }

    /// One value per field, in the fields' order.
    pub fn extract(&self, scope: Node<'_>) -> Vec<Value> {
        self.fields.iter().map(|f| f.extract(scope)).collect()
    }
}

/// The links `selector` finds below `scope`, as written in the page: an
/// element's `href`, or the string itself when the query asks for strings
/// (`::attr(href)`, `@href`). Elements without an `href` are skipped.
pub fn links(selector: &Selector, scope: Node<'_>) -> Result<Vec<String>, String> {
    Ok(selector
        .hits(scope)?
        .into_iter()
        .filter_map(|hit| match hit {
            Hit::Node(n) if n.kind() == NodeKind::Element => n.attr("href").map(str::to_string),
            other => Some(other.to_string_value()),
        })
        .collect())
}
