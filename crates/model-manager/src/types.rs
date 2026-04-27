//! Core types for the model manager.
//!
//! [`ModelRef`] is the human-facing name plus quantization tag a caller
//! uses to request a model. [`ModelManifest`] is what the registry returns:
//! everything needed to pull and verify the bytes. [`ModelId`] is the
//! stable on-chain identifier that will replace name-based lookup in
//! Phase 1.

use std::fmt;

use arknet_crypto::hash::Sha256Digest;
use serde::{Deserialize, Serialize};
use url::Url;

/// Caller-facing model reference: org + name + quantization tag.
///
/// Example: `meta-llama/Llama-3-7B-Instruct-Q4_K_M`.
///
/// The quant suffix is parsed off so the same base model can be requested
/// at multiple quantizations without string-matching ambiguity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRef {
    /// Organization / publisher (e.g. `meta-llama`).
    pub org: String,
    /// Base model name (e.g. `Llama-3-7B-Instruct`).
    pub name: String,
    /// Requested quantization (e.g. `Q4_K_M`, `F16`).
    pub quant: GgufQuant,
}

impl ModelRef {
    /// Parse from canonical form `<org>/<name>-<QUANT>`.
    ///
    /// The last dash-separated segment is interpreted as the quant tag.
    /// Fails if the reference is not in `org/name-quant` shape or the
    /// quant tag is unrecognized.
    pub fn parse(s: &str) -> Result<Self, String> {
        let (org, rest) = s
            .split_once('/')
            .ok_or_else(|| format!("model ref missing '/': {s}"))?;
        let (name, quant_str) = rest
            .rsplit_once('-')
            .ok_or_else(|| format!("model ref missing '-<QUANT>' suffix: {s}"))?;
        let quant = GgufQuant::parse(quant_str)
            .ok_or_else(|| format!("unknown quantization tag: {quant_str}"))?;
        Ok(Self {
            org: org.to_string(),
            name: name.to_string(),
            quant,
        })
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}-{}", self.org, self.name, self.quant.as_str())
    }
}

/// Opaque stable identifier for a model, as it will appear on-chain in
/// Phase 1. A 32-byte hash of the canonical manifest so two nodes that
/// see the same bytes arrive at the same ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelId(pub [u8; 32]);

impl ModelId {
    /// Hex-encoded representation with a `model:` prefix.
    pub fn to_hex_string(self) -> String {
        format!("model:{}", hex::encode(self.0))
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex_string())
    }
}

/// GGUF quantization tags we recognize for Phase 0.
///
/// This is the subset llama.cpp ships with. More can be added when real
/// models land in Phase 2 — adding a variant does not break the wire
/// format because quant is parsed from a string tag, not a numeric enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum GgufQuant {
    /// Full-precision float32.
    F32,
    /// Half-precision float16.
    F16,
    /// 4-bit k-quant, medium size.
    Q4KM,
    /// 4-bit k-quant, small size.
    Q4KS,
    /// 5-bit k-quant, medium size.
    Q5KM,
    /// 8-bit standard quant.
    Q8_0,
}

impl GgufQuant {
    /// Canonical string tag (matches HuggingFace / llama.cpp convention).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::F32 => "F32",
            Self::F16 => "F16",
            Self::Q4KM => "Q4_K_M",
            Self::Q4KS => "Q4_K_S",
            Self::Q5KM => "Q5_K_M",
            Self::Q8_0 => "Q8_0",
        }
    }

    /// Parse from canonical tag. Case-insensitive on the alphabetic part.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "F32" => Some(Self::F32),
            "F16" => Some(Self::F16),
            "Q4_K_M" => Some(Self::Q4KM),
            "Q4_K_S" => Some(Self::Q4KS),
            "Q5_K_M" => Some(Self::Q5KM),
            "Q8_0" => Some(Self::Q8_0),
            _ => None,
        }
    }

    /// The numeric type id as it appears in the GGUF header's `general.file_type`
    /// field. Values come from llama.cpp's `enum llama_ftype`.
    pub fn gguf_file_type(&self) -> u32 {
        match self {
            Self::F32 => 0,
            Self::F16 => 1,
            Self::Q4KS => 14,
            Self::Q4KM => 15,
            Self::Q5KM => 17,
            Self::Q8_0 => 7,
        }
    }
}

/// Everything needed to fetch and verify a model's bytes.
///
/// Returned by the registry. In Phase 0 it comes from a local JSON file;
/// in Phase 1 it is reconstructed from on-chain state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelManifest {
    /// Stable on-chain ID (Phase 1).
    pub id: ModelId,
    /// The reference this manifest was resolved for.
    pub model_ref: ModelRef,
    /// Download mirrors, tried in order until one succeeds.
    pub mirrors: Vec<Url>,
    /// Expected SHA-256 of the on-disk file after download.
    pub sha256: Sha256Digest,
    /// Expected file size in bytes. Used for early mismatch detection.
    pub size_bytes: u64,
    /// Declared quantization; must match what the GGUF header says.
    pub quant: GgufQuant,
    /// SPDX license identifier (informational).
    pub license: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modelref_parse_roundtrip() {
        let s = "meta-llama/Llama-3-7B-Instruct-Q4_K_M";
        let r = ModelRef::parse(s).expect("parse");
        assert_eq!(r.org, "meta-llama");
        assert_eq!(r.name, "Llama-3-7B-Instruct");
        assert_eq!(r.quant, GgufQuant::Q4KM);
        assert_eq!(r.to_string(), s);
    }

    #[test]
    fn modelref_rejects_missing_slash() {
        assert!(ModelRef::parse("llama-7b-Q4_K_M").is_err());
    }

    #[test]
    fn modelref_rejects_missing_quant_suffix() {
        assert!(ModelRef::parse("meta/llama").is_err());
    }

    #[test]
    fn modelref_rejects_unknown_quant() {
        assert!(ModelRef::parse("meta/llama-ZZZZ").is_err());
    }

    #[test]
    fn gguf_quant_roundtrip() {
        for q in [
            GgufQuant::F32,
            GgufQuant::F16,
            GgufQuant::Q4KM,
            GgufQuant::Q4KS,
            GgufQuant::Q5KM,
            GgufQuant::Q8_0,
        ] {
            assert_eq!(GgufQuant::parse(q.as_str()), Some(q));
        }
    }

    #[test]
    fn gguf_quant_parse_is_case_insensitive() {
        assert_eq!(GgufQuant::parse("q4_k_m"), Some(GgufQuant::Q4KM));
        assert_eq!(GgufQuant::parse("f16"), Some(GgufQuant::F16));
    }

    #[test]
    fn modelid_display_has_prefix() {
        let id = ModelId([0u8; 32]);
        assert!(id.to_string().starts_with("model:"));
    }
}
