//! Queued competitions as the pages show them. A queued competition takes entries without a seat
//! count and splits them into pools when its window starts; each pool is then a competition of
//! its own.
//!
//! The competition API describes them with `kind` (`single`, `queued` or `pool`); a queue's
//! `pool_rules` (`min_players`, `max_players`), `max_entries` and, once formed, `pools` (each
//! pool's `id` and `size`); and a pool's `parent_id` and `pool_index`. The pages read the same
//! fields from the competition's serialized form, so they need nothing more from the domain, and
//! a competition without them shows as a single one.

use serde::{ser, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

/// The most players a pool holds, when a queue doesn't say.
pub const MAX_POOL_PLAYERS: u64 = coordinator_escrow::pools::MAX_POOL_PLAYERS as u64;

/// How a competition takes entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Queue {
    /// A fixed number of seats, all filled before the window starts.
    #[default]
    Single,
    /// Entries without a seat count, split into pools at the start.
    Queued(QueueView),
    /// One pool of a queued competition.
    Pool(PoolOf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueView {
    /// The fewest players a pool starts with, if the competition says.
    pub min_players: Option<u64>,
    pub max_players: u64,
    /// The most entries the queue takes, if it has a cap.
    pub max_entries: Option<u64>,
    /// Its pools, in order, once they are formed.
    pub pools: Vec<PoolLink>,
}

/// A pool of a queued competition, linked from the competition's page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolLink {
    pub id: String,
    /// Its players, if the competition says.
    pub size: Option<u64>,
}

/// The queued competition a pool came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolOf {
    pub parent_id: String,
    /// Where the pool is in its competition's pools, from 0.
    pub index: Option<u64>,
}

/// The competition API's fields that describe a queue or a pool.
const FIELDS: &[&str] = &[
    "kind",
    "pool_rules",
    "max_entries",
    "pools",
    "parent_id",
    "pool_index",
];

impl Queue {
    /// How `competition` takes entries, from the API fields of its serialized form.
    pub fn of(competition: &impl Serialize) -> Self {
        Self::from_fields(&serialized_fields(competition, FIELDS))
    }

    /// Anything missing or unreadable leaves out that detail; without a readable `kind`, or a
    /// pool without its parent, the competition shows as a single one.
    pub fn from_fields(fields: &Map<String, Value>) -> Self {
        let number = |value: Option<&Value>| value.and_then(Value::as_u64);
        match fields.get("kind").and_then(Value::as_str) {
            Some("queued") => {
                let rules = fields.get("pool_rules");
                Queue::Queued(QueueView {
                    min_players: number(rules.and_then(|rules| rules.get("min_players"))),
                    max_players: number(rules.and_then(|rules| rules.get("max_players")))
                        .unwrap_or(MAX_POOL_PLAYERS),
                    max_entries: number(fields.get("max_entries")),
                    pools: fields
                        .get("pools")
                        .and_then(Value::as_array)
                        .map(|pools| pools.iter().filter_map(PoolLink::from_value).collect())
                        .unwrap_or_default(),
                })
            }
            Some("pool") => match fields.get("parent_id").and_then(competition_id) {
                Some(parent_id) => Queue::Pool(PoolOf {
                    parent_id,
                    index: number(fields.get("pool_index")),
                }),
                None => Queue::Single,
            },
            _ => Queue::Single,
        }
    }

    pub fn queued(&self) -> Option<&QueueView> {
        match self {
            Queue::Queued(queue) => Some(queue),
            _ => None,
        }
    }
}

impl QueueView {
    /// The note that says how entries are grouped.
    pub fn pool_note(&self) -> String {
        format!(
            "Players are split into pools of up to {} at the start",
            self.max_players
        )
    }
}

impl PoolLink {
    fn from_value(pool: &Value) -> Option<Self> {
        Some(Self {
            id: pool.get("id").and_then(competition_id)?,
            size: pool.get("size").and_then(Value::as_u64),
        })
    }

    pub fn url(&self) -> String {
        format!("/competitions/{}/leaderboard", self.id)
    }
}

impl PoolOf {
    pub fn url(&self) -> String {
        format!("/competitions/{}/leaderboard", self.parent_id)
    }

