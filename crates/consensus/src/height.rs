//! `malachitebft_core_types::Height` adapter over our existing
//! [`arknet_common::types::Height`] alias.
//!
//! We keep a newtype rather than impl'ing the foreign trait on the raw
//! `u64` alias directly — otherwise adding new methods to our own
//! `Height` alias in the future would collide with the trait's
//! requirements.

use borsh::{BorshDeserialize, BorshSerialize};
use malachitebft_core_types::Height as MalachiteHeight;
use serde::{Deserialize, Serialize};

use arknet_common::types::Height as ChainHeight;

/// Block height, one-to-one with [`arknet_common::types::Height`].
///
/// Exists only so we can implement malachite's `Height` trait without
/// orphan-rule issues. Convert freely via `From` / `Into`.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
)]
pub struct Height(pub u64);

impl Height {
    /// Construct from the chain-wide height alias.
    pub const fn from_chain(h: ChainHeight) -> Self {
        Self(h)
    }

    /// Unwrap back to the chain-wide height alias.
    pub const fn to_chain(self) -> ChainHeight {
        self.0
    }
}

impl std::fmt::Display for Height {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<ChainHeight> for Height {
    fn from(h: ChainHeight) -> Self {
        Self(h)
    }
}

impl From<Height> for ChainHeight {
    fn from(h: Height) -> Self {
        h.0
    }
}

impl MalachiteHeight for Height {
    const ZERO: Self = Self(0);
    const INITIAL: Self = Self(1);

    fn increment_by(&self, n: u64) -> Self {
        Self(self.0.saturating_add(n))
    }

    fn decrement_by(&self, n: u64) -> Option<Self> {
        self.0.checked_sub(n).map(Self)
    }

    fn as_u64(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_and_initial_match_spec() {
        assert_eq!(Height::ZERO, Height(0));
        assert_eq!(Height::INITIAL, Height(1));
    }

    #[test]
    fn increment_decrement_roundtrip() {
        let h = Height::INITIAL;
        let after = h.increment().increment().increment();
        assert_eq!(after, Height(4));
        assert_eq!(after.decrement().unwrap(), Height(3));
    }

    #[test]
    fn decrement_below_zero_returns_none() {
        assert_eq!(Height::ZERO.decrement(), None);
    }

    #[test]
    fn chain_roundtrip() {
        let c: ChainHeight = 42;
        let h: Height = c.into();
        assert_eq!(h, Height(42));
        assert_eq!(ChainHeight::from(h), 42);
    }

    #[test]
    fn display_is_bare_number() {
        assert_eq!(Height(7).to_string(), "7");
    }

    #[test]
    fn borsh_roundtrip() {
        let h = Height(9_000_000);
        let bytes = borsh::to_vec(&h).unwrap();
        let decoded: Height = borsh::from_slice(&bytes).unwrap();
        assert_eq!(h, decoded);
    }
}
