//! Expressions this walk evaluates.
//!
//! A closed jq subset, with streams inside filters and a single JSON value at
//! the workflow boundary. Array constructors collect streams. Unsupported
//! syntax and uncollected zero/multiple results are expression errors.
//!
//! `set` strings are literals unless they contain `${ ... }`. A string whose
//! whole value is one interpolation becomes that value. Other interpolations
//! are text. `if`, `when`, `for.in`, `while`, `input.from`, `output.as`, and
//! `export.as` are expressions, not interpolated strings.

use std::collections::BTreeMap;

use serde_json::Map;
use serde_json::Number;
use serde_json::Value;

pub(crate) fn evaluate(
    source: &str,
    dot: &Value,
    vars: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let expr = parse(source).map_err(|message| format!("{message} in `{source}`"))?;
    let mut values = eval(&expr, dot, vars)?;
    if values.len() != 1 {
        return Err(format!(
            "expression must produce one value, got {} in `{source}`",
            values.len()
        ));
    }
    Ok(values.remove(0))
}

pub(crate) fn evaluate_data(
    value: &Value,
    dot: &Value,
    vars: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    match value {
        Value::String(text) => evaluate_string(text, dot, vars),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(evaluate_data(item, dot, vars)?);
            }
            Ok(Value::Array(out))
        }
        Value::Object(object) => {
            let mut out = Map::new();
            for (key, child) in object {
                out.insert(key.clone(), evaluate_data(child, dot, vars)?);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

fn evaluate_string(
    text: &str,
    dot: &Value,
    vars: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    if let Some(inner) = sole_interpolation(text) {
        return evaluate(inner, dot, vars);
    }
    if !text.contains("${") {
        return Ok(Value::String(text.to_string()));
    }
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = interpolation_end(after)?;
        let value = evaluate(after[..end].trim(), dot, vars)?;
        out.push_str(&stringify(&value));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(Value::String(out))
}

pub(crate) fn runtime_expression(text: &str) -> Option<&str> {
    sole_interpolation(text)
}

fn sole_interpolation(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix("${")?;
    let end = interpolation_end(rest).ok()?;
    if end + 1 != rest.len() {
        return None;
    }
    Some(rest[..end].trim())
}

/// `source` begins just after `${`. Returns the index of the closing `}`.
fn interpolation_end(source: &str) -> Result<usize, String> {
    let mut depth = 1i32;
    let mut in_string = false;
    let mut escape = false;
    for (index, ch) in source.char_indices() {
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(index);
                }
            }
            _ => {}
        }
    }
    Err("unclosed `${`".to_string())
}