    /// `Pool 2`, or just `Pool` when its place is unknown.
    pub fn label(&self) -> String {
        match self.index {
            Some(index) => format!("Pool {}", index.saturating_add(1)),
            None => "Pool".to_owned(),
        }
    }
}

/// A competition id, only if it is one: ids end up in links.
fn competition_id(value: &Value) -> Option<String> {
    Uuid::parse_str(value.as_str()?)
        .ok()
        .map(|id| id.to_string())
}

/// The named top-level fields of `value` as it serializes, without serializing any other field:
/// a competition's contract and signatures are never walked.
fn serialized_fields(value: &impl Serialize, names: &'static [&'static str]) -> Map<String, Value> {
    let mut picker = FieldPicker {
        names,
        fields: Map::new(),
        pending: None,
    };
    // Only a struct or a map has fields; anything else leaves none.
    let _ = value.serialize(&mut picker);
    picker.fields
}

struct FieldPicker {
    names: &'static [&'static str],
    fields: Map<String, Value>,
    /// A wanted map key whose value comes next.
    pending: Option<String>,
}

impl FieldPicker {
    fn pick<T: ?Sized + Serialize>(
        &mut self,
        key: String,
        value: &T,
    ) -> Result<(), serde_json::Error> {
        self.fields.insert(key, serde_json::to_value(value)?);
        Ok(())
    }
}

fn no_fields() -> serde_json::Error {
    ser::Error::custom("only a struct or a map has fields")
}

macro_rules! no_fields {
    ($($method:ident($($arg:ty),*) -> $ok:ty;)*) => {
        $(fn $method(self, $(_: $arg),*) -> Result<$ok, serde_json::Error> {
            Err(no_fields())
        })*
    };
}

type Nothing = ser::Impossible<(), serde_json::Error>;

impl ser::Serializer for &mut FieldPicker {
    type Ok = ();
    type Error = serde_json::Error;
    type SerializeSeq = Nothing;
    type SerializeTuple = Nothing;
    type SerializeTupleStruct = Nothing;
    type SerializeTupleVariant = Nothing;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Nothing;

