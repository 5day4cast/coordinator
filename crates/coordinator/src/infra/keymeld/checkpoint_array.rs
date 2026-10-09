//! Classify checkpoint arrays without indexing every scalar in a binary payload.
use serde::de::{Deserializer, SeqAccess, Visitor};
use serde_json::value::RawValue;

pub(super) fn has_nested_values(text: &str) -> serde_json::Result<bool> {
    struct Classify;

    impl<'de> Visitor<'de> for Classify {
        type Value = bool;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a JSON array")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<bool, A::Error> {
            let mut nested = false;
            while let Some(value) = sequence.next_element::<&'de RawValue>()? {
                nested |= matches!(value.get().as_bytes().first(), Some(b'{' | b'['));
            }
            Ok(nested)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_str(text);
    let nested = deserializer.deserialize_seq(Classify)?;
    deserializer.end()?;
    Ok(nested)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_nested_values_from_scalars_and_brackets_inside_strings() {
        for text in [
            "[]",
            "[0, 255, -1, 1.2, null, true]",
            r#"["[", "{", "\\\""]"#,
        ] {
            assert!(!has_nested_values(text).unwrap(), "{text}");
        }
        for text in ["[{}]", "[[]]", "[0, null, {\"body\": [0, 1]}]", "[[0], 1]"] {
            assert!(has_nested_values(text).unwrap(), "{text}");
        }
    }

    #[test]
    fn consumes_the_entire_array_and_rejects_malformed_or_trailing_data() {
        for text in ["{}", "[0,]", "[{}, broken]", "[{}] false", "[0"] {
            assert!(has_nested_values(text).is_err(), "{text}");
        }
    }
}
