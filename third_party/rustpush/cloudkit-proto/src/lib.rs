#![allow(warnings)]

include!(concat!(env!("OUT_DIR"), "/cloudkitp.rs"));

use std::{io::Cursor, time::{Duration, SystemTime}};

use base64::engine::general_purpose;
use prost::Message;
use record::field::{value::Type as FieldType, Value};
use serde::{de::DeserializeOwned, Serialize};
use disjoint_impls::disjoint_impls;

use crate::sealed::KindSealed;

impl request_operation::header::Database {
    pub fn ck_type(&self) -> &'static str {
        match self {
            Self::PrivateDb => "Private",
            Self::PublicDb => "Public",
            Self::SharedDb => "Shared",
        }
    }
}

pub trait CloudKitEncryptor {
    fn encrypt_data(&self, data: &[u8], field_name: &str) -> Vec<u8>;
    fn decrypt_data(&self, dec: &[u8], field_name: &str) -> Vec<u8>;
}

struct FakeCloudKitEncryptor;
impl CloudKitEncryptor for FakeCloudKitEncryptor {
    fn decrypt_data(&self, _: &[u8], _: &str) -> Vec<u8> {
        panic!()
    }
    fn encrypt_data(&self, _: &[u8], _: &str) -> Vec<u8> {
        panic!()
    }
}

pub trait CloudKitRecord {
    fn to_record(&self) -> Vec<record::Field> {
        self.to_record_encrypted(None::<&FakeCloudKitEncryptor>)
    }
    fn from_record(value: &[record::Field]) -> Self
    where
        Self: Sized {
        Self::from_record_encrypted(value, None::<&FakeCloudKitEncryptor>)
    }

    fn to_record_encrypted(&self, encryptor: Option<&impl CloudKitEncryptor>) -> Vec<record::Field>;
    fn from_record_encrypted(value: &[record::Field], encryptor: Option<&impl CloudKitEncryptor>) -> Self;

    fn record_type() -> &'static str;
}

impl<T: CloudKitRecord> CloudKitRecord for &T {
    fn record_type() -> &'static str {
        T::record_type()
    }

    fn to_record_encrypted(&self, e: Option<&impl CloudKitEncryptor>) -> Vec<record::Field> {
        T::to_record_encrypted(&self, e)
    }

    fn from_record_encrypted(value: &[record::Field], e: Option<&impl CloudKitEncryptor>) -> Self
        where
            Self: Sized {
        panic!("Cannot from with a ref")
    }
}

pub trait CloudKitEncryptedValue {
    fn to_value_encrypted(&self, encryptor: &impl CloudKitEncryptor, field_name: &str) -> Option<record::field::Value>;
    fn from_value_encrypted(value: &record::field::Value, encryptor: &impl CloudKitEncryptor, field_name: &str) -> Option<Self>
    where
        Self: Sized;
}

pub trait CloudKitValue {
    fn to_value(&self) -> Option<record::field::Value>;
    fn from_value(value: &record::field::Value) -> Option<Self>
    where
        Self: Sized;
}

impl<T: CloudKitEncryptedValue> CloudKitEncryptedValue for Option<T> {
    fn to_value_encrypted(&self, encryptor: &impl CloudKitEncryptor, field_name: &str) -> Option<record::field::Value> {
        self.as_ref().and_then(|a| a.to_value_encrypted(encryptor, field_name))
    }
    fn from_value_encrypted(value: &record::field::Value, encryptor: &impl CloudKitEncryptor, field_name: &str) -> Option<Self>
        where
            Self: Sized {
        Some(T::from_value_encrypted(value, encryptor, field_name))
    }
}

pub mod sealed {
    pub trait KindSealed {}
    pub enum ProtoKind {}
    impl KindSealed for ProtoKind {}
    pub enum PlistKind {}
    impl KindSealed for PlistKind {}
}

pub trait CloudKitBytesKind {
    type Kind: sealed::KindSealed;
}

