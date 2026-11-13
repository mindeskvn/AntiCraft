//! Hand-written protobuf + proto3-JSON codec for `exa.host_bridge_pb` (SPEC §7.1).
//!
//! Only varint (wire type 0) and length-delimited (wire type 2) fields are
//! produced; unknown fields of any wire type are skipped on decode.

use serde::{Deserialize, Serialize};

#[allow(dead_code)]
pub const SERVICE: &str = "exa.host_bridge_pb.HostBridgeService";
pub const PATH_GET_UPDATE_STATUS: &str = "/exa.host_bridge_pb.HostBridgeService/GetUpdateStatus";
pub const PATH_APPLY_UPDATE: &str = "/exa.host_bridge_pb.HostBridgeService/ApplyUpdate";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("truncated varint")]
    TruncatedVarint,
    #[error("varint overflow")]
    VarintOverflow,
    #[error("truncated length-delimited field")]
    TruncatedBytes,
    #[error("unsupported wire type {0}")]
    WireType(u8),
    #[error("invalid utf-8 in string field")]
    Utf8,
}

/// Minimal protobuf message trait.
pub trait Message: Sized + Default {
    fn encode(&self, out: &mut Vec<u8>);
    fn decode(buf: &[u8]) -> Result<Self, DecodeError>;
    fn encode_to_vec(&self) -> Vec<u8> {
        let mut v = Vec::new();
        self.encode(&mut v);
        v
    }
}

// ---------------------------------------------------------------- primitives

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn get_varint(buf: &[u8], pos: &mut usize) -> Result<u64, DecodeError> {
    let mut result: u64 = 0;
    for shift in (0..64).step_by(7) {
        let b = *buf.get(*pos).ok_or(DecodeError::TruncatedVarint)?;
        *pos += 1;
        result |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
    }
    Err(DecodeError::VarintOverflow)
}

fn put_tag(out: &mut Vec<u8>, field: u32, wire: u8) {
    put_varint(out, (u64::from(field) << 3) | u64::from(wire));
}

pub fn put_bytes_field(out: &mut Vec<u8>, field: u32, data: &[u8]) {
    put_tag(out, field, 2);
    put_varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

/// proto3: default (empty) strings are not emitted.
pub fn put_string_field(out: &mut Vec<u8>, field: u32, s: &str) {
    if !s.is_empty() {
        put_bytes_field(out, field, s.as_bytes());
    }
}

/// proto3: `false` is not emitted.
pub fn put_bool_field(out: &mut Vec<u8>, field: u32, b: bool) {
    if b {
        put_tag(out, field, 0);
        put_varint(out, 1);
    }
}

/// A decoded field value.
pub enum FieldValue<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Other,
}

/// Iterate over `(field_number, value)` pairs, skipping fixed32/fixed64.
pub fn for_each_field<'a>(
    buf: &'a [u8],
    mut f: impl FnMut(u32, FieldValue<'a>) -> Result<(), DecodeError>,
) -> Result<(), DecodeError> {
    let mut pos = 0;
    while pos < buf.len() {
        let key = get_varint(buf, &mut pos)?;
        let field = (key >> 3) as u32;
        let wire = (key & 7) as u8;
        match wire {
            0 => f(field, FieldValue::Varint(get_varint(buf, &mut pos)?))?,
            2 => {
                let len = get_varint(buf, &mut pos)? as usize;
                let end = pos.checked_add(len).ok_or(DecodeError::TruncatedBytes)?;
                let data = buf.get(pos..end).ok_or(DecodeError::TruncatedBytes)?;
                pos = end;
                f(field, FieldValue::Bytes(data))?;
            }
            1 => {
                pos = pos.checked_add(8).filter(|p| *p <= buf.len()).ok_or(DecodeError::TruncatedBytes)?;
                f(field, FieldValue::Other)?;
            }
            5 => {
                pos = pos.checked_add(4).filter(|p| *p <= buf.len()).ok_or(DecodeError::TruncatedBytes)?;
                f(field, FieldValue::Other)?;
            }
            w => return Err(DecodeError::WireType(w)),
        }
    }
    Ok(())
}

