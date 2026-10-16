//! The condition and template grammar (ADR 0066 §4).
//!
//! A condition is `<address> <field-path> <operator> <literal>`; a template
//! puts a variable where the address goes: `$cr .checks == passing`. Field
//! path segments and literals are bare when they are a bare word and JSON
//! strings otherwise. The printer writes the canonical form: single spaces,
//! bare where possible, and JSON quoting exactly as `serde_json` prints it, so
//! print-then-parse returns exactly what went in. The strict parsers refuse
//! every other spelling; `resolve` accepts the lenient one.

use std::{fmt, str::FromStr};

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use crate::{lenient::resolve, Address, Error, ForgeLookup};

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operator {
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
}

impl Operator {
    pub const ALL: [Operator; 6] = [
        Operator::Equal,
        Operator::NotEqual,
        Operator::LessThan,
        Operator::LessThanOrEqual,
        Operator::GreaterThan,
        Operator::GreaterThanOrEqual,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Operator::Equal => "==",
            Operator::NotEqual => "!=",
            Operator::LessThan => "<",
            Operator::LessThanOrEqual => "<=",
            Operator::GreaterThan => ">",
            Operator::GreaterThanOrEqual => ">=",
        }
    }
}

impl fmt::Display for Operator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Operator {
    type Err = Error;

    fn from_str(token: &str) -> Result<Self, Self::Err> {
        Operator::ALL.into_iter().find(|operator| operator.as_str() == token).ok_or_else(|| Error::UnknownOperator(token.to_string()))
    }
}

/// A path into the addressed thing's fields: `.status.phase`,
/// `.labels."good first issue"`. The empty path, printed `.`, is the thing
/// itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct FieldPath(Vec<String>);

impl FieldPath {
    pub fn new(segments: Vec<String>) -> Self {
        Self(segments)
    }

    pub fn segments(&self) -> &[String] {
        &self.0
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str(".");
        }
        for segment in &self.0 {
            f.write_str(".")?;
            write_word(f, segment, is_bare_segment)?;
        }
        Ok(())
    }
}

impl FromStr for FieldPath {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut cursor = Cursor::new(text, Mode::Strict);
        let path = cursor.field_path()?;
        cursor.end()?;
        Ok(path)
    }
}

/// A condition: the addressed thing's field compared with a literal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Condition {
    pub address: Address,
    pub path: FieldPath,
    pub operator: Operator,
    pub literal: String,
}

impl Condition {
    /// Parses typed input: any address input [`resolve`] accepts, runs of
    /// whitespace, non-canonical JSON quoting, a single-quoted literal and an
    /// unquoted literal of several words (joined by single spaces).
    pub fn resolve(input: &str, forges: &dyn ForgeLookup) -> Result<Self, Error> {
        let mut cursor = Cursor::new(input.trim(), Mode::Lenient);
        let address = resolve(cursor.token(), forges)?;
        cursor.separator()?;
        let (path, operator, literal) = cursor.comparison()?;
        Ok(Self { address, path, operator, literal })
    }
}

impl fmt::Display for Condition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {} ", self.address, self.path, self.operator)?;
        write_word(f, &self.literal, is_bare_literal)
    }
}

impl FromStr for Condition {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut cursor = Cursor::new(text, Mode::Strict);
        let address = cursor.token().parse()?;
        cursor.separator()?;
        let (path, operator, literal) = cursor.comparison()?;
        Ok(Self { address, path, operator, literal })
    }
}

/// A template's subject: a variable bound when the template is instantiated,
/// with optional arguments: `$cr`, `$artifact(reviewer,review-round,head)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Variable {
    name: String,
    arguments: Vec<String>,
}

impl Variable {
    /// A variable named by a lowercase word: `[a-z][a-z0-9_-]*`.
    pub fn new(name: impl Into<String>, arguments: Vec<String>) -> Result<Self, Error> {
        let name = name.into();
        let mut bytes = name.bytes();
        let valid = bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
            && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-');
        if !valid {
            return Err(Error::InvalidVariable(name));
        }
        Ok(Self { name, arguments })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }
}

