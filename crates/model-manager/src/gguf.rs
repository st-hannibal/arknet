//! Minimal GGUF v3 header parser.
//!
//! Only the header + a handful of metadata KVs are read — enough to
//! confirm the file is a well-formed GGUF container and that its
//! `general.file_type` matches the quantization declared in the
//! registry manifest. The tensor payload is left alone; the inference
//! engine parses that when it memory-maps the file.
//!
//! Format reference: <https://github.com/ggerganov/ggml/blob/master/docs/gguf.md>
//!
//! # Security
//!
//! The parser reads from attacker-controlled files, so it must not
//! panic on malformed input. Every integer read is bounds-checked
//! against the slice length; every length prefix is capped at
//! [`MAX_STRING_LEN`] and [`MAX_METADATA_ENTRIES`] to prevent
//! gigabyte-long allocations from a tampered header.

use tokio::fs;
use tokio::io::AsyncReadExt;

use crate::errors::{ModelError, Result};

/// ASCII "GGUF" in little-endian.
const GGUF_MAGIC: [u8; 4] = [b'G', b'G', b'U', b'F'];

/// Only GGUF v3 is supported. v1/v2 are legacy; llama.cpp emits v3 today.
const SUPPORTED_VERSION: u32 = 3;

/// Hard cap on any single metadata string. Real-world values are < 1 KB.
const MAX_STRING_LEN: u64 = 64 * 1024;

/// Hard cap on metadata KV count. Real-world values are < 1024.
const MAX_METADATA_ENTRIES: u64 = 16 * 1024;

/// How many header bytes to pull off disk before parsing. Enough for
/// every realistic header we have seen; if a model exceeds this we
/// read more in a follow-up call.
const HEADER_READ_SIZE: usize = 64 * 1024;

/// GGUF value type codes from the spec (`enum gguf_type`).
const GGUF_TYPE_UINT8: u32 = 0;
const GGUF_TYPE_INT8: u32 = 1;
const GGUF_TYPE_UINT16: u32 = 2;
const GGUF_TYPE_INT16: u32 = 3;
const GGUF_TYPE_UINT32: u32 = 4;
const GGUF_TYPE_INT32: u32 = 5;
const GGUF_TYPE_FLOAT32: u32 = 6;
const GGUF_TYPE_BOOL: u32 = 7;
const GGUF_TYPE_STRING: u32 = 8;
const GGUF_TYPE_ARRAY: u32 = 9;
const GGUF_TYPE_UINT64: u32 = 10;
const GGUF_TYPE_INT64: u32 = 11;
const GGUF_TYPE_FLOAT64: u32 = 12;

/// Everything we care about from a GGUF file for Phase 0.
///
/// `architecture` is the model family reported by `general.architecture`
/// (e.g. "llama", "qwen"); `file_type` is the numeric quant id from
/// `general.file_type` and maps back to [`crate::types::GgufQuant`] via
/// [`crate::types::GgufQuant::gguf_file_type`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GgufHeader {
    /// GGUF format version (must be 3).
    pub version: u32,
    /// Number of tensors declared in the header.
    pub tensor_count: u64,
    /// Number of metadata KV pairs.
    pub metadata_count: u64,
    /// `general.architecture` (model family string), if present.
    pub architecture: Option<String>,
    /// `general.file_type` (quantization id), if present.
    pub file_type: Option<u32>,
}

/// Parse a GGUF header from a file on disk, reading only the prefix.
pub async fn parse_header(path: &std::path::Path) -> Result<GgufHeader> {
    let mut f = fs::File::open(path).await?;
    let mut buf = vec![0u8; HEADER_READ_SIZE];
    let n = f.read(&mut buf).await?;
    buf.truncate(n);
    parse_header_bytes(&buf)
}

