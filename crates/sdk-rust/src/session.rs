//! Session keys — ephemeral Ed25519 keypairs authorized by a main wallet.
//!
//! A session key limits exposure: if it leaks, only the `spending_limit`
//! is at risk and only until `expiry_ms`. The main wallet stays safe.

use std::time::Duration;

use arknet_common::types::{PubKey, Signature, Timestamp};
use arknet_compute::wire::{DelegationCert, DelegationSigningBody, DELEGATION_DOMAIN};
use arknet_crypto::keys::SigningKey;

use crate::errors::{Result, SdkError};
use crate::wallet::Wallet;

/// An ephemeral signing key authorized by a main wallet via a
/// [`DelegationCert`]. Use this instead of the main wallet for
/// inference requests.
pub struct SessionKey {
    signing_key: SigningKey,
    cert: DelegationCert,
    spent: u128,
}

impl SessionKey {
    /// Create a session key authorized by `wallet`.
    ///
    /// Generates a fresh Ed25519 keypair, builds a [`DelegationCert`],
    /// and signs it with the main wallet.
    pub fn create(wallet: &Wallet, spending_limit: u128, expiry: Duration) -> Result<Self> {
        let signing_key = SigningKey::generate();
        let session_pubkey = signing_key.verifying_key().to_pubkey();

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| SdkError::Session(format!("clock error: {e}")))?
            .as_millis() as Timestamp;
        let expiry_ms = now_ms + expiry.as_millis() as Timestamp;

        let body = DelegationSigningBody {
            domain: DELEGATION_DOMAIN,
            session_pubkey: &session_pubkey,
            spending_limit,
            expiry_ms,
            main_wallet_address: wallet.address(),
        };
        let signing_bytes =
            borsh::to_vec(&body).map_err(|e| SdkError::Session(format!("encode: {e}")))?;
        let main_wallet_signature = wallet.sign(&signing_bytes);

        let cert = DelegationCert {
            session_pubkey,
            spending_limit,
            expiry_ms,
            main_wallet_address: *wallet.address(),
            main_wallet_pubkey: wallet.public_key(),
            main_wallet_signature,
        };

        Ok(Self {
            signing_key,
            cert,
            spent: 0,
        })
    }

    /// Sign a message with the session key.
    pub fn sign(&self, message: &[u8]) -> Signature {
        arknet_crypto::signatures::sign(&self.signing_key, message)
    }

    /// The session key's public key.
    pub fn public_key(&self) -> PubKey {
        self.signing_key.verifying_key().to_pubkey()
    }

    /// The delegation certificate.
    pub fn delegation(&self) -> &DelegationCert {
        &self.cert
    }

    /// The main wallet address (for billing lookups).
    pub fn main_wallet_address(&self) -> &arknet_common::types::Address {
        &self.cert.main_wallet_address
    }

    /// Whether this session key has expired.
    pub fn is_expired(&self) -> bool {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as Timestamp)
            .unwrap_or(0);
        now_ms > self.cert.expiry_ms
    }

    /// Record a spend against the session's limit.
    pub fn record_spend(&mut self, amount: u128) -> Result<()> {
        let new_total = self.spent + amount;
        if new_total > self.cert.spending_limit {
            return Err(SdkError::Session(format!(
                "spending limit exceeded: {} + {} > {}",
                self.spent, amount, self.cert.spending_limit
            )));
        }
        self.spent = new_total;
        Ok(())
    }

    /// How much of the spending limit remains.
    pub fn remaining_limit(&self) -> u128 {
        self.cert.spending_limit.saturating_sub(self.spent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arknet_compute::wire::verify_delegation;

    #[test]
    fn create_and_verify_delegation() {
        let wallet = Wallet::from_seed(&[0x42; 32]);
        let session = SessionKey::create(&wallet, 1_000_000, Duration::from_secs(3600)).unwrap();

        let cert = session.delegation();
        assert_eq!(cert.main_wallet_address, *wallet.address());
        assert_eq!(cert.main_wallet_pubkey, wallet.public_key());
        assert_eq!(cert.session_pubkey, session.public_key());
        assert_eq!(cert.spending_limit, 1_000_000);

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as Timestamp;
        verify_delegation(cert, now_ms).expect("delegation should verify");
    }

    #[test]
    fn session_sign_produces_valid_signature() {
        let wallet = Wallet::from_seed(&[0x01; 32]);
        let session = SessionKey::create(&wallet, 100, Duration::from_secs(60)).unwrap();
        let msg = b"test message";
        let sig = session.sign(msg);

        arknet_crypto::signatures::verify(&session.public_key(), msg, &sig)
            .expect("session signature should verify");
    }

    #[test]
    fn expired_delegation_rejected() {
        let wallet = Wallet::from_seed(&[0x02; 32]);
        let session = SessionKey::create(&wallet, 100, Duration::from_secs(0)).unwrap();
        let cert = session.delegation();

        std::thread::sleep(std::time::Duration::from_millis(10));
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as Timestamp;
        let result = verify_delegation(cert, now_ms);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expired"));
    }

    #[test]
    fn spending_limit_enforced() {
        let wallet = Wallet::from_seed(&[0x03; 32]);
        let mut session = SessionKey::create(&wallet, 100, Duration::from_secs(3600)).unwrap();

        session.record_spend(60).unwrap();
        assert_eq!(session.remaining_limit(), 40);

        session.record_spend(40).unwrap();
        assert_eq!(session.remaining_limit(), 0);

        let err = session.record_spend(1).unwrap_err();
        assert!(err.to_string().contains("spending limit exceeded"));
    }

    #[test]
    fn is_expired_returns_false_for_fresh_session() {
        let wallet = Wallet::from_seed(&[0x04; 32]);
        let session = SessionKey::create(&wallet, 100, Duration::from_secs(3600)).unwrap();
        assert!(!session.is_expired());
    }

    #[test]
    fn different_wallets_produce_different_delegations() {
        let w1 = Wallet::from_seed(&[0x10; 32]);
        let w2 = Wallet::from_seed(&[0x20; 32]);
        let s1 = SessionKey::create(&w1, 100, Duration::from_secs(60)).unwrap();
        let s2 = SessionKey::create(&w2, 100, Duration::from_secs(60)).unwrap();
        assert_ne!(
            s1.delegation().main_wallet_address,
            s2.delegation().main_wallet_address
        );
    }

    #[test]
    fn forged_delegation_rejected() {
        let wallet = Wallet::from_seed(&[0x05; 32]);
        let session = SessionKey::create(&wallet, 100, Duration::from_secs(3600)).unwrap();
        let mut forged = session.delegation().clone();
        forged.spending_limit = 999_999_999;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as Timestamp;
        let result = verify_delegation(&forged, now_ms);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("signature invalid"));
    }
}
