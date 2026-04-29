//! End-to-end lifecycle tests against a real RocksDB-backed chain state.
//!
//! Exercises the §9 + §10 flow: deposit → withdraw → complete,
//! delegation, slashing with pro-rata delegator impact, and the
//! bootstrap-epoch transition of the validator-set ranker.

use arknet_chain::account::Account;
use arknet_chain::stake_apply::{apply_stake_op, UNBONDING_PERIOD_BLOCKS};
use arknet_chain::state::State;
use arknet_chain::transactions::{StakeOp, StakeRole};
use arknet_chain::validator::ValidatorInfo;
use arknet_common::types::{Address, Amount, NodeId, PubKey};
use arknet_staking::slashing::{apply_slash, Offense};
use arknet_staking::validator_set::{rank_candidates, recompute_validator_set};

fn tmp_state() -> (tempfile::TempDir, State) {
    let tmp = tempfile::tempdir().unwrap();
    let state = State::open(tmp.path()).unwrap();
    (tmp, state)
}

fn fund(state: &State, addr: Address, amount: Amount) {
    let mut ctx = state.begin_block();
    ctx.set_account(
        &addr,
        &Account {
            balance: amount,
            nonce: 0,
        },
    )
    .unwrap();
    ctx.commit().unwrap();
}

#[test]
fn deposit_then_withdraw_queues_unbonding() {
    let (_tmp, state) = tmp_state();
    let operator = Address::new([1; 20]);
    let node_id = NodeId::new([9; 32]);
    fund(&state, operator, 100_000);

    // Deposit 25k.
    {
        let mut ctx = state.begin_block();
        let out = apply_stake_op(
            &mut ctx,
            &StakeOp::Deposit {
                node_id,
                role: StakeRole::Validator,
                pool_id: None,
                amount: 25_000,
                delegator: None,
            },
            &operator,
            100,
        )
        .unwrap();
        assert!(matches!(
            out,
            arknet_chain::apply::TxOutcome::Applied { .. }
        ));
        ctx.commit().unwrap();
    }

    // Withdraw 10k → unbonding queued for height 100 + UNBONDING_PERIOD_BLOCKS.
    {
        let mut ctx = state.begin_block();
        let _ = apply_stake_op(
            &mut ctx,
            &StakeOp::Withdraw {
                node_id,
                role: StakeRole::Validator,
                pool_id: None,
                amount: 10_000,
            },
            &operator,
            101,
        )
        .unwrap();
        ctx.commit().unwrap();
    }

    let stake = state
        .get_stake(&node_id, StakeRole::Validator, None, None)
        .unwrap()
        .unwrap();
    assert_eq!(stake.amount, 15_000);

    let unbondings = state.iter_unbondings_for_node(&node_id).unwrap();
    assert_eq!(unbondings.len(), 1);
    assert_eq!(unbondings[0].amount, 10_000);
    assert_eq!(unbondings[0].completes_at, 101 + UNBONDING_PERIOD_BLOCKS);
}

#[test]
fn complete_before_window_is_rejected() {
    let (_tmp, state) = tmp_state();
    let operator = Address::new([1; 20]);
    let node_id = NodeId::new([9; 32]);
    fund(&state, operator, 100_000);

    // Deposit + immediately withdraw.
    {
        let mut ctx = state.begin_block();
        let _ = apply_stake_op(
            &mut ctx,
            &StakeOp::Deposit {
                node_id,
                role: StakeRole::Validator,
                pool_id: None,
                amount: 25_000,
                delegator: None,
            },
            &operator,
            100,
        )
        .unwrap();
        let _ = apply_stake_op(
            &mut ctx,
            &StakeOp::Withdraw {
                node_id,
                role: StakeRole::Validator,
                pool_id: None,
                amount: 10_000,
            },
            &operator,
            101,
        )
        .unwrap();
        ctx.commit().unwrap();
    }

    // Try to complete one block later — window not elapsed.
    {
        let mut ctx = state.begin_block();
        let out = apply_stake_op(
            &mut ctx,
            &StakeOp::Complete {
                node_id,
                role: StakeRole::Validator,
                pool_id: None,
                unbond_id: 0,
            },
            &operator,
            102,
        )
        .unwrap();
        match out {
            arknet_chain::apply::TxOutcome::Rejected(
                arknet_chain::apply::RejectReason::UnbondingNotComplete { .. },
            ) => {}
            other => panic!("expected UnbondingNotComplete, got {other:?}"),
        }
    }
}