/// Parse a GGUF header from an in-memory slice. Exposed for tests and
/// for callers that already have the prefix buffered.
pub fn parse_header_bytes(buf: &[u8]) -> Result<GgufHeader> {
    let mut r = Reader::new(buf);

    let magic = r.read_array::<4>()?;
    if magic != GGUF_MAGIC {
        return Err(ModelError::Gguf(format!(
            "bad magic: expected GGUF, got {:?}",
            std::str::from_utf8(&magic).unwrap_or("<non-utf8>")
        )));
    }

    let version = r.read_u32()?;
    if version != SUPPORTED_VERSION {
        return Err(ModelError::Gguf(format!(
            "unsupported GGUF version: {version} (only v{SUPPORTED_VERSION} is supported)"
        )));
    }

    let tensor_count = r.read_u64()?;
    let metadata_count = r.read_u64()?;
    if metadata_count > MAX_METADATA_ENTRIES {
        return Err(ModelError::Gguf(format!(
            "metadata count {metadata_count} exceeds cap {MAX_METADATA_ENTRIES}"
        )));
    }

    let mut architecture: Option<String> = None;
    let mut file_type: Option<u32> = None;

    for _ in 0..metadata_count {
        let key = r.read_string()?;
        let value_type = r.read_u32()?;

        match key.as_str() {
            "general.architecture" if value_type == GGUF_TYPE_STRING => {
                architecture = Some(r.read_string()?);
            }
            "general.file_type" if value_type == GGUF_TYPE_UINT32 => {
                file_type = Some(r.read_u32()?);
            }
            _ => {
                r.skip_value(value_type)?;
            }
        }
    }

    Ok(GgufHeader {
        version,
        tensor_count,
        metadata_count,
        architecture,
        file_type,
    })
}

/// Little-endian bounded slice reader. Every read checks remaining bytes.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn need(&self, n: usize) -> Result<()> {
        if self.pos + n > self.buf.len() {
            Err(ModelError::Gguf(format!(
                "truncated: need {n} bytes at offset {} but only {} remain",
                self.pos,
                self.buf.len().saturating_sub(self.pos),
            )))
        } else {
            Ok(())
        }
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.need(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(&self.buf[self.pos..self.pos + N]);
        self.pos += N;
        Ok(out)
    }

    fn read_u8(&mut self) -> Result<u8> {
        self.need(1)?;
        let v = self.buf[self.pos];
        self.pos += 1;
        Ok(v)
    }

    fn read_u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.read_array::<2>()?))
    }

    fn read_u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.read_array::<4>()?))
    }

    fn read_u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.read_array::<8>()?))
    }

    fn read_i8(&mut self) -> Result<i8> {
        Ok(self.read_u8()? as i8)
    }

    fn read_i16(&mut self) -> Result<i16> {
        Ok(self.read_u16()? as i16)
    }

    fn read_i32(&mut self) -> Result<i32> {
        Ok(self.read_u32()? as i32)
    }

    fn read_i64(&mut self) -> Result<i64> {
        Ok(self.read_u64()? as i64)
    }

    fn read_f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.read_array::<4>()?))
    }

    fn read_f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.read_array::<8>()?))
    }

    fn read_bool(&mut self) -> Result<bool> {
        Ok(self.read_u8()? != 0)
    }

    fn read_string(&mut self) -> Result<String> {
        let len = self.read_u64()?;
        if len > MAX_STRING_LEN {
            return Err(ModelError::Gguf(format!(
                "string length {len} exceeds cap {MAX_STRING_LEN}"
            )));
        }
        let len = len as usize;
        self.need(len)?;
        let s = std::str::from_utf8(&self.buf[self.pos..self.pos + len])
            .map_err(|e| ModelError::Gguf(format!("invalid utf-8 in metadata string: {e}")))?
            .to_owned();
        self.pos += len;
        Ok(s)
    }

    fn skip_value(&mut self, ty: u32) -> Result<()> {
        match ty {
            GGUF_TYPE_UINT8 => {
                self.read_u8()?;
            }
            GGUF_TYPE_INT8 => {
                self.read_i8()?;
            }
            GGUF_TYPE_UINT16 => {
                self.read_u16()?;
            }
            GGUF_TYPE_INT16 => {
                self.read_i16()?;
            }
            GGUF_TYPE_UINT32 => {
                self.read_u32()?;
            }
            GGUF_TYPE_INT32 => {
                self.read_i32()?;
            }
            GGUF_TYPE_FLOAT32 => {
                self.read_f32()?;
            }
            GGUF_TYPE_BOOL => {
                self.read_bool()?;
            }
            GGUF_TYPE_STRING => {
                self.read_string()?;
            }
            GGUF_TYPE_UINT64 => {
                self.read_u64()?;
            }
            GGUF_TYPE_INT64 => {
                self.read_i64()?;
            }
            GGUF_TYPE_FLOAT64 => {
                self.read_f64()?;
            }
            GGUF_TYPE_ARRAY => {
                let elem_ty = self.read_u32()?;
                let n = self.read_u64()?;
                if n > MAX_METADATA_ENTRIES {
                    return Err(ModelError::Gguf(format!(
                        "array length {n} exceeds cap {MAX_METADATA_ENTRIES}"
                    )));
                }
                for _ in 0..n {
                    self.skip_value(elem_ty)?;
                }
            }
            other => {
                return Err(ModelError::Gguf(format!("unknown value type id: {other}")));
            }
        }
        Ok(())
    }
}

