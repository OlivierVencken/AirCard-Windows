//! Build metadata candidates while retaining exact original bytes

use std::collections::HashSet;
use std::fmt;

use anyhow::{Result, ensure};
use serde::de::{Deserialize, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

use super::artwork::{DynamicConfiguration, sha256};

pub const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_DEPTH: usize = 64;

struct UniqueValue(usize);

impl<'de> DeserializeSeed<'de> for UniqueValue {
    type Value = Value;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        if self.0 > MAX_DEPTH {
            return Err(serde::de::Error::custom("JSON nesting exceeds 64 levels"));
        }
        let raw = Box::<serde_json::value::RawValue>::deserialize(deserializer)?;
        let mut parser = serde_json::Deserializer::from_str(raw.get());
        // Parse objects explicitly to avoid serde_json arbitrary-number marker collisions
        use serde::Deserializer;
        match raw.get().as_bytes()[0] {
            b'{' => parser
                .deserialize_map(self)
                .map_err(serde::de::Error::custom),
            b'[' => parser
                .deserialize_seq(self)
                .map_err(serde::de::Error::custom),
            b'-' | b'0'..=b'9' => serde_json::from_str::<serde_json::Number>(raw.get())
                .map(Value::Number)
                .map_err(serde::de::Error::custom),
            _ => serde_json::from_str(raw.get()).map_err(serde::de::Error::custom),
        }
    }
}

impl<'de> Visitor<'de> for UniqueValue {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("JSON with unique object keys")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut values = Map::new();
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON key: {key}"
                )));
            }
            values.insert(key, map.next_value_seed(UniqueValue(self.0 + 1))?);
        }
        Ok(Value::Object(values))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(UniqueValue(self.0 + 1))? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
}

