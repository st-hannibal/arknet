//! VRF-gated verifier selection.
//!
//! §11: every anchored receipt is re-executed by some subset of
//! verifiers sampled by VRF. The sampling invariants:
//!
//! - **Unpredictable** to the compute node — a would-be cheater
//!   cannot route malicious jobs to known-friendly verifiers.
//! - **Publicly verifiable** — a dispute transaction can prove the
//!   verifier was selected by including the VRF output.
//!
//! # Phase 1 construction
//!
//! Each verifier computes `(proof, output) = VRF_sk(job_id || block_hash)`.
//! The low 64 bits of `output` are compared to a threshold derived
//! from the sampling rate `p` and the total output space:
//!
//! ```text
//!   threshold = floor(2^64 * p)
//!   selected  = u64::from_be_bytes(output[..8]) < threshold
//! ```
//!
//! This is a Bernoulli(p) gate per (verifier, job) pair. The VRF
//! provides a publicly-verifiable proof the gate returned `true`.
//!
//! Phase 3 may tighten this to committee sampling (top-k verifiers by
//! VRF output) so the expected committee size is deterministic;
//! Bernoulli sampling keeps the math simple for Phase 1.

use arknet_common::types::{BlockHash, Hash256, JobId};
use arknet_crypto::keys::{SigningKey, VerifyingKey};
use arknet_crypto::vrf::{prove, verify_proof, VrfProof};

use crate::errors::{Result, VerifierError};

/// Default verifier sampling rate (5% per §11).
///
/// Expressed as a fraction; converted to a `u64` threshold via
/// [`sampling_threshold`].
pub const DEFAULT_SAMPLING_RATE: f64 = 0.05;

/// Domain tag mixed into the VRF input so a VRF proof produced for
/// `(job_id, block_hash)` cannot be replayed as a proof for anything
/// else (e.g. a governance beacon).
pub const VRF_DOMAIN: &[u8] = b"arknet-verifier-vrf-v1";

/// `(selected, proof, raw_output)` tuple returned by [`select_verifier`].
#[derive(Clone, Debug)]
pub struct Selection {
    /// Was this verifier selected for the job?
    pub selected: bool,
    /// VRF proof that the verifier can submit in a dispute.
    pub proof: VrfProof,
    /// Raw 32-byte VRF output. First 8 bytes are the sampling gate.
    pub output: Hash256,
}

/// Turn a sampling rate into a `u64` threshold.
///
/// Saturates outside `[0.0, 1.0]`: anything <= 0 → threshold 0
/// (never selected), anything >= 1 → threshold `u64::MAX` (always
/// selected).
pub fn sampling_threshold(rate: f64) -> u64 {
    if rate <= 0.0 {
        return 0;
    }
    if rate >= 1.0 {
        return u64::MAX;
    }
    // u64::MAX as f64 loses precision on the low bits; that's fine —
    // we're converting a probability to a threshold, not a hash.
    (rate * (u64::MAX as f64)) as u64
}

/// Build the VRF input bytes for a given `(job_id, block_hash)`.
/// Public so a light client can re-derive the same input to check a
/// dispute's `vrf_proof`.
pub fn vrf_input(job_id: &JobId, block_hash: &BlockHash) -> Vec<u8> {
    let mut buf = Vec::with_capacity(VRF_DOMAIN.len() + 64);
    buf.extend_from_slice(VRF_DOMAIN);
    buf.extend_from_slice(&job_id.0);
    buf.extend_from_slice(block_hash.as_bytes());
    buf
}

/// Run the selection gate. `sk` is the verifier's signing key;
/// `rate` is a number in `[0, 1]`.
pub fn select_verifier(
    sk: &SigningKey,
    job_id: &JobId,
    block_hash: &BlockHash,
    rate: f64,
) -> Selection {
    let input = vrf_input(job_id, block_hash);
    let (proof, raw_output) = prove(sk, &input);
    let output: Hash256 = *raw_output.as_bytes();
    let gate = u64::from_be_bytes(output[..8].try_into().expect("slice is 8 bytes"));
    let threshold = sampling_threshold(rate);
    Selection {
        selected: gate < threshold,
        proof,
        output,
    }
}

