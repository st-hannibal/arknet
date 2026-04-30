//! End-to-end economic flow: escrow lock → settle → reward split.
//!
//! Exercises the chain-level apply handlers through a real RocksDB
//! state, proving that user funds move correctly through the
//! escrow → distribution pipeline.

use arknet_chain::account::Account;
use arknet_chain::apply::{apply_tx, TxOutcome};
use arknet_chain::state::State;
use arknet_chain::transactions::{SignedTransaction, Transaction};
use arknet_common::types::{Address, JobId, PubKey, Signature, SignatureScheme};

fn tmp_state() -> (tempfile::TempDir, State) {
    let tmp = tempfile::tempdir().unwrap();
    let state = State::open(tmp.path()).unwrap();
    (tmp, state)
}

fn sign(tx: Transaction) -> SignedTransaction {
    SignedTransaction {
        tx,
        signer: PubKey::ed25519([1; 32]),
        signature: Signature::new(SignatureScheme::Ed25519, vec![2; 64]).unwrap(),
    }
}

fn fund(state: &State, addr: Address, balance: u128) {
    let mut ctx = state.begin_block();
    ctx.set_account(&addr, &Account { balance, nonce: 0 })
        .unwrap();
    ctx.commit().unwrap();
}

#[test]
fn escrow_lock_debits_user() {
    let (_tmp, state) = tmp_state();
    let user = Address::new([1; 20]);
    fund(&state, user, 1_000_000);

    let stx = sign(Transaction::EscrowLock {
        from: user,
        job_id: JobId::new([9; 32]),
        amount: 500_000,
        nonce: 0,
        fee: 50_000,
    });

    let mut ctx = state.begin_block();
    let out = apply_tx(&mut ctx, &stx).unwrap();
    assert!(matches!(out, TxOutcome::Applied { .. }));
    ctx.commit().unwrap();

    let acct = state.get_account(&user).unwrap().unwrap();
    assert_eq!(acct.balance, 1_000_000 - 500_000 - 50_000);
    assert_eq!(acct.nonce, 1);
}

#[test]
fn escrow_settle_distributes_to_recipients() {
    let (_tmp, state) = tmp_state();
    let user = Address::new([1; 20]);
    let compute = Address::new([2; 20]);
    let verifier = Address::new([3; 20]);
    let router = Address::new([4; 20]);
    let treasury = Address::new([5; 20]);
    let job = JobId::new([42; 32]);

    fund(&state, user, 10_000_000);

    // Lock escrow.
    {
        let stx = sign(Transaction::EscrowLock {
            from: user,
            job_id: job,
            amount: 1_000_000,
            nonce: 0,
            fee: 50_000,
        });
        let mut ctx = state.begin_block();
        let out = apply_tx(&mut ctx, &stx).unwrap();
        assert!(matches!(out, TxOutcome::Applied { .. }));
        ctx.commit().unwrap();
    }

    // Settle escrow.
    {
        let stx = sign(Transaction::EscrowSettle {
            job_id: job,
            batch_id: [0; 32],
            compute_addr: compute,
            verifier_addr: verifier,
            router_addr: router,
            treasury_addr: treasury,
        });
        let mut ctx = state.begin_block();
        let out = apply_tx(&mut ctx, &stx).unwrap();
        assert!(matches!(out, TxOutcome::Applied { .. }));
        ctx.commit().unwrap();
    }

    // Check balances: 75+5=80% to compute (delegators go to compute
    // in Phase 1), 7% verifier, 5% router, 5% treasury, 3% burned.
    let c = state.get_account(&compute).unwrap().unwrap();
    let v = state.get_account(&verifier).unwrap().unwrap();
    let r = state.get_account(&router).unwrap().unwrap();
    let t = state.get_account(&treasury).unwrap().unwrap();

    assert_eq!(c.balance, 800_000, "compute+delegators = 80%");
    assert_eq!(v.balance, 70_000, "verifier = 7%");
    assert_eq!(r.balance, 50_000, "router = 5%");
    // Treasury gets remainder (5% + rounding).
    assert_eq!(t.balance, 1_000_000 - 800_000 - 70_000 - 50_000 - 30_000);
    // 3% burned = 30_000 → nobody's balance.
    let total_credited = c.balance + v.balance + r.balance + t.balance;
    assert_eq!(total_credited + 30_000, 1_000_000);
}

#[test]
fn reward_mint_credits_recipients() {
    let (_tmp, state) = tmp_state();
    let compute = Address::new([2; 20]);
    let verifier = Address::new([3; 20]);
    let router = Address::new([4; 20]);
    let treasury = Address::new([5; 20]);
    let job = JobId::new([77; 32]);

    let stx = sign(Transaction::RewardMint {
        job_id: job,
        total_reward: 2_000_000,
        compute_addr: compute,
        verifier_addr: verifier,
        router_addr: router,
        treasury_addr: treasury,
        output_tokens: 100,
    });

    let mut ctx = state.begin_block();
    let out = apply_tx(&mut ctx, &stx).unwrap();
    assert!(matches!(out, TxOutcome::Applied { .. }));
    ctx.commit().unwrap();

    let c = state.get_account(&compute).unwrap().unwrap();
    let v = state.get_account(&verifier).unwrap().unwrap();
    let r = state.get_account(&router).unwrap().unwrap();
    let t = state.get_account(&treasury).unwrap().unwrap();

    // Same 75+5/7/5/5/3 split on 2M.
    assert_eq!(c.balance, 1_600_000);
    assert_eq!(v.balance, 140_000);
    assert_eq!(r.balance, 100_000);
    let burned = 60_000u128;
    assert_eq!(
        c.balance + v.balance + r.balance + t.balance + burned,
        2_000_000
    );
}

#[test]
fn double_escrow_same_job_rejected() {
    let (_tmp, state) = tmp_state();
    let user = Address::new([1; 20]);
    fund(&state, user, 10_000_000);
    let job = JobId::new([11; 32]);

    // First lock — accepted.
    {
        let stx = sign(Transaction::EscrowLock {
            from: user,
            job_id: job,
            amount: 100,
            nonce: 0,
            fee: 50_000,
        });
        let mut ctx = state.begin_block();
        let out = apply_tx(&mut ctx, &stx).unwrap();
        assert!(matches!(out, TxOutcome::Applied { .. }));
        ctx.commit().unwrap();
    }

    // Second lock same job — rejected.
    {
        let stx = sign(Transaction::EscrowLock {
            from: user,
            job_id: job,
            amount: 100,
            nonce: 1,
            fee: 50_000,
        });
        let mut ctx = state.begin_block();
        let out = apply_tx(&mut ctx, &stx).unwrap();
        assert!(matches!(out, TxOutcome::Rejected(_)));
    }
}

#[test]
fn insufficient_balance_rejects_escrow() {
    let (_tmp, state) = tmp_state();
    let user = Address::new([1; 20]);
    fund(&state, user, 1_000);

    let stx = sign(Transaction::EscrowLock {
        from: user,
        job_id: JobId::new([22; 32]),
        amount: 2_000,
        nonce: 0,
        fee: 50_000,
    });

    let mut ctx = state.begin_block();
    let out = apply_tx(&mut ctx, &stx).unwrap();
    assert!(matches!(out, TxOutcome::Rejected(_)));
}