disjoint_impls! {
    pub trait CloudKitBytes {
        fn to_bytes(&self) -> Vec<u8>;
        fn from_bytes(v: Vec<u8>) -> Self;
    }

    impl CloudKitBytes for Vec<u8> {
        fn to_bytes(&self) -> Vec<u8> {
            self.clone()
        }
        fn from_bytes(v: Vec<u8>) -> Self {
            v
        }
    }
    impl<T: Message + Default + CloudKitBytesKind<Kind = sealed::ProtoKind>> CloudKitBytes for T {
        fn from_bytes(v: Vec<u8>) -> Self {
            let result = Self::decode(&v[..]);
            if let Err(e) = &result {
                // Never the bytes themselves: these fields hold handles and
                // message text. prost's error names the field and the wire
                // type, not the value.
                crate::DECODE_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                log::debug!(
                    "{} did not decode ({e}); read as empty. Wire shape: {}",
                    std::any::type_name::<Self>(),
                    crate::proto_wire_shape(&v)
                );
            }
            result.unwrap_or_default()
        }
        fn to_bytes(&self) -> Vec<u8> {
            self.encode_to_vec()
        }
    }

    impl<T: Serialize + DeserializeOwned + CloudKitBytesKind<Kind = sealed::PlistKind>> CloudKitBytes for T {
        fn from_bytes(v: Vec<u8>) -> Self {
            // The panic message carries neither the bytes nor plist's error,
            // which can quote the offending value.
            plist::from_bytes(&v).unwrap_or_else(|_| {
                panic!(
                    "{} plist did not deserialize ({} bytes)",
                    std::any::type_name::<Self>(),
                    v.len()
                )
            })
        }

        fn to_bytes(&self) -> Vec<u8> {
            let mut buf = vec![];
            let cursor = Cursor::new(&mut buf);
            plist::to_writer_binary(cursor, self).expect("Serialization failed!");
            buf
        }
    }
}

impl<T: CloudKitBytes> CloudKitEncryptedValue for T {
    fn to_value_encrypted(&self, encryptor: &impl CloudKitEncryptor, field_name: &str) -> Option<record::field::Value> {
        Some(record::field::Value {
            r#type: Some(FieldType::EncryptedBytesType as i32),
            bytes_value: Some(encryptor.encrypt_data(&self.to_bytes(), field_name)),
            is_encrypted: Some(true),
            ..Default::default()
        })
    }

    fn from_value_encrypted(value: &record::field::Value, encryptor: &impl CloudKitEncryptor, field_name: &str) -> Option<Self> {
        let data = encryptor.decrypt_data(value.bytes_value.as_ref()?, field_name);
        if data.is_empty() { return None }
        Some(Self::from_bytes(data))
    }
}

impl<T: CloudKitBytes> CloudKitEncryptedValue for Vec<T> {
    fn to_value_encrypted(&self, encryptor: &impl CloudKitEncryptor, field_name: &str) -> Option<record::field::Value> {
        Some(record::field::Value {
            r#type: Some(FieldType::EncryptedBytesListType as i32),
            list_values: self.iter().filter_map(|a| a.to_bytes().to_value_encrypted(encryptor, field_name)).collect(),
            is_encrypted: Some(false),
            ..Default::default()
        })
    }

    fn from_value_encrypted(value: &record::field::Value, encryptor: &impl CloudKitEncryptor, field_name: &str) -> Option<Self> {
        Some(value.list_values.iter().filter_map(|v| CloudKitEncryptedValue::from_value_encrypted(v, encryptor, field_name)).collect())
    }
}


impl CloudKitEncryptedValue for i64 {
    fn to_value_encrypted(&self, encryptor: &impl CloudKitEncryptor, field_name: &str) -> Option<record::field::Value> {
        Some(record::field::Value {
            r#type: Some(FieldType::Int64Type as i32),
            bytes_value: Some(encryptor.encrypt_data(&record::field::EncryptedValue {
                signed_value: Some(*self),
                ..Default::default()
            }.encode_to_vec(), field_name)),
            is_encrypted: Some(true),
            ..Default::default()
        })
    }

    fn from_value_encrypted(value: &record::field::Value, encryptor: &impl CloudKitEncryptor, field_name: &str) -> Option<Self> {
        Some(record::field::EncryptedValue::decode(&encryptor.decrypt_data(value.bytes_value.as_ref().unwrap(), field_name)[..]).unwrap().signed_value())
    }
}

