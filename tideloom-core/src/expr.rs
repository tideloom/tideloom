//! Expressions this walk evaluates.
//!
//! The language is the subset control flow needs: paths, literals, comparisons,
//! arithmetic, `and` / `or` / `not`, arrays, objects, and `|`. It is not jq.
//! Anything else fails as an expression error so a later slice can replace
//! this evaluator without changing the walk.
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
    eval(&expr, dot, vars)
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
        let mut left = self.parse_or()?;
        while self.eat_op("|") {
            let right = self.parse_or()?;
            left = Expr::Pipe(Box::new(left), Box::new(right));
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

fn eval(expr: &Expr, dot: &Value, vars: &BTreeMap<String, Value>) -> Result<Value, String> {
    match expr {
        Expr::Dot => Ok(dot.clone()),
        Expr::Var(name) => vars
            .get(name)
            .cloned()
            .ok_or_else(|| format!("unknown variable `${name}`")),
        Expr::Field(base, name) => {
            let value = eval(base, dot, vars)?;
            match value {
                Value::Object(object) => Ok(object.get(name).cloned().unwrap_or(Value::Null)),
                Value::Null => Ok(Value::Null),
                _ => Err(format!("cannot read field `{name}` on a non-object")),
            }
        }
        Expr::Index(base, index) => {
            let value = eval(base, dot, vars)?;
            let index = eval(index, dot, vars)?;
            index_value(&value, &index)
        }
        Expr::Lit(value) => Ok(value.clone()),
        Expr::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(eval(item, dot, vars)?);
            }
            Ok(Value::Array(out))
        }
        Expr::Object(fields) => {
            let mut object = Map::new();
            for (key, value) in fields {
                object.insert(key.clone(), eval(value, dot, vars)?);
            }
            Ok(Value::Object(object))
        }
        Expr::Not(inner) => {
            let value = eval(inner, dot, vars)?;
            match value {
                Value::Bool(flag) => Ok(Value::Bool(!flag)),
                _ => Err("`not` expects a boolean".to_string()),
            }
        }
        Expr::Neg(inner) => {
            let value = eval(inner, dot, vars)?;
            match as_num(&value) {
                Some(Num::Int(number)) => number
                    .checked_neg()
                    .map(Value::from)
                    .ok_or_else(|| "numeric overflow".to_string()),
                Some(Num::Float(number)) => finite_number(-number),
                None => Err("`-` expects a number".to_string()),
            }
        }
        Expr::Binary(op, left, right) => eval_binary(*op, left, right, dot, vars),
        Expr::Pipe(left, right) => {
            let value = eval(left, dot, vars)?;
            eval(right, &value, vars)
        }
    }
}

fn eval_binary(
    op: BinOp,
    left: &Expr,
    right: &Expr,
    dot: &Value,
    vars: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    if matches!(op, BinOp::And | BinOp::Or) {
        let left = eval(left, dot, vars)?;
        let Value::Bool(flag) = left else {
            return Err(format!("`{}` expects a boolean", bin_name(op)));
        };
        if (op == BinOp::And && !flag) || (op == BinOp::Or && flag) {
            return Ok(Value::Bool(flag));
        }
        let right = eval(right, dot, vars)?;
        return match right {
            Value::Bool(flag) => Ok(Value::Bool(flag)),
            _ => Err(format!("`{}` expects a boolean", bin_name(op))),
        };
    }
    let left = eval(left, dot, vars)?;
    let right = eval(right, dot, vars)?;
    match op {
        BinOp::Eq => Ok(Value::Bool(left == right)),
        BinOp::Ne => Ok(Value::Bool(left != right)),
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => compare(op, &left, &right),
        BinOp::Add => add(&left, &right),
        BinOp::Sub => arith(op, &left, &right),
        BinOp::Mul | BinOp::Div => arith(op, &left, &right),
        BinOp::And | BinOp::Or => unreachable!("handled above"),
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
    if let (Value::String(left), Value::String(right)) = (left, right) {
        return Ok(Value::String(format!("{left}{right}")));
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
    fn rejects_jq_functions() {
        let error = evaluate("map(.)", &json!({}), &BTreeMap::new()).unwrap_err();
        assert!(error.contains("expected a value"), "{error}");
    }
}
