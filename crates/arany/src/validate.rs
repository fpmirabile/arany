use serde::de::{DeserializeSeed, Error, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, Path};

pub(crate) const MAX_REVISION_BYTES: usize = 128;
const REVISION_RANGE_SEPARATOR_BYTES: usize = 2;
pub(crate) const MAX_REVISION_RANGE_BYTES: usize =
    2 * MAX_REVISION_BYTES + REVISION_RANGE_SEPARATOR_BYTES;

pub(crate) const FORBIDDEN_NAMES: [&str; 7] = [
    ".git", ".aws", ".ssh", ".gnupg", ".env", ".npmrc", ".pypirc",
];

pub(crate) fn valid_revision(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_REVISION_BYTES {
        return false;
    }
    let (name, mut suffix) = value.split_at(value.find(['~', '^']).unwrap_or(value.len()));
    let name_valid = !name.is_empty()
        && !name.contains("..")
        && name.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with(['.', '-'])
                && !part.ends_with('.')
                && !part.ends_with(".lock")
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        });
    let mut steps = 0;
    while let Some(rest) = suffix.strip_prefix(['~', '^']) {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 4 {
            return false;
        }
        suffix = &rest[digits..];
        steps += 1;
    }
    name_valid && suffix.is_empty() && steps <= 4
}

pub(crate) const MAX_PATH_BYTES: usize = 4096;
pub(crate) const MAX_RELATIVE_PATH_BYTES: usize = 512;

pub(crate) fn valid_relative(value: &str, root: bool) -> bool {
    if value.is_empty() || value == "." {
        return root;
    }
    value.len() <= MAX_RELATIVE_PATH_BYTES
        && !value.chars().any(char::is_control)
        && !value.contains('\\')
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".." && !forbidden(part))
        && Path::new(value)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

pub(crate) fn forbidden(value: &str) -> bool {
    FORBIDDEN_NAMES
        .iter()
        .any(|name| value.eq_ignore_ascii_case(name))
        || value
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(".env."))
}

pub(crate) fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

pub(crate) fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn deserialize_value_bounded<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
    max_bytes: usize,
    max_nodes: usize,
    max_depth: usize,
    count_keys: bool,
) -> Result<Value, D::Error> {
    let mut budget = Budget {
        count_keys,
        nodes: 0,
        bytes: 0,
        max_nodes,
        max_bytes,
        max_depth,
    };
    Bounded {
        budget: &mut budget,
        depth: 0,
    }
    .deserialize(deserializer)
}

#[derive(Debug)]
pub(crate) enum JsonError {
    Limit,
    Operation,
}

pub(crate) fn parse_json_bounded(
    bytes: &[u8],
    max_bytes: usize,
    max_nodes: usize,
    max_depth: usize,
) -> Result<Value, JsonError> {
    if bytes.len() > max_bytes {
        return Err(JsonError::Limit);
    }
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let value = deserialize_value_bounded(&mut decoder, max_bytes, max_nodes, max_depth, false)
        .map_err(|_| JsonError::Operation)?;
    decoder.end().map_err(|_| JsonError::Operation)?;
    Ok(value)
}

struct Budget {
    nodes: usize,
    bytes: usize,
    max_nodes: usize,
    max_bytes: usize,
    max_depth: usize,
    count_keys: bool,
}
struct Bounded<'a> {
    budget: &'a mut Budget,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for Bounded<'_> {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        if self.depth > self.budget.max_depth || self.budget.nodes >= self.budget.max_nodes {
            return Err(D::Error::custom("metadata limit"));
        }
        self.budget.nodes += 1;
        deserializer.deserialize_any(self)
    }
}

impl Bounded<'_> {
    fn scalar<E: Error>(&mut self, value: &str) -> Result<(), E> {
        self.budget.bytes = self
            .budget
            .bytes
            .checked_add(value.len())
            .ok_or_else(|| E::custom("metadata limit"))?;
        if self.budget.bytes > self.budget.max_bytes {
            return Err(E::custom("metadata limit"));
        }
        Ok(())
    }
}