pub fn parse_unique(bytes: &[u8]) -> Result<Value> {
    ensure!(bytes.len() <= MAX_JSON_BYTES, "JSON exceeds 4 MiB");
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = UniqueValue(0).deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

pub fn candidate(original: &[u8], config: &DynamicConfiguration) -> Result<(Vec<u8>, Vec<u8>)> {
    config.validate()?;
    let mut value = parse_unique(original)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Original pass JSON must be an object"))?;
    ensure!(
        !object.is_empty(),
        "Supply the complete captured pass JSON, not an empty object"
    );
    let dynamic = serde_json::to_value(config)?;
    if let Some(existing) = object.get("dynamicLayerConfiguration") {
        // Only add typed effect members to an exact supported subset. Preserve
        // every existing value; unknown members, removals and preset changes fail.
        if existing != &dynamic
            && existing.get("version") == Some(&serde_json::json!(2))
            && existing.get("parallaxEnabled") == Some(&serde_json::json!(true))
            && existing.as_object().is_some_and(|members| {
                members
                    .iter()
                    .all(|(key, value)| dynamic.get(key) == Some(value))
            })
        {
            let mut changes = Vec::new();
            for key in ["crossDissolve", "transactionEffect", "neutralEmitter"] {
                if existing.get(key).is_none()
                    && let Some(value) = dynamic.get(key)
                {
                    changes.push(serde_json::json!({"operation":"add", "path":format!("/dynamicLayerConfiguration/{key}"), "value":value}));
                }
            }
            object.insert("dynamicLayerConfiguration".into(), dynamic.clone());
            let diff = serde_json::json!({
                "original_sha256": sha256(original),
                "changes": changes
            });
            return Ok((
                serde_json::to_vec_pretty(&value)?,
                serde_json::to_vec_pretty(&diff)?,
            ));
        }
        ensure!(
            existing == &dynamic,
            "This card has a different effects configuration; it cannot be replaced automatically"
        );
        // Preserve existing matching metadata byte-for-byte
        let diff = serde_json::json!({
            "original_sha256": sha256(original), "changes": []
        });
        return Ok((original.to_vec(), serde_json::to_vec_pretty(&diff)?));
    }
    object.insert("dynamicLayerConfiguration".into(), dynamic.clone());
    let diff = serde_json::json!({
        "original_sha256": sha256(original),
        "changes": [{"operation": "add", "path": "/dynamicLayerConfiguration", "value": dynamic}]
    });
    Ok((
        serde_json::to_vec_pretty(&value)?,
        serde_json::to_vec_pretty(&diff)?,
    ))
}

/// Managed-only replacement. The caller must reconcile the complete fresh
/// snapshot against owned history before supplying its exact metadata checksum.
/// The legacy additive-only `candidate` contract remains unchanged.
pub(super) fn replace_managed(
    original: &[u8],
    expected_sha256: &str,
    desired: super::configuration::Selection,
) -> Result<Vec<u8>> {
    ensure!(
        sha256(original) == expected_sha256,
        "Managed metadata preimage changed"
    );
    let mut value = parse_unique(original)?;
    let object = value
        .as_object_mut()
        .filter(|object| !object.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Complete captured pass JSON is required"))?;
    let dynamic = desired
        .native_configuration()
        .map(serde_json::to_value)
        .transpose()?;
    if object.get("dynamicLayerConfiguration") == dynamic.as_ref() {
        return Ok(original.to_vec());
    }
    match dynamic {
        Some(dynamic) => {
            object.insert("dynamicLayerConfiguration".into(), dynamic);
        }
        None => {
            object.remove("dynamicLayerConfiguration");
        }
    }
    let bytes = serde_json::to_vec_pretty(&value)?;
    ensure!(
        bytes.len() <= MAX_JSON_BYTES,
        "Managed metadata exceeds 4 MiB"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::super::artwork::{
        EmitterConfiguration, EmitterDensity, EmitterPhysics, EmitterStyle,
    };
    use super::*;

    #[test]
    fn particles_add_without_removing_cross_dissolve_or_transaction_members() {
        for cross in [false, true] {
            for transaction in [false, true] {
                let config = DynamicConfiguration {
                    version: 2,
                    parallax_enabled: true,
                    cross_dissolve: cross.then(Default::default),
                    transaction_effect: transaction.then(Default::default),
                    neutral_emitter: Some(EmitterConfiguration {
                        style: EmitterStyle::Twinkling,
                        density: EmitterDensity::Low,
                        physics_effect: EmitterPhysics::Off,
                    }),
                };
                let mut base = serde_json::to_value(&config).unwrap();
                base.as_object_mut().unwrap().remove("neutralEmitter");
                let original = serde_json::json!({"serialNumber":"preserved", "opaque":[null,1,"a"], "dynamicLayerConfiguration":base});
                let (bytes, diff) =
                    candidate(&serde_json::to_vec(&original).unwrap(), &config).unwrap();
                let mut expected = original.clone();
                expected["dynamicLayerConfiguration"]["neutralEmitter"] =
                    serde_json::to_value(&config).unwrap()["neutralEmitter"].clone();
                assert_eq!(parse_unique(&bytes).unwrap(), expected);
                assert_eq!(
                    parse_unique(&diff).unwrap()["changes"]
                        .as_array()
                        .unwrap()
                        .len(),
                    1
                );
                assert_eq!(
                    parse_unique(&diff).unwrap()["changes"][0]["path"],
                    "/dynamicLayerConfiguration/neutralEmitter"
                );
                assert_eq!(candidate(&bytes, &config).unwrap().0, bytes);
                let mut disabled = config.clone();
                disabled.neutral_emitter = None;
                assert!(candidate(&bytes, &disabled).is_err());
                let mut unknown = original.clone();
                unknown["dynamicLayerConfiguration"]["unknown"] = true.into();
                assert!(candidate(&serde_json::to_vec(&unknown).unwrap(), &config).is_err());
                let mut changed = expected.clone();
                changed["dynamicLayerConfiguration"]["neutralEmitter"]["style"] = "falling".into();
                assert!(candidate(&serde_json::to_vec(&changed).unwrap(), &config).is_err());
                if cross {
                    let mut disabled = config.clone();
                    disabled.cross_dissolve = None;
                    assert!(candidate(&bytes, &disabled).is_err());
                }
                if transaction {
                    let mut disabled = config.clone();
                    disabled.transaction_effect = None;
                    assert!(candidate(&bytes, &disabled).is_err());
                }
            }
        }
    }

    #[test]
    fn transaction_adds_only_to_exact_supported_layers_and_preserves_repeats() {
        for cross in [false, true] {
            let config = DynamicConfiguration {
                version: 2,
                parallax_enabled: true,
                neutral_emitter: None,
                cross_dissolve: cross.then(Default::default),
                transaction_effect: Some(Default::default()),
            };
            let mut base = serde_json::to_value(&config).unwrap();
            base.as_object_mut().unwrap().remove("transactionEffect");
            let original = serde_json::to_vec(&serde_json::json!({"serialNumber":"retained", "opaque":[null,123], "dynamicLayerConfiguration":base})).unwrap();
            let (bytes, diff) = candidate(&original, &config).unwrap();
            assert_eq!(
                parse_unique(&diff).unwrap()["changes"][0]["path"],
                "/dynamicLayerConfiguration/transactionEffect"
            );
            assert_eq!(candidate(&bytes, &config).unwrap().0, bytes);
            let mut disabled = config.clone();
            disabled.transaction_effect = None;
            assert!(candidate(&bytes, &disabled).is_err());
            let mut unknown = parse_unique(&original).unwrap();
            unknown["dynamicLayerConfiguration"]["unknown"] = true.into();
            assert!(candidate(&serde_json::to_vec(&unknown).unwrap(), &config).is_err());
            let mut wrong = parse_unique(&bytes).unwrap();
            wrong["dynamicLayerConfiguration"]["transactionEffect"]["style"] = "other".into();
            assert!(candidate(&serde_json::to_vec(&wrong).unwrap(), &config).is_err());
        }
    }

    #[test]
    fn cross_dissolve_upgrades_only_exact_parallax_and_preserves_repeat_bytes() {
        let mut config = DynamicConfiguration {
            version: 2,
            parallax_enabled: true,
            neutral_emitter: None,
            cross_dissolve: Some(Default::default()),
            transaction_effect: None,
        };
        let original = br#" {"serialNumber":"captured","large":1844674407370955161601,"dynamicLayerConfiguration":{"parallaxEnabled":true,"version":2}} "#;
        let (bytes, diff) = candidate(original, &config).unwrap();
        let value = parse_unique(&bytes).unwrap();
        assert_eq!(
            value["dynamicLayerConfiguration"]["crossDissolve"],
            serde_json::json!({"intensity":"default"})
        );
        assert_eq!(value["large"].to_string(), "1844674407370955161601");
        assert_eq!(
            parse_unique(&diff).unwrap()["changes"][0]["path"],
            "/dynamicLayerConfiguration/crossDissolve"
        );
        assert_eq!(candidate(&bytes, &config).unwrap().0, bytes);
        for existing in [
            serde_json::json!({"version":2,"parallaxEnabled":true,"extra":true}),
            serde_json::json!({"version":2,"parallaxEnabled":false}),
            serde_json::json!({"version":2,"parallaxEnabled":true,"neutralEmitter":{"style":"twinkling","density":"low","physicsEffect":"off"}}),
            serde_json::json!({"version":2,"parallaxEnabled":true,"crossDissolve":{"intensity":"high"}}),
        ] {
            let input = serde_json::to_vec(
                &serde_json::json!({"serialNumber":"test","dynamicLayerConfiguration":existing}),
            )
            .unwrap();
            assert!(candidate(&input, &config).is_err());
        }
        config.cross_dissolve = None;
        assert!(
            candidate(&bytes, &config).is_err(),
            "Disabling must not silently replace installed effects; restore first"
        );
    }

    #[test]
    fn changing_artwork_preserves_existing_parallax_metadata_exactly() {
        let original = b" {\"serialNumber\":\"test\", \"dynamicLayerConfiguration\":{\"parallaxEnabled\":true,\"version\":2},\"opaque\":1844674407370955161601}\n";
        let config = DynamicConfiguration {
            version: 2,
            parallax_enabled: true,
            neutral_emitter: None,
            cross_dissolve: None,
            transaction_effect: None,
        };
        let (bytes, diff) = candidate(original, &config).unwrap();
        assert_eq!(bytes, original);
        assert_eq!(
            parse_unique(&diff).unwrap()["changes"],
            serde_json::json!([])
        );
        for other in [
            br#"{"dynamicLayerConfiguration":{"version":2,"parallaxEnabled":false}}"#.as_slice(),
            br#"{"dynamicLayerConfiguration":{"version":2,"parallaxEnabled":true,"extra":true}}"#
                .as_slice(),
        ] {
            assert!(candidate(other, &config).is_err());
        }
    }

    #[test]
    fn rejects_ambiguous_or_unbounded_json() {
        for input in [
            r#"{"a":1,"a":2}"#,
            r#"{"a":[{"b":1,"b":2}]}"#,
            r#"{"a":1,"\u0061":2}"#,
            "{",
            "{} {}",
        ] {
            assert!(parse_unique(input.as_bytes()).is_err(), "{input}");
        }
        assert!(parse_unique(&vec![b' '; MAX_JSON_BYTES + 1]).is_err());
        let deep = format!("{}0{}", "[".repeat(66), "]".repeat(66));
        assert!(parse_unique(deep.as_bytes()).is_err());
    }

    #[test]
    fn patches_only_dynamic_member_and_binds_exact_original() {
        let original = b" {\"serialNumber\":\"captured\",\"opaque\":[null,true,{\"n\":42}]}\n";
        let config = DynamicConfiguration {
            version: 2,
            parallax_enabled: true,
            neutral_emitter: None,
            cross_dissolve: None,
            transaction_effect: None,
        };
        let (bytes, diff) = candidate(original, &config).unwrap();
        let mut patched = parse_unique(&bytes).unwrap();
        patched
            .as_object_mut()
            .unwrap()
            .remove("dynamicLayerConfiguration");
        assert_eq!(patched, parse_unique(original).unwrap());
        assert_eq!(
            parse_unique(&diff).unwrap()["original_sha256"],
            sha256(original)
        );
        for input in ["[]", "null", "{}", r#"{"dynamicLayerConfiguration":null}"#] {
            assert!(candidate(input.as_bytes(), &config).is_err());
        }
    }

    #[test]
    fn preserves_large_precise_numbers_and_private_marker_objects() {
        let original = br#"{"large":1844674407370955161601,"decimal":1.234567890123456789,"marker":{"$serde_json::private::Number":"123"}}"#;
        let config = DynamicConfiguration {
            version: 2,
            parallax_enabled: true,
            neutral_emitter: None,
            cross_dissolve: None,
            transaction_effect: None,
        };
        let (bytes, _) = candidate(original, &config).unwrap();
        let before = parse_unique(original).unwrap();
        let mut after = parse_unique(&bytes).unwrap();
        after
            .as_object_mut()
            .unwrap()
            .remove("dynamicLayerConfiguration");
        assert_eq!(before, after);
        assert!(after["marker"].is_object());
        assert_eq!(after["large"].to_string(), "1844674407370955161601");
        assert_eq!(after["decimal"].to_string(), "1.234567890123456789");
    }
}