    no_fields! {
        serialize_bool(bool) -> ();
        serialize_i8(i8) -> ();
        serialize_i16(i16) -> ();
        serialize_i32(i32) -> ();
        serialize_i64(i64) -> ();
        serialize_u8(u8) -> ();
        serialize_u16(u16) -> ();
        serialize_u32(u32) -> ();
        serialize_u64(u64) -> ();
        serialize_f32(f32) -> ();
        serialize_f64(f64) -> ();
        serialize_char(char) -> ();
        serialize_str(&str) -> ();
        serialize_bytes(&[u8]) -> ();
        serialize_none() -> ();
        serialize_unit() -> ();
        serialize_unit_struct(&'static str) -> ();
        serialize_unit_variant(&'static str, u32, &'static str) -> ();
        serialize_seq(Option<usize>) -> Nothing;
        serialize_tuple(usize) -> Nothing;
        serialize_tuple_struct(&'static str, usize) -> Nothing;
        serialize_tuple_variant(&'static str, u32, &'static str, usize) -> Nothing;
        serialize_struct_variant(&'static str, u32, &'static str, usize) -> Nothing;
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<(), serde_json::Error> {
        value.serialize(self)
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        value: &T,
    ) -> Result<(), serde_json::Error> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<(), serde_json::Error> {
        Err(no_fields())
    }

    fn serialize_map(self, _: Option<usize>) -> Result<Self, serde_json::Error> {
        Ok(self)
    }

    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self, serde_json::Error> {
        Ok(self)
    }
}

impl ser::SerializeStruct for &mut FieldPicker {
    type Ok = ();
    type Error = serde_json::Error;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), serde_json::Error> {
        if self.names.contains(&key) {
            self.pick(key.to_owned(), value)?;
        }
        Ok(())
    }

    fn end(self) -> Result<(), serde_json::Error> {
        Ok(())
    }
}

/// A struct with a flattened field serializes as a map.
impl ser::SerializeMap for &mut FieldPicker {
    type Ok = ();
    type Error = serde_json::Error;

    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), serde_json::Error> {
        self.pending = match serde_json::to_value(key)? {
            Value::String(key) if self.names.contains(&key.as_str()) => Some(key),
            _ => None,
        };
        Ok(())
    }

    fn serialize_value<T: ?Sized + Serialize>(
        &mut self,
        value: &T,
    ) -> Result<(), serde_json::Error> {
        match self.pending.take() {
            Some(key) => self.pick(key, value),
            None => Ok(()),
        }
    }

    fn end(self) -> Result<(), serde_json::Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PARENT: &str = "01a0c225-f3c4-71f3-9f62-4b74859cfc25";
    const POOL: &str = "01a0c226-0000-7000-8000-000000000001";

    fn fields(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn a_competition_without_a_kind_is_single() {
        assert_eq!(Queue::from_fields(&Map::new()), Queue::Single);
        assert_eq!(
            Queue::from_fields(&fields(json!({ "kind": "single" }))),
            Queue::Single
        );
        assert_eq!(
            Queue::from_fields(&fields(json!({ "kind": 3 }))),
            Queue::Single
        );
    }

    #[test]
    fn a_queue_reads_its_rules_cap_and_pools() {
        let queue = Queue::from_fields(&fields(json!({
            "kind": "queued",
            "pool_rules": { "min_players": 3, "max_players": 20 },
            "max_entries": 200,
            "pools": [
                { "id": POOL, "size": 17 },
                { "id": "../admin", "size": 18 },
                { "id": PARENT },
            ],
        })));
        assert_eq!(
            queue,
            Queue::Queued(QueueView {
                min_players: Some(3),
                max_players: 20,
                max_entries: Some(200),
                pools: vec![
                    PoolLink {
                        id: POOL.into(),
                        size: Some(17)
                    },
                    PoolLink {
                        id: PARENT.into(),
                        size: None
                    },
                ],
            })
        );
        let bare = Queue::from_fields(&fields(json!({ "kind": "queued", "pools": null })));
        assert_eq!(
            bare,
            Queue::Queued(QueueView {
                min_players: None,
                max_players: 25,
                max_entries: None,
                pools: vec![],
            })
        );
        assert_eq!(
            bare.queued().unwrap().pool_note(),
            "Players are split into pools of up to 25 at the start"
        );
    }

    #[test]
    fn a_pool_links_back_to_its_competition() {
        let pool = Queue::from_fields(&fields(
            json!({ "kind": "pool", "parent_id": PARENT, "pool_index": 1 }),
        ));
        let Queue::Pool(of) = &pool else {
            panic!("{pool:?}");
        };
        assert_eq!(of.url(), format!("/competitions/{PARENT}/leaderboard"));
        assert_eq!(of.label(), "Pool 2");
        assert_eq!(
            Queue::from_fields(&fields(json!({ "kind": "pool", "parent_id": "x" }))),
            Queue::Single
        );
    }

    /// The fields come from the competition's serialized form, flattened or not, and nothing
    /// else is serialized.
    #[test]
    fn only_the_queue_fields_are_serialized() {
        struct Heavy;
        impl Serialize for Heavy {
            fn serialize<S: ser::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                panic!("a field the pages don't read was serialized");
            }
        }
        #[derive(Serialize)]
        struct Rules {
            min_players: u64,
            max_players: u64,
        }
        #[derive(Serialize)]
        struct Competition {
            id: &'static str,
            signed_contract: Heavy,
            kind: &'static str,
            pool_rules: Option<Rules>,
            #[serde(skip_serializing_if = "Option::is_none")]
            max_entries: Option<u64>,
        }
        let competition = Competition {
            id: PARENT,
            signed_contract: Heavy,
            kind: "queued",
            pool_rules: Some(Rules {
                min_players: 2,
                max_players: 25,
            }),
            max_entries: None,
        };
        assert_eq!(
            Queue::of(&competition)
                .queued()
                .map(|queue| queue.min_players),
            Some(Some(2))
        );

        #[derive(Serialize)]
        struct Flattened {
            signed_contract: Heavy,
            #[serde(flatten)]
            pool: Parent,
        }
        #[derive(Serialize)]
        struct Parent {
            kind: &'static str,
            parent_id: &'static str,
        }
        let flattened = Flattened {
            signed_contract: Heavy,
            pool: Parent {
                kind: "pool",
                parent_id: PARENT,
            },
        };
        assert!(matches!(Queue::of(&flattened), Queue::Pool(_)));
        assert_eq!(Queue::of(&"not a struct"), Queue::Single);
    }
}