impl CloudKitEncryptedValue for String {
    fn to_value_encrypted(&self, encryptor: &impl CloudKitEncryptor, field_name: &str) -> Option<record::field::Value> {
        Some(record::field::Value {
            r#type: Some(FieldType::StringType as i32),
            bytes_value: Some(encryptor.encrypt_data(&record::field::EncryptedValue {
                string_value: Some(self.clone()),
                ..Default::default()
            }.encode_to_vec(), field_name)),
            is_encrypted: Some(true),
            ..Default::default()
        })
    }

    fn from_value_encrypted(value: &record::field::Value, encryptor: &impl CloudKitEncryptor, field_name: &str) -> Option<Self> {
        Some(record::field::EncryptedValue::decode(&encryptor.decrypt_data(value.bytes_value.as_ref().unwrap(), field_name)[..]).unwrap().string_value().to_string())
    }
}

impl CloudKitEncryptedValue for SystemTime {
    fn to_value_encrypted(&self, encryptor: &impl CloudKitEncryptor, field_name: &str) -> Option<record::field::Value> {
        // start at apple epoch
        let duration = self.duration_since(SystemTime::UNIX_EPOCH).expect("Before Unix Epoch?").as_secs_f64() - 978307200f64;

        Some(record::field::Value {
            r#type: Some(FieldType::DateType as i32),
            bytes_value: Some(encryptor.encrypt_data(&record::field::EncryptedValue {
                date_value: Some(Date { time: Some(duration) }),
                ..Default::default()
            }.encode_to_vec(), field_name)),
            is_encrypted: Some(true),
            ..Default::default()
        })
    }

    fn from_value_encrypted(value: &record::field::Value, encryptor: &impl CloudKitEncryptor, field_name: &str) -> Option<Self> {
        let d = record::field::EncryptedValue::decode(&encryptor.decrypt_data(value.bytes_value.as_ref().unwrap(), field_name)[..]).unwrap().date_value.unwrap();
        let secs = d.time.expect("Date misses time??");
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs_f64(secs + 978307200f64))
    }
}

/// CloudKit fields that failed to decode against their schema (and were read
/// as the type's default) since the last [`take_decode_failures`].
static DECODE_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Take and reset the count of fields that failed to decode, so a caller
/// can report them in one line per sync instead of one per record.
pub fn take_decode_failures() -> u64 {
    DECODE_FAILURES.swap(0, std::sync::atomic::Ordering::Relaxed)
}

/// The top-level protobuf layout of `bytes`: each field's number and wire
/// type, and the length of length-delimited ones. Never a value: this is
/// what a log line may say about a CloudKit field, which holds handles and
/// message text. Example: `27 bytes [1:varint 2:varint 3:len(12)]`.
pub fn proto_wire_shape(bytes: &[u8]) -> String {
    fn varint(b: &[u8], pos: &mut usize) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *b.get(*pos)?;
            *pos += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }
    const MAX_FIELDS: usize = 24;
    let mut fields: Vec<String> = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        if fields.len() == MAX_FIELDS {
            fields.push("...".into());
            break;
        }
        let Some(key) = varint(bytes, &mut pos) else {
            fields.push("malformed".into());
            break;
        };
        let number = key >> 3;
        let shape = match key & 7 {
            0 => varint(bytes, &mut pos).map(|_| "varint".to_string()),
            1 => {
                pos += 8;
                Some("i64".into())
            }
            2 => varint(bytes, &mut pos).map(|len| {
                pos = pos.saturating_add(len as usize);
                format!("len({len})")
            }),
            5 => {
                pos += 4;
                Some("i32".into())
            }
            wire => {
                fields.push(format!("{number}:wire{wire}"));
                break;
            }
        };
        match shape {
            Some(shape) if pos <= bytes.len() => fields.push(format!("{number}:{shape}")),
            Some(shape) => {
                fields.push(format!("{number}:{shape} truncated"));
                break;
            }
            None => {
                fields.push(format!("{number}:malformed"));
                break;
            }
        }
    }
    format!("{} bytes [{}]", bytes.len(), fields.join(" "))
}