fn as_string(b: &[u8]) -> Result<String, DecodeError> {
    String::from_utf8(b.to_vec()).map_err(|_| DecodeError::Utf8)
}

// ------------------------------------------------------------------ messages

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GetUpdateStatusRequest {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UpdateStatus {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GetUpdateStatusResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<UpdateStatus>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApplyUpdateRequest {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApplyUpdateResponse {
    pub accepted: bool,
}

impl Message for GetUpdateStatusRequest {
    fn encode(&self, _out: &mut Vec<u8>) {}
    fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        for_each_field(buf, |_, _| Ok(()))?;
        Ok(Self {})
    }
}

impl Message for ApplyUpdateRequest {
    fn encode(&self, _out: &mut Vec<u8>) {}
    fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        for_each_field(buf, |_, _| Ok(()))?;
        Ok(Self {})
    }
}

impl Message for UpdateStatus {
    fn encode(&self, out: &mut Vec<u8>) {
        put_string_field(out, 1, &self.current_version);
        put_string_field(out, 2, &self.latest_version);
        put_bool_field(out, 3, self.update_available);
    }
    fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        let mut m = Self::default();
        for_each_field(buf, |f, v| {
            match (f, v) {
                (1, FieldValue::Bytes(b)) => m.current_version = as_string(b)?,
                (2, FieldValue::Bytes(b)) => m.latest_version = as_string(b)?,
                (3, FieldValue::Varint(n)) => m.update_available = n != 0,
                _ => {}
            }
            Ok(())
        })?;
        Ok(m)
    }
}

impl Message for GetUpdateStatusResponse {
    fn encode(&self, out: &mut Vec<u8>) {
        if let Some(s) = &self.status {
            put_bytes_field(out, 1, &s.encode_to_vec());
        }
    }
    fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        let mut m = Self::default();
        for_each_field(buf, |f, v| {
            if let (1, FieldValue::Bytes(b)) = (f, v) {
                m.status = Some(UpdateStatus::decode(b)?);
            }
            Ok(())
        })?;
        Ok(m)
    }
}

impl Message for ApplyUpdateResponse {
    fn encode(&self, out: &mut Vec<u8>) {
        put_bool_field(out, 1, self.accepted);
    }
    fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        let mut m = Self::default();
        for_each_field(buf, |f, v| {
            if let (1, FieldValue::Varint(n)) = (f, v) {
                m.accepted = n != 0;
            }
            Ok(())
        })?;
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut b = Vec::new();
            put_varint(&mut b, v);
            let mut p = 0;
            assert_eq!(get_varint(&b, &mut p).unwrap(), v);
            assert_eq!(p, b.len());
        }
    }

    #[test]
    fn status_roundtrip() {
        let r = GetUpdateStatusResponse {
            status: Some(UpdateStatus {
                current_version: "2.26.0".into(),
                latest_version: "2.27.0".into(),
                update_available: true,
            }),
        };
        let bytes = r.encode_to_vec();
        assert_eq!(GetUpdateStatusResponse::decode(&bytes).unwrap(), r);
        let a = ApplyUpdateResponse { accepted: true };
        assert_eq!(a.encode_to_vec(), vec![0x08, 0x01]);
        assert_eq!(ApplyUpdateResponse::decode(&a.encode_to_vec()).unwrap(), a);
    }

    #[test]
    fn json_names() {
        let r = GetUpdateStatusResponse {
            status: Some(UpdateStatus {
                current_version: "1".into(),
                latest_version: "1".into(),
                update_available: false,
            }),
        };
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains("currentVersion") && j.contains("latestVersion") && j.contains("updateAvailable"));
    }

    #[test]
    fn truncated_is_error() {
        assert!(UpdateStatus::decode(&[0x0a, 0x05, b'a']).is_err());
    }
}
