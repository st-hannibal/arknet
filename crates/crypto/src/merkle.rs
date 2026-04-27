//! Binary Merkle tree over SHA-256.
//!
//! Used to aggregate inference receipts, transaction batches, and state
//! commitments. Duplication-attack resistant: leaves are domain-separated
//! from internal nodes via a 1-byte tag prefix (RFC 6962 style), so an
//! attacker cannot replace a leaf with a pair of internal nodes that hash
//! the same way.
//!
//! Design choices:
//! - **SHA-256 only** (matches [`docs/PROTOCOL_SPEC.md`] state-root format).
//! - **No external crate** — this logic is small and consensus-critical,
//!   we own the audit surface.
//! - **Odd-number layers duplicate the last node** (Bitcoin-style). Safe
//!   against second-preimage here because of domain separation.

use crate::errors::{CryptoError, Result};
use crate::hash::{Sha256Digest, Sha256Stream};

const LEAF_TAG: u8 = 0x00;
const NODE_TAG: u8 = 0x01;

/// A Merkle inclusion proof.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MerkleProof {
    /// Zero-based leaf index.
    pub leaf_index: usize,
    /// Total number of leaves in the original tree.
    pub leaf_count: usize,
    /// Sibling hashes, lowest layer first.
    pub siblings: Vec<Sha256Digest>,
}

/// A built Merkle tree. Holds the full leaf + node layers for proof generation.
#[derive(Clone, Debug)]
pub struct MerkleTree {
    layers: Vec<Vec<Sha256Digest>>,
}

impl MerkleTree {
    /// Build a tree over the given leaves (each leaf an arbitrary byte slice).
    ///
    /// Returns an error if `leaves` is empty — empty trees have no meaningful root.
    pub fn new<I, T>(leaves: I) -> Result<Self>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
    {
        let leaf_hashes: Vec<Sha256Digest> =
            leaves.into_iter().map(|l| hash_leaf(l.as_ref())).collect();
        if leaf_hashes.is_empty() {
            return Err(CryptoError::InvalidInput(
                "cannot build Merkle tree over 0 leaves".into(),
            ));
        }

        let mut layers = vec![leaf_hashes];
        while layers.last().unwrap().len() > 1 {
            let prev = layers.last().unwrap();
            let mut next = Vec::with_capacity(prev.len().div_ceil(2));
            for pair in prev.chunks(2) {
                let left = pair[0];
                let right = if pair.len() == 2 { pair[1] } else { pair[0] };
                next.push(hash_node(&left, &right));
            }
            layers.push(next);
        }

        Ok(Self { layers })
    }

    /// Root hash of the tree.
    pub fn root(&self) -> Sha256Digest {
        // INVARIANT: `new` guarantees at least one layer with at least one leaf,
        // and the loop pushes until the top layer has exactly 1 element.
        self.layers
            .last()
            .and_then(|l| l.first())
            .copied()
            .unwrap_or_default()
    }

    /// Number of leaves.
    pub fn leaf_count(&self) -> usize {
        self.layers.first().map(Vec::len).unwrap_or(0)
    }

    /// Generate an inclusion proof for the leaf at `index`.
    pub fn proof(&self, index: usize) -> Result<MerkleProof> {
        let leaf_count = self.leaf_count();
        if index >= leaf_count {
            return Err(CryptoError::InvalidInput(format!(
                "leaf index {index} out of range for {leaf_count} leaves"
            )));
        }

        let mut siblings = Vec::new();
        let mut idx = index;
        for layer in &self.layers[..self.layers.len() - 1] {
            let sibling_idx = if idx % 2 == 0 {
                // left child — sibling is to the right (or self if last and odd)
                if idx + 1 < layer.len() {
                    idx + 1
                } else {
                    idx
                }
            } else {
                idx - 1
            };
            siblings.push(layer[sibling_idx]);
            idx /= 2;
        }

        Ok(MerkleProof {
            leaf_index: index,
            leaf_count,
            siblings,
        })
    }
}

/// Verify a Merkle inclusion proof against an expected root.
pub fn verify_proof(root: &Sha256Digest, leaf: &[u8], proof: &MerkleProof) -> Result<()> {
    if proof.leaf_index >= proof.leaf_count {
        return Err(CryptoError::MerkleInvalid("leaf index out of range".into()));
    }

    // The proof length must match the tree height (ceil(log2(leaf_count)) for leaf_count > 1).
    let expected_height = if proof.leaf_count <= 1 {
        0
    } else {
        // We compute against the actual width at each layer because odd layers
        // use the "last-duplicate" rule.
        compute_expected_siblings(proof.leaf_count)
    };
    if proof.siblings.len() != expected_height {
        return Err(CryptoError::MerkleInvalid(format!(
            "expected {} sibling(s), got {}",
            expected_height,
            proof.siblings.len()
        )));
    }

    let mut current = hash_leaf(leaf);
    let mut idx = proof.leaf_index;
    let mut layer_width = proof.leaf_count;

    for sibling in &proof.siblings {
        let on_left = idx % 2 == 0;
        // Handle the last-node-duplicated case: if we're on the left and
        // there's no right sibling, the sibling we expect in the proof is
        // equal to current. That's only valid if layer_width is odd and
        // idx is the last element.
        let is_self_sibling = on_left && idx == layer_width - 1 && layer_width % 2 == 1;

        if is_self_sibling {
            if sibling != &current {
                return Err(CryptoError::MerkleInvalid(
                    "self-sibling mismatch at odd layer".into(),
                ));
            }
            current = hash_node(&current, &current);
        } else if on_left {
            current = hash_node(&current, sibling);
        } else {
            current = hash_node(sibling, &current);
        }

        idx /= 2;
        layer_width = layer_width.div_ceil(2);
    }

    if current == *root {
        Ok(())
    } else {
        Err(CryptoError::MerkleInvalid("root mismatch".into()))
    }
}

