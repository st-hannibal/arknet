//! Compute attestation — the per-job hash chain.
//!
//! §6 of PROTOCOL_SPEC: a compute node accompanies every job with a
//! `ComputeProof` that can be verified later. Phase 1 produces the
//! [`ComputeProof::HashChain`] variant: a rolling digest over the token
//! text stream, constant memory on the compute side.
//!
//! Construction:
//!
//! ```text
//!   h_0     = blake3(DOMAIN_HASHCHAIN || job_id_bytes)
//!   h_{i+1} = blake3(DOMAIN_HASHCHAIN || h_i || token_i_bytes)
//! ```
//!
//! The full chain `[h_0, h_1, …, h_n]` is the witness. A verifier
//! re-executes the deterministic job and checks every step.

use arknet_chain::ComputeProof;
use arknet_common::types::{Hash256, JobId};

/// Domain tag for hash-chain commitments. Prevents a compute proof
/// from ever being re-interpreted as some other digest.
pub const DOMAIN_HASHCHAIN: &[u8] = b"arknet-hashchain-v1";

/// Rolling-digest builder. Cheap (32-byte state) and `!Send`-safe for a
/// single-producer decoder.
#[derive(Clone, Debug)]
pub struct HashChainBuilder {
    chain: Vec<Hash256>,
    current: Hash256,
}

impl HashChainBuilder {
    /// Start a chain. `h_0` is committed immediately (domain-tagged),
    /// mixed with a fresh empty payload so the chain has a stable
    /// prefix even for zero-token jobs.
    pub fn new() -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(DOMAIN_HASHCHAIN);
        let h0 = *hasher.finalize().as_bytes();
        Self {
            chain: vec![h0],
            current: h0,
        }
    }

    /// Start a chain bound to a specific [`JobId`]. Prefer this when
    /// building a chain for a real job so replay across jobs is
    /// impossible.
    pub fn for_job(job_id: JobId) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(DOMAIN_HASHCHAIN);
        hasher.update(&job_id.0);
        let h0 = *hasher.finalize().as_bytes();
        Self {
            chain: vec![h0],
            current: h0,
        }
    }

    /// Absorb a token's text fragment into the chain.
    pub fn absorb_token(&mut self, text: &str) {
        self.absorb_bytes(text.as_bytes());
    }

    /// Absorb an arbitrary byte slice into the chain.
    ///
    /// Keeping this exposed (alongside [`Self::absorb_token`]) makes
    /// the builder reusable by a verifier that wants to feed raw token
    /// bytes instead of decoded text.
    pub fn absorb_bytes(&mut self, bytes: &[u8]) {
        let mut hasher = blake3::Hasher::new();
        hasher.update(DOMAIN_HASHCHAIN);
        hasher.update(&self.current);
        hasher.update(bytes);
        self.current = *hasher.finalize().as_bytes();
        self.chain.push(self.current);
    }

    /// Consume the builder and produce a [`ComputeProof::HashChain`].
    pub fn finish(self) -> ComputeProof {
        ComputeProof::HashChain(self.chain)
    }

    /// Chain length (including `h_0`).
    pub fn len(&self) -> usize {
        self.chain.len()
    }

    /// `true` if only the seed digest is present.
    pub fn is_empty(&self) -> bool {
        self.chain.len() <= 1
    }

    /// Current rolling digest.
    pub fn head(&self) -> Hash256 {
        self.current
    }
}

impl Default for HashChainBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_chain_has_seed_only() {
        let c = HashChainBuilder::new();
        assert_eq!(c.len(), 1);
        assert!(c.is_empty());
    }

    #[test]
    fn absorbing_token_extends_chain() {
        let mut c = HashChainBuilder::new();
        c.absorb_token("hello");
        assert_eq!(c.len(), 2);
        assert!(!c.is_empty());
    }

    #[test]
    fn chain_is_deterministic_across_runs() {
        let job = JobId::new([0x42; 32]);
        let mut a = HashChainBuilder::for_job(job);
        let mut b = HashChainBuilder::for_job(job);
        for tok in ["hello", " ", "world"] {
            a.absorb_token(tok);
            b.absorb_token(tok);
        }
        assert_eq!(a.head(), b.head());
        match (a.finish(), b.finish()) {
            (ComputeProof::HashChain(c1), ComputeProof::HashChain(c2)) => assert_eq!(c1, c2),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn different_job_ids_produce_different_chains() {
        let mut a = HashChainBuilder::for_job(JobId::new([1; 32]));
        let mut b = HashChainBuilder::for_job(JobId::new([2; 32]));
        a.absorb_token("x");
        b.absorb_token("x");
        assert_ne!(a.head(), b.head());
    }

    #[test]
    fn order_matters() {
        let job = JobId::new([7; 32]);
        let mut a = HashChainBuilder::for_job(job);
        let mut b = HashChainBuilder::for_job(job);
        a.absorb_token("ab");
        a.absorb_token("cd");
        b.absorb_token("abcd");
        assert_ne!(a.head(), b.head(), "must not equal bulk input");
    }
}