#[test]
fn complete_after_window_credits_account() {
    let (_tmp, state) = tmp_state();
    let operator = Address::new([1; 20]);
    let node_id = NodeId::new([9; 32]);
    fund(&state, operator, 100_000);

    // Deposit 25k, withdraw 10k.
    {
        let mut ctx = state.begin_block();
        let _ = apply_stake_op(
            &mut ctx,
            &StakeOp::Deposit {
                node_id,
                role: StakeRole::Validator,
                pool_id: None,
                amount: 25_000,
                delegator: None,
            },
            &operator,
            100,
        )
        .unwrap();
        let _ = apply_stake_op(
            &mut ctx,
            &StakeOp::Withdraw {
                node_id,
                role: StakeRole::Validator,
                pool_id: None,
                amount: 10_000,
            },
            &operator,
            101,
        )
        .unwrap();
        ctx.commit().unwrap();
    }

    // Balance after deposit: 100k - 25k = 75k.
    assert_eq!(
        state.get_account(&operator).unwrap().unwrap().balance,
        75_000
    );

    // Complete at the exact window boundary.
    {
        let mut ctx = state.begin_block();
        let out = apply_stake_op(
            &mut ctx,
            &StakeOp::Complete {
                node_id,
                role: StakeRole::Validator,
                pool_id: None,
                unbond_id: 0,
            },
            &operator,
            101 + UNBONDING_PERIOD_BLOCKS,
        )
        .unwrap();
        assert!(matches!(
            out,
            arknet_chain::apply::TxOutcome::Applied { .. }
        ));
        ctx.commit().unwrap();
    }

    // Balance restored to 75k + 10k = 85k.
    assert_eq!(
        state.get_account(&operator).unwrap().unwrap().balance,
        85_000
    );
    // Unbonding entry deleted.
    let u = state.iter_unbondings_for_node(&node_id).unwrap();
    assert!(u.is_empty());
}

#[test]
fn delegator_gets_separate_entry() {
    let (_tmp, state) = tmp_state();
    let operator = Address::new([1; 20]);
    let delegator = Address::new([2; 20]);
    let node_id = NodeId::new([9; 32]);
    fund(&state, operator, 100_000);
    fund(&state, delegator, 100_000);

    // Operator self-stake.
    {
        let mut ctx = state.begin_block();
        let _ = apply_stake_op(
            &mut ctx,
            &StakeOp::Deposit {
                node_id,
                role: StakeRole::Validator,
                pool_id: None,
                amount: 20_000,
                delegator: None,
            },
            &operator,
            100,
        )
        .unwrap();
        ctx.commit().unwrap();
    }

    // Delegator stake.
    {
        let mut ctx = state.begin_block();
        let _ = apply_stake_op(
            &mut ctx,
            &StakeOp::Deposit {
                node_id,
                role: StakeRole::Validator,
                pool_id: None,
                amount: 5_000,
                delegator: Some(delegator),
            },
            &delegator,
            100,
        )
        .unwrap();
        ctx.commit().unwrap();
    }

    // Two distinct stake entries exist under the same (node, role, pool).
    let self_stake = state
        .get_stake(&node_id, StakeRole::Validator, None, None)
        .unwrap()
        .unwrap();
    let del_stake = state
        .get_stake(&node_id, StakeRole::Validator, None, Some(&delegator))
        .unwrap()
        .unwrap();
    assert_eq!(self_stake.amount, 20_000);
    assert_eq!(del_stake.amount, 5_000);

    // Balances debited correctly.
    assert_eq!(
        state.get_account(&operator).unwrap().unwrap().balance,
        80_000
    );
    assert_eq!(
        state.get_account(&delegator).unwrap().unwrap().balance,
        95_000
    );
}

