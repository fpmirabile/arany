use super::ToolError;
use super::types::{MAX_TOOL_RESULT_BYTES, hex_digest};
use serde::de::{DeserializeSeed, Error, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fmt;

struct Budget {
    nodes: usize,
    bytes: usize,
    max_nodes: usize,
    max_bytes: usize,
    max_depth: usize,
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
            let value = map.next_value_seed(Bounded {
                budget: self.budget,
                depth: self.depth + 1,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

pub(super) fn metadata(expected_name: &str, bytes: &[u8]) -> Result<Value, ToolError> {
    if bytes.len() > MAX_TOOL_RESULT_BYTES {
        return Err(ToolError::Limit);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ToolError::Configuration)?;
    let text = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or(ToolError::Configuration)?;
    let boundary = text
        .split_inclusive('\n')
        .scan(0, |offset, line| {
            let start = *offset;
            *offset += line.len();
            Some((start, line.trim_end_matches(['\r', '\n'])))
        })
        .find_map(|(offset, line)| (line == "---").then_some(offset))
        .ok_or(ToolError::Configuration)?;
    let yaml = &text[..boundary];
    let mut documents = yaml_serde::Deserializer::from_str(yaml);
    let mut budget = Budget {
        nodes: 0,
        bytes: 0,
        max_nodes: 256,
        max_bytes: 16 * 1024,
        max_depth: 8,
    };
    let metadata = Bounded {
        budget: &mut budget,
        depth: 0,
    }
    .deserialize(documents.next().ok_or(ToolError::Configuration)?)
    .map_err(|_| ToolError::Configuration)?;
    if documents.next().is_some()
        || !metadata.is_object()
        || metadata["name"].as_str() != Some(expected_name)
    {
        return Err(ToolError::Configuration);
    }
    let description = metadata["description"]
        .as_str()
        .filter(|value| !value.trim().is_empty() && value.len() <= 1024)
        .ok_or(ToolError::Configuration)?;
    Ok(
        json!({"name":expected_name,"description":description,"sha256":hex_digest(bytes),"behavioral_fields":"guidance only; no permissions, interpolation, installation or fork activation"}),
    )
}

pub(super) fn parse_json(bytes: &[u8], max_bytes: usize) -> Result<Value, ToolError> {
    if bytes.len() > max_bytes {
        return Err(ToolError::Limit);
    }
    let mut budget = Budget {
        nodes: 0,
        bytes: 0,
        max_nodes: 4096,
        max_bytes,
        max_depth: 32,
    };
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let value = Bounded {
        budget: &mut budget,
        depth: 0,
    }
    .deserialize(&mut decoder)
    .map_err(|_| ToolError::Operation)?;
    decoder.end().map_err(|_| ToolError::Operation)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn portable_metadata_accepts_real_scalars_and_rejects_ambiguity() {
        for source in [
            "---\nname: useful\ndescription: 'Useful: a skill'\n---\nBody",
            "---\nname: useful\ndescription: >\n  Read selected files\n  carefully.\nallowed-tools: Bash\n---\nBody",
            "---\r\nname: useful\r\ndescription: >-\r\n  Read selected files\r\n---\r\nBody",
            "---\nname: useful\ndescription: !!str Read selected files\n---\nBody",
        ] {
            assert_eq!(
                metadata("useful", source.as_bytes()).expect("valid metadata")["name"],
                "useful"
            );
        }
        for source in [
            "---\nname: useful\nname: shadow\ndescription: ok\n---\nbody",
            "---\nname: other\ndescription: ok\n---\nbody",
            "---\nname: useful\ndescription: !execute whoami\n---\nbody",
            "body",
            "---\nname: useful\ndescription: false\n---\nbody",
        ] {
            assert!(
                metadata("useful", source.as_bytes()).is_err(),
                "hostile metadata"
            );
        }
    }
}
