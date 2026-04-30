//! Integration tests for Week 3-4 state-application pipeline.
//!
//! Covers cross-module behavior: genesis → state → apply_tx → commit →
//! reopen. Unit tests in each module exercise individual paths; these
//! tests ensure the pieces compose correctly.

use std::path::PathBuf;

use arknet_chain::{
    apply_tx, genesis::genesis_to_validator_info, load_genesis, Account, RejectReason, State,
    Transaction, TxOutcome,
};
use arknet_common::types::{Address, Amount, PubKey, Signature, SignatureScheme};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/devnet-genesis.toml")
}

fn sign(tx: Transaction) -> arknet_chain::SignedTransaction {
    arknet_chain::SignedTransaction {
        tx,
        signer: PubKey::ed25519([1; 32]),
        signature: Signature::new(SignatureScheme::Ed25519, vec![2; 64]).unwrap(),
    }
}

#[test]
fn devnet_genesis_fixture_loads_and_passes_fair_launch_check() {
    let cfg = load_genesis(&fixture_path()).expect("fixture loads");
    assert_eq!(cfg.chain_id, "arknet-devnet-1");
    assert_eq!(cfg.validators.len(), 4);
    assert!(cfg.initial_accounts.is_empty());
    for gv in &cfg.validators {
        let info = genesis_to_validator_info(gv).unwrap();
        assert!(info.is_genesis);
        assert_eq!(info.bonded_stake, 0);
    }
}

#[test]
fn state_applies_1000_transfers_deterministically() {
    let tmp1 = tempfile::tempdir().unwrap();
    let tmp2 = tempfile::tempdir().unwrap();
    let s1 = State::open(tmp1.path()).unwrap();
    let s2 = State::open(tmp2.path()).unwrap();

    for state in [&s1, &s2] {
        // Fund Alice.
        let alice = Address::new([1; 20]);
        let mut ctx = state.begin_block();
        ctx.set_account(
            &alice,
            &Account {
                balance: 1_000_000_000,
                nonce: 0,
            },
        )
        .unwrap();
        ctx.commit().unwrap();

        // Push 100 transfers to 10 distinct recipients.
        let mut ctx = state.begin_block();
        for n in 0..100u64 {
            let to = Address::new([((n % 10) as u8).saturating_add(2); 20]);
            let stx = sign(Transaction::Transfer {
                from: alice,
                to,
                amount: 1,
                nonce: n,
                fee: 21_000,
            });
            let outcome = apply_tx(&mut ctx, &stx).unwrap();
            assert!(
                matches!(outcome, TxOutcome::Applied { .. }),
                "transfer {n} should apply: {outcome:?}"
            );
        }
        ctx.commit().unwrap();
    }

    assert_eq!(s1.state_root(), s2.state_root());
}

#[test]
fn state_survives_reopen_after_transfers() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().to_owned();
    let alice = Address::new([1; 20]);
    let bob = Address::new([2; 20]);

    let root_before;
    let alice_before;
    let bob_before;
    {
        let state = State::open(&path).unwrap();
        let mut ctx = state.begin_block();
        ctx.set_account(
            &alice,
            &Account {
                balance: 1_000_000,
                nonce: 0,
            },
        )
        .unwrap();
        ctx.commit().unwrap();

        let mut ctx = state.begin_block();
        for n in 0..5 {
            let outcome = apply_tx(
                &mut ctx,
                &sign(Transaction::Transfer {
                    from: alice,
                    to: bob,
                    amount: 100,
                    nonce: n,
                    fee: 21_000,
                }),
            )
            .unwrap();
            assert!(matches!(outcome, TxOutcome::Applied { .. }));
        }
        ctx.commit().unwrap();

        root_before = state.state_root();
        alice_before = state.get_account(&alice).unwrap().unwrap();
        bob_before = state.get_account(&bob).unwrap().unwrap();
    }

    let state = State::open(&path).unwrap();
    assert_eq!(state.state_root(), root_before);
    assert_eq!(state.get_account(&alice).unwrap().unwrap(), alice_before);
    assert_eq!(state.get_account(&bob).unwrap().unwrap(), bob_before);
}

#[test]
fn lenient_rejection_keeps_block_state_consistent() {
    let tmp = tempfile::tempdir().unwrap();
    let state = State::open(tmp.path()).unwrap();

    let alice = Address::new([1; 20]);
    let bob = Address::new([2; 20]);
    {
        let mut ctx = state.begin_block();
        ctx.set_account(
            &alice,
            &Account {
                balance: 1_000_000,
                nonce: 0,
            },
        )
        .unwrap();
        ctx.commit().unwrap();
    }

    // One bad tx between two good ones — the block commits with exactly
    // the two good ones applied.
    let mut ctx = state.begin_block();
    let good1 = sign(Transaction::Transfer {
        from: alice,
        to: bob,
        amount: 10,
        nonce: 0,
        fee: 21_000,
    });
    let bogus = sign(Transaction::Transfer {
        from: alice,
        to: bob,
        amount: 10,
        nonce: 999, // replayed/skipped
        fee: 21_000,
    });
    let good2 = sign(Transaction::Transfer {
        from: alice,
        to: bob,
        amount: 20,
        nonce: 1,
        fee: 21_000,
    });

    assert!(matches!(
        apply_tx(&mut ctx, &good1).unwrap(),
        TxOutcome::Applied { .. }
    ));
    assert!(matches!(
        apply_tx(&mut ctx, &bogus).unwrap(),
        TxOutcome::Rejected(RejectReason::NonceMismatch { .. })
    ));
    assert!(matches!(
        apply_tx(&mut ctx, &good2).unwrap(),
        TxOutcome::Applied { .. }
    ));
    ctx.commit().unwrap();

    let a = state.get_account(&alice).unwrap().unwrap();
    let b = state.get_account(&bob).unwrap().unwrap();
    // Alice moved from 1_000_000 - 10 - 20 - 21_000*2 = 999_970 - 42_000
    let expected_alice: Amount = 1_000_000 - 10 - 20 - 2 * 21_000;
    assert_eq!(a.balance, expected_alice);
    assert_eq!(a.nonce, 2);
    assert_eq!(b.balance, 30);
}