fn hash_leaf(bytes: &[u8]) -> Sha256Digest {
    let mut h = Sha256Stream::new();
    h.update(&[LEAF_TAG]).update(bytes);
    h.finalize()
}

fn hash_node(left: &Sha256Digest, right: &Sha256Digest) -> Sha256Digest {
    let mut h = Sha256Stream::new();
    h.update(&[NODE_TAG])
        .update(left.as_bytes())
        .update(right.as_bytes());
    h.finalize()
}

fn compute_expected_siblings(mut width: usize) -> usize {
    let mut n = 0;
    while width > 1 {
        width = width.div_ceil(2);
        n += 1;
    }
    n
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_of_one_leaf_has_matching_root() {
        let tree = MerkleTree::new([b"alpha".as_slice()]).unwrap();
        assert_eq!(tree.root(), hash_leaf(b"alpha"));
        assert_eq!(tree.leaf_count(), 1);
    }

    #[test]
    fn tree_rejects_zero_leaves() {
        let res = MerkleTree::new::<[&[u8]; 0], _>([]);
        assert!(res.is_err());
    }

    #[test]
    fn proof_roundtrip_power_of_two() {
        let leaves: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d"];
        let tree = MerkleTree::new(leaves.clone()).unwrap();
        let root = tree.root();

        for (i, leaf) in leaves.iter().enumerate() {
            let proof = tree.proof(i).unwrap();
            verify_proof(&root, leaf, &proof).unwrap();
        }
    }

    #[test]
    fn proof_roundtrip_odd_count() {
        let leaves: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d", b"e"];
        let tree = MerkleTree::new(leaves.clone()).unwrap();
        let root = tree.root();

        for (i, leaf) in leaves.iter().enumerate() {
            let proof = tree.proof(i).unwrap();
            verify_proof(&root, leaf, &proof).unwrap();
        }
    }

    #[test]
    fn proof_roundtrip_large() {
        let leaves: Vec<Vec<u8>> = (0..257u16).map(|i| i.to_be_bytes().to_vec()).collect();
        let tree = MerkleTree::new(&leaves).unwrap();
        let root = tree.root();

        for (i, leaf) in leaves.iter().enumerate() {
            let proof = tree.proof(i).unwrap();
            verify_proof(&root, leaf, &proof).unwrap();
        }
    }

    #[test]
    fn tampered_leaf_fails_verification() {
        let leaves: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d"];
        let tree = MerkleTree::new(leaves).unwrap();
        let root = tree.root();
        let proof = tree.proof(2).unwrap();
        let res = verify_proof(&root, b"TAMPERED", &proof);
        assert!(matches!(res, Err(CryptoError::MerkleInvalid(_))));
    }

    #[test]
    fn tampered_sibling_fails_verification() {
        let leaves: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d"];
        let tree = MerkleTree::new(leaves).unwrap();
        let root = tree.root();
        let mut proof = tree.proof(0).unwrap();
        if let Some(s) = proof.siblings.first_mut() {
            s.0[0] ^= 1;
        }
        assert!(verify_proof(&root, b"a", &proof).is_err());
    }

    #[test]
    fn wrong_index_fails_verification() {
        let leaves: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d"];
        let tree = MerkleTree::new(leaves).unwrap();
        let root = tree.root();
        let mut proof = tree.proof(0).unwrap();
        proof.leaf_index = 1;
        // Proof was for position 0, not 1 — verification must fail.
        assert!(verify_proof(&root, b"a", &proof).is_err());
    }

    #[test]
    fn out_of_range_index_is_rejected() {
        let leaves: Vec<&[u8]> = vec![b"a", b"b"];
        let tree = MerkleTree::new(leaves).unwrap();
        assert!(tree.proof(2).is_err());
    }

    #[test]
    fn leaf_domain_separation_prevents_forgery() {
        // An internal node `hash_node(a_hash, b_hash)` must not collide with
        // any leaf hash. The domain-separation tags guarantee this.
        let a_leaf = hash_leaf(b"a");
        let b_leaf = hash_leaf(b"b");
        let combined = hash_node(&a_leaf, &b_leaf);
        let as_leaf = hash_leaf(combined.as_bytes());
        assert_ne!(combined, as_leaf, "domain separation must hold");
    }

    proptest::proptest! {
        #[test]
        fn proof_roundtrip_property(leaves in proptest::collection::vec(
            proptest::collection::vec(proptest::num::u8::ANY, 0..64),
            1..32,
        )) {
            let tree = MerkleTree::new(&leaves).unwrap();
            let root = tree.root();
            for (i, leaf) in leaves.iter().enumerate() {
                let proof = tree.proof(i).unwrap();
                proptest::prop_assert!(verify_proof(&root, leaf, &proof).is_ok());
            }
        }
    }
}