impl<'de> Visitor<'de> for Bounded<'_> {
    type Value = Value;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded portable Skill metadata")
    }
    fn visit_str<E: Error>(mut self, value: &str) -> Result<Value, E> {
        self.scalar(value)?;
        Ok(json!(value))
    }
    fn visit_string<E: Error>(mut self, value: String) -> Result<Value, E> {
        self.scalar(&value)?;
        Ok(json!(value))
    }
    fn visit_bool<E: Error>(self, value: bool) -> Result<Value, E> {
        Ok(json!(value))
    }
    fn visit_i64<E: Error>(self, value: i64) -> Result<Value, E> {
        Ok(json!(value))
    }
    fn visit_u64<E: Error>(self, value: u64) -> Result<Value, E> {
        Ok(json!(value))
    }
    fn visit_f64<E: Error>(self, value: f64) -> Result<Value, E> {
        if value.is_finite() {
            Ok(json!(value))
        } else {
            Err(E::custom("nonfinite metadata"))
        }
    }
    fn visit_unit<E: Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E: Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(Bounded {
            budget: self.budget,
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut values = serde_json::Map::new();
        let mut seen = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(A::Error::custom("duplicate metadata key"));
            }
            self.budget.bytes = self
                .budget
                .bytes
                .checked_add(key.len())
                .ok_or_else(|| A::Error::custom("metadata limit"))?;
            if self.budget.bytes > self.budget.max_bytes {
                return Err(A::Error::custom("metadata limit"));
            }
            if self.budget.count_keys {
                self.budget.nodes += 1;
                if self.budget.nodes > self.budget.max_nodes {
                    return Err(A::Error::custom("metadata limit"));
                }
            }
            let value = map.next_value_seed(Bounded {
                budget: self.budget,
                depth: self.depth + 1,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

pub fn bounded_vec<'de, D, T, const LIMIT: usize>(
    deserializer: D,
    limit_error: &'static str,
) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    bounded_sequence::<D, T, serde::de::IgnoredAny, LIMIT>(
        deserializer,
        format_args!("at most {LIMIT} entries"),
        limit_error,
        |_| Ok(()),
    )
}

// Excess preserves each caller's first-excess behavior: decode T for worker/catalog, skip for images/CLI.
pub(crate) fn bounded_sequence<'de, D, T, Excess, const LIMIT: usize>(
    deserializer: D,
    expecting: impl fmt::Display,
    limit_error: &'static str,
    check: impl FnMut(&[T]) -> Result<(), &'static str>,
) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
    Excess: serde::Deserialize<'de>,
{
    struct Sequence<T, Excess, F, V> {
        limit: usize,
        expecting: F,
        limit_error: &'static str,
        check: V,
        marker: std::marker::PhantomData<(T, Excess)>,
    }
    impl<'de, T, Excess, F, V> Visitor<'de> for Sequence<T, Excess, F, V>
    where
        T: serde::Deserialize<'de>,
        Excess: serde::Deserialize<'de>,
        F: fmt::Display,
        V: FnMut(&[T]) -> Result<(), &'static str>,
    {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "{}", self.expecting)
        }

        fn visit_seq<A: SeqAccess<'de>>(mut self, mut sequence: A) -> Result<Vec<T>, A::Error> {
            let mut values = Vec::new();
            while values.len() < self.limit {
                let Some(value) = sequence.next_element()? else {
                    return Ok(values);
                };
                values.push(value);
                (self.check)(&values).map_err(A::Error::custom)?;
            }
            if sequence.next_element::<Excess>()?.is_some() {
                return Err(A::Error::custom(self.limit_error));
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Sequence::<T, Excess, _, _> {
        limit: LIMIT,
        expecting,
        limit_error,
        check,
        marker: std::marker::PhantomData,
    })
}