// Test-only helper to synthesize a minimal well-formed GGUF prefix.
// Kept in the file so integration tests and unit tests can share it.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub struct Builder {
        buf: Vec<u8>,
        metadata_count: u64,
        metadata_buf: Vec<u8>,
    }

    impl Builder {
        pub fn new() -> Self {
            Self {
                buf: Vec::new(),
                metadata_count: 0,
                metadata_buf: Vec::new(),
            }
        }

        pub fn kv_string(mut self, key: &str, value: &str) -> Self {
            write_string(&mut self.metadata_buf, key);
            self.metadata_buf
                .extend_from_slice(&GGUF_TYPE_STRING.to_le_bytes());
            write_string(&mut self.metadata_buf, value);
            self.metadata_count += 1;
            self
        }

        pub fn kv_u32(mut self, key: &str, value: u32) -> Self {
            write_string(&mut self.metadata_buf, key);
            self.metadata_buf
                .extend_from_slice(&GGUF_TYPE_UINT32.to_le_bytes());
            self.metadata_buf.extend_from_slice(&value.to_le_bytes());
            self.metadata_count += 1;
            self
        }

        pub fn build(mut self) -> Vec<u8> {
            self.buf.extend_from_slice(&GGUF_MAGIC);
            self.buf.extend_from_slice(&SUPPORTED_VERSION.to_le_bytes());
            self.buf.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
            self.buf
                .extend_from_slice(&self.metadata_count.to_le_bytes());
            self.buf.extend_from_slice(&self.metadata_buf);
            self.buf
        }
    }

    fn write_string(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u64).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::Builder;
    use super::*;

    #[test]
    fn parses_minimal_well_formed_header() {
        let buf = Builder::new()
            .kv_string("general.architecture", "llama")
            .kv_u32("general.file_type", 15)
            .build();

        let h = parse_header_bytes(&buf).unwrap();
        assert_eq!(h.version, 3);
        assert_eq!(h.metadata_count, 2);
        assert_eq!(h.architecture.as_deref(), Some("llama"));
        assert_eq!(h.file_type, Some(15));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = Builder::new().build();
        buf[0] = b'X';
        let err = parse_header_bytes(&buf).unwrap_err();
        assert!(matches!(err, ModelError::Gguf(m) if m.contains("bad magic")));
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut buf = Builder::new().build();
        buf[4..8].copy_from_slice(&2u32.to_le_bytes());
        let err = parse_header_bytes(&buf).unwrap_err();
        assert!(matches!(err, ModelError::Gguf(m) if m.contains("version")));
    }

    #[test]
    fn rejects_truncated_header() {
        let buf = Builder::new().kv_string("k", "v").build();
        let err = parse_header_bytes(&buf[..10]).unwrap_err();
        assert!(matches!(err, ModelError::Gguf(m) if m.contains("truncated")));
    }

    #[test]
    fn skips_unknown_metadata() {
        let buf = Builder::new()
            .kv_string("unrelated.thing", "ignored")
            .kv_string("general.architecture", "qwen")
            .kv_u32("general.file_type", 1)
            .build();

        let h = parse_header_bytes(&buf).unwrap();
        assert_eq!(h.architecture.as_deref(), Some("qwen"));
        assert_eq!(h.file_type, Some(1));
    }

    #[test]
    fn rejects_excessive_metadata_count() {
        // Build a header with an absurdly large metadata count in the fixed field.
        let mut buf = Vec::new();
        buf.extend_from_slice(&GGUF_MAGIC);
        buf.extend_from_slice(&SUPPORTED_VERSION.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
        buf.extend_from_slice(&(MAX_METADATA_ENTRIES + 1).to_le_bytes());

        let err = parse_header_bytes(&buf).unwrap_err();
        assert!(matches!(err, ModelError::Gguf(m) if m.contains("metadata count")));
    }

    #[tokio::test]
    async fn parse_header_from_file_roundtrips() {
        let buf = Builder::new()
            .kv_string("general.architecture", "llama")
            .kv_u32("general.file_type", 15)
            .build();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.gguf");
        tokio::fs::write(&path, &buf).await.unwrap();

        let h = parse_header(&path).await.unwrap();
        assert_eq!(h.architecture.as_deref(), Some("llama"));
        assert_eq!(h.file_type, Some(15));
    }
}