fn stringify(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[derive(Debug)]
enum Expr {
    Dot,
    Var(String),
    Field(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Lit(Value),
    Array(Vec<Expr>),
    Object(Vec<(String, Expr)>),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Pipe(Box<Expr>, Box<Expr>),
    Iterate(Box<Expr>),
    Function(String, Option<Box<Expr>>),
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    Default(Box<Expr>, Box<Expr>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BinOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    And,
    Or,
}

fn parse(source: &str) -> Result<Expr, String> {
    let mut parser = Parser { source, index: 0 };
    let expr = parser.parse_pipe()?;
    parser.skip();
    if parser.index != parser.source.len() {
        return Err(format!("unexpected `{}`", parser.snippet()));
    }
    Ok(expr)
}

struct Parser<'a> {
    source: &'a str,
    index: usize,
}

impl<'a> Parser<'a> {
    fn parse_pipe(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_default()?;
        while self.eat_op("|") {
            let right = self.parse_default()?;
            left = Expr::Pipe(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_default(&mut self) -> Result<Expr, String> {
        let left = self.parse_or()?;
        if self.eat_op("//") {
            return Ok(Expr::Default(
                Box::new(left),
                Box::new(self.parse_default()?),
            ));
        }
        Ok(left)
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and()?;
        while self.eat_op("or") {
            let right = self.parse_and()?;
            left = Expr::Binary(BinOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_cmp()?;
        while self.eat_op("and") {
            let right = self.parse_cmp()?;
            left = Expr::Binary(BinOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_cmp(&mut self) -> Result<Expr, String> {
        let left = self.parse_sum()?;
        let op = if self.eat_op("==") {
            BinOp::Eq
        } else if self.eat_op("!=") {
            BinOp::Ne
        } else if self.eat_op("<=") {
            BinOp::Le
        } else if self.eat_op(">=") {
            BinOp::Ge
        } else if self.eat_op("<") {
            BinOp::Lt
        } else if self.eat_op(">") {
            BinOp::Gt
        } else {
            return Ok(left);
        };
        let right = self.parse_sum()?;
        Ok(Expr::Binary(op, Box::new(left), Box::new(right)))
    }

    fn parse_sum(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_prod()?;
        loop {
            let op = if self.eat_op("+") {
                BinOp::Add
            } else if self.eat_op("-") {
                BinOp::Sub
            } else {
                break;
            };
            let right = self.parse_prod()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_prod(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_unary()?;
        loop {
            let op = if self.eat_op("*") {
                BinOp::Mul
            } else if self.eat_op("/") {
                BinOp::Div
            } else {
                break;
            };
            let right = self.parse_unary()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        if self.eat_op("not") {
            return Ok(Expr::Not(Box::new(self.parse_unary()?)));
        }
        if self.eat_op("-") {
            return Ok(Expr::Neg(Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        self.skip();
        if self.eat_keyword("if") {
            let condition = self.parse_pipe()?;
            if !self.eat_keyword("then") {
                return Err("expected `then`".to_string());
            }
            let yes = self.parse_pipe()?;
            if !self.eat_keyword("else") {
                return Err("expected `else`".to_string());
            }
            let no = self.parse_pipe()?;
            if !self.eat_keyword("end") {
                return Err("expected `end`".to_string());
            }
            return Ok(Expr::If(Box::new(condition), Box::new(yes), Box::new(no)));
        }
        if self.eat_char('(') {
            let expr = self.parse_pipe()?;
            if !self.eat_char(')') {
                return Err("expected `)`".to_string());
            }
            return Ok(expr);
        }
        if self.eat_char('[') {
            return self.parse_array();
        }
        if self.eat_char('{') {
            return self.parse_object();
        }
        if self.eat_keyword("true") {
            return Ok(Expr::Lit(Value::Bool(true)));
        }
        if self.eat_keyword("false") {
            return Ok(Expr::Lit(Value::Bool(false)));
        }
        if self.eat_keyword("null") {
            return Ok(Expr::Lit(Value::Null));
        }
        if self.peek() == Some('"') {
            return Ok(Expr::Lit(Value::String(self.parse_string()?)));
        }
        if self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            return Ok(Expr::Lit(self.parse_number()?));
        }
        if self.peek() == Some('.') || self.peek() == Some('$') {
            return self.parse_path();
        }
        if let Some(name) = self.parse_ident() {
            let takes_argument = match name.as_str() {
                "map" | "select" | "has" | "split" | "join" | "startswith" | "endswith"
                | "contains" => true,
                "length" | "keys" | "add" | "tostring" | "tonumber" | "ascii_downcase" => false,
                _ => return Err(format!("unsupported function `{name}`")),
            };
            let argument = if takes_argument {
                if !self.eat_char('(') {
                    return Err(format!("`{name}` expects an argument"));
                }
                let argument = self.parse_pipe()?;
                if !self.eat_char(')') {
                    return Err("expected `)`".to_string());
                }
                Some(Box::new(argument))
            } else {
                None
            };
            return Ok(Expr::Function(name, argument));
        }
        Err(format!("expected a value, found `{}`", self.snippet()))
    }

    fn parse_array(&mut self) -> Result<Expr, String> {
        let mut items = Vec::new();
        self.skip();
        if self.eat_char(']') {
            return Ok(Expr::Array(items));
        }
        loop {
            items.push(self.parse_pipe()?);
            self.skip();
            if self.eat_char(']') {
                return Ok(Expr::Array(items));
            }
            if !self.eat_char(',') {
                return Err("expected `,` or `]`".to_string());
            }
            self.skip();
            if self.eat_char(']') {
                return Ok(Expr::Array(items));
            }
        }
    }

    fn parse_object(&mut self) -> Result<Expr, String> {
        let mut fields = Vec::new();
        self.skip();
        if self.eat_char('}') {
            return Ok(Expr::Object(fields));
        }
        loop {
            let key = self.parse_key()?;
            if !self.eat_char(':') {
                return Err("expected `:`".to_string());
            }
            fields.push((key, self.parse_pipe()?));
            self.skip();
            if self.eat_char('}') {
                return Ok(Expr::Object(fields));
            }
            if !self.eat_char(',') {
                return Err("expected `,` or `}`".to_string());
            }
            self.skip();
            if self.eat_char('}') {
                return Ok(Expr::Object(fields));
            }
        }
    }

    fn parse_key(&mut self) -> Result<String, String> {
        self.skip();
        if self.peek() == Some('"') {
            return self.parse_string();
        }
        self.parse_ident()
            .ok_or_else(|| format!("expected a field name, found `{}`", self.snippet()))
    }

    fn parse_path(&mut self) -> Result<Expr, String> {
        let mut expr = if self.eat_char('.') {
            Expr::Dot
        } else if self.eat_char('$') {
            let name = self
                .parse_ident()
                .ok_or_else(|| "expected a variable name".to_string())?;
            Expr::Var(name)
        } else {
            return Err("expected a path".to_string());
        };
        if matches!(expr, Expr::Dot)
            && self.peek().is_some_and(is_ident_start)
            && let Some(name) = self.parse_ident()
        {
            expr = Expr::Field(Box::new(expr), name);
        }
        loop {
            if self.eat_char('.') {
                let name = self
                    .parse_ident()
                    .ok_or_else(|| "expected a field name".to_string())?;
                expr = Expr::Field(Box::new(expr), name);
            } else if self.eat_char('[') {
                if self.eat_char(']') {
                    expr = Expr::Iterate(Box::new(expr));
                    continue;
                }
                let index = self.parse_pipe()?;
                if !self.eat_char(']') {
                    return Err("expected `]`".to_string());
                }
                expr = Expr::Index(Box::new(expr), Box::new(index));
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_ident(&mut self) -> Option<String> {
        self.skip();
        let rest = &self.source[self.index..];
        let mut chars = rest.chars();
        let first = chars.next()?;
        if !is_ident_start(first) {
            return None;
        }
        let mut len = first.len_utf8();
        for ch in chars {
            if !is_ident_continue(ch) {
                break;
            }
            len += ch.len_utf8();
        }
        let name = rest[..len].to_string();
        self.index += len;
        Some(name)
    }

    fn parse_string(&mut self) -> Result<String, String> {
        if !self.eat_char('"') {
            return Err("expected a string".to_string());
        }
        let mut out = String::new();
        loop {
            let Some(ch) = self.bump() else {
                return Err("unterminated string".to_string());
            };
            match ch {
                '"' => return Ok(out),
                '\\' => {
                    let Some(escaped) = self.bump() else {
                        return Err("unterminated string escape".to_string());
                    };
                    match escaped {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => {
                            let hex = self.bump_hex()?;
                            let code = u32::from_str_radix(&hex, 16)
                                .map_err(|_| "invalid `\\u` escape".to_string())?;
                            let ch = char::from_u32(code)
                                .ok_or_else(|| "invalid `\\u` escape".to_string())?;
                            out.push(ch);
                        }
                        other => return Err(format!("unknown string escape `\\{other}`")),
                    }
                }
                other => out.push(other),
            }
        }
    }

    fn bump_hex(&mut self) -> Result<String, String> {
        let mut hex = String::new();
        for _ in 0..4 {
            let Some(ch) = self.bump() else {
                return Err("short `\\u` escape".to_string());
            };
            if !ch.is_ascii_hexdigit() {
                return Err("invalid `\\u` escape".to_string());
            }
            hex.push(ch);
        }
        Ok(hex)
    }

    fn parse_number(&mut self) -> Result<Value, String> {
        let start = self.index;
        self.scan_digits();
        if self.peek() == Some('.') {
            let save = self.index;
            self.bump();
            if self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                self.scan_digits();
            } else {
                self.index = save;
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            let save = self.index;
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            if self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                self.scan_digits();
            } else {
                self.index = save;
            }
        }
        let text = &self.source[start..self.index];
        if text.contains(['.', 'e', 'E']) {
            let number: f64 = text
                .parse()
                .map_err(|_| format!("invalid number `{text}`"))?;
            Number::from_f64(number)
                .map(Value::Number)
                .ok_or_else(|| format!("invalid number `{text}`"))
        } else {
            let number: i64 = text
                .parse()
                .map_err(|_| format!("invalid number `{text}`"))?;
            Ok(Value::from(number))
        }
    }

    fn scan_digits(&mut self) {
        while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            self.bump();
        }
    }

    fn eat_op(&mut self, op: &str) -> bool {
        self.skip();
        if !self.source[self.index..].starts_with(op) {
            return false;
        }
        let after = self.index + op.len();
        if op == "/" && self.source[after..].starts_with('/') {
            return false;
        }
        if matches!(op, "<" | ">" | "!" | "=") && self.source[after..].starts_with('=') {
            return false;
        }
        if op.chars().all(|ch| ch.is_ascii_alphabetic())
            && self.source[after..]
                .chars()
                .next()
                .is_some_and(is_ident_continue)
        {
            return false;
        }
        self.index = after;
        true
    }

    fn eat_keyword(&mut self, word: &str) -> bool {
        self.eat_op(word)
    }

    fn eat_char(&mut self, expected: char) -> bool {
        self.skip();
        if self.peek() == Some(expected) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn skip(&mut self) {
        while self.peek().is_some_and(|ch| ch.is_whitespace()) {
            self.bump();
        }
    }

    fn peek(&self) -> Option<char> {
        self.source[self.index..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.index += ch.len_utf8();
        Some(ch)
    }

    fn snippet(&self) -> String {
        let rest = self.source[self.index..]
            .chars()
            .take(24)
            .collect::<String>();
        if rest.is_empty() {
            "end".to_string()
        } else {
            rest
        }
    }
}

fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn eval(expr: &Expr, dot: &Value, vars: &BTreeMap<String, Value>) -> Result<Vec<Value>, String> {
    match expr {
        Expr::Dot => Ok(vec![dot.clone()]),
        Expr::Var(name) => vars
            .get(name)
            .cloned()
            .map(|value| vec![value])
            .ok_or_else(|| format!("unknown variable `${name}`")),
        Expr::Field(base, name) => eval(base, dot, vars)?
            .into_iter()
            .map(|value| match value {
                Value::Object(object) => Ok(object.get(name).cloned().unwrap_or(Value::Null)),
                Value::Null => Ok(Value::Null),
                _ => Err(format!("cannot read field `{name}` on a non-object")),
            })
            .collect(),
        Expr::Index(base, index) => {
            let mut out = Vec::new();
            for value in eval(base, dot, vars)? {
                for index in eval(index, dot, vars)? {
                    out.push(index_value(&value, &index)?);
                }
            }
            Ok(out)
        }
        Expr::Lit(value) => Ok(vec![value.clone()]),
        Expr::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.extend(eval(item, dot, vars)?);
            }
            Ok(vec![Value::Array(out)])
        }
        Expr::Object(fields) => {
            let mut objects = vec![Map::new()];
            for (key, value) in fields {
                let values = eval(value, dot, vars)?;
                let mut next = Vec::new();
                for object in objects {
                    for value in &values {
                        let mut object = object.clone();
                        object.insert(key.clone(), value.clone());
                        next.push(object);
                    }
                }
                objects = next;
            }
            Ok(objects.into_iter().map(Value::Object).collect())
        }
        Expr::Not(inner) => eval(inner, dot, vars)?
            .into_iter()
            .map(|value| match value {
                Value::Bool(flag) => Ok(Value::Bool(!flag)),
                _ => Err("`not` expects a boolean".to_string()),
            })
            .collect(),
        Expr::Neg(inner) => eval(inner, dot, vars)?
            .into_iter()
            .map(|value| match as_num(&value) {
                Some(Num::Int(number)) => number
                    .checked_neg()
                    .map(Value::from)
                    .ok_or_else(|| "numeric overflow".to_string()),
                Some(Num::Float(number)) => finite_number(-number),
                None => Err("`-` expects a number".to_string()),
            })
            .collect(),
        Expr::Binary(op, left, right) => {
            let mut out = Vec::new();
            for left in eval(left, dot, vars)? {
                if matches!(op, BinOp::And | BinOp::Or) {
                    let Value::Bool(flag) = left else {
                        return Err(format!("`{}` expects a boolean", bin_name(*op)));
                    };
                    if (*op == BinOp::And && !flag) || (*op == BinOp::Or && flag) {
                        out.push(Value::Bool(flag));
                        continue;
                    }
                }
                for right in eval(right, dot, vars)? {
                    out.push(eval_binary(*op, &left, &right)?);
                }
            }
            Ok(out)
        }
        Expr::Pipe(left, right) => {
            let mut out = Vec::new();
            for value in eval(left, dot, vars)? {
                out.extend(eval(right, &value, vars)?);
            }
            Ok(out)
        }
        Expr::Iterate(base) => {
            let mut out = Vec::new();
            for value in eval(base, dot, vars)? {
                out.extend(iterate(value)?);
            }
            Ok(out)
        }
        Expr::If(condition, yes, no) => {
            let mut out = Vec::new();
            for value in eval(condition, dot, vars)? {
                out.extend(eval(if truthy(&value) { yes } else { no }, dot, vars)?);
            }
            Ok(out)
        }
        Expr::Default(left, right) => {
            let values: Vec<_> = eval(left, dot, vars)?.into_iter().filter(truthy).collect();
            if values.is_empty() {
                eval(right, dot, vars)
            } else {
                Ok(values)
            }
        }
        Expr::Function(name, argument) => {
            if let Some(argument) = argument {
                if name == "map" {
                    let mut out = Vec::new();
                    for value in iterate(dot.clone())? {
                        out.extend(eval(argument, &value, vars)?);
                    }
                    return Ok(vec![Value::Array(out)]);
                }
                let mut out = Vec::new();
                for value in eval(argument, dot, vars)? {
                    if name == "select" {
                        if truthy(&value) {
                            out.push(dot.clone());
                        }
                    } else {
                        out.push(eval_function(name, dot, Some(&value))?);
                    }
                }
                Ok(out)
            } else {
                Ok(vec![eval_function(name, dot, None)?])
            }
        }
    }
}

fn eval_binary(op: BinOp, left: &Value, right: &Value) -> Result<Value, String> {
    match op {
        BinOp::Eq => Ok(Value::Bool(left == right)),
        BinOp::Ne => Ok(Value::Bool(left != right)),
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => compare(op, left, right),
        BinOp::Add => add(left, right),
        BinOp::Sub | BinOp::Mul | BinOp::Div => arith(op, left, right),
        BinOp::And | BinOp::Or => match right {
            Value::Bool(_) => Ok(right.clone()),
            _ => Err(format!("`{}` expects a boolean", bin_name(op))),
        },
    }
}

fn truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
}

fn iterate(value: Value) -> Result<Vec<Value>, String> {
    match value {
        Value::Array(items) => Ok(items),
        Value::Object(object) => Ok(object.into_values().collect()),
        _ => Err("iteration expects an array or object".to_string()),
    }
}

fn eval_function(name: &str, dot: &Value, argument: Option<&Value>) -> Result<Value, String> {
    match (name, dot, argument) {
        ("length", Value::Null, _) => Ok(Value::from(0)),
        ("length", Value::String(text), _) => Ok(Value::from(text.chars().count())),
        ("length", Value::Array(items), _) => Ok(Value::from(items.len())),
        ("length", Value::Object(object), _) => Ok(Value::from(object.len())),
        ("length", Value::Number(number), _) if number.is_u64() => Ok(dot.clone()),
        ("length", Value::Number(_), _) => match as_num(dot) {
            Some(Num::Int(number)) => Ok(Value::from(number.unsigned_abs())),
            Some(Num::Float(number)) => finite_number(number.abs()),
            None => Err("invalid number".to_string()),
        },
        ("has", Value::Object(object), Some(Value::String(key))) => {
            Ok(Value::Bool(object.contains_key(key)))
        }
        ("has", Value::Array(items), Some(Value::Number(index))) => {
            let index = index
                .as_i64()
                .ok_or_else(|| "`has` expects an integer array index".to_string())?;
            Ok(Value::Bool(
                usize::try_from(index).is_ok_and(|index| index < items.len()),
            ))
        }
        ("keys", Value::Object(object), _) => {
            let mut keys: Vec<_> = object.keys().cloned().collect();
            keys.sort();
            Ok(Value::Array(keys.into_iter().map(Value::String).collect()))
        }
        ("keys", Value::Array(items), _) => {
            Ok(Value::Array((0..items.len()).map(Value::from).collect()))
        }
        ("add", Value::Array(_) | Value::Object(_), _) => {
            let mut sum = Value::Null;
            for value in iterate(dot.clone())? {
                sum = add(&sum, &value)?;
            }
            Ok(sum)
        }
        ("tostring", _, _) => Ok(Value::String(stringify(dot))),
        ("tonumber", Value::Number(_), _) => Ok(dot.clone()),
        ("tonumber", Value::String(text), _) => {
            let text = text.trim();
            if let Ok(number) = text.parse::<i64>() {
                return Ok(Value::from(number));
            }
            if let Ok(number) = text.parse::<u64>() {
                return Ok(Value::from(number));
            }
            let number = text
                .parse::<f64>()
                .map_err(|_| "`tonumber` expects a numeric string".to_string())?;
            finite_number(number)
        }
        ("ascii_downcase", Value::String(text), _) => Ok(Value::String(text.to_ascii_lowercase())),
        ("split", Value::String(text), Some(Value::String(separator))) => {
            let items = if text.is_empty() {
                Vec::new()
            } else if separator.is_empty() {
                text.chars()
                    .map(|ch| Value::String(ch.to_string()))
                    .collect()
            } else {
                text.split(separator)
                    .map(|part| Value::String(part.to_string()))
                    .collect()
            };
            Ok(Value::Array(items))
        }
        ("join", Value::Array(items), Some(Value::String(separator))) => {
            let parts: Result<Vec<_>, _> = items
                .iter()
                .map(|value| match value {
                    Value::Null => Ok(String::new()),
                    Value::Array(_) | Value::Object(_) => {
                        Err("`join` expects scalar elements".to_string())
                    }
                    _ => Ok(stringify(value)),
                })
                .collect();
            Ok(Value::String(parts?.join(separator)))
        }
        ("startswith", Value::String(text), Some(Value::String(prefix))) => {
            Ok(Value::Bool(text.starts_with(prefix)))
        }
        ("endswith", Value::String(text), Some(Value::String(suffix))) => {
            Ok(Value::Bool(text.ends_with(suffix)))
        }
        ("contains", _, Some(value)) => Ok(Value::Bool(contains(dot, value)?)),
        _ => Err(format!("invalid input or argument for `{name}`")),
    }
}

fn contains(container: &Value, value: &Value) -> Result<bool, String> {
    match (container, value) {
        (Value::String(text), Value::String(part)) => Ok(text.contains(part)),
        (Value::Array(items), Value::Array(parts)) => Ok(parts.iter().all(|part| {
            items
                .iter()
                .any(|item| contains(item, part).unwrap_or(false))
        })),
        (Value::Object(object), Value::Object(parts)) => Ok(parts.iter().all(|(key, part)| {
            object
                .get(key)
                .is_some_and(|item| contains(item, part).unwrap_or(false))
        })),
        (Value::Number(_), Value::Number(_)) => {
            if let (Some(left), Some(right)) = (as_num(container), as_num(value)) {
                return Ok(match (left, right) {
                    (Num::Int(left), Num::Int(right)) => left == right,
                    (left, right) => left.float() == right.float(),
                });
            }
            Err("invalid number".to_string())
        }
        (Value::Null, Value::Null) | (Value::Bool(_), Value::Bool(_)) => Ok(container == value),
        _ => Err("`contains` expects inputs of the same type".to_string()),
    }
}

fn compare(op: BinOp, left: &Value, right: &Value) -> Result<Value, String> {
    let ordering = match (left, right) {
        (Value::String(left), Value::String(right)) => left.cmp(right),
        _ => match (as_num(left), as_num(right)) {
            (Some(left), Some(right)) => cmp_num(left, right),
            _ => return Err("comparison expects two numbers or two strings".to_string()),
        },
    };
    let flag = match op {
        BinOp::Lt => ordering.is_lt(),
        BinOp::Le => ordering.is_le(),
        BinOp::Gt => ordering.is_gt(),
        BinOp::Ge => ordering.is_ge(),
        _ => unreachable!("compare op"),
    };
    Ok(Value::Bool(flag))
}

fn add(left: &Value, right: &Value) -> Result<Value, String> {
    if left.is_null() {
        return Ok(right.clone());
    }
    if right.is_null() {
        return Ok(left.clone());
    }
    if let (Value::String(left), Value::String(right)) = (left, right) {
        return Ok(Value::String(format!("{left}{right}")));
    }
    if let (Value::Array(left), Value::Array(right)) = (left, right) {
        return Ok(Value::Array(left.iter().chain(right).cloned().collect()));
    }
    if let (Value::Object(left), Value::Object(right)) = (left, right) {
        let mut object = left.clone();
        object.extend(right.clone());
        return Ok(Value::Object(object));
    }
    arith(BinOp::Add, left, right)
}

fn arith(op: BinOp, left: &Value, right: &Value) -> Result<Value, String> {
    let (Some(left), Some(right)) = (as_num(left), as_num(right)) else {
        return Err(format!("`{}` expects numbers", bin_name(op)));
    };
    match (left, right) {
        (Num::Int(left), Num::Int(right)) => {
            let value = match op {
                BinOp::Add => left.checked_add(right),
                BinOp::Sub => left.checked_sub(right),
                BinOp::Mul => left.checked_mul(right),
                BinOp::Div => {
                    if right == 0 {
                        return Err("division by zero".to_string());
                    }
                    left.checked_div(right)
                }
                _ => unreachable!("arith op"),
            };
            value
                .map(Value::from)
                .ok_or_else(|| "numeric overflow".to_string())
        }
        (left, right) => {
            let left = left.float();
            let right = right.float();
            let value = match op {
                BinOp::Add => left + right,
                BinOp::Sub => left - right,
                BinOp::Mul => left * right,
                BinOp::Div => {
                    if right == 0.0 {
                        return Err("division by zero".to_string());
                    }
                    left / right
                }
                _ => unreachable!("arith op"),
            };
            finite_number(value)
        }
    }
}

fn index_value(value: &Value, index: &Value) -> Result<Value, String> {
    match (value, index) {
        (Value::Array(items), Value::Number(number)) => {
            let Some(index) = number.as_i64() else {
                return Err("array index must be an integer".to_string());
            };
            let Some(position) = array_index(items.len(), index) else {
                return Ok(Value::Null);
            };
            Ok(items.get(position).cloned().unwrap_or(Value::Null))
        }
        (Value::Object(object), Value::String(key)) => {
            Ok(object.get(key).cloned().unwrap_or(Value::Null))
        }
        (Value::Null, _) => Ok(Value::Null),
        (Value::Array(_), _) => Err("array index must be an integer".to_string()),
        (Value::Object(_), _) => Err("object index must be a string".to_string()),
        _ => Err("value cannot be indexed".to_string()),
    }
}

fn array_index(len: usize, index: i64) -> Option<usize> {
    if index >= 0 {
        let index = usize::try_from(index).ok()?;
        (index < len).then_some(index)
    } else {
        let from_end = usize::try_from(index.checked_neg()?).ok()?;
        len.checked_sub(from_end)
    }
}

#[derive(Clone, Copy)]
enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    fn float(self) -> f64 {
        match self {
            Self::Int(value) => value as f64,
            Self::Float(value) => value,
        }
    }
}

fn as_num(value: &Value) -> Option<Num> {
    let number = value.as_number()?;
    if let Some(integer) = number.as_i64() {
        return Some(Num::Int(integer));
    }
    number.as_f64().map(Num::Float)
}

fn cmp_num(left: Num, right: Num) -> std::cmp::Ordering {
    match (left, right) {
        (Num::Int(left), Num::Int(right)) => left.cmp(&right),
        (left, right) => left.float().total_cmp(&right.float()),
    }
}

fn finite_number(value: f64) -> Result<Value, String> {
    Number::from_f64(value)
        .map(Value::Number)
        .ok_or_else(|| "invalid number".to_string())
}

fn bin_name(op: BinOp) -> &'static str {
    match op {
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::And => "and",
        BinOp::Or => "or",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::evaluate;
    use super::evaluate_data;

    fn ev(source: &str, dot: serde_json::Value) -> serde_json::Value {
        evaluate(source, &dot, &BTreeMap::new()).unwrap_or_else(|error| panic!("{error}"))
    }

    #[test]
    fn evaluates_paths_comparisons_and_pipes() {
        let dot = json!({"priority": "high", "items": ["a", "b"], "n": 2});
        assert_eq!(ev(".", dot.clone()), dot);
        assert_eq!(ev(".priority", dot.clone()), json!("high"));
        assert_eq!(ev(".priority == \"high\"", dot.clone()), json!(true));
        assert_eq!(ev(".items[0]", dot.clone()), json!("a"));
        assert_eq!(ev(".items | .[1]", dot.clone()), json!("b"));
        assert_eq!(ev(".n + 1", dot.clone()), json!(3));
        assert_eq!(ev("not (.priority == \"low\")", dot.clone()), json!(true));
        assert_eq!(
            ev("{ lane: .priority, n: .n }", dot.clone()),
            json!({"lane": "high", "n": 2})
        );
        assert_eq!(ev(".missing", dot), json!(null));
    }

    #[test]
    fn interpolates_set_strings() {
        let dot = json!({"color": "red", "n": 2});
        let vars = BTreeMap::new();
        assert_eq!(
            evaluate_data(&json!("${ .color }"), &dot, &vars).unwrap(),
            json!("red")
        );
        assert_eq!(
            evaluate_data(&json!("paint ${ .color } ${ .n }"), &dot, &vars).unwrap(),
            json!("paint red 2")
        );
        assert_eq!(
            evaluate_data(&json!({"ok": true, "label": "${ .color }"}), &dot, &vars).unwrap(),
            json!({"ok": true, "label": "red"})
        );
    }

    #[test]
    fn evaluates_the_function_subset_and_rejects_invalid_inputs() {
        let cases = [
            (
                "map(. * 3)",
                json!([2, -1, 4]),
                json!([6, -3, 12]),
                json!(2),
            ),
            (
                "[select(.n > 2)]",
                json!({"n": 3}),
                json!([{"n": 3}]),
                json!(3),
            ),
            ("length", json!("é水🙂"), json!(3), json!(true)),
            ("has(\"x\")", json!({"x": null}), json!(true), json!([])),
            (
                "keys",
                json!({"z": 0, "a": 1}),
                json!(["a", "z"]),
                json!(null),
            ),
            ("add", json!([2, -5, 9]), json!(6), json!([1, "x"])),
            ("tonumber", json!(" -1.25e2 "), json!(-125.0), json!("1x")),
            ("ascii_downcase", json!("AÉZ水"), json!("aÉz水"), json!([])),
            (
                "split(\"::\")",
                json!("a::::b::"),
                json!(["a", "", "b", ""]),
                json!(3),
            ),
            (
                "join(\"/\")",
                json!(["a", null, true, 2]),
                json!("a//true/2"),
                json!([{}]),
            ),
            (
                "startswith(\"ab\")",
                json!("abcd"),
                json!(true),
                json!(null),
            ),
            ("endswith(\"cd\")", json!("abcd"), json!(true), json!(3)),
            (
                "contains({a: [\"bc\"]})",
                json!({"a": ["abcd", 1], "b": 2}),
                json!(true),
                json!([]),
            ),
        ];
        for (source, input, expected, invalid) in cases {
            assert_eq!(ev(source, input), expected, "{source}");
            assert!(
                evaluate(source, &invalid, &BTreeMap::new()).is_err(),
                "{source}"
            );
        }
        // tostring accepts every JSON type; its error case is invalid arity.
        assert_eq!(
            ev("tostring", json!({"a": [1, false]})),
            json!(r#"{"a":[1,false]}"#)
        );
        assert_eq!(ev("tostring", json!("hello")), json!("hello"));
        assert!(evaluate("tostring(.)", &json!(null), &BTreeMap::new()).is_err());
    }

    #[test]
    fn function_boundaries_follow_jq_value_semantics() {
        let cases = [
            (
                "map(select(. > 1) | . + 10)",
                json!([3, 0, 2]),
                json!([13, 12]),
            ),
            ("map(.)", json!({"b": 2, "a": 1}), json!([1, 2])),
            ("map(.[])", json!([[2, 4], [], [3]]), json!([2, 4, 3])),
            ("[select(false)]", json!(5), json!([])),
            ("[select(0)]", json!(5), json!([5])),
            ("length", json!(null), json!(0)),
            ("length", json!(-4), json!(4)),
            ("length", json!(u64::MAX), json!(u64::MAX)),
            ("length", json!({"a": 1, "b": 2}), json!(2)),
            ("length", json!([false, null]), json!(2)),
            ("has(0)", json!([null]), json!(true)),
            ("has(1)", json!([null]), json!(false)),
            ("has(-1)", json!([null]), json!(false)),
            ("has(\"missing\")", json!({}), json!(false)),
            ("keys", json!([8, 3]), json!([0, 1])),
            ("add", json!([]), json!(null)),
            ("add", json!([null, 3, null, 5]), json!(8)),
            ("add", json!(["a", "bc"]), json!("abc")),
            ("add", json!([[2], [3, 4]]), json!([2, 3, 4])),
            (
                "add",
                json!([{"a": 1}, {"a": 2, "b": 3}]),
                json!({"a": 2, "b": 3}),
            ),
            ("add", json!({"a": 3, "b": 5}), json!(8)),
            ("tonumber", json!(7), json!(7)),
            ("tonumber", json!("+01"), json!(1)),
            ("tonumber", json!(".5"), json!(0.5)),
            ("split(\"\")", json!("é水"), json!(["é", "水"])),
            ("split(\"\")", json!(""), json!([])),
            ("split(\"-\")", json!(""), json!([])),
            ("split(\"-\")", json!("-"), json!(["", ""])),
            ("join(\"-\")", json!([]), json!("")),
            ("startswith(\"ab\")", json!("zab"), json!(false)),
            ("endswith(\"ab\")", json!("abz"), json!(false)),
            ("contains(\"bc\")", json!("abcd"), json!(true)),
            ("contains(1.0)", json!(1), json!(true)),
            ("contains(-0.0)", json!(0), json!(true)),
            ("contains(\"bd\")", json!("abcd"), json!(false)),
            ("contains([2, 2])", json!([2]), json!(true)),
            ("contains({a: null})", json!({}), json!(false)),
            ("contains({a: \"x\"})", json!({"a": 1}), json!(false)),
            ("contains([false])", json!([1, "a"]), json!(false)),
        ];
        for (source, input, expected) in cases {
            assert_eq!(ev(source, input), expected, "{source}");
        }
        for (source, input) in [
            ("has(0.5)", json!([])),
            ("tonumber", json!(true)),
            ("tonumber", json!("null")),
            ("tonumber", json!("1e9999")),
            ("tonumber", json!("NaN")),
            ("tonumber", json!("inf")),
            ("split(2)", json!("text")),
            ("join(2)", json!([])),
            ("startswith(2)", json!("text")),
            ("endswith(2)", json!("text")),
            ("add", json!([i64::MAX, 1])),
        ] {
            assert!(
                evaluate(source, &input, &BTreeMap::new()).is_err(),
                "{source}"
            );
        }
    }

    #[test]
    fn conditionals_and_defaults_are_lazy_and_preserve_the_input() {
        assert_eq!(
            ev("if .ok then .n else 1 / 0 end", json!({"ok": true, "n": 4})),
            json!(4)
        );
        assert_eq!(ev("if . then 1 / 0 else 9 end", json!(false)), json!(9));
        assert_eq!(
            ev("if . then if . == 0 then 2 else 3 end else 4 end", json!(0)),
            json!(2)
        );
        assert_eq!(
            ev(
                "map(if . then 1 else 2 end)",
                json!([false, null, 0, "", [], {}])
            ),
            json!([2, 2, 1, 1, 1, 1])
        );
        assert_eq!(
            ev(".n // .fallback // 9", json!({"n": false, "fallback": 3})),
            json!(3)
        );
        assert_eq!(ev("0 // (1 / 0)", json!(null)), json!(0));
        assert_eq!(ev(".missing // 8 / 2", json!({})), json!(4));
        assert_eq!(
            ev("[.[] // 7]", json!([false, null, 0, ""])),
            json!([0, ""])
        );
        assert_eq!(ev("[.[] // 7]", json!([])), json!([7]));
        assert_eq!(
            ev("[.[] | . // 7]", json!([false, 2, null])),
            json!([7, 2, 7])
        );
        assert_eq!(ev("[select(false)] // 7", json!(null)), json!([]));
        for source in [
            "if . then 1 end",
            "if true then 1 else end",
            "if true then .bad else 1 end",
            "null // (1 / 0)",
            "1 //",
            "if",
        ] {
            assert!(
                evaluate(source, &json!(2), &BTreeMap::new()).is_err(),
                "{source}"
            );
        }
    }

    #[test]
    fn iteration_collects_streams_and_rejects_uncollected_results() {
        assert_eq!(
            ev(
                "[.[] | select(.n > 1) | .n * 2]",
                json!([{"n": 3}, {"n": 1}, {"n": 2}])
            ),
            json!([6, 4])
        );
        assert_eq!(ev("[.[] + 1]", json!([2, 5])), json!([3, 6]));
        assert_eq!(
            ev("[.[] | {n: ., label: tostring}]", json!([2, 5])),
            json!([{"n": 2, "label": "2"}, {"n": 5, "label": "5"}])
        );
        assert_eq!(ev("[.[][]]", json!([[2, 4], [], [3]])), json!([2, 4, 3]));
        assert_eq!(ev("[.[]]", json!({"b": 5, "a": 2})), json!([2, 5]));
        assert_eq!(ev("[.[]]", json!([])), json!([]));
        let vars = BTreeMap::from([("increment".to_string(), json!(3))]);
        assert_eq!(
            evaluate("map(. + $increment)", &json!([2, 5]), &vars).unwrap(),
            json!([5, 8])
        );
        for (source, input) in [
            (".[]", json!([2, 5])),
            (".[]", json!([])),
            ("[.[]]", json!(null)),
            ("select(false)", json!(2)),
        ] {
            assert!(evaluate(source, &input, &vars).is_err(), "{source}");
        }
    }

    #[test]
    fn new_filters_work_in_a_workflow_and_fail_as_expression_faults() {
        let definition = crate::Definition::from_yaml(
            r#"
document:
  dsl: '1.0.0'
  namespace: default
  name: expressions
  version: '0.1.0'
do:
  - transform:
      input:
        from: '[.items[] | select(.enabled) | .name | ascii_downcase]'
      set:
        names: '${ . | join(",") }'
        count: '${ length }'
      output:
        as: 'if .count > 0 then .names else "empty" end'
"#,
        )
        .unwrap();
        let outcome = crate::start(
            &definition,
            &json!({"items": [
                {"enabled": true, "name": "ALPHA"},
                {"enabled": false, "name": "IGNORED"},
                {"enabled": true, "name": "Beta"}
            ]}),
        );
        assert!(
            matches!(outcome, crate::Outcome::Completed { output, .. } if output == json!("alpha,beta"))
        );
        let outcome = crate::start(&definition, &json!({"items": 3}));
        let crate::Outcome::Faulted { fault, .. } = outcome else {
            panic!("expected an expression fault")
        };
        assert_eq!(fault.status(), 400);
        assert_eq!(fault.instance(), "/do/0/transform");
        assert_eq!(
            fault.error_type(),
            "https://open-workflow-specification.org/spec/1.0.0/errors/expression"
        );
    }

    #[test]
    fn unsupported_syntax_and_wrong_arity_remain_errors() {
        for source in [
            "reduce .[]",
            "sort",
            "map()",
            "length(.)",
            "has()",
            "has(1; 2)",
            ".[]?",
            "if true then 1 else 2 end trailing",
        ] {
            assert!(
                evaluate(source, &json!([]), &BTreeMap::new()).is_err(),
                "{source}"
            );
        }
    }
}
