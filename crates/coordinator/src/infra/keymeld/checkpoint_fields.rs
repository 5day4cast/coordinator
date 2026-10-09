//! Encode protocol fields and journal entries through serde's borrowed traversal.
//! This is deliberately an object visitor, not a new JSON serializer: each leaf
//! still uses serde_json and the existing authenticated part format.
use super::*;
use serde::ser::{self, Impossible, SerializeMap, SerializeStruct};

#[derive(Clone, Copy)]
enum Level {
    Base,
    Journal,
    Entries,
}

struct Node {
    manifest: Manifest,
    len: usize,
}

struct Encoder<'a> {
    key: &'a SessionSecret,
    session: &'a SessionId,
    known: &'a BTreeSet<[u8; 32]>,
    bodies: BTreeMap<[u8; 32], Vec<u8>>,
    serialized: usize,
    max_buffer_capacity: usize,
}
impl<'a> Encoder<'a> {
    fn new(key: &'a SessionSecret, session: &'a SessionId, known: &'a BTreeSet<[u8; 32]>) -> Self {
        Self {
            key,
            session,
            known,
            bodies: BTreeMap::new(),
            serialized: 0,
            max_buffer_capacity: 0,
        }
    }
    fn value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<Node, serde_json::Error> {
        let mut json = Zeroizing::new(Vec::new());
        serde_json::to_writer(Limited(&mut json), value)?;
        self.serialized += json.len();
        self.max_buffer_capacity = self.max_buffer_capacity.max(json.capacity());
        let raw: &RawValue = serde_json::from_slice(&json)?;
        let manifest = split(self.key, self.session, raw, &mut self.bodies, self.known)
            .map_err(ser::Error::custom)?;
        Ok(Node {
            manifest,
            len: json.len(),
        })
    }
}

struct Limited<'a>(&'a mut Vec<u8>);
impl Write for Limited<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_STATE_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other(
                "Confidential checkpoint exceeds size limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn encode_base(
    key: &SessionSecret,
    session: &SessionId,
    state: &ProtocolState,
    known: &BTreeSet<[u8; 32]>,
) -> Result<Parts, KeymeldError> {
    let mut encoder = Encoder::new(key, session, known);
    let node = state
        .serialize(Object::new(&mut encoder, Level::Base))
        .map_err(|error| failure(error.to_string()))?;
    Ok(Parts {
        manifest: node.manifest,
        bodies: encoder.bodies,
        plaintext_len: node.len,
        serialized_len: encoder.serialized,
        max_buffer_capacity: encoder.max_buffer_capacity,
    })
}

pub(super) fn encode_journal(
    key: &SessionSecret,
    session: &SessionId,
    base: &Parts,
    journal: &ConfidentialJournal,
    known: &BTreeSet<[u8; 32]>,
) -> Result<Parts, KeymeldError> {
    let mut encoder = Encoder::new(key, session, known);
    let journal = journal
        .serialize(Object::new(&mut encoder, Level::Journal))
        .map_err(|error| failure(error.to_string()))?;
    let Manifest::Object(mut fields) = base.manifest.clone() else {
        return Err(failure("Invalid checkpoint base manifest"));
    };
    // Base has no journal field. Add its key, colon, and the separating comma.
    let plaintext_len = base.plaintext_len + journal.len + 10 + usize::from(!fields.is_empty());
    if plaintext_len > MAX_STATE_BYTES {
        return Err(failure("Confidential checkpoint exceeds size limit"));
    }
    fields.insert("journal".into(), journal.manifest);
    let mut bodies = base.bodies.clone();
    bodies.extend(encoder.bodies);
    Ok(Parts {
        manifest: Manifest::Object(fields),
        bodies,
        plaintext_len,
        serialized_len: base.serialized_len + encoder.serialized,
        max_buffer_capacity: base.max_buffer_capacity.max(encoder.max_buffer_capacity),
    })
}