/// Verify someone else's selection claim. Returns the raw output on
/// success. Used by the chain when checking a [`arknet_chain::Dispute`]
/// to confirm the verifier was actually allowed to submit.
pub fn verify_selection(
    vk: &VerifyingKey,
    job_id: &JobId,
    block_hash: &BlockHash,
    proof: &VrfProof,
    rate: f64,
) -> Result<Hash256> {
    let input = vrf_input(job_id, block_hash);
    let output = verify_proof(vk, &input, proof)
        .map_err(|e| VerifierError::Signing(format!("vrf verify: {e}")))?;
    let raw: Hash256 = *output.as_bytes();
    let gate = u64::from_be_bytes(raw[..8].try_into().expect("slice is 8 bytes"));
    let threshold = sampling_threshold(rate);
    if gate >= threshold {
        return Err(VerifierError::NotSelected);
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_common::types::{BlockHash, JobId};

    fn job(i: u8) -> JobId {
        JobId::new([i; 32])
    }

    fn block() -> BlockHash {
        BlockHash::new([0x55; 32])
    }

    #[test]
    fn threshold_at_boundaries() {
        assert_eq!(sampling_threshold(0.0), 0);
        assert_eq!(sampling_threshold(1.0), u64::MAX);
        assert_eq!(sampling_threshold(-0.1), 0);
        assert_eq!(sampling_threshold(1.5), u64::MAX);
    }

    #[test]
    fn selection_is_deterministic_per_sk_job() {
        let sk = SigningKey::from_seed(&[0x11; 32]);
        let a = select_verifier(&sk, &job(1), &block(), 0.5);
        let b = select_verifier(&sk, &job(1), &block(), 0.5);
        assert_eq!(a.output, b.output);
        assert_eq!(a.selected, b.selected);
    }

    #[test]
    fn zero_rate_never_selects() {
        let sk = SigningKey::from_seed(&[0x22; 32]);
        let sel = select_verifier(&sk, &job(3), &block(), 0.0);
        assert!(!sel.selected);
    }

    #[test]
    fn one_rate_always_selects() {
        let sk = SigningKey::from_seed(&[0x33; 32]);
        let sel = select_verifier(&sk, &job(3), &block(), 1.0);
        assert!(sel.selected);
    }

    #[test]
    fn approximately_matches_rate_over_many_jobs() {
        // Sample 5000 jobs at rate = 5% and expect the hit count to
        // fall in a generous band (binomial stderr ≈ 0.3% → use 3σ).
        let sk = SigningKey::from_seed(&[0x44; 32]);
        let rate = 0.05;
        let trials = 5000usize;
        let mut hits = 0usize;
        for i in 0..trials {
            let job = JobId::new([(i & 0xff) as u8; 32]);
            let seed = [(i >> 8) as u8; 32];
            let blk = BlockHash::new(seed);
            let sel = select_verifier(&sk, &job, &blk, rate);
            if sel.selected {
                hits += 1;
            }
        }
        let observed = hits as f64 / trials as f64;
        let expected = rate;
        // 3σ band
        let stderr = (rate * (1.0 - rate) / trials as f64).sqrt();
        let lo = expected - 3.0 * stderr;
        let hi = expected + 3.0 * stderr;
        assert!(
            lo <= observed && observed <= hi,
            "observed rate {observed} outside 3σ band [{lo}, {hi}]"
        );
    }

    #[test]
    fn verify_selection_roundtrip_when_selected() {
        let sk = SigningKey::from_seed(&[0x55; 32]);
        let vk = sk.verifying_key();
        // Use rate=1.0 to guarantee selection and a roundtrip test.
        let sel = select_verifier(&sk, &job(9), &block(), 1.0);
        assert!(sel.selected);
        let out = verify_selection(&vk, &job(9), &block(), &sel.proof, 1.0).expect("verify");
        assert_eq!(out, sel.output);
    }

    #[test]
    fn verify_selection_fails_when_not_selected() {
        let sk = SigningKey::from_seed(&[0x66; 32]);
        let vk = sk.verifying_key();
        let sel = select_verifier(&sk, &job(9), &block(), 0.0);
        assert!(!sel.selected);
        let err = verify_selection(&vk, &job(9), &block(), &sel.proof, 0.0).unwrap_err();
        assert!(matches!(err, VerifierError::NotSelected));
    }

    #[test]
    fn verify_selection_fails_on_wrong_pubkey() {
        let sk = SigningKey::from_seed(&[0x77; 32]);
        let wrong = SigningKey::from_seed(&[0x88; 32]);
        let sel = select_verifier(&sk, &job(9), &block(), 1.0);
        let err = verify_selection(&wrong.verifying_key(), &job(9), &block(), &sel.proof, 1.0)
            .unwrap_err();
        assert!(matches!(err, VerifierError::Signing(_)));
    }
}