#[test]
fn third_party_delegation_rejected() {
    let (_tmp, state) = tmp_state();
    let sender = Address::new([1; 20]);
    let some_other_addr = Address::new([2; 20]);
    let node_id = NodeId::new([9; 32]);
    fund(&state, sender, 100_000);

    let mut ctx = state.begin_block();
    let out = apply_stake_op(
        &mut ctx,
        &StakeOp::Deposit {
            node_id,
            role: StakeRole::Validator,
            pool_id: None,
            amount: 1_000,
            delegator: Some(some_other_addr),
        },
        &sender,
        100,
    )
    .unwrap();
    assert!(matches!(
        out,
        arknet_chain::apply::TxOutcome::Rejected(
            arknet_chain::apply::RejectReason::ThirdPartyDelegation
        )
    ));
}

#[test]
fn slashing_100_percent_zeroes_all_entries() {
    let (_tmp, state) = tmp_state();
    let operator = Address::new([1; 20]);
    let delegator = Address::new([2; 20]);
    let reporter = Address::new([3; 20]);
    let treasury = Address::new([4; 20]);
    let node_id = NodeId::new([9; 32]);
    fund(&state, operator, 100_000);
    fund(&state, delegator, 100_000);

    // Seed: 20k self + 5k delegator.
    {
        let mut ctx = state.begin_block();
        let _ = apply_stake_op(
            &mut ctx,
            &StakeOp::Deposit {
                node_id,
                role: StakeRole::Validator,
                pool_id: None,
                amount: 20_000,
                delegator: None,
            },
            &operator,
            100,
        )
        .unwrap();
        let _ = apply_stake_op(
            &mut ctx,
            &StakeOp::Deposit {
                node_id,
                role: StakeRole::Validator,
                pool_id: None,
                amount: 5_000,
                delegator: Some(delegator),
            },
            &delegator,
            100,
        )
        .unwrap();
        ctx.commit().unwrap();
    }

    // Slash 100% (WrongModelHash = 100%).
    let mut ctx = state.begin_block();
    let report = apply_slash(
        &mut ctx,
        &node_id,
        StakeRole::Validator,
        Offense::WrongModelHash,
        &reporter,
        &treasury,
    )
    .unwrap();
    ctx.commit().unwrap();

    // Both entries are drained to amount=0, which triggers the
    // `is_empty()` delete path in `set_stake` — the entries vanish
    // entirely rather than linger as zero-amount rows.
    assert!(state
        .get_stake(&node_id, StakeRole::Validator, None, None)
        .unwrap()
        .is_none());
    assert!(state
        .get_stake(&node_id, StakeRole::Validator, None, Some(&delegator))
        .unwrap()
        .is_none());

    // Report sums.
    assert_eq!(report.total_slashed, 25_000);
    assert_eq!(report.burned, 22_500); // 90%
    assert_eq!(report.to_reporter, 1_250); // 5%
    assert_eq!(report.to_treasury, 1_250); // 5%

    // Reporter + treasury credited.
    assert_eq!(
        state.get_account(&reporter).unwrap().unwrap().balance,
        1_250
    );
    assert_eq!(
        state.get_account(&treasury).unwrap().unwrap().balance,
        1_250
    );
}

#[test]
fn slashing_low_severity_preserves_most_stake() {
    let (_tmp, state) = tmp_state();
    let operator = Address::new([1; 20]);
    let reporter = Address::new([3; 20]);
    let treasury = Address::new([4; 20]);
    let node_id = NodeId::new([9; 32]);
    fund(&state, operator, 100_000);

    {
        let mut ctx = state.begin_block();
        let _ = apply_stake_op(
            &mut ctx,
            &StakeOp::Deposit {
                node_id,
                role: StakeRole::Validator,
                pool_id: None,
                amount: 10_000,
                delegator: None,
            },
            &operator,
            100,
        )
        .unwrap();
        ctx.commit().unwrap();
    }

    // ExtendedDowntime = 1%.
    let mut ctx = state.begin_block();
    let report = apply_slash(
        &mut ctx,
        &node_id,
        StakeRole::Validator,
        Offense::ExtendedDowntime,
        &reporter,
        &treasury,
    )
    .unwrap();
    ctx.commit().unwrap();

    assert_eq!(report.total_slashed, 100);
    let e = state
        .get_stake(&node_id, StakeRole::Validator, None, None)
        .unwrap()
        .unwrap();
    assert_eq!(e.amount, 9_900);
}