#[cfg(test)]
mod wire_shape_tests {
    use super::proto_wire_shape;

    /// Pin: the shape names fields, wire types and lengths, never values.
    /// This is the layout of the record reported from a real sync: an
    /// unsigned 1, an unsigned 0 and a 12-byte string (a phone number).
    #[test]
    fn shape_has_no_values() {
        let mut bytes = vec![0x08, 0x01, 0x10, 0x00, 0x1a, 12];
        bytes.extend_from_slice(b"+15555550123");
        let shape = proto_wire_shape(&bytes);
        assert_eq!(shape, "18 bytes [1:varint 2:varint 3:len(12)]");
        assert!(!shape.contains("555"));
    }

    #[test]
    fn shape_marks_truncated_and_malformed_input() {
        assert_eq!(proto_wire_shape(&[0x1a, 0x05, b'a']), "3 bytes [3:len(5) truncated]");
        assert_eq!(proto_wire_shape(&[0x08, 0xff]), "2 bytes [1:malformed]");
        assert_eq!(proto_wire_shape(&[]), "0 bytes []");
    }

    #[test]
    fn shape_reads_fixed_width_fields() {
        let bytes = [0x09, 0, 0, 0, 0, 0, 0, 0, 0, 0x15, 0, 0, 0, 0];
        assert_eq!(proto_wire_shape(&bytes), "14 bytes [1:i64 2:i32]");
    }
}

pub fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        write!(&mut s, "{:02x}", b).unwrap();
    }
    s
}

pub fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    general_purpose::STANDARD.encode(data)
}

impl CloudKitEncryptedValue for Asset {
    fn to_value_encrypted(&self, encryptor: &impl CloudKitEncryptor, field_name: &str) -> Option<record::field::Value> {
        let field_name = format!("{}-{}-{}", field_name, base64_encode(self.signature()), base64_encode(self.reference_signature()));

        let mut copy = self.clone();
        let protection_info = copy.protection_info.as_mut().unwrap().protection_info.as_mut().unwrap();
        *protection_info = encryptor.encrypt_data(&protection_info, &field_name);
        Some(record::field::Value {
            r#type: Some(FieldType::AssetType as i32),
            asset_value: Some(copy),
            ..Default::default()
        })
    }

    fn from_value_encrypted(value: &record::field::Value, encryptor: &impl CloudKitEncryptor, field_name: &str) -> Option<Self> {
        let mut copy = value.asset_value.clone();
        if let Some(copy) = &mut copy {
            let context = format!("{}-{}-{}", field_name, base64_encode(copy.signature()), base64_encode(copy.reference_signature()));

            let protection_info = copy.protection_info.as_mut().unwrap().protection_info.as_mut().unwrap();
            *protection_info = encryptor.decrypt_data(&protection_info, &context);
            // println!("decrypted data {}", encode_hex(protection_info));
        }
        copy
    }
}

impl<T: CloudKitValue> CloudKitValue for Option<T> {
    fn to_value(&self) -> Option<record::field::Value> {
        self.as_ref().and_then(|a| a.to_value())
    }
    fn from_value(value: &record::field::Value) -> Option<Self>
        where
            Self: Sized {
        Some(T::from_value(value))
    }
}

impl<T: CloudKitBytes> CloudKitValue for T {
    fn to_value(&self) -> Option<record::field::Value> {
        Some(record::field::Value {
            r#type: Some(FieldType::BytesType as i32),
            bytes_value: Some(self.to_bytes()),
            ..Default::default()
        })
    }

    fn from_value(value: &record::field::Value) -> Option<Self> {
        value.bytes_value.clone().map(|t| Self::from_bytes(t))
    }
}

impl CloudKitValue for SystemTime {
    fn to_value(&self) -> Option<record::field::Value> {
        let apple_epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(978307200);
        let duration = self.duration_since(apple_epoch).expect("Before Apple Epoch?").as_secs_f64();

        Some(record::field::Value {
            r#type: Some(FieldType::DateType as i32),
            date_value: Some(Date { time: Some(duration) }),
            ..Default::default()
        })
    }

