use serde::Deserializer;
use serde::de::{self, DeserializeOwned, IntoDeserializer, Visitor};
use serde_json::{Map, Value};

use crate::error::CommandError;

pub(super) struct Args(Map<String, Value>);

impl Args {
    pub(super) fn new(value: Value) -> Result<Self, CommandError> {
        match value {
            Value::Object(map) => Ok(Self(map)),
            Value::Null => Ok(Self(Map::new())),
            _ => Err(CommandError::BadRequest(
                "Invoke args must be a JSON object".into(),
            )),
        }
    }

    fn value(&self, name: &str) -> Result<Option<&Value>, CommandError> {
        let alias = alternate_case(name);
        match (self.0.get(name), self.0.get(&alias)) {
            (Some(value), Some(other)) if value != other => Err(CommandError::BadRequest(format!(
                "Conflicting argument aliases: {name} / {alias}"
            ))),
            (value, other) => Ok(value.or(other)),
        }
    }

    pub(super) fn required<T: DeserializeOwned>(&self, name: &str) -> Result<T, CommandError> {
        let value = self
            .value(name)?
            .ok_or_else(|| CommandError::BadRequest(format!("Missing argument: {name}")))?;
        super::decode(value.clone(), name)
    }

    pub(super) fn optional<T: DeserializeOwned>(
        &self,
        name: &str,
    ) -> Result<Option<T>, CommandError> {
        match self.value(name)? {
            None | Some(Value::Null) => Ok(None),
            Some(value) => super::decode(value.clone(), name).map(Some),
        }
    }

    pub(super) fn default<T: DeserializeOwned + Default>(
        &self,
        name: &str,
    ) -> Result<T, CommandError> {
        Ok(self.optional(name)?.unwrap_or_default())
    }
}

fn alternate_case(name: &str) -> String {
    if name.contains('_') {
        let mut upper = false;
        name.chars()
            .filter_map(|c| {
                if c == '_' {
                    upper = true;
                    None
                } else if upper {
                    upper = false;
                    Some(c.to_ascii_uppercase())
                } else {
                    Some(c)
                }
            })
            .collect()
    } else {
        let mut result = String::new();
        for c in name.chars() {
            if c.is_ascii_uppercase() {
                result.push('_');
                result.push(c.to_ascii_lowercase());
            } else {
                result.push(c);
            }
        }
        result
    }
}

/// Match only fields declared by a typed DTO. Opaque Value maps (cards, settings,
/// messages, provider bodies and extension data) must never have their keys rewritten.
pub(super) fn decode<T: DeserializeOwned>(value: Value) -> Result<T, serde_json::Error> {
    T::deserialize(StructAliases(value))
}

struct StructAliases(Value);

impl<'de> Deserializer<'de> for StructAliases {
    type Error = serde_json::Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.0.into_deserializer().deserialize_any(visitor)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        let mut value = self.0;
        if let Value::Object(map) = &mut value {
            for field in fields {
                let alias = alternate_case(field);
                if alias == *field {
                    continue;
                }
                if let Some(other) = map.remove(&alias) {
                    if let Some(current) = map.get(*field) {
                        if current != &other {
                            return Err(de::Error::custom(format!(
                                "Conflicting DTO aliases: {field} / {alias}"
                            )));
                        }
                    } else {
                        map.insert((*field).to_owned(), other);
                    }
                }
            }
        }
        value
            .into_deserializer()
            .deserialize_struct(name, fields, visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.0.into_deserializer().deserialize_option(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0
            .into_deserializer()
            .deserialize_enum(name, variants, visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0
            .into_deserializer()
            .deserialize_newtype_struct(name, visitor)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct seq tuple tuple_struct
        map identifier ignored_any
    }
}