#[test]
fn ranker_orders_by_stake_descending() {
    let (_tmp, state) = tmp_state();
    let operator = Address::new([0xaa; 20]);
    fund(&state, operator, 1_000_000);

    let low = NodeId::new([1; 32]);
    let mid = NodeId::new([2; 32]);
    let high = NodeId::new([3; 32]);

    // Register validator records (these exist in the `validators` CF).
    {
        let mut ctx = state.begin_block();
        for nid in [low, mid, high] {
            ctx.set_validator(
                &nid,
                &ValidatorInfo {
                    node_id: nid,
                    consensus_key: PubKey::ed25519([nid.as_bytes()[0]; 32]),
                    operator,
                    bonded_stake: 0,
                    voting_power: 1,
                    is_genesis: true,
                    jailed: false,
                },
            )
            .unwrap();
        }
        // Seed stakes — ascending numeric bytes, descending rank.
        let _ = apply_stake_op(
            &mut ctx,
            &StakeOp::Deposit {
                node_id: low,
                role: StakeRole::Validator,
                pool_id: None,
                amount: 1_000,
                delegator: None,
            },
            &operator,
            10,
        )
        .unwrap();
        let _ = apply_stake_op(
            &mut ctx,
            &StakeOp::Deposit {
                node_id: mid,
                role: StakeRole::Validator,
                pool_id: None,
                amount: 5_000,
                delegator: None,
            },
            &operator,
            10,
        )
        .unwrap();
        let _ = apply_stake_op(
            &mut ctx,
            &StakeOp::Deposit {
                node_id: high,
                role: StakeRole::Validator,
                pool_id: None,
                amount: 10_000,
                delegator: None,
            },
            &operator,
            10,
        )
        .unwrap();
        ctx.commit().unwrap();
    }

    let ctx = state.begin_block();
    let ranked = rank_candidates(&ctx).unwrap();
    assert_eq!(ranked.len(), 3);
    assert_eq!(ranked[0].total_stake, 10_000);
    assert_eq!(ranked[1].total_stake, 5_000);
    assert_eq!(ranked[2].total_stake, 1_000);
}

#[test]
fn recompute_post_bootstrap_evicts_zero_stake() {
    use arknet_chain::bootstrap::BOOTSTRAP_MAX_BLOCKS;

    let (_tmp, state) = tmp_state();
    let operator = Address::new([0xaa; 20]);
    let funded = NodeId::new([1; 32]);
    let starving = NodeId::new([2; 32]);

    fund(&state, operator, 100_000);

    // Both validators registered; only `funded` has stake.
    {
        let mut ctx = state.begin_block();
        for nid in [funded, starving] {
            ctx.set_validator(
                &nid,
                &ValidatorInfo {
                    node_id: nid,
                    consensus_key: PubKey::ed25519([nid.as_bytes()[0]; 32]),
                    operator,
                    bonded_stake: 0,
                    voting_power: 1,
                    is_genesis: false, // not genesis → subject to stake minimum
                    jailed: false,
                },
            )
            .unwrap();
        }
        let _ = apply_stake_op(
            &mut ctx,
            &StakeOp::Deposit {
                node_id: funded,
                role: StakeRole::Validator,
                pool_id: None,
                amount: 50_000,
                delegator: None,
            },
            &operator,
            10,
        )
        .unwrap();
        ctx.commit().unwrap();
    }

    // Post-bootstrap height → recompute evicts zero-stake validator.
    {
        let mut ctx = state.begin_block();
        let active = recompute_validator_set(&mut ctx, BOOTSTRAP_MAX_BLOCKS + 1).unwrap();
        ctx.commit().unwrap();
        assert_eq!(active, 1);
    }
    assert!(state.get_validator(&funded).unwrap().is_some());
    assert!(state.get_validator(&starving).unwrap().is_none());
}