impl fmt::Display for Variable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "${}", self.name)?;
        if self.arguments.is_empty() {
            return Ok(());
        }
        f.write_str("(")?;
        for (index, argument) in self.arguments.iter().enumerate() {
            if index > 0 {
                f.write_str(",")?;
            }
            write_word(f, argument, is_bare_segment)?;
        }
        f.write_str(")")
    }
}

/// A condition with a variable in place of its address.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Template {
    pub subject: Variable,
    pub path: FieldPath,
    pub operator: Operator,
    pub literal: String,
}

impl Template {
    /// Parses typed input, as [`Condition::resolve`] does. The field path may
    /// also follow the variable with no space: `$cr.state == merged`.
    pub fn resolve(input: &str) -> Result<Self, Error> {
        let mut cursor = Cursor::new(input.trim(), Mode::Lenient);
        let subject = cursor.variable()?;
        if cursor.peek() != Some('.') {
            cursor.separator()?;
        }
        let (path, operator, literal) = cursor.comparison()?;
        Ok(Self { subject, path, operator, literal })
    }

    /// The condition this template becomes once its subject is bound.
    pub fn bind(&self, address: Address) -> Condition {
        Condition { address, path: self.path.clone(), operator: self.operator, literal: self.literal.clone() }
    }
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {} ", self.subject, self.path, self.operator)?;
        write_word(f, &self.literal, is_bare_literal)
    }
}

impl FromStr for Template {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut cursor = Cursor::new(text, Mode::Strict);
        let subject = cursor.variable()?;
        cursor.separator()?;
        let (path, operator, literal) = cursor.comparison()?;
        Ok(Self { subject, path, operator, literal })
    }
}

macro_rules! text_serde {
    ($type:ty) => {
        impl Serialize for $type {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                String::deserialize(deserializer)?.parse().map_err(de::Error::custom)
            }
        }
    };
}

text_serde!(Condition);
text_serde!(Template);
text_serde!(FieldPath);

/// A field-path segment or variable argument that needs no quoting.
fn is_bare_segment(word: &str) -> bool {
    !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// A literal that needs no quoting. Literals may also hold `. : / + @`, so
/// versions, timestamps and addresses stay readable.
fn is_bare_literal(word: &str) -> bool {
    !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"_-.:/+@".contains(&byte))
}

fn write_word(f: &mut fmt::Formatter<'_>, word: &str, bare: fn(&str) -> bool) -> fmt::Result {
    if bare(word) {
        f.write_str(word)
    } else {
        f.write_str(&serde_json::to_string(word).expect("a string always serializes"))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Strict,
    Lenient,
}

struct Cursor<'a> {
    text: &'a str,
    position: usize,
    mode: Mode,
}

impl<'a> Cursor<'a> {
    fn new(text: &'a str, mode: Mode) -> Self {
        Self { text, position: 0, mode }
    }

    fn rest(&self) -> &'a str {
        &self.text[self.position..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn syntax(&self, expected: &'static str) -> Error {
        Error::Syntax { input: self.text.to_string(), position: self.position, expected }
    }

    /// Everything up to the next whitespace.
    fn token(&mut self) -> &'a str {
        let rest = self.rest();
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        self.position += end;
        &rest[..end]
    }

    /// One space, or in lenient mode any run of whitespace.
    fn separator(&mut self) -> Result<(), Error> {
        let rest = self.rest();
        let skipped = match self.mode {
            Mode::Strict => usize::from(rest.starts_with(' ')),
            Mode::Lenient => rest.len() - rest.trim_start().len(),
        };
        if skipped == 0 {
            return Err(self.syntax("a space"));
        }
        self.position += skipped;
        Ok(())
    }

    fn end(&self) -> Result<(), Error> {
        if self.rest().is_empty() {
            Ok(())
        } else {
            Err(self.syntax("the end of input"))
        }
    }

    /// `<field-path> <operator> <literal>` through the end of input.
    fn comparison(&mut self) -> Result<(FieldPath, Operator, String), Error> {
        let path = self.field_path()?;
        self.separator()?;
        let operator = self.token().parse()?;
        self.separator()?;
        let literal = self.literal()?;
        Ok((path, operator, literal))
    }

