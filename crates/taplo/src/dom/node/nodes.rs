use std::fmt::Write;
use std::iter::once;
use std::sync::Arc;

use logos::Lexer;
use once_cell::unsync::OnceCell;
use rowan::NodeOrToken;
use rowan::TextRange;
use time::macros::format_description;

use super::DomNode;
use super::Node;
use crate::dom::Entries;
use crate::dom::KeyOrIndex;
use crate::dom::Keys;
use crate::dom::error::Error;
use crate::syntax::SyntaxElement;
use crate::syntax::SyntaxKind;
use crate::util::shared::Shared;
use crate::util::unescape;

macro_rules! wrap_node {
    (
    $(#[$attrs:meta])*
    $vis:vis struct $name:ident {
        inner: $inner:ident
    }
    ) => {
        $(#[$attrs])*
        $vis struct $name {
            pub(crate) inner: Arc<$inner>,
        }

        impl $crate::private::Sealed for $name {}
        impl $crate::dom::node::DomNode for $name {
            fn syntax(&self) -> Option<&$crate::syntax::SyntaxElement> {
                self.inner.syntax.as_ref()
            }

            fn errors(&self) -> &$crate::util::shared::Shared<Vec<$crate::dom::error::Error>> {
                &self.inner.errors
            }

            fn validate_node(&self) -> Result<(), &$crate::util::shared::Shared<Vec<$crate::dom::error::Error>>> {
                self.validate_impl()
            }
        }

        impl $inner {
            #[allow(dead_code)]
            pub(crate) fn wrap(self) -> $name {
                self.into()
            }
        }

        impl From<$inner> for $name {
            fn from(inner: $inner) -> $name {
                $name {
                    inner: Arc::new(inner)
                }
            }
        }
    };
}

#[derive(Debug)]
pub(crate) struct TableInner {
  pub(crate) errors:  Shared<Vec<Error>>,
  pub(crate) syntax:  Option<SyntaxElement>,
  pub(crate) header:  bool,
  pub(crate) kind:    TableKind,
  pub(crate) entries: Shared<Entries>,
}

wrap_node! {
    #[derive(Debug, Clone)]
    pub struct Table { inner: TableInner }
}

impl Table {
  pub fn get(&self, key: impl Into<Key>) -> Option<Node> {
    let key = key.into();
    let entries = self.inner.entries.read();
    entries.lookup.get(&key).cloned()
  }

  pub fn entries(&self) -> &Shared<Entries> {
    &self.inner.entries
  }

  pub fn kind(&self) -> TableKind {
    self.inner.kind
  }

  /// Add an entry and also collect errors on conflicts.
  pub(crate) fn add_entry(&self, key: Key, node: Node) {
    self.inner.entries.update(|entries| {
      if let Some((existing_key, value)) = entries.lookup.get_key_value(&key) {
        match (value, &node) {
          (Node::Table(existing_table), Node::Table(new_table))
            if existing_table.inner.kind == TableKind::Pseudo && new_table.inner.kind == TableKind::Pseudo =>
          {
            let new_entries = new_table.entries().read();
            for (k, n) in new_entries.iter() {
              if let Some(additional_syntax) = k.syntax() {
                existing_key.inner.additional_syntaxes.update(|syntaxes| {
                  syntaxes.push(additional_syntax.clone());
                });
              }

              existing_table.add_entry(k.clone(), n.clone());
            }
            return;
          }
          _ => {}
        }

        self.inner.errors.update(|errors| {
          errors.push(Error::ConflictingKeys {
            key:   key.clone(),
            other: existing_key.clone(),
          })
        });
      }

      entries.add(key, node);
    });
  }

  fn validate_impl(&self) -> Result<(), &Shared<Vec<Error>>> {
    if self.errors().read().as_ref().is_empty() {
      Ok(())
    } else {
      Err(self.errors())
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
  Regular,
  Inline,
  Pseudo,
}

#[derive(Debug)]
pub(crate) struct KeyInner {
  pub(crate) errors:   Shared<Vec<Error>>,
  pub(crate) syntax:   Option<SyntaxElement>,
  pub(crate) is_valid: bool,
  pub(crate) value:    OnceCell<String>,

  /// The same key can appear at multiple positions
  /// in a TOML document.
  ///
  /// # Example
  ///  
  /// In the following both `table` and `inner` appear multiple times
  /// despite being the same key in the DOM.
  ///
  /// ```toml
  /// [table.inner.something]
  /// [table.inner.something_else]
  /// ```
  pub(crate) additional_syntaxes: Shared<Vec<SyntaxElement>>,
}

wrap_node! {
    #[derive(Debug, Clone)]
    pub struct Key { inner: KeyInner }
}

impl<S> From<S> for Key
where
  S: Into<String>,
{
  fn from(s: S) -> Self {
    Key::new(s)
  }
}

impl Key {
  /// Return a new key with the given value.
  ///
  /// # Remarks
  ///
  /// This **does not** check or modify the input string.
  pub fn new(key: impl Into<String>) -> Self {
    KeyInner {
      errors:              Default::default(),
      syntax:              None,
      is_valid:            true,
      value:               OnceCell::from(key.into()),
      additional_syntaxes: Default::default(),
    }
    .wrap()
  }

  /// An unescaped value of the key.
  pub fn value(&self) -> &str {
    self.inner.value.get_or_init(|| {
      self
        .inner
        .syntax
        .as_ref()
        .and_then(NodeOrToken::as_token)
        .map(|s| {
          if s.text().starts_with('\'') {
            let string = s.text();
            let string = string.strip_prefix('\'').unwrap_or(string);
            let string = string.strip_suffix('\'').unwrap_or(string);
            string.to_string()
          } else if s.text().starts_with('"') {
            let string = s.text();
            let string = string.strip_prefix('"').unwrap_or(string);
            let string = string.strip_suffix('"').unwrap_or(string);
            match unescape(string) {
              Ok(s) => s,
              Err(_) => {
                self.inner.errors.update(|errors| {
                  errors.push(Error::InvalidEscapeSequence {
                    string: s.clone().into()
                  })
                });
                String::new()
              }
            }
          } else {
            s.text().to_string()
          }
        })
        .unwrap_or_default()
    })
  }

  pub fn text_ranges(&self) -> impl ExactSizeIterator<Item = TextRange> {
    let additional_syntaxes = self.inner.additional_syntaxes.read();

    let mut ranges = Vec::with_capacity(1 + additional_syntaxes.len());
    if let Some(s) = self.syntax() {
      ranges.push(s.text_range());
    }

    ranges.extend(additional_syntaxes.iter().map(|s| s.text_range()));

    ranges.into_iter()
  }

  fn validate_impl(&self) -> Result<(), &Shared<Vec<Error>>> {
    if !self.inner.is_valid {
      return Err(self.errors());
    }

    let _ = self.value();
    if self.errors().read().as_ref().is_empty() {
      Ok(())
    } else {
      Err(self.errors())
    }
  }

  pub fn join(&self, key: impl Into<KeyOrIndex>) -> Keys {
    Keys::new(once(self.clone().into()).chain(once(key.into())))
  }
}

impl AsRef<str> for Key {
  fn as_ref(&self) -> &str {
    self.value()
  }
}

impl core::fmt::Display for Key {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    if let Some(s) = self.syntax() {
      return s.fmt(f);
    }

    if !matches!(Lexer::<SyntaxKind>::new(self.value()).next(), Some(Ok(SyntaxKind::IDENT)) | None) {
      f.write_char('\'')?;
      self.value().fmt(f)?;
      f.write_char('\'')?;
      return Ok(());
    }

    self.value().fmt(f)
  }
}

impl PartialEq for Key {
  fn eq(&self, other: &Self) -> bool {
    if !self.inner.is_valid || !other.inner.is_valid {
      return false;
    }

    self.value().eq(other.value())
  }
}

impl Eq for Key {}

impl std::hash::Hash for Key {
  fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
    if !self.inner.is_valid {
      return 0.hash(state);
    }

    self.value().hash(state)
  }
}

#[derive(Debug)]
pub(crate) struct ArrayInner {
  pub(crate) errors: Shared<Vec<Error>>,
  pub(crate) syntax: Option<SyntaxElement>,
  pub(crate) kind:   ArrayKind,
  pub(crate) items:  Shared<Vec<Node>>,
}

wrap_node! {
    #[derive(Debug, Clone)]
    pub struct Array { inner: ArrayInner }
}

impl Array {
  pub fn items(&self) -> &Shared<Vec<Node>> {
    &self.inner.items
  }

  pub fn kind(&self) -> ArrayKind {
    self.inner.kind
  }

  fn validate_impl(&self) -> Result<(), &Shared<Vec<Error>>> {
    if self.errors().read().as_ref().is_empty() {
      Ok(())
    } else {
      Err(self.errors())
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayKind {
  Tables,
  Inline,
}

impl ArrayKind {
  /// Returns `true` if the array kind is [`Tables`].
  ///
  /// [`Tables`]: ArrayKind::Tables
  pub fn is_tables(&self) -> bool {
    matches!(self, Self::Tables)
  }

  /// Returns `true` if the array kind is [`Inline`].
  ///
  /// [`Inline`]: ArrayKind::Inline
  pub fn is_inline(&self) -> bool {
    matches!(self, Self::Inline)
  }
}

#[derive(Debug)]
pub(crate) struct BoolInner {
  pub(crate) errors: Shared<Vec<Error>>,
  pub(crate) syntax: Option<SyntaxElement>,
  pub(crate) value:  OnceCell<bool>,
}

wrap_node! {
    #[derive(Debug, Clone)]
    pub struct Bool { inner: BoolInner }
}

impl Bool {
  /// A boolean value.
  pub fn value(&self) -> bool {
    *self.inner.value.get_or_init(|| {
      self
        .syntax()
        .and_then(|s| s.as_token())
        .and_then(|s| s.text().parse().ok())
        .unwrap_or_default()
    })
  }

  fn validate_impl(&self) -> Result<(), &Shared<Vec<Error>>> {
    if self.errors().read().as_ref().is_empty() {
      Ok(())
    } else {
      Err(self.errors())
    }
  }
}

#[derive(Debug)]
pub(crate) struct StrInner {
  pub(crate) errors: Shared<Vec<Error>>,
  pub(crate) syntax: Option<SyntaxElement>,
  pub(crate) repr:   StrRepr,
  pub(crate) value:  OnceCell<String>,
}

wrap_node! {
    #[derive(Debug, Clone)]
    pub struct Str { inner: StrInner }
}

impl Str {
  /// An unescaped value of the string.
  pub fn value(&self) -> &str {
    self.inner.value.get_or_init(|| {
      self
        .inner
        .syntax
        .as_ref()
        .map(|s| match self.inner.repr {
          StrRepr::Basic => {
            let string = s.as_token().unwrap().text();
            let string = string.strip_prefix('"').unwrap_or(string);
            let string = string.strip_suffix('"').unwrap_or(string);
            match unescape(string) {
              Ok(s) => s,
              Err(_) => {
                self.inner.errors.update(|errors| {
                  errors.push(Error::InvalidEscapeSequence {
                    string: s.clone()
                  })
                });
                String::new()
              }
            }
          }
          StrRepr::Literal => {
            let string = s.as_token().unwrap().text();
            let string = string.strip_prefix('\'').unwrap_or(string);
            let string = string.strip_suffix('\'').unwrap_or(string);
            string.to_string()
          }
          StrRepr::MultiLine => {
            let string = s.as_token().unwrap().text();
            let string = string.strip_prefix(r#"""""#).unwrap_or(string);
            let string = match string.strip_prefix("\r\n") {
              Some(s) => s,
              None => string.strip_prefix('\n').unwrap_or(string),
            };
            let string = string.strip_suffix(r#"""""#).unwrap_or(string);
            match unescape(string) {
              Ok(s) => s,
              Err(_) => {
                self.inner.errors.update(|errors| {
                  errors.push(Error::InvalidEscapeSequence {
                    string: s.clone()
                  })
                });
                String::new()
              }
            }
          }
          StrRepr::MultiLineLiteral => {
            let string = s.as_token().unwrap().text();
            let string = string.strip_prefix(r#"'''"#).unwrap_or(string);
            let string = match string.strip_prefix("\r\n") {
              Some(s) => s,
              None => string.strip_prefix('\n').unwrap_or(string),
            };
            let string = string.strip_suffix(r#"'''"#).unwrap_or(string);
            string.to_string()
          }
        })
        .unwrap_or_default()
    })
  }

  fn validate_impl(&self) -> Result<(), &Shared<Vec<Error>>> {
    let _ = self.value();
    if self.errors().read().as_ref().is_empty() {
      Ok(())
    } else {
      Err(self.errors())
    }
  }
}

#[derive(Debug, Copy, Clone)]
pub enum StrRepr {
  Basic,
  MultiLine,
  Literal,
  MultiLineLiteral,
}

#[derive(Debug)]
pub(crate) struct IntegerInner {
  pub(crate) errors: Shared<Vec<Error>>,
  pub(crate) syntax: Option<SyntaxElement>,
  pub(crate) repr:   IntegerRepr,
  pub(crate) value:  OnceCell<IntegerValue>,
}

wrap_node! {
    #[derive(Debug, Clone)]
    pub struct Integer { inner: IntegerInner }
}

impl Integer {
  /// An integer value.
  pub fn value(&self) -> IntegerValue {
    *self.inner.value.get_or_init(|| {
      if let Some(s) = self.syntax().and_then(|s| s.as_token()) {
        let int_text = s.text().replace('_', "");

        match self.inner.repr {
          IntegerRepr::Dec => {
            if s.text().starts_with('-') {
              IntegerValue::Negative(int_text.parse().unwrap_or_default())
            } else {
              IntegerValue::Positive(int_text.parse().unwrap_or_default())
            }
          }
          IntegerRepr::Bin => IntegerValue::Positive(u64::from_str_radix(int_text.trim_start_matches("0b"), 2).unwrap_or_default()),
          IntegerRepr::Oct => IntegerValue::Positive(u64::from_str_radix(int_text.trim_start_matches("0o"), 8).unwrap_or_default()),
          IntegerRepr::Hex => IntegerValue::Positive(u64::from_str_radix(int_text.trim_start_matches("0x"), 16).unwrap_or_default()),
        }
      } else {
        IntegerValue::Positive(0)
      }
    })
  }

  fn validate_impl(&self) -> Result<(), &Shared<Vec<Error>>> {
    if self.errors().read().as_ref().is_empty() {
      Ok(())
    } else {
      Err(self.errors())
    }
  }
}

#[derive(Debug, Copy, Clone)]
pub enum IntegerRepr {
  Dec,
  Bin,
  Oct,
  Hex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegerValue {
  Negative(i64),
  Positive(u64),
}

impl IntegerValue {
  /// Returns `true` if the integer value is [`Negative`].
  ///
  /// [`Negative`]: IntegerValue::Negative
  pub fn is_negative(&self) -> bool {
    matches!(self, Self::Negative(..))
  }

  /// Returns `true` if the integer value is [`Positive`].
  ///
  /// [`Positive`]: IntegerValue::Positive
  pub fn is_positive(&self) -> bool {
    matches!(self, Self::Positive(..))
  }

  pub fn as_negative(&self) -> Option<i64> {
    if let Self::Negative(v) = self { Some(*v) } else { None }
  }

  pub fn as_positive(&self) -> Option<u64> {
    if let Self::Positive(v) = self { Some(*v) } else { None }
  }
}

impl core::fmt::Display for IntegerValue {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      IntegerValue::Negative(v) => v.fmt(f),
      IntegerValue::Positive(v) => v.fmt(f),
    }
  }
}

#[derive(Debug)]
pub(crate) struct FloatInner {
  pub(crate) errors: Shared<Vec<Error>>,
  pub(crate) syntax: Option<SyntaxElement>,
  pub(crate) value:  OnceCell<f64>,
}

wrap_node! {
    #[derive(Debug, Clone)]
    pub struct Float { inner: FloatInner }
}

impl Float {
  /// A float value.
  pub fn value(&self) -> f64 {
    *self.inner.value.get_or_init(|| {
      if let Some(text) = self.syntax().and_then(|s| s.as_token()).map(|s| s.text()) {
        text.replace('_', "").replace("nan", "NaN").parse().unwrap()
      } else {
        0_f64
      }
    })
  }

  fn validate_impl(&self) -> Result<(), &Shared<Vec<Error>>> {
    let _ = self.value();
    if self.errors().read().as_ref().is_empty() {
      Ok(())
    } else {
      Err(self.errors())
    }
  }
}

#[derive(Debug)]
pub(crate) struct DateTimeInner {
  pub(crate) errors: Shared<Vec<Error>>,
  pub(crate) syntax: Option<SyntaxElement>,
  pub(crate) value:  OnceCell<DateTimeValue>,
}

wrap_node! {
    #[derive(Debug, Clone)]
    pub struct DateTime { inner: DateTimeInner }
}

impl DateTime {
  pub fn value(&self) -> DateTimeValue {
    *self.inner.value.get_or_init(|| {
      if let Some(token) = self.syntax().and_then(|s| s.as_token()) {
        let mut text = token.text().to_string();

        // SAFETY: we're replacing single-byte characters.
        unsafe {
          for b in text.as_bytes_mut() {
            if *b == b' ' || *b == b't' {
              *b = b'T';
            } else if *b == b'z' {
              *b = b'Z';
            } else if *b == b',' {
              *b = b'.';
            }
          }
        }

        match token.kind() {
          SyntaxKind::DATE_TIME_OFFSET => {
            if let Ok(d) = time::OffsetDateTime::parse(&text, &time::format_description::well_known::Rfc3339) {
              return DateTimeValue::OffsetDateTime(d);
            }
          }
          SyntaxKind::DATE_TIME_LOCAL => {
            let desc = if text.contains('.') {
              format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond]")
            } else {
              format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]")
            };

            if let Ok(d) = time::PrimitiveDateTime::parse(&text, &desc) {
              return DateTimeValue::LocalDateTime(d);
            }
          }
          SyntaxKind::DATE => {
            if let Ok(d) = time::Date::parse(&text, &format_description!("[year]-[month]-[day]")) {
              return DateTimeValue::Date(d);
            }
          }
          SyntaxKind::TIME => {
            let desc = if text.contains('.') {
              format_description!("[hour]:[minute]:[second].[subsecond]")
            } else {
              format_description!("[hour]:[minute]:[second]")
            };

            if let Ok(d) = time::Time::parse(&text, &desc) {
              return DateTimeValue::Time(d);
            }
          }
          _ => {}
        }

        DateTimeValue::OffsetDateTime(time::OffsetDateTime::UNIX_EPOCH)
      } else {
        DateTimeValue::OffsetDateTime(time::OffsetDateTime::UNIX_EPOCH)
      }
    })
  }

  fn validate_impl(&self) -> Result<(), &Shared<Vec<Error>>> {
    if self.errors().read().as_ref().is_empty() {
      Ok(())
    } else {
      Err(self.errors())
    }
  }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum DateTimeValue {
  OffsetDateTime(time::OffsetDateTime),
  LocalDateTime(time::PrimitiveDateTime),
  Date(time::Date),
  Time(time::Time),
}

impl core::fmt::Display for DateTimeValue {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      DateTimeValue::OffsetDateTime(dt) => dt.format(&time::format_description::well_known::Rfc3339).unwrap().fmt(f),
      DateTimeValue::LocalDateTime(dt) => dt
        .format(if dt.time().nanosecond() > 0 {
          &format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond]")
        } else {
          &format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]")
        })
        .unwrap()
        .fmt(f),
      DateTimeValue::Date(date) => date.format(&format_description!("[year]-[month]-[day]")).unwrap().fmt(f),
      DateTimeValue::Time(time) => time
        .format(if time.nanosecond() > 0 {
          &format_description!("[hour]:[minute]:[second].[subsecond]")
        } else {
          &format_description!("[hour]:[minute]:[second]")
        })
        .unwrap()
        .fmt(f),
    }
  }
}

#[derive(Debug)]
pub(crate) struct InvalidInner {
  pub(crate) errors: Shared<Vec<Error>>,
  pub(crate) syntax: Option<SyntaxElement>,
}

wrap_node! {
    #[derive(Debug, Clone)]
    pub struct Invalid { inner: InvalidInner }
}

impl Invalid {
  fn validate_impl(&self) -> Result<(), &Shared<Vec<Error>>> {
    if self.errors().read().as_ref().is_empty() {
      Ok(())
    } else {
      Err(self.errors())
    }
  }
}

#[cfg(test)]
mod tests {
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;

  use super::Key;
  use super::Node;
  use crate::dom::Keys;
  use crate::dom::error::Error;
  use crate::dom::node::DomNode;
  use crate::parser::parse;

  fn clean_dom(source: &str) -> Result<Node, TestFailure> {
    let parsed = parse(source);
    ensure(parsed.errors.is_empty(), "the DOM insertion fixture must parse cleanly")?;
    Ok(parsed.into_dom())
  }

  fn path(root: &Node, dotted: &str) -> Result<Node, TestFailure> {
    let keys = ensure_ok(dotted.parse::<Keys>(), "the test path must be valid dotted keys")?;
    ensure_some(root.path(&keys), "the expected DOM path must exist")
  }

  fn conflict_count(root: &Node) -> usize {
    match root.validate() {
      Ok(()) => 0,
      Err(errors) => errors.filter(|error| matches!(error, Error::ConflictingKeys { .. })).count(),
    }
  }

  #[test]
  fn shared_pseudo_tables_merge_entries_and_source_ranges() -> Result<(), TestFailure> {
    let root = clean_dom("a.b.c = 1\na.b.d = 2\n")?;
    ensure(
      matches!(path(&root, "a.b.c")?, Node::Integer(_)),
      "the first dotted-key leaf must survive the pseudo-table merge",
    )?;
    ensure(
      matches!(path(&root, "a.b.d")?, Node::Integer(_)),
      "the second dotted-key leaf must survive the pseudo-table merge",
    )?;

    let Node::Table(root_table) = &root else {
      return ensure(false, "a parsed document root must be a table");
    };
    let entries = root_table.entries().read();
    let root_key = ensure_some(entries.iter().next().map(|(key, _)| key), "the root pseudo-table key must exist")?;
    ensure_eq(
      &root_key.text_ranges().count(),
      &2,
      "the shared root key must retain both source occurrences",
    )?;

    let Node::Table(a_table) = path(&root, "a")? else {
      return ensure(false, "the shared root path must remain a table");
    };
    let a_entries = a_table.entries().read();
    let b_key = ensure_some(
      a_entries.iter().next().map(|(key, _)| key),
      "the nested pseudo-table key must exist",
    )?;
    ensure_eq(
      &b_key.text_ranges().count(),
      &2,
      "the shared nested key must retain both source occurrences",
    )?;
    ensure_eq(&conflict_count(&root), &0, "compatible pseudo tables must not create conflicts")
  }

  #[test]
  fn incompatible_collisions_report_conflicts_and_keep_history() -> Result<(), TestFailure> {
    let value_one = path(&clean_dom("a = 1\n")?, "a")?;
    let value_two = path(&clean_dom("a = 2\n")?, "a")?;
    let pseudo_table = path(&clean_dom("a.b = 1\n")?, "a")?;
    let regular_table = path(&clean_dom("[a]\n")?, "a")?;

    for (existing, incoming) in [
      (pseudo_table.clone(), value_two.clone()),
      (regular_table, value_two.clone()),
      (value_one, value_two),
    ] {
      let Node::Table(root_table) = clean_dom("")? else {
        return ensure(false, "an empty document root must be a table");
      };
      root_table.add_entry(Key::new("collision"), existing);
      root_table.add_entry(Key::new("collision"), incoming.clone());
      let root = Node::Table(root_table.clone());
      ensure(
        conflict_count(&root) > 0,
        "every incompatible collision kind must report a key conflict",
      )?;
      ensure_eq(
        &root_table.entries().read().len(),
        &2,
        "replacement must retain insertion history alongside the lookup winner",
      )?;
      let incoming_syntax = incoming.syntax().map(ToString::to_string);
      let lookup_syntax = root_table
        .get("collision")
        .and_then(|node| node.syntax().map(ToString::to_string));
      ensure(lookup_syntax == incoming_syntax, "the incoming value must become the lookup winner")?;
    }
    Ok(())
  }

  #[test]
  fn nonconflicting_insertion_is_clean() -> Result<(), TestFailure> {
    let root = clean_dom("a = 1\nb = 2\n")?;
    ensure_eq(&conflict_count(&root), &0, "distinct keys must not create a conflict")?;
    ensure(
      matches!(path(&root, "a")?, Node::Integer(_)) && matches!(path(&root, "b")?, Node::Integer(_)),
      "both distinct entries must remain addressable",
    )
  }
}
