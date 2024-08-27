use serde::{Deserialize, Deserializer, Serializer};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

pub fn serialize<S>(value: &Option<OffsetDateTime>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if let Some(datetime) = value {
        let s = datetime
            .format(&Rfc3339)
            .map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&s)
    } else {
        serializer.serialize_str("null")
    }
}

#[allow(dead_code)]
pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<OffsetDateTime>, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    if s == "null" {
        Ok(None)
    } else {
        OffsetDateTime::parse(&s, &Rfc3339)
            .map(Some)
            .map_err(serde::de::Error::custom)
    }
}