    fn from_value(value: &record::field::Value) -> Option<Self> {
        value.date_value.clone().map(|d| {
            let secs = d.time.expect("Date misses time??");
            let apple_epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(978307200);
            apple_epoch + Duration::from_secs_f64(secs)
        })
    }
}

impl CloudKitValue for String {
    fn to_value(&self) -> Option<record::field::Value> {
        Some(record::field::Value {
            r#type: Some(FieldType::StringType as i32),
            string_value: Some(self.clone()),
            ..Default::default()
        })
    }

    fn from_value(value: &record::field::Value) -> Option<Self> {
        value.string_value.clone()
    }
}

impl CloudKitValue for location::Coordinate {
    fn to_value(&self) -> Option<record::field::Value> {
        Some(record::field::Value {
            r#type: Some(FieldType::LocationType as i32),
            location_value: Some(self.clone()),
            ..Default::default()
        })
    }

    fn from_value(value: &record::field::Value) -> Option<Self> {
        value.location_value.clone()
    }
}

impl CloudKitValue for record::Reference {
    fn to_value(&self) -> Option<record::field::Value> {
        Some(record::field::Value {
            r#type: Some(FieldType::ReferenceType as i32),
            reference_value: Some(self.clone()),
            ..Default::default()
        })
    }

    fn from_value(value: &record::field::Value) -> Option<Self> {
        value.reference_value.clone()
    }
}

impl CloudKitValue for Asset {
    fn to_value(&self) -> Option<record::field::Value> {
        Some(record::field::Value {
            r#type: Some(FieldType::AssetType as i32),
            asset_value: Some(self.clone()),
            ..Default::default()
        })
    }

    fn from_value(value: &record::field::Value) -> Option<Self> {
        value.asset_value.clone()
    }
}

impl CloudKitValue for i64 {
    fn to_value(&self) -> Option<record::field::Value> {
        Some(record::field::Value {
            r#type: Some(FieldType::Int64Type as i32),
            signed_value: Some(*self),
            ..Default::default()
        })
    }

    fn from_value(value: &record::field::Value) -> Option<Self> {
        value.signed_value
    }
}

impl CloudKitValue for f64 {
    fn to_value(&self) -> Option<record::field::Value> {
        Some(record::field::Value {
            r#type: Some(FieldType::DoubleType as i32),
            double_value: Some(*self),
            ..Default::default()
        })
    }

    fn from_value(value: &record::field::Value) -> Option<Self> {
        value.double_value
    }
}

impl<T: CloudKitValue + Clone> CloudKitValue for Vec<T> {
    fn to_value(&self) -> Option<record::field::Value> {
        if self.is_empty() {
            return None;
        }
        let first_type = T::to_value(&self[0])?.r#type.unwrap_or_default();
        Some(record::field::Value {
            r#type: Some(match first_type {
                t if t == FieldType::DateType as i32 => FieldType::DateListType as i32,
                t if t == FieldType::BytesType as i32 => FieldType::BytesListType as i32,
                t if t == FieldType::LocationType as i32 => FieldType::LocationListType as i32,
                t if t == FieldType::ReferenceType as i32 => FieldType::ReferenceListType as i32,
                t if t == FieldType::AssetType as i32 => FieldType::AssetListType as i32,
                t if t == FieldType::StringType as i32 => FieldType::StringListType as i32,
                t if t == FieldType::Int64Type as i32 => FieldType::Int64ListType as i32,
                t if t == FieldType::DoubleType as i32 => FieldType::DoubleListType as i32,
                _ => FieldType::ListType as i32,
            }),
            list_values: self.iter().cloned().filter_map(|a| a.to_value()).collect(),
            ..Default::default()
        })
    }

    fn from_value(value: &record::field::Value) -> Option<Self> {
        value.list_values.iter().map(T::from_value).collect()
    }
}

impl CloudKitValue for Package {
    fn to_value(&self) -> Option<record::field::Value> {
        Some(record::field::Value {
            r#type: Some(FieldType::PackageType as i32),
            package_value: Some(self.clone()),
            ..Default::default()
        })
    }

    fn from_value(value: &record::field::Value) -> Option<Self> {
        value.package_value.clone()
    }
}