    fn field_path(&mut self) -> Result<FieldPath, Error> {
        if self.peek() != Some('.') {
            return Err(self.syntax("a field path starting with `.`"));
        }
        self.position += 1;
        let mut segments = Vec::new();
        if matches!(self.peek(), None | Some(' ' | '\t' | '\n' | '\r')) {
            return Ok(FieldPath(segments));
        }
        loop {
            segments.push(self.word(is_bare_segment, "a bare or quoted field name")?);
            if self.peek() != Some('.') {
                return Ok(FieldPath(segments));
            }
            self.position += 1;
        }
    }

    /// A bare word or a JSON string. Strict mode refuses a quoted word that
    /// could be bare and any JSON spelling other than `serde_json`'s.
    fn word(&mut self, bare: fn(&str) -> bool, expected: &'static str) -> Result<String, Error> {
        if self.peek() == Some('"') {
            return self.quoted(bare);
        }
        let rest = self.rest();
        // Lenient input accepts the old grammar's segments: any run of
        // characters that do not end a segment, such as `kind:bug`. The
        // printer quotes them, so the canonical form stays strict.
        let ends_word = |c: char| match self.mode {
            Mode::Strict => !(c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            Mode::Lenient => c.is_whitespace() || matches!(c, '.' | ',' | '(' | ')' | '"'),
        };
        let end = rest.find(ends_word).unwrap_or(rest.len());
        if end == 0 {
            return Err(self.syntax(expected));
        }
        self.position += end;
        Ok(rest[..end].to_string())
    }

    fn quoted(&mut self, bare: fn(&str) -> bool) -> Result<String, Error> {
        let start = self.position;
        let rest = self.rest();
        let mut escaped = false;
        let mut end = None;
        for (index, c) in rest.char_indices().skip(1) {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => {
                    end = Some(index + 1);
                    break;
                }
                _ => {}
            }
        }
        let end = end.ok_or_else(|| self.syntax("a closing `\"`"))?;
        let spelling = &rest[..end];
        let value: String = serde_json::from_str(spelling).map_err(|_| self.syntax("a JSON string"))?;
        if self.mode == Mode::Strict && (bare(&value) || serde_json::to_string(&value).expect("a string always serializes") != spelling) {
            return Err(Error::NonCanonicalQuoting(spelling.to_string()));
        }
        self.position = start + end;
        Ok(value)
    }

    /// The literal runs to the end of input.
    fn literal(&mut self) -> Result<String, Error> {
        let rest = self.rest();
        if rest.starts_with('"') {
            let value = self.quoted(is_bare_literal)?;
            self.end()?;
            return Ok(value);
        }
        match self.mode {
            Mode::Strict if is_bare_literal(rest) => {
                self.position = self.text.len();
                Ok(rest.to_string())
            }
            Mode::Strict => Err(self.syntax("a bare or quoted literal")),
            Mode::Lenient => {
                let value = match rest.strip_prefix('\'').and_then(|inner| inner.strip_suffix('\'')) {
                    Some(inner) => inner.to_string(),
                    None => rest.split_whitespace().collect::<Vec<_>>().join(" "),
                };
                if value.is_empty() {
                    return Err(self.syntax("a literal"));
                }
                self.position = self.text.len();
                Ok(value)
            }
        }
    }

    /// `$name` or `$name(<argument>,…)`.
    fn variable(&mut self) -> Result<Variable, Error> {
        if self.peek() != Some('$') {
            return Err(self.syntax("a `$` variable"));
        }
        self.position += 1;
        let rest = self.rest();
        let end = rest.find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')).unwrap_or(rest.len());
        let name = &rest[..end];
        self.position += end;
        let mut arguments = Vec::new();
        if self.peek() == Some('(') {
            self.position += 1;
            loop {
                arguments.push(self.word(is_bare_segment, "a bare or quoted argument")?);
                match self.peek() {
                    Some(',') => self.position += 1,
                    Some(')') => {
                        self.position += 1;
                        break;
                    }
                    _ => return Err(self.syntax("`,` or `)`")),
                }
            }
        }
        Variable::new(name, arguments)
    }
}