struct Object<'a, 'b> {
    encoder: &'a mut Encoder<'b>,
    level: Level,
    fields: BTreeMap<String, Manifest>,
    len: usize,
    pending_key: Option<String>,
}
impl<'a, 'b> Object<'a, 'b> {
    fn new(encoder: &'a mut Encoder<'b>, level: Level) -> Self {
        Self {
            encoder,
            level,
            fields: BTreeMap::new(),
            len: 2,
            pending_key: None,
        }
    }
    fn field<T: ?Sized + Serialize>(
        &mut self,
        key: &str,
        value: &T,
    ) -> Result<(), serde_json::Error> {
        if matches!(self.level, Level::Base) && key == "journal" {
            return Ok(());
        }
        let node = if matches!(self.level, Level::Journal)
            && matches!(key, "commands" | "signing_batches")
        {
            value.serialize(Object::new(self.encoder, Level::Entries))?
        } else {
            self.encoder.value(value)?
        };
        self.len +=
            serde_json::to_string(key)?.len() + 1 + node.len + usize::from(!self.fields.is_empty());
        if self.len > MAX_STATE_BYTES {
            return Err(ser::Error::custom(
                "Confidential checkpoint exceeds size limit",
            ));
        }
        if self.fields.insert(key.into(), node.manifest).is_some() {
            return Err(ser::Error::custom("Duplicate checkpoint field"));
        }
        Ok(())
    }
    fn finish(self) -> Result<Node, serde_json::Error> {
        if self.pending_key.is_some() {
            return Err(ser::Error::custom("Missing checkpoint field value"));
        }
        Ok(Node {
            manifest: Manifest::Object(self.fields),
            len: self.len,
        })
    }
}
impl SerializeStruct for Object<'_, '_> {
    type Ok = Node;
    type Error = serde_json::Error;
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        self.field(key, value)
    }
    fn end(self) -> Result<Node, Self::Error> {
        self.finish()
    }
}
impl SerializeMap for Object<'_, '_> {
    type Ok = Node;
    type Error = serde_json::Error;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), Self::Error> {
        if self.pending_key.is_some() {
            return Err(ser::Error::custom("Missing checkpoint field value"));
        }
        self.pending_key = Some(serde_json::from_str(&serde_json::to_string(key)?)?);
        Ok(())
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        let key = self
            .pending_key
            .take()
            .ok_or_else(|| ser::Error::custom("Missing checkpoint field key"))?;
        self.field(&key, value)
    }
    fn end(self) -> Result<Node, Self::Error> {
        self.finish()
    }
}

macro_rules! reject_scalar {
    ($($name:ident($ty:ty)),* $(,)?) => {$ (
        fn $name(self, _value: $ty) -> Result<Node, Self::Error> { Err(ser::Error::custom("Expected checkpoint object")) }
    )*};
}
impl<'a, 'b> ser::Serializer for Object<'a, 'b> {
    type Ok = Node;
    type Error = serde_json::Error;
    type SerializeSeq = Impossible<Node, Self::Error>;
    type SerializeTuple = Impossible<Node, Self::Error>;
    type SerializeTupleStruct = Impossible<Node, Self::Error>;
    type SerializeTupleVariant = Impossible<Node, Self::Error>;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Impossible<Node, Self::Error>;
    reject_scalar! { serialize_bool(bool), serialize_i8(i8), serialize_i16(i16), serialize_i32(i32), serialize_i64(i64), serialize_u8(u8), serialize_u16(u16), serialize_u32(u32), serialize_u64(u64), serialize_f32(f32), serialize_f64(f64), serialize_char(char), serialize_str(&str), serialize_bytes(&[u8]) }
    fn serialize_none(self) -> Result<Node, Self::Error> {
        self.serialize_unit()
    }
    fn serialize_unit(self) -> Result<Node, Self::Error> {
        Err(ser::Error::custom("Expected checkpoint object"))
    }
    fn serialize_some<T: ?Sized + Serialize>(self, _value: &T) -> Result<Node, Self::Error> {
        self.serialize_unit()
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<Node, Self::Error> {
        self.serialize_unit()
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
    ) -> Result<Node, Self::Error> {
        self.serialize_unit()
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _value: &T,
    ) -> Result<Node, Self::Error> {
        self.serialize_unit()
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<Node, Self::Error> {
        self.serialize_unit()
    }
    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Err(ser::Error::custom("Expected checkpoint object"))
    }
    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Err(ser::Error::custom("Expected checkpoint object"))
    }
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Err(ser::Error::custom("Expected checkpoint object"))
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Err(ser::Error::custom("Expected checkpoint object"))
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<Self, Self::Error> {
        Ok(self)
    }
    fn serialize_struct(self, _name: &'static str, _len: usize) -> Result<Self, Self::Error> {
        Ok(self)
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Err(ser::Error::custom("Expected checkpoint object"))
    }
}
