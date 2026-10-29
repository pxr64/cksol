use super::{event::*, *};
use crate::{
    constants::{FEE_PER_SIGNATURE, GET_BALANCE_CYCLES, RENT_EXEMPTION_THRESHOLD},
    rpc::BlockHeight,
    sol_transfer::MAX_SIGNATURES,
    state::{audit::process_event, read_state},
    test_fixtures::{
        AUTOMATIC_DEPOSIT_FEE, DEPOSIT_SOL_FEE, DEPOSIT_SOL_REQUIRED_CYCLES,
        MINIMUM_DEPOSIT_AMOUNT, MINIMUM_WITHDRAWAL_AMOUNT, WITHDRAWAL_FEE, account, address,
        arb::arb_event,
        deposit_id,
        events::{
            accept_withdrawal, accept_withdrawal_at, create_withdrawal_batch_transaction,
            credit_sweep, expire_transaction, fail_transaction, queue_deposit, submit_sweep,
            submit_withdrawal, submit_withdrawal_batch_transaction, succeed_transaction,
        },
        init_balance, init_schnorr_master_key, init_state, ledger_canister_id, planned_sweep,
        queued_deposit,
        runtime::TestCanisterRuntime,
        signature, sol_rpc_canister_id, valid_init_args,
    },
    utils::insertion_ordered_map::InsertionOrderedMap,
};
use assert_matches::assert_matches;
use cksol_types_internal::{Ed25519KeyName, InitArgs, SolanaNetwork, UpgradeArgs};
use ic_stable_structures::Storable;
use proptest::prelude::*;
use std::borrow::Cow;

proptest! {
    #[test]
    fn event_minicbor_roundtrip(event in arb_event()) {
        let bytes = event.to_bytes();
        let decoded = Event::from_bytes(Cow::Borrowed(&bytes));
        assert_eq!(event, decoded);
    }
}

mod spl_orchestrator_configuration {
    use super::*;

    #[test]
    fn should_reject_orchestrator_equal_to_another_canister_on_init_and_upgrade() {
        let args = valid_init_args();
        for id in [args.sol_rpc_canister_id, args.ledger_canister_id] {
            assert_matches!(
                State::try_from(InitArgs {
                    ledger_suite_orchestrator_id: Some(id),
                    ..valid_init_args()
                }),
                Err(InvalidStateError::InvalidCanisterId(_))
            );
            let mut state = State::try_from(valid_init_args()).unwrap();
            assert_matches!(
                state.upgrade(UpgradeArgs {
                    ledger_suite_orchestrator_id: Some(id),
                    ..UpgradeArgs::default()
                }),
                Err(InvalidStateError::InvalidCanisterId(_))
            );
        }
    }

    #[test]
    fn should_reject_anonymous_and_management_orchestrators_on_init_and_upgrade() {
        for id in [Principal::anonymous(), Principal::management_canister()] {
            assert_matches!(
                State::try_from(InitArgs {
                    ledger_suite_orchestrator_id: Some(id),
                    ..valid_init_args()
                }),
                Err(InvalidStateError::InvalidCanisterId(_))
            );
            let mut state = State::try_from(valid_init_args()).unwrap();
            assert_matches!(
                state.upgrade(UpgradeArgs {
                    ledger_suite_orchestrator_id: Some(id),
                    ..UpgradeArgs::default()
                }),
                Err(InvalidStateError::InvalidCanisterId(_))
            );
        }
    }

    // Simulate the pre-orchestrator CBOR layout by removing the last array field.
    // Existing field indices must stay unchanged and old events must remain readable.
    fn without_last_field(bytes: &[u8]) -> Vec<u8> {
        let mut decoder = minicbor::Decoder::new(bytes);
        let len = decoder.array().unwrap().unwrap();
        assert!(len < 24);
        for _ in 0..len - 1 {
            decoder.skip().unwrap();
        }
        let mut legacy = bytes[..decoder.position()].to_vec();
        legacy[0] = 0x80 + (len as u8 - 1);
        legacy
    }

    #[test]
    fn should_decode_legacy_lifecycle_events_without_an_orchestrator_field() {
        let mut init = InitArgs {
            ledger_suite_orchestrator_id: Some(Principal::from_slice(&[42])),
            ..valid_init_args()
        };
        let encoded = minicbor::to_vec(&init).unwrap();
        let decoded: InitArgs = minicbor::decode(&without_last_field(&encoded)).unwrap();
        init.ledger_suite_orchestrator_id = None;
        assert_eq!(decoded, init);
        let mut upgrade = UpgradeArgs {
            ledger_suite_orchestrator_id: Some(Principal::from_slice(&[42])),
            ..UpgradeArgs::default()
        };
        let encoded = minicbor::to_vec(&upgrade).unwrap();
        let decoded: UpgradeArgs = minicbor::decode(&without_last_field(&encoded)).unwrap();
        upgrade.ledger_suite_orchestrator_id = None;
        assert_eq!(decoded, upgrade);
    }

    #[test]
    fn should_preserve_orchestrator_in_cbor_lifecycle_args() {
        let id = Principal::from_slice(&[42, 10]);
        let init = InitArgs {
            ledger_suite_orchestrator_id: Some(id),
            ..valid_init_args()
        };
        let decoded: InitArgs = minicbor::decode(&minicbor::to_vec(&init).unwrap()).unwrap();
        assert_eq!(decoded, init);
        let upgrade = UpgradeArgs {
            ledger_suite_orchestrator_id: Some(id),
            ..UpgradeArgs::default()
        };
        let decoded: UpgradeArgs = minicbor::decode(&minicbor::to_vec(&upgrade).unwrap()).unwrap();
        assert_eq!(decoded, upgrade);
    }
}

mod cache_minter_public_key {
    use super::*;

    use ic_ed25519::{PocketIcMasterPublicKeyId, PublicKey};

    #[test]
    fn should_ignore_caching_the_same_key_again() {
        let mut state = state();
        let key = schnorr_public_key(1);

        state.cache_minter_public_key(key.clone());
        state.cache_minter_public_key(key.clone());

        assert_eq!(state.minter_public_key(), Some(&key));
    }

    #[test]
    #[should_panic(expected = "BUG: attempt to overwrite the minter public key")]
    fn should_panic_when_caching_a_different_key() {
        let mut state = state();

        state.cache_minter_public_key(schnorr_public_key(1));
        state.cache_minter_public_key(schnorr_public_key(2));
    }

    fn state() -> State {
        State::try_from(valid_init_args()).unwrap()
    }

    fn schnorr_public_key(chain_code_byte: u8) -> SchnorrPublicKey {
        SchnorrPublicKey {
            public_key: PublicKey::pocketic_key(PocketIcMasterPublicKeyId::Key1),
            chain_code: [chain_code_byte; 32],
        }
    }
}

mod minter_public_key_fetched {
    use super::*;
    use crate::{
        state::audit::replay_events,
        test_fixtures::{minter_public_key_fetched_event, schnorr_master_key},
    };

    #[test]
    fn should_make_the_key_available_after_replay() {
        let state = replay_events(vec![
            Event {
                timestamp: 0,
                payload: EventType::Init(valid_init_args()),
            },
            Event {
                timestamp: 1,
                payload: minter_public_key_fetched_event(),
            },
        ]);

        assert_eq!(state.minter_public_key(), Some(&schnorr_master_key()));
    }
}

mod queued_deposits {
    use super::*;
    use crate::state::audit::replay_events;

    #[test]
    fn should_replay_queued_deposits_like_direct_transitions() {
        let queued_at = |deposit_id| 10 * (deposit_id + 1);
        let queued = |deposit_id| Event {
            timestamp: queued_at(deposit_id),
            payload: EventType::QueuedDeposit {
                deposit_id,
                account: queued_deposit(deposit_id).account,
                address: queued_deposit(deposit_id).address,
                balance: queued_deposit(deposit_id).balance,
            },
        };
        let init = Event {
            timestamp: 0,
            payload: EventType::Init(valid_init_args()),
        };
        let key_fetched = Event {
            timestamp: 0,
            payload: crate::test_fixtures::minter_public_key_fetched_event(),
        };
        let mut expected = State::try_from(valid_init_args()).unwrap();
        expected.cache_minter_public_key(crate::test_fixtures::schnorr_master_key());
        for deposit_id in 0..2 {
            expected.process_queued_deposit(
                deposit_id,
                queued_deposit(deposit_id),
                queued_at(deposit_id),
            );
        }

        let replayed = replay_events([init, key_fetched, queued(0), queued(1)]);

        assert_eq!(replayed, expected);
        assert_eq!(
            replayed.deposits().oldest_in_flight_queued_at(),
            Some(queued_at(0))
        );
    }
}

mod watched_deposit_addresses {
    use super::*;
    use crate::test_fixtures::{
        MINIMUM_DEPOSIT_AMOUNT, REGISTERED_AT, at, flow::deposit::DepositFlow, watch,
    };

    #[test]
    fn should_leave_the_watchlist_when_a_deposit_is_queued() {
        init_state();
        init_schnorr_master_key();
        watch(at(REGISTERED_AT), account(1));
        watch(at(REGISTERED_AT), account(2));

        DepositFlow::queue(account(1), MINIMUM_DEPOSIT_AMOUNT);

        read_state(|s| {
            assert_eq!(s.watchlist().get(at(REGISTERED_AT), &account(1)), None);
            assert!(s.watchlist().get(at(REGISTERED_AT), &account(2)).is_some());
        });
    }
}

mod swept_deposits {
    use super::*;
    use crate::{
        state::{audit::replay_events, reset_state},
        storage::{reset_events, with_event_iter},
        test_fixtures::{
            DEFAULT_BLOCK_HEIGHT,
            events::{credit_sweep, quarantine_sweep, queue_deposits, submit_sweep},
            flow::deposit::{CreditedSweepFlow, DepositFlow, SweepFlow},
            sweep_message,
        },
    };

    const SWEEP_SIGNATURE_INDEX: usize = 0xAA;

    #[test]
    fn should_move_queued_deposits_to_swept_with_signature_and_received_amount() {
        init_state();
        init_schnorr_master_key();
        let [first, _, third] = queue_deposits();
        let sweep_signature = signature(SWEEP_SIGNATURE_INDEX);

        submit_sweep(sweep_signature, vec![2, 0]);

        read_state(|s| {
            assert_eq!(
                s.deposits().swept().get(&sweep_signature),
                Some(&planned_sweep([(0, first), (2, third)]))
            );
            assert_eq!(s.deposits().queued().keys().collect::<Vec<_>>(), vec![&1]);
            assert_matches!(
                s.submitted_transactions().get(&sweep_signature).unwrap(),
                MinterTransaction::SweepDeposit { .. }
            );
            assert_eq!(s.balance(), 0);
        });
    }

    #[test]
    fn should_finalize_the_sweep_when_the_transaction_succeeds() {
        init_state();
        init_schnorr_master_key();
        let [first, _, third] = queue_deposits();
        let sweep_signature = signature(SWEEP_SIGNATURE_INDEX);
        submit_sweep(sweep_signature, vec![2, 0]);

        succeed_transaction(sweep_signature);

        read_state(|s| {
            assert!(s.submitted_transactions().is_empty());
            assert!(s.deposits().swept().is_empty());
            assert_eq!(
                s.deposits().finalized().get(&sweep_signature),
                Some(&planned_sweep([(0, first), (2, third)]))
            );
            assert_eq!(s.balance(), 0);
        });
    }

    #[test]
    fn should_drop_swept_deposits_when_the_sweep_fails_or_expires() {
        type RecordOutcome = fn(Signature);
        let outcomes: [(&str, RecordOutcome); 2] = [
            ("failed", fail_transaction),
            ("expired", expire_transaction),
        ];
        for (outcome, record_outcome) in outcomes {
            reset_state();
            reset_events();
            init_state();
            init_schnorr_master_key();
            queue_deposits::<3>();
            let sweep_signature = signature(SWEEP_SIGNATURE_INDEX);
            submit_sweep(sweep_signature, vec![2, 0]);

            record_outcome(sweep_signature);

            read_state(|s| {
                assert!(s.submitted_transactions().is_empty(), "{outcome}");
                assert!(s.deposits().swept().is_empty(), "{outcome}");
                assert_eq!(s.deposits().dropped().len(), 2, "{outcome}");
                assert_eq!(s.balance(), 0, "{outcome}");
            });
        }
    }

    #[test]
    fn should_credit_the_balance_with_the_amount_received_by_the_sweep() {
        init_state();
        init_schnorr_master_key();
        let [first, _, third] = queue_deposits();
        let sweep_signature = signature(SWEEP_SIGNATURE_INDEX);
        submit_sweep(sweep_signature, vec![2, 0]);
        succeed_transaction(sweep_signature);
        let amount_received = planned_sweep([(0, first), (2, third)]).expected_received();

        credit_sweep(sweep_signature, amount_received);

        read_state(|s| {
            assert!(s.deposits().finalized().is_empty());
            assert_eq!(
                s.deposits().pending_mints().keys().collect::<Vec<_>>(),
                vec![&0, &2]
            );
            assert_eq!(s.balance(), amount_received);
        });
    }

    #[test]
    #[should_panic(expected = "exceeding the 399 lamports received")]
    fn should_panic_when_the_mints_exceed_the_amount_received() {
        init_state();
        init_schnorr_master_key();
        queue_deposits::<3>();
        let sweep_signature = signature(SWEEP_SIGNATURE_INDEX);
        submit_sweep(sweep_signature, vec![2, 0]);
        succeed_transaction(sweep_signature);

        mutate_state(|s| {
            process_event(
                s,
                EventType::CreditedSweep {
                    signature: sweep_signature,
                    amount_received: 399,
                    mints: vec![
                        CreditedDeposit {
                            deposit_id: 0,
                            amount_to_mint: 100,
                        },
                        CreditedDeposit {
                            deposit_id: 2,
                            amount_to_mint: 300,
                        },
                    ],
                },
                &TestCanisterRuntime::new().add_times([0, 0]),
            )
        });
    }

    #[test]
    fn should_mint_pending_deposit_without_changing_the_balance() {
        init_state();
        let credited = credit_sweep_of_two_deposits(0);
        let [to_mint, _] = credited.pending_mints();

        to_mint.mint(42);

        read_state(|s| assert_eq!(s.balance(), credited.amount_received));
    }

    #[test]
    fn should_quarantine_pending_mint_without_changing_the_balance() {
        init_state();
        let credited = credit_sweep_of_two_deposits(0);
        let [to_quarantine, _] = credited.pending_mints();

        to_quarantine.quarantine();

        read_state(|s| assert_eq!(s.balance(), credited.amount_received));
    }

    #[test]
    fn should_replay_the_recorded_events_including_created_at_time() {
        const CREDITED_AT: u64 = 1_234_000_000;
        init_state();
        let credited = credit_sweep_of_two_deposits(CREDITED_AT);
        let [to_mint, sibling] = credited.pending_mints();
        to_mint.mint(42);
        let recorded_events: Vec<Event> = with_event_iter(|events| events.collect());

        let replayed = replay_events(
            [
                Event {
                    timestamp: 0,
                    payload: EventType::Init(crate::test_fixtures::init_args_with_nonce_account()),
                },
                Event {
                    timestamp: 0,
                    payload: crate::test_fixtures::minter_public_key_fetched_event(),
                },
            ]
            .into_iter()
            .chain(recorded_events),
        );

        assert_eq!(
            replayed.deposits().pending_mints()[&sibling.deposit_id].created_at_time,
            CREDITED_AT
        );
        read_state(|live_state| assert_eq!(&replayed, live_state));
    }

    fn credit_sweep_of_two_deposits(credited_at: u64) -> CreditedSweepFlow {
        init_schnorr_master_key();
        let deposits = [1, 2].map(|i| DepositFlow::queue(account(i), 1_000_000 * i as u64));
        SweepFlow::of(deposits)
            .submit(signature(SWEEP_SIGNATURE_INDEX))
            .succeed()
            .credit_at(credited_at)
    }

    #[test]
    fn should_quarantine_finalized_deposits_without_crediting_the_balance() {
        init_state();
        init_schnorr_master_key();
        queue_deposits::<3>();
        let sweep_signature = signature(SWEEP_SIGNATURE_INDEX);
        submit_sweep(sweep_signature, vec![2, 0]);
        succeed_transaction(sweep_signature);

        quarantine_sweep(sweep_signature);

        read_state(|s| {
            assert!(s.deposits().finalized().is_empty());
            assert_eq!(s.deposits().quarantined().len(), 2);
            assert_eq!(s.balance(), 0);
        });
    }

    #[test]
    #[should_panic(
        expected = "BUG: a sweep was submitted before the minter public key was recorded"
    )]
    fn should_panic_when_a_sweep_is_submitted_without_the_minter_public_key() {
        let mut state = State::try_from(valid_init_args()).unwrap();

        state.process_transaction_submitted(
            &signature(SWEEP_SIGNATURE_INDEX),
            &sweep_message([(0, queued_deposit(0))]),
            &[Signer::Account(queued_deposit(0).account)],
            &TransactionPurpose::SweepDeposit {
                deposit_ids: vec![0],
                block_height: DEFAULT_BLOCK_HEIGHT,
            },
            0,
        );
    }
}

mod nonce_accounts {
    use super::*;
    use crate::{state::audit::replay_events, test_fixtures::durable_nonce};

    #[test]
    fn should_only_add_a_nonce_account_once_no_incomplete_withdrawal_targets_it() {
        init_state();
        init_balance();
        let destination = address(0);
        let add_destination = || {
            mutate_state(|s| {
                s.upgrade(UpgradeArgs {
                    nonce_accounts_to_add: Some(vec![destination.to_string()]),
                    ..Default::default()
                })
            })
        };

        accept_withdrawal(account(1), 0, MINIMUM_WITHDRAWAL_AMOUNT);
        assert_eq!(
            add_destination(),
            Err(InvalidStateError::NonceAccountIsWithdrawalDestination(
                destination
            ))
        );

        create_withdrawal_batch_transaction(durable_nonce(1), vec![0]);
        assert_eq!(
            add_destination(),
            Err(InvalidStateError::NonceAccountIsWithdrawalDestination(
                destination
            ))
        );

        submit_withdrawal_batch_transaction(signature(1), durable_nonce(1), vec![0]);
        assert_eq!(
            add_destination(),
            Err(InvalidStateError::NonceAccountIsWithdrawalDestination(
                destination
            ))
        );

        succeed_transaction(signature(1));
        add_destination().unwrap();
        assert!(read_state(|s| s
            .nonce_pool_addresses()
            .contains(&destination)));
    }

    #[test]
    fn should_fail_init_with_malformed_nonce_account() {
        let err = State::try_from(InitArgs {
            nonce_accounts: vec!["not-a-base58-address".to_string()],
            ..valid_init_args()
        })
        .unwrap_err();

        assert_matches!(err, InvalidStateError::InvalidNonceAccount(_));
    }

    #[test]
    fn should_add_nonce_accounts_through_upgrades() {
        let mut state = State::try_from(valid_init_args()).unwrap();

        state
            .upgrade(UpgradeArgs {
                nonce_accounts_to_add: Some(vec![nonce_account(1), nonce_account(2)]),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(
            pool_addresses(&state),
            vec![nonce_account(1), nonce_account(2)]
        );
    }

    #[test]
    fn should_replay_nonce_accounts_like_direct_transitions() {
        let init_args = InitArgs {
            nonce_accounts: vec![nonce_account(1)],
            ..valid_init_args()
        };
        let upgrade_args = UpgradeArgs {
            nonce_accounts_to_add: Some(vec![nonce_account(2)]),
            ..Default::default()
        };
        let mut expected = State::try_from(init_args.clone()).unwrap();
        expected.upgrade(upgrade_args.clone()).unwrap();

        let replayed = replay_events([
            Event {
                timestamp: 0,
                payload: EventType::Init(init_args),
            },
            Event {
                timestamp: 1,
                payload: EventType::Upgrade(upgrade_args),
            },
        ]);

        assert_eq!(replayed, expected);
        assert_eq!(
            pool_addresses(&replayed),
            vec![nonce_account(1), nonce_account(2)]
        );
    }

    fn nonce_account(i: usize) -> String {
        address(i).to_string()
    }

    fn pool_addresses(state: &State) -> Vec<String> {
        state
            .nonce_pool_addresses()
            .iter()
            .map(Address::to_string)
            .collect()
    }
}

mod state_validation {
    use super::*;

    #[test]
    fn should_fail_with_invalid_args() {
        // minimum_deposit_amount below the fee of a full sweep + rent exemption threshold
        let maximum_sweep_fee = MAX_SIGNATURES * FEE_PER_SIGNATURE;
        let minimum_required = maximum_sweep_fee + RENT_EXEMPTION_THRESHOLD;
        assert_fails_both(
            InitArgs {
                minimum_deposit_amount: minimum_required - 1,
                ..valid_init_args()
            },
            UpgradeArgs {
                minimum_deposit_amount: Some(minimum_required - 1),
                ..Default::default()
            },
            |e| {
                e == &InvalidStateError::InvalidMinimumDepositAmount {
                    minimum_deposit_amount: minimum_required - 1,
                    maximum_sweep_fee,
                    rent_exemption_threshold: RENT_EXEMPTION_THRESHOLD,
                }
            },
        );
        // minimum_deposit_amount leaves an empty main address below the rent exemption threshold
        let minimum_funding_main_address = 2 * RENT_EXEMPTION_THRESHOLD + FEE_PER_SIGNATURE;
        assert_fails_both(
            InitArgs {
                minimum_deposit_amount: minimum_funding_main_address - 1,
                ..valid_init_args()
            },
            UpgradeArgs {
                minimum_deposit_amount: Some(minimum_funding_main_address - 1),
                ..Default::default()
            },
            |e| {
                e == &InvalidStateError::MinimumDepositAmountLeavesMainAddressBelowRent {
                    minimum_deposit_amount: minimum_funding_main_address - 1,
                    rent_exemption_threshold: RENT_EXEMPTION_THRESHOLD,
                    fee_per_signature: FEE_PER_SIGNATURE,
                }
            },
        );
        // withdrawal_fee exceeds minimum_withdrawal_amount - rent exemption threshold
        assert_fails_both(
            InitArgs {
                withdrawal_fee: MINIMUM_WITHDRAWAL_AMOUNT - RENT_EXEMPTION_THRESHOLD + 1,
                ..valid_init_args()
            },
            UpgradeArgs {
                withdrawal_fee: Some(MINIMUM_WITHDRAWAL_AMOUNT - RENT_EXEMPTION_THRESHOLD + 1),
                ..Default::default()
            },
            |e| matches!(e, InvalidStateError::InvalidMinimumWithdrawalAmount { .. }),
        );
        // minimum_withdrawal_amount below withdrawal_fee + rent exemption threshold
        assert_fails_both(
            InitArgs {
                minimum_withdrawal_amount: WITHDRAWAL_FEE + RENT_EXEMPTION_THRESHOLD - 1,
                ..valid_init_args()
            },
            UpgradeArgs {
                minimum_withdrawal_amount: Some(WITHDRAWAL_FEE + RENT_EXEMPTION_THRESHOLD - 1),
                ..Default::default()
            },
            |e| matches!(e, InvalidStateError::InvalidMinimumWithdrawalAmount { .. }),
        );
        let minimum_required = GET_BALANCE_CYCLES + DEPOSIT_SOL_FEE;
        assert_fails_both(
            InitArgs {
                deposit_sol_required_cycles: (minimum_required - 1) as u64,
                ..valid_init_args()
            },
            UpgradeArgs {
                deposit_sol_required_cycles: Some((minimum_required - 1) as u64),
                ..Default::default()
            },
            |e| {
                e == &InvalidStateError::DepositSolRequiredCyclesTooLow {
                    required_cycles: minimum_required - 1,
                    get_balance_cycles: GET_BALANCE_CYCLES,
                    deposit_sol_fee: DEPOSIT_SOL_FEE,
                }
            },
        );
        let maximum_fee = DEPOSIT_SOL_REQUIRED_CYCLES - GET_BALANCE_CYCLES;
        assert_fails_both(
            InitArgs {
                deposit_sol_fee: (maximum_fee + 1) as u64,
                ..valid_init_args()
            },
            UpgradeArgs {
                deposit_sol_fee: Some((maximum_fee + 1) as u64),
                ..Default::default()
            },
            |e| {
                e == &InvalidStateError::DepositSolRequiredCyclesTooLow {
                    required_cycles: DEPOSIT_SOL_REQUIRED_CYCLES,
                    get_balance_cycles: GET_BALANCE_CYCLES,
                    deposit_sol_fee: maximum_fee + 1,
                }
            },
        );
        let maximum_automatic_deposit_fee =
            MINIMUM_DEPOSIT_AMOUNT - RENT_EXEMPTION_THRESHOLD - FEE_PER_SIGNATURE - 1;
        assert_fails_both(
            InitArgs {
                automatic_deposit_fee: Some(maximum_automatic_deposit_fee + 1),
                ..valid_init_args()
            },
            UpgradeArgs {
                automatic_deposit_fee: Some(maximum_automatic_deposit_fee + 1),
                ..Default::default()
            },
            |e| {
                e == &InvalidStateError::AutomaticDepositFeeTooHigh {
                    automatic_deposit_fee: maximum_automatic_deposit_fee + 1,
                    max_automatic_deposit_fee: maximum_automatic_deposit_fee,
                }
            },
        );
        let minimum_deposit_amount_for_fee =
            AUTOMATIC_DEPOSIT_FEE + RENT_EXEMPTION_THRESHOLD + FEE_PER_SIGNATURE;
        assert_fails_both(
            InitArgs {
                minimum_deposit_amount: minimum_deposit_amount_for_fee,
                ..valid_init_args()
            },
            UpgradeArgs {
                minimum_deposit_amount: Some(minimum_deposit_amount_for_fee),
                ..Default::default()
            },
            |e| matches!(e, InvalidStateError::AutomaticDepositFeeTooHigh { .. }),
        );
    }

    #[test]
    fn should_not_constrain_an_unconfigured_automatic_deposit_fee() {
        let minimum_deposit_amount = 2 * RENT_EXEMPTION_THRESHOLD + FEE_PER_SIGNATURE;

        State::try_from(InitArgs {
            minimum_deposit_amount,
            automatic_deposit_fee: None,
            ..valid_init_args()
        })
        .unwrap();
    }

    #[test]
    fn should_succeed_at_boundary_conditions() {
        // minimum_deposit_amount can equal twice the rent exemption threshold + one signature fee
        let minimum_required = 2 * RENT_EXEMPTION_THRESHOLD + FEE_PER_SIGNATURE;
        let init_args_without_automatic_deposit_fee = InitArgs {
            automatic_deposit_fee: None,
            ..valid_init_args()
        };
        State::try_from(InitArgs {
            minimum_deposit_amount: minimum_required,
            ..init_args_without_automatic_deposit_fee.clone()
        })
        .unwrap();
        State::try_from(init_args_without_automatic_deposit_fee)
            .unwrap()
            .upgrade(UpgradeArgs {
                minimum_deposit_amount: Some(minimum_required),
                ..Default::default()
            })
            .unwrap();
        // minimum_withdrawal_amount can equal withdrawal_fee + rent exemption threshold
        let minimum_required = WITHDRAWAL_FEE + RENT_EXEMPTION_THRESHOLD;
        assert_succeeds_both(
            InitArgs {
                minimum_withdrawal_amount: minimum_required,
                ..valid_init_args()
            },
            UpgradeArgs {
                minimum_withdrawal_amount: Some(minimum_required),
                ..Default::default()
            },
        );
        let minimum_required = GET_BALANCE_CYCLES + DEPOSIT_SOL_FEE;
        assert_succeeds_both(
            InitArgs {
                deposit_sol_required_cycles: minimum_required as u64,
                ..valid_init_args()
            },
            UpgradeArgs {
                deposit_sol_required_cycles: Some(minimum_required as u64),
                ..Default::default()
            },
        );
        let maximum_fee = DEPOSIT_SOL_REQUIRED_CYCLES - GET_BALANCE_CYCLES;
        assert_succeeds_both(
            InitArgs {
                deposit_sol_fee: maximum_fee as u64,
                ..valid_init_args()
            },
            UpgradeArgs {
                deposit_sol_fee: Some(maximum_fee as u64),
                ..Default::default()
            },
        );
        let maximum_automatic_deposit_fee =
            MINIMUM_DEPOSIT_AMOUNT - RENT_EXEMPTION_THRESHOLD - FEE_PER_SIGNATURE - 1;
        assert_succeeds_both(
            InitArgs {
                automatic_deposit_fee: Some(maximum_automatic_deposit_fee),
                ..valid_init_args()
            },
            UpgradeArgs {
                automatic_deposit_fee: Some(maximum_automatic_deposit_fee),
                ..Default::default()
            },
        );
    }

    fn assert_fails_both(
        init_args: InitArgs,
        upgrade_args: UpgradeArgs,
        check: impl Fn(&InvalidStateError) -> bool + Copy,
    ) {
        let err = State::try_from(init_args).unwrap_err();
        assert!(check(&err), "init: unexpected error: {err:?}");
        let mut state = State::try_from(valid_init_args()).unwrap();
        let err = state.upgrade(upgrade_args).unwrap_err();
        assert!(check(&err), "upgrade: unexpected error: {err:?}");
    }

    fn assert_succeeds_both(init_args: InitArgs, upgrade_args: UpgradeArgs) {
        State::try_from(init_args).unwrap();
        let mut state = State::try_from(valid_init_args()).unwrap();
        state.upgrade(upgrade_args).unwrap();
    }
}

mod state_from_init_args {
    use super::*;

    #[test]
    fn should_succeed() {
        let state = State::try_from(valid_init_args()).unwrap();

        assert_eq!(
            state,
            State {
                ledger_suite_orchestrator_id: None,
                minter_public_key: None,
                master_key_name: Ed25519KeyName::MainnetProdKey1,
                ledger_canister_id: ledger_canister_id(),
                sol_rpc_canister_id: sol_rpc_canister_id(),
                solana_network: SolanaNetwork::Mainnet,
                deposit_sol_fee: DEPOSIT_SOL_FEE,
                withdrawal_fee: WITHDRAWAL_FEE,
                minimum_withdrawal_amount: MINIMUM_WITHDRAWAL_AMOUNT,
                minimum_deposit_amount: MINIMUM_DEPOSIT_AMOUNT,
                deposit_sol_required_cycles: DEPOSIT_SOL_REQUIRED_CYCLES,
                automatic_deposit_fee: Some(AUTOMATIC_DEPOSIT_FEE),
                pending_deposit_sol_request_guards: BTreeSet::new(),
                pending_deposit_spl_request_guards: BTreeSet::new(),
                pending_withdrawal_request_guards: BTreeSet::new(),
                pending_operating_top_up_guards: BTreeSet::new(),
                deposits: Deposits::default(),
                spl_deposits: SplDeposits::default(),
                supported_spl_tokens: BTreeMap::new(),
                pending_withdrawal_requests: BTreeMap::new(),
                created_withdrawal_requests: BTreeMap::new(),
                sent_withdrawal_requests: BTreeMap::new(),
                successful_withdrawal_requests: BTreeMap::new(),
                failed_withdrawal_requests: BTreeMap::new(),
                submitted_transactions: InsertionOrderedMap::new(),
                created_withdrawal_txs: BTreeMap::new(),
                succeeded_transactions: BTreeSet::new(),
                failed_transactions: InsertionOrderedMap::new(),
                nonce_pool: DurableNoncePool::default(),
                active_tasks: BTreeSet::new(),
                balance: 0,
                spl_sweep_reservations: BTreeMap::new(),
                operating_balance: 0,
                pending_spl_withdrawal_requests: BTreeMap::new(),
                created_spl_withdrawal_requests: BTreeMap::new(),
                created_spl_withdrawal_txs: BTreeMap::new(),
                sent_spl_withdrawal_requests: BTreeMap::new(),
                successful_spl_withdrawal_requests: BTreeMap::new(),
                failed_spl_withdrawal_requests: BTreeMap::new(),
                spl_withdrawal_token_burns: BTreeSet::new(),
                pending_spl_withdrawal_fee_reimbursements: BTreeMap::new(),
                completed_spl_withdrawal_fee_reimbursements: BTreeMap::new(),
                quarantined_spl_withdrawal_fee_reimbursements: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn should_fail_with_invalid_canister_ids() {
        fn assert_init_fails(args: InitArgs, check: impl Fn(&InvalidStateError) -> bool) {
            let err = State::try_from(args).unwrap_err();
            assert!(check(&err), "unexpected error: {err:?}");
        }
        // Anonymous sol_rpc_canister_id
        assert_init_fails(
            InitArgs {
                sol_rpc_canister_id: Principal::anonymous(),
                ..valid_init_args()
            },
            |e| matches!(e, InvalidStateError::InvalidCanisterId(_)),
        );
        // Anonymous ledger_canister_id
        assert_init_fails(
            InitArgs {
                ledger_canister_id: Principal::anonymous(),
                ..valid_init_args()
            },
            |e| matches!(e, InvalidStateError::InvalidCanisterId(_)),
        );
        // Identical canister IDs
        assert_init_fails(
            InitArgs {
                sol_rpc_canister_id: sol_rpc_canister_id(),
                ledger_canister_id: sol_rpc_canister_id(),
                ..valid_init_args()
            },
            |e| matches!(e, InvalidStateError::InvalidCanisterId(_)),
        );
    }
}

mod state_upgrade {
    use super::*;

    fn initial_state() -> State {
        State::try_from(valid_init_args()).unwrap()
    }

    #[test]
    fn should_update_fields() {
        let new_canister_id = Principal::from_slice(&[3_u8; 20]);
        let new_minimum_deposit_amount = MINIMUM_DEPOSIT_AMOUNT * 2;
        let new_minimum_withdrawal_amount = MINIMUM_WITHDRAWAL_AMOUNT * 2;
        let new_withdrawal_fee = WITHDRAWAL_FEE / 2;
        let new_deposit_sol_required_cycles = (DEPOSIT_SOL_REQUIRED_CYCLES * 2) as u64;
        let new_automatic_deposit_fee = AUTOMATIC_DEPOSIT_FEE * 2;

        let mut state = initial_state();
        state
            .upgrade(UpgradeArgs {
                sol_rpc_canister_id: Some(new_canister_id),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(state.sol_rpc_canister_id(), new_canister_id);

        let mut state = initial_state();
        state
            .upgrade(UpgradeArgs {
                ..Default::default()
            })
            .unwrap();

        let mut state = initial_state();
        state
            .upgrade(UpgradeArgs {
                minimum_deposit_amount: Some(new_minimum_deposit_amount),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(state.minimum_deposit_amount(), new_minimum_deposit_amount);

        let mut state = initial_state();
        state
            .upgrade(UpgradeArgs {
                minimum_withdrawal_amount: Some(new_minimum_withdrawal_amount),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            state.minimum_withdrawal_amount(),
            new_minimum_withdrawal_amount
        );

        let mut state = initial_state();
        state
            .upgrade(UpgradeArgs {
                withdrawal_fee: Some(new_withdrawal_fee),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(state.withdrawal_fee(), new_withdrawal_fee);

        let mut state = initial_state();
        state
            .upgrade(UpgradeArgs {
                deposit_sol_required_cycles: Some(new_deposit_sol_required_cycles),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            state.deposit_sol_required_cycles(),
            new_deposit_sol_required_cycles as u128
        );

        let mut state = initial_state();
        state
            .upgrade(UpgradeArgs {
                automatic_deposit_fee: Some(new_automatic_deposit_fee),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            state.automatic_deposit_fee(),
            Some(new_automatic_deposit_fee)
        );
    }

    #[test]
    fn should_keep_the_automatic_deposit_fee_when_the_upgrade_omits_it() {
        let mut state = State::try_from(InitArgs {
            automatic_deposit_fee: None,
            ..valid_init_args()
        })
        .unwrap();
        assert_eq!(state.automatic_deposit_fee(), None);

        state
            .upgrade(UpgradeArgs {
                automatic_deposit_fee: Some(AUTOMATIC_DEPOSIT_FEE),
                ..Default::default()
            })
            .unwrap();
        state.upgrade(UpgradeArgs::default()).unwrap();

        assert_eq!(state.automatic_deposit_fee(), Some(AUTOMATIC_DEPOSIT_FEE));
    }

    #[test]
    fn should_fail_when_sol_rpc_canister_id_is_anonymous() {
        assert_matches!(
            initial_state().upgrade(UpgradeArgs {
                sol_rpc_canister_id: Some(Principal::anonymous()),
                ..Default::default()
            }),
            Err(InvalidStateError::InvalidCanisterId(_))
        );
    }

    // This test ensures the canister state is rolled back after a failed upgrade
    #[test]
    #[should_panic = "InvalidMinimumDepositAmount"]
    fn should_panic_when_upgrade_fails() {
        let mut state = initial_state();
        let new_minimum_deposit_amount = 1;

        process_event(
            &mut state,
            EventType::Upgrade(UpgradeArgs {
                minimum_deposit_amount: Some(new_minimum_deposit_amount),
                ..Default::default()
            }),
            &TestCanisterRuntime::new().add_times([0, 0]),
        );
    }
}

#[test]
fn should_track_balance_through_deposits_withdrawals_and_failures() {
    const DEPOSIT_1: u64 = 500_000_000;
    const DEPOSIT_2: u64 = 300_000_000;
    const DEPOSIT_3: u64 = 200_000_000;
    const WITHDRAWAL_1: u64 = 50_000_000;
    const WITHDRAWAL_2: u64 = 80_000_000;
    const TRANSFER_1: u64 = WITHDRAWAL_1 - WITHDRAWAL_FEE;
    const TRANSFER_2: u64 = WITHDRAWAL_2 - WITHDRAWAL_FEE;

    init_state();
    init_schnorr_master_key();
    assert_eq!(read_state(|s| s.balance()), 0);

    // Queueing and sweeping deposits does not change the balance
    queue_deposit(deposit_id(0), account(1), DEPOSIT_1);
    queue_deposit(deposit_id(1), account(2), DEPOSIT_2);
    submit_sweep(signature(0xAA), vec![deposit_id(0), deposit_id(1)]);
    assert_eq!(read_state(|s| s.balance()), 0);

    // A finalized sweep does not change the balance until it is credited
    succeed_transaction(signature(0xAA));
    assert_eq!(read_state(|s| s.balance()), 0);

    // Crediting the sweep adds the amount received: balance += total_deposits - tx_fee(2 signers)
    let expected = DEPOSIT_1 + DEPOSIT_2 - 2 * FEE_PER_SIGNATURE;
    credit_sweep(signature(0xAA), expected);
    assert_eq!(read_state(|s| s.balance()), expected);

    // Accepting withdrawals does not change the balance
    accept_withdrawal(account(3), 0, WITHDRAWAL_1);
    accept_withdrawal(account(4), 1, WITHDRAWAL_2);
    assert_eq!(read_state(|s| s.balance()), expected);

    // Creating a withdrawal transaction (1 signature): balance -= total_transfers + tx_fee
    submit_withdrawal(signature(0xBB), vec![0, 1]);
    let expected = expected - TRANSFER_1 - TRANSFER_2 - FEE_PER_SIGNATURE;
    assert_eq!(read_state(|s| s.balance()), expected);

    // Finalizing a withdrawal does not change the balance
    succeed_transaction(signature(0xBB));
    assert_eq!(read_state(|s| s.balance()), expected);

    // A failed sweep does not credit the balance
    queue_deposit(deposit_id(2), account(5), DEPOSIT_3);
    submit_sweep(signature(0xCC), vec![deposit_id(2)]);
    fail_transaction(signature(0xCC));
    assert_eq!(read_state(|s| s.balance()), expected);
}

mod oldest_incomplete_withdrawal_created_at {
    use super::*;

    const AMOUNT: u64 = 50_000_000;

    #[test]
    fn should_be_none_when_no_incomplete_withdrawals() {
        init_state();
        assert_eq!(
            read_state(|s| s.oldest_incomplete_withdrawal_created_at()),
            None
        );
    }

    #[test]
    fn should_return_timestamp_of_single_pending_withdrawal() {
        init_state();
        init_balance();
        accept_withdrawal_at(account(1), 0, AMOUNT, 1_000_000_000);
        assert_eq!(
            read_state(|s| s.oldest_incomplete_withdrawal_created_at()),
            Some(1_000_000_000)
        );
    }

    #[test]
    fn should_return_oldest_timestamp_with_multiple_pending_withdrawals() {
        init_state();
        init_balance();
        accept_withdrawal_at(account(1), 0, AMOUNT, 1_000_000_000);
        accept_withdrawal_at(account(2), 1, AMOUNT, 2_000_000_000);
        accept_withdrawal_at(account(3), 2, AMOUNT, 3_000_000_000);
        assert_eq!(
            read_state(|s| s.oldest_incomplete_withdrawal_created_at()),
            Some(1_000_000_000)
        );
    }

    #[test]
    fn should_persist_through_submission() {
        init_state();
        init_balance();
        accept_withdrawal_at(account(1), 0, AMOUNT, 1_000_000_000);
        accept_withdrawal_at(account(2), 1, AMOUNT, 2_000_000_000);

        submit_withdrawal(signature(0xAA), vec![0, 1]);

        // Both withdrawals are now sent but still incomplete
        assert_eq!(
            read_state(|s| s.oldest_incomplete_withdrawal_created_at()),
            Some(1_000_000_000)
        );
    }

    #[test]
    fn should_update_when_oldest_withdrawal_succeeds() {
        init_state();
        init_balance();
        accept_withdrawal_at(account(1), 0, AMOUNT, 1_000_000_000);
        accept_withdrawal_at(account(2), 1, AMOUNT, 2_000_000_000);

        submit_withdrawal(signature(0xAA), vec![0]);
        submit_withdrawal(signature(0xBB), vec![1]);
        succeed_transaction(signature(0xAA));

        assert_eq!(
            read_state(|s| s.oldest_incomplete_withdrawal_created_at()),
            Some(2_000_000_000)
        );
    }

    #[test]
    fn should_update_when_oldest_withdrawal_fails() {
        init_state();
        init_balance();
        accept_withdrawal_at(account(1), 0, AMOUNT, 1_000_000_000);
        accept_withdrawal_at(account(2), 1, AMOUNT, 2_000_000_000);

        submit_withdrawal(signature(0xAA), vec![0]);
        submit_withdrawal(signature(0xBB), vec![1]);
        fail_transaction(signature(0xAA));

        assert_eq!(
            read_state(|s| s.oldest_incomplete_withdrawal_created_at()),
            Some(2_000_000_000)
        );
    }

    #[test]
    fn should_be_none_when_all_withdrawals_are_finalized() {
        init_state();
        init_balance();
        accept_withdrawal_at(account(1), 0, AMOUNT, 1_000_000_000);
        accept_withdrawal_at(account(2), 1, AMOUNT, 2_000_000_000);

        submit_withdrawal(signature(0xAA), vec![0, 1]);
        succeed_transaction(signature(0xAA));

        assert_eq!(
            read_state(|s| s.oldest_incomplete_withdrawal_created_at()),
            None
        );
    }
}

mod withdrawal_batches {
    use super::*;
    use crate::{
        sol_transfer::{BATCH_WITHDRAWAL_TX_FEE, MAX_WITHDRAWALS_PER_NONCE_TX},
        test_fixtures::durable_nonce,
    };

    const MAX_AMOUNT_TO_TRANSFER: Lamport =
        u64::MAX - WITHDRAWAL_FEE - BATCH_WITHDRAWAL_TX_FEE - RENT_EXEMPTION_THRESHOLD;
    const NUM_REQUESTS_FOR_TWO_BATCHES: usize = MAX_WITHDRAWALS_PER_NONCE_TX + 1;
    const COST_OF_TWO_BATCHES: Lamport = NUM_REQUESTS_FOR_TWO_BATCHES as u64
        * MINIMUM_WITHDRAWAL_AMOUNT
        + 2 * BATCH_WITHDRAWAL_TX_FEE
        + RENT_EXEMPTION_THRESHOLD;

    #[test]
    fn should_be_empty_when_no_pending_withdrawals() {
        let state = state();

        assert_eq!(state.withdrawal_batches().next(), None);
    }

    proptest! {
        #[test]
        fn should_batch_single_request_when_balance_covers_amount_and_fee(
            amount_to_transfer in MINIMUM_WITHDRAWAL_AMOUNT..=MAX_AMOUNT_TO_TRANSFER
        ) {
            let mut state = state();
            state.balance = amount_to_transfer + BATCH_WITHDRAWAL_TX_FEE + RENT_EXEMPTION_THRESHOLD;
            let requests = [withdrawal_request(0, amount_to_transfer)];
            accept_withdrawal_requests(&mut state, requests.clone());

            let batches: Vec<_> = state.withdrawal_batches().collect();

            prop_assert_eq!(batches, vec![vec![requests[0].clone()]]);
        }

        #[test]
        fn should_be_empty_when_balance_does_not_cover_amount_and_fee(
            amount_to_transfer in MINIMUM_WITHDRAWAL_AMOUNT..=MAX_AMOUNT_TO_TRANSFER,
            shortfall in 1..=MINIMUM_WITHDRAWAL_AMOUNT + BATCH_WITHDRAWAL_TX_FEE
        ) {
            let mut state = state();
            state.balance = amount_to_transfer + BATCH_WITHDRAWAL_TX_FEE + RENT_EXEMPTION_THRESHOLD
                - shortfall;
            let requests = [withdrawal_request(0, amount_to_transfer)];
            accept_withdrawal_requests(&mut state, requests);

            prop_assert_eq!(state.withdrawal_batches().next(), None);
        }

        #[test]
        fn should_split_requests_into_batches_of_max_size_when_balance_covers_both_fees(
            balance in COST_OF_TWO_BATCHES..=u64::MAX
        ) {
            let mut state = state();
            state.balance = balance;
            let requests = withdrawal_requests(NUM_REQUESTS_FOR_TWO_BATCHES);
            accept_withdrawal_requests(&mut state, requests.clone());

            let batches: Vec<_> = state.withdrawal_batches().collect();

            prop_assert_eq!(
                batches,
                vec![
                    requests[..MAX_WITHDRAWALS_PER_NONCE_TX].to_vec(),
                    requests[MAX_WITHDRAWALS_PER_NONCE_TX..].to_vec()
                ]
            );
        }

        #[test]
        fn should_not_start_second_batch_when_balance_does_not_cover_its_amount_and_fee(
            shortfall in 1..=MINIMUM_WITHDRAWAL_AMOUNT + BATCH_WITHDRAWAL_TX_FEE
        ) {
            let mut state = state();
            state.balance = COST_OF_TWO_BATCHES - shortfall;
            let requests = withdrawal_requests(NUM_REQUESTS_FOR_TWO_BATCHES);
            accept_withdrawal_requests(&mut state, requests.clone());

            let batches: Vec<_> = state.withdrawal_batches().collect();

            prop_assert_eq!(batches, vec![requests[..MAX_WITHDRAWALS_PER_NONCE_TX].to_vec()]);
        }
    }

    #[test]
    fn should_hold_back_the_rent_exemption_threshold() {
        let mut state = state();
        let requests = [withdrawal_request(0, MINIMUM_WITHDRAWAL_AMOUNT)];
        accept_withdrawal_requests(&mut state, requests.clone());
        state.balance =
            MINIMUM_WITHDRAWAL_AMOUNT + BATCH_WITHDRAWAL_TX_FEE + RENT_EXEMPTION_THRESHOLD - 1;

        assert_eq!(state.withdrawal_batches().next(), None);

        state.balance += 1;

        assert_eq!(state.withdrawal_batches().next(), Some(requests.to_vec()));
    }

    #[test]
    fn should_be_empty_when_cost_overflows() {
        let mut state = state();
        state.balance = u64::MAX;
        let request = WithdrawalRequest {
            amount_to_transfer: u64::MAX,
            burned_amount: u64::MAX,
            ..withdrawal_request(0, MINIMUM_WITHDRAWAL_AMOUNT)
        };
        accept_withdrawal_requests(&mut state, [request]);

        assert_eq!(state.withdrawal_batches().next(), None);
    }

    #[test]
    fn should_stop_at_first_unaffordable_request_without_skipping_it() {
        let mut state = state();
        state.balance =
            2 * MINIMUM_WITHDRAWAL_AMOUNT + BATCH_WITHDRAWAL_TX_FEE + RENT_EXEMPTION_THRESHOLD;
        let requests = [
            withdrawal_request(0, MINIMUM_WITHDRAWAL_AMOUNT),
            withdrawal_request(1, 2 * MINIMUM_WITHDRAWAL_AMOUNT),
            withdrawal_request(2, MINIMUM_WITHDRAWAL_AMOUNT),
        ];
        accept_withdrawal_requests(&mut state, requests.clone());

        let batches: Vec<_> = state.withdrawal_batches().collect();

        assert_eq!(batches, vec![vec![requests[0].clone()]]);
    }

    #[test]
    fn should_not_create_a_transaction_without_pending_withdrawals() {
        let mut state = state();
        state.balance = u64::MAX;
        state.nonce_pool.add_accounts([address(1)]).unwrap();

        assert!(!state.can_create_withdrawal_transaction());
    }

    #[test]
    fn should_not_create_a_transaction_without_a_free_nonce_account() {
        let mut state = state();
        state.balance = u64::MAX;
        accept_withdrawal_requests(
            &mut state,
            [withdrawal_request(0, MINIMUM_WITHDRAWAL_AMOUNT)],
        );
        state.nonce_pool.add_accounts([address(1)]).unwrap();
        state.nonce_pool.bind(&address(1), durable_nonce(1));

        assert!(!state.can_create_withdrawal_transaction());
    }

    #[test]
    fn should_not_create_a_transaction_without_an_affordable_batch() {
        let mut state = state();
        state.balance =
            MINIMUM_WITHDRAWAL_AMOUNT + BATCH_WITHDRAWAL_TX_FEE + RENT_EXEMPTION_THRESHOLD - 1;
        accept_withdrawal_requests(
            &mut state,
            [withdrawal_request(0, MINIMUM_WITHDRAWAL_AMOUNT)],
        );
        state.nonce_pool.add_accounts([address(1)]).unwrap();

        assert!(!state.can_create_withdrawal_transaction());
    }

    #[test]
    fn should_create_a_transaction_with_an_affordable_batch_and_a_free_nonce_account() {
        let mut state = state();
        state.balance =
            MINIMUM_WITHDRAWAL_AMOUNT + BATCH_WITHDRAWAL_TX_FEE + RENT_EXEMPTION_THRESHOLD;
        accept_withdrawal_requests(
            &mut state,
            [withdrawal_request(0, MINIMUM_WITHDRAWAL_AMOUNT)],
        );
        state.nonce_pool.add_accounts([address(1)]).unwrap();

        assert!(state.can_create_withdrawal_transaction());
    }

    fn state() -> State {
        State::try_from(valid_init_args()).unwrap()
    }

    fn accept_withdrawal_requests<I: IntoIterator<Item = WithdrawalRequest>>(
        state: &mut State,
        requests: I,
    ) {
        for (index, request) in requests.into_iter().enumerate() {
            state.process_accepted_withdrawal(&request, index as u64);
        }
    }

    fn withdrawal_requests(count: usize) -> Vec<WithdrawalRequest> {
        (0..count)
            .map(|burn_index| withdrawal_request(burn_index, MINIMUM_WITHDRAWAL_AMOUNT))
            .collect()
    }

    fn withdrawal_request(burn_index: usize, amount_to_transfer: Lamport) -> WithdrawalRequest {
        WithdrawalRequest {
            account: account(burn_index),
            solana_address: [0u8; 32],
            burn_block_index: LedgerBurnIndex::from(burn_index as u64),
            amount_to_transfer,
            burned_amount: amount_to_transfer + WITHDRAWAL_FEE,
        }
    }
}

mod withdrawal_transactions {
    use super::*;
    use crate::{
        sol_transfer::BATCH_WITHDRAWAL_TX_FEE,
        state::audit::replay_events,
        test_fixtures::{
            MINTER_ADDRESS, NONCE_ACCOUNT, durable_nonce,
            events::{create_withdrawal_batch_transaction, submit_withdrawal_batch_transaction},
            minter_public_key_fetched_event, queued_deposit_of, sweep_message,
            withdrawal_batch_message,
        },
    };
    use cksol_types::{TxFinalizedStatus, WithdrawSolStatus};

    const AMOUNT_TO_TRANSFER: u64 = MINIMUM_WITHDRAWAL_AMOUNT - WITHDRAWAL_FEE;

    #[test]
    fn should_debit_the_balance_and_bucket_the_requests_at_creation() {
        init_state();
        init_balance();
        let balance_before = read_state(|s| s.balance());
        accept_withdrawal(account(1), 0, MINIMUM_WITHDRAWAL_AMOUNT);

        create_withdrawal_batch_transaction(durable_nonce(1), vec![0]);

        read_state(|s| {
            assert_eq!(
                s.balance(),
                balance_before - AMOUNT_TO_TRANSFER - BATCH_WITHDRAWAL_TX_FEE
            );
            assert!(s.pending_withdrawal_requests().is_empty());
            assert!(s.created_withdrawal_requests().contains_key(&0_u64.into()));
            assert!(s.created_withdrawal_txs().contains_key(&NONCE_ACCOUNT));
            assert_eq!(s.withdrawal_status(0), WithdrawSolStatus::Pending);
        });
    }

    #[test]
    fn should_move_the_created_transaction_in_flight_once_submitted() {
        init_state();
        init_balance();
        accept_withdrawal(account(1), 0, MINIMUM_WITHDRAWAL_AMOUNT);
        create_withdrawal_batch_transaction(durable_nonce(1), vec![0]);
        let balance_after_creation = read_state(|s| s.balance());

        submit_withdrawal_batch_transaction(signature(7), durable_nonce(1), vec![0]);

        read_state(|s| {
            assert_eq!(s.balance(), balance_after_creation);
            assert!(s.created_withdrawal_txs().is_empty());
            assert!(s.created_withdrawal_requests().is_empty());
            let transaction = s.submitted_transactions().get(&signature(7)).unwrap();
            let MinterTransaction::Withdrawal {
                nonce_account,
                nonce_value,
                ..
            } = transaction
            else {
                panic!("expected a withdrawal transaction, got {transaction:?}");
            };
            assert_eq!(*nonce_account, NONCE_ACCOUNT);
            assert_eq!(*nonce_value, durable_nonce(1));
            assert_eq!(
                s.withdrawal_status(0),
                WithdrawSolStatus::TxSent {
                    transaction_id: signature(7).into()
                }
            );
        });
    }

    #[test]
    fn should_free_the_nonce_account_and_settle_the_request_on_success() {
        init_state();
        init_balance();
        accept_withdrawal(account(1), 0, MINIMUM_WITHDRAWAL_AMOUNT);
        create_withdrawal_batch_transaction(durable_nonce(1), vec![0]);
        submit_withdrawal_batch_transaction(signature(7), durable_nonce(1), vec![0]);

        succeed_transaction(signature(7));

        read_state(|s| {
            assert!(s.submitted_transactions().is_empty());
            assert_eq!(
                s.withdrawal_status(0),
                WithdrawSolStatus::TxFinalized(TxFinalizedStatus::Success {
                    transaction_id: signature(7).into(),
                })
            );
        });
        assert_nonce_account_free();
    }

    #[test]
    fn should_free_the_nonce_account_and_settle_the_request_on_failure() {
        init_state();
        init_balance();
        accept_withdrawal(account(1), 0, MINIMUM_WITHDRAWAL_AMOUNT);
        create_withdrawal_batch_transaction(durable_nonce(1), vec![0]);
        submit_withdrawal_batch_transaction(signature(7), durable_nonce(1), vec![0]);

        fail_transaction(signature(7));

        read_state(|s| {
            assert!(s.submitted_transactions().is_empty());
            assert_matches!(
                s.failed_transactions().get(&signature(7)),
                Some(MinterTransaction::Withdrawal { .. })
            );
            assert_eq!(
                s.withdrawal_status(0),
                WithdrawSolStatus::TxFinalized(TxFinalizedStatus::Failure {
                    transaction_id: signature(7).into(),
                })
            );
        });
        assert_nonce_account_free();
    }

    #[test]
    fn should_rebuild_the_bound_pool_and_created_bucket_from_a_log_ending_after_creation() {
        let replayed = replay_events(log_of(funded_log_until_created_transaction()));

        assert_eq!(
            replayed.balance(),
            DEPOSIT_AMOUNT - FEE_PER_SIGNATURE - AMOUNT_TO_TRANSFER - BATCH_WITHDRAWAL_TX_FEE
        );
        assert!(
            replayed
                .created_withdrawal_requests()
                .contains_key(&0_u64.into())
        );
        assert_eq!(
            replayed
                .created_withdrawal_txs()
                .get(&NONCE_ACCOUNT)
                .map(|tx| tx.nonce_value),
            Some(durable_nonce(1))
        );
        assert_eq!(replayed.withdrawal_status(0), WithdrawSolStatus::Pending);
        assert_eq!(replayed.nonce_pool().num_free_accounts(), 0);
    }

    #[test]
    fn should_rebuild_the_in_flight_map_from_a_log_ending_after_submission() {
        let mut events = funded_log_until_created_transaction();
        events.push(EventType::SubmittedTransaction {
            signature: signature(7),
            message: withdrawal_batch_message(
                NONCE_ACCOUNT,
                durable_nonce(1),
                &[(Address::from([0u8; 32]), AMOUNT_TO_TRANSFER)],
            )
            .into(),
            signers: vec![Signer::Minter],
            purpose: TransactionPurpose::Withdrawal {
                burn_indices: vec![0_u64.into()],
            },
        });

        let replayed = replay_events(log_of(events));

        assert!(replayed.created_withdrawal_txs().is_empty());
        let transaction = replayed
            .submitted_transactions()
            .get(&signature(7))
            .unwrap();
        let MinterTransaction::Withdrawal {
            nonce_account,
            nonce_value,
            ..
        } = transaction
        else {
            panic!("expected a withdrawal transaction, got {transaction:?}");
        };
        assert_eq!(*nonce_account, NONCE_ACCOUNT);
        assert_eq!(*nonce_value, durable_nonce(1));
        assert_eq!(
            replayed.withdrawal_status(0),
            WithdrawSolStatus::TxSent {
                transaction_id: signature(7).into()
            }
        );
    }

    #[test]
    #[should_panic(expected = "does not start with an AdvanceNonceAccount instruction")]
    fn should_panic_when_a_nonce_withdrawal_does_not_advance_a_nonce_account() {
        init_state();
        init_balance();
        accept_withdrawal(account(1), 0, MINIMUM_WITHDRAWAL_AMOUNT);
        create_withdrawal_batch_transaction(durable_nonce(1), vec![0]);
        let message_without_nonce_advance = solana_message::Message::new_with_blockhash(
            &[solana_system_interface::instruction::transfer(
                &MINTER_ADDRESS,
                &NONCE_ACCOUNT,
                AMOUNT_TO_TRANSFER,
            )],
            Some(&MINTER_ADDRESS),
            &durable_nonce(1),
        );

        mutate_state(|s| {
            s.process_transaction_submitted(
                &signature(7),
                &message_without_nonce_advance.into(),
                &[Signer::Minter],
                &TransactionPurpose::Withdrawal {
                    burn_indices: vec![0_u64.into()],
                },
                0,
            )
        });
    }

    #[test]
    #[should_panic(expected = "does not carry the message bound to nonce account")]
    fn should_panic_when_a_nonce_withdrawal_differs_from_the_bound_message() {
        init_state();
        init_balance();
        accept_withdrawal(account(1), 0, MINIMUM_WITHDRAWAL_AMOUNT);
        create_withdrawal_batch_transaction(durable_nonce(1), vec![0]);
        let message_with_another_amount = withdrawal_batch_message(
            NONCE_ACCOUNT,
            durable_nonce(1),
            &[(Address::from([0u8; 32]), AMOUNT_TO_TRANSFER + 1)],
        );

        mutate_state(|s| {
            s.process_transaction_submitted(
                &signature(7),
                &message_with_another_amount.into(),
                &[Signer::Minter],
                &TransactionPurpose::Withdrawal {
                    burn_indices: vec![0_u64.into()],
                },
                0,
            )
        });
    }

    #[test]
    #[should_panic(expected = "does not serve the withdrawal requests bound to nonce account")]
    fn should_panic_when_a_nonce_withdrawal_serves_other_withdrawal_requests() {
        init_state();
        init_balance();
        accept_withdrawal(account(1), 0, MINIMUM_WITHDRAWAL_AMOUNT);
        accept_withdrawal(account(2), 1, MINIMUM_WITHDRAWAL_AMOUNT);
        create_withdrawal_batch_transaction(durable_nonce(1), vec![0, 1]);

        submit_withdrawal_batch_transaction(signature(7), durable_nonce(1), vec![0]);
    }

    const DEPOSIT_AMOUNT: u64 = 500_000_000;

    fn funded_log_until_created_transaction() -> Vec<EventType> {
        let credited_amount = DEPOSIT_AMOUNT - FEE_PER_SIGNATURE;
        let funding_deposit = queued_deposit_of(account(9), credited_amount);
        vec![
            minter_public_key_fetched_event(),
            EventType::QueuedDeposit {
                deposit_id: deposit_id(0),
                account: funding_deposit.account,
                address: funding_deposit.address,
                balance: funding_deposit.balance,
            },
            EventType::SubmittedTransaction {
                signature: signature(9),
                message: sweep_message([(deposit_id(0), funding_deposit)]),
                signers: vec![Signer::Account(funding_deposit.account)],
                purpose: TransactionPurpose::SweepDeposit {
                    deposit_ids: vec![deposit_id(0)],
                    block_height: BlockHeight::new(0),
                },
            },
            EventType::SucceededTransaction {
                signature: signature(9),
            },
            EventType::CreditedSweep {
                signature: signature(9),
                amount_received: credited_amount,
                mints: vec![CreditedDeposit {
                    deposit_id: deposit_id(0),
                    amount_to_mint: credited_amount,
                }],
            },
            EventType::MintedSweptDeposit {
                deposit_id: deposit_id(0),
                mint_block_index: 9_u64.into(),
            },
            EventType::AcceptedWithdrawalRequest(WithdrawalRequest {
                account: account(1),
                solana_address: [0u8; 32],
                burn_block_index: 0_u64.into(),
                burned_amount: MINIMUM_WITHDRAWAL_AMOUNT,
                amount_to_transfer: AMOUNT_TO_TRANSFER,
            }),
            EventType::CreatedWithdrawalTransaction {
                burn_indices: vec![0_u64.into()],
                nonce_account: NONCE_ACCOUNT,
                nonce_value: durable_nonce(1),
            },
        ]
    }

    fn log_of(events: Vec<EventType>) -> Vec<Event> {
        std::iter::once(EventType::Init(InitArgs {
            nonce_accounts: vec![NONCE_ACCOUNT.to_string()],
            ..valid_init_args()
        }))
        .chain(events)
        .map(|payload| Event {
            timestamp: 0,
            payload,
        })
        .collect()
    }

    fn assert_nonce_account_free() {
        read_state(|s| {
            assert_eq!(
                s.nonce_pool().free_accounts().collect::<Vec<_>>(),
                vec![&NONCE_ACCOUNT]
            )
        });
    }
}

mod spl_withdrawals {
    use crate::{
        constants::FEE_PER_SIGNATURE,
        lifecycle,
        numeric::LedgerBurnIndex,
        spl_transfer::{build_spl_withdrawal_instructions, build_spl_withdrawal_message},
        state::{
            CompletedSplWithdrawalFeeReimbursement, MinterTransaction,
            PendingSplWithdrawalFeeReimbursement, PendingSplWithdrawalRequest, State, TokenProgram,
            audit::{process_event, replay_events},
            event::{
                Event, EventType, Signer, SplWithdrawalFeeReimbursementRequest,
                SplWithdrawalRequest, SplWithdrawalRequestTerms, TransactionPurpose,
                VersionedMessage, WithdrawalRequest,
            },
            mutate_state, read_state, reset_state,
        },
        storage::{reset_events, with_event_iter},
        test_fixtures::{
            MINTER_ADDRESS, NONCE_ACCOUNT, account, init_args_with_nonce_account, init_balance_to,
            minter_public_key_fetched_event, minter_signature, runtime::TestCanisterRuntime,
            spl_token, valid_init_args,
        },
    };
    use candid::Principal;
    use cksol_types_internal::InitArgs;
    use ic_stable_structures::Storable;
    use sol_rpc_types::Lamport;
    use solana_hash::Hash;
    use solana_transaction::Message;
    use std::{
        collections::BTreeMap,
        panic::{AssertUnwindSafe, catch_unwind},
    };

    fn request(mint_byte: u8) -> SplWithdrawalRequestTerms {
        let token = spl_token([mint_byte; 32].into(), TokenProgram::Classic);
        SplWithdrawalRequestTerms {
            account: account(1),
            mint: token.mint,
            ledger_id: token.ledger_id,
            address: [9; 32].into(),
            amount: 1_000_000,
            fee: 1_000_000,
            create_destination_account: false,
        }
    }

    fn record(payload: EventType, timestamp: u64) {
        let runtime = TestCanisterRuntime::new().with_time(timestamp);
        mutate_state(|state| {
            // Failed IC message execution rolls state changes back. Apply to a copy
            // so caught assertion failures in these tests model that behavior.
            let mut updated = state.clone();
            process_event(&mut updated, payload, &runtime);
            *state = updated;
        });
    }

    fn setup() -> SplWithdrawalRequestTerms {
        setup_with(valid_init_args())
    }

    fn setup_with(init_args: InitArgs) -> SplWithdrawalRequestTerms {
        lifecycle::init(init_args, TestCanisterRuntime::new().with_time(1));
        let request = request(2);
        record(
            EventType::AddedSplToken(spl_token(request.mint, TokenProgram::Classic)),
            2,
        );
        request
    }

    #[test]
    fn should_replay_a_completed_fee_reimbursement_and_preserve_other_pending_refunds() {
        let request = setup();
        for id in [100_u64, 101] {
            reimburse_fee(id, request.fee, request.account);
        }
        let completed = EventType::ReimbursedSplWithdrawalFee {
            cksol_burn_block_index: 100.into(),
            mint_block_index: 200.into(),
        };
        record(completed.clone(), 40);
        assert!(catch_unwind(AssertUnwindSafe(|| record(completed, 50))).is_err());
        assert!(
            catch_unwind(AssertUnwindSafe(|| reimburse_fee(
                100,
                request.fee,
                request.account
            )))
            .is_err()
        );
        assert!(catch_unwind(AssertUnwindSafe(|| accept(100, 7, &request))).is_err());
        let replayed = assert_replay_matches();
        assert_eq!(
            replayed.completed_spl_withdrawal_fee_reimbursements(),
            &BTreeMap::from([(
                100.into(),
                CompletedSplWithdrawalFeeReimbursement {
                    request: SplWithdrawalFeeReimbursementRequest {
                        cksol_burn_block_index: 100.into(),
                        reimbursed_amount: request.fee,
                        to: request.account,
                    },
                    mint_block_index: 200.into(),
                    created_at: 30,
                    reimbursed_at: 40,
                }
            )])
        );
        assert_eq!(
            replayed
                .pending_spl_withdrawal_fee_reimbursements()
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![101.into()]
        );
    }

    #[test]
    fn should_reject_fee_reimbursement_completion_without_a_pending_refund() {
        setup();
        for id in [100_u64, 101] {
            assert!(
                catch_unwind(AssertUnwindSafe(|| record(
                    EventType::ReimbursedSplWithdrawalFee {
                        cksol_burn_block_index: id.into(),
                        mint_block_index: 200.into(),
                    },
                    40
                )))
                .is_err()
            );
        }
        let replayed = assert_replay_matches();
        assert!(
            replayed
                .completed_spl_withdrawal_fee_reimbursements()
                .is_empty()
        );
    }

    #[test]
    fn should_replay_quarantine_and_keep_the_original_refund_out_of_the_pending_queue() {
        let request = setup();
        for id in [100_u64, 101] {
            reimburse_fee(id, request.fee, request.account);
        }
        let original = read_state(|state| {
            state.pending_spl_withdrawal_fee_reimbursements()[&100.into()].clone()
        });
        let quarantined = EventType::QuarantinedSplWithdrawalFeeReimbursement {
            cksol_burn_block_index: 100.into(),
        };
        record(quarantined.clone(), 40);
        assert!(catch_unwind(AssertUnwindSafe(|| record(quarantined, 50))).is_err());
        assert!(
            catch_unwind(AssertUnwindSafe(|| reimburse_fee(
                100,
                request.fee,
                request.account
            )))
            .is_err()
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| record(
                EventType::ReimbursedSplWithdrawalFee {
                    cksol_burn_block_index: 100.into(),
                    mint_block_index: 200.into(),
                },
                50
            )))
            .is_err()
        );
        let replayed = assert_replay_matches();
        assert_eq!(
            replayed.quarantined_spl_withdrawal_fee_reimbursements(),
            &BTreeMap::from([(100.into(), original)])
        );
        assert_eq!(
            replayed
                .pending_spl_withdrawal_fee_reimbursements()
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![101.into()]
        );
    }

    #[test]
    fn should_reject_quarantine_without_a_pending_refund() {
        setup();
        for id in [100_u64, 101] {
            assert!(
                catch_unwind(AssertUnwindSafe(|| record(
                    EventType::QuarantinedSplWithdrawalFeeReimbursement {
                        cksol_burn_block_index: id.into(),
                    },
                    40
                )))
                .is_err()
            );
        }
        assert!(
            assert_replay_matches()
                .quarantined_spl_withdrawal_fee_reimbursements()
                .is_empty()
        );
    }

    fn accepted_request(
        id: u64,
        token_burn: u64,
        terms: &SplWithdrawalRequestTerms,
    ) -> SplWithdrawalRequest {
        SplWithdrawalRequest {
            terms: terms.clone(),
            cksol_burn_block_index: id.into(),
            ckspl_burn_block_index: token_burn.into(),
        }
    }

    fn spl_transaction_created(id: u64) -> EventType {
        EventType::CreatedSplWithdrawalTransaction {
            cksol_burn_block_index: id.into(),
            nonce_account: NONCE_ACCOUNT,
            nonce_value: Hash::new_from_array([18; 32]),
        }
    }

    /// The message of the withdrawal bound to the test nonce account.
    fn bound_message(state: &State) -> Message {
        let created = &state.created_spl_withdrawal_txs()[&NONCE_ACCOUNT];
        state
            .bound_spl_withdrawal_message(&NONCE_ACCOUNT, created)
            .unwrap()
    }

    fn submitted_spl_withdrawal(id: u64) -> EventType {
        EventType::SubmittedTransaction {
            signature: minter_signature(),
            message: read_state(bound_message).into(),
            signers: vec![Signer::Minter],
            purpose: TransactionPurpose::SplWithdrawal {
                cksol_burn_block_index: id.into(),
            },
        }
    }

    fn setup_created_spl_withdrawal(
        token_program: TokenProgram,
        create_destination_account: bool,
    ) -> SplWithdrawalRequestTerms {
        let mut request = setup_with_nonce();
        let token = spl_token([4; 32].into(), token_program);
        record(EventType::AddedSplToken(token.clone()), 10);
        request.mint = token.mint;
        request.ledger_id = token.ledger_id;
        request.create_destination_account = create_destination_account;
        accept(100, 7, &request);
        record(spl_transaction_created(100), 30);
        request
    }

    #[test]
    fn should_replay_submitted_spl_withdrawal_and_keep_its_nonce_bound() {
        let request = setup_created_spl_withdrawal(TokenProgram::Classic, false);
        accept(101, 8, &request);
        let submitted = submitted_spl_withdrawal(100);
        record(submitted.clone(), 40);
        assert!(catch_unwind(AssertUnwindSafe(|| record(submitted, 41))).is_err());

        let replayed = assert_replay_matches();
        assert!(replayed.created_spl_withdrawal_txs().is_empty());
        assert!(replayed.created_spl_withdrawal_requests().is_empty());
        assert!(
            replayed
                .pending_spl_withdrawal_requests()
                .contains_key(&101.into())
        );
        let sent = &replayed.sent_spl_withdrawal_requests()[&100.into()];
        assert_eq!(sent.request, accepted_request(100, 7, &request));
        assert_eq!(sent.created_at, 20);
        assert_eq!(sent.signature, minter_signature());
        assert_eq!(replayed.nonce_pool().num_free_accounts(), 0);
        assert!(matches!(
            replayed.submitted_transactions().get(&sent.signature),
            Some(MinterTransaction::Withdrawal {
                nonce_account, nonce_value, submitted_at: 40, ..
            }) if *nonce_account == NONCE_ACCOUNT && *nonce_value == Hash::new_from_array([18; 32])
        ));
        assert!(
            catch_unwind(AssertUnwindSafe(|| record(
                EventType::ExpiredTransaction {
                    signature: sent.signature
                },
                50
            )))
            .is_err()
        );
        assert_replay_matches();
    }

    #[test]
    fn should_reject_spl_submission_that_does_not_match_the_binding() {
        setup_created_spl_withdrawal(TokenProgram::Token2022, true);
        let original = read_state(Clone::clone);
        for change in 0..5 {
            let mut submitted = submitted_spl_withdrawal(100);
            let EventType::SubmittedTransaction {
                message: VersionedMessage::Legacy(message),
                signers,
                purpose,
                ..
            } = &mut submitted
            else {
                unreachable!()
            };
            match change {
                0 => {
                    *purpose = TransactionPurpose::SplWithdrawal {
                        cksol_burn_block_index: 101.into(),
                    }
                }
                1 => message.recent_blockhash = Hash::new_from_array([19; 32]),
                2 => message.instructions.last_mut().unwrap().data.push(0),
                3 => signers.clear(),
                4 => {
                    message.instructions.remove(0);
                }
                _ => unreachable!(),
            }
            assert!(catch_unwind(AssertUnwindSafe(|| record(submitted, 40))).is_err());
            assert_eq!(assert_replay_matches(), original);
        }
    }

    #[test]
    fn should_move_a_succeeded_spl_withdrawal_to_successful() {
        for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
            reset_state();
            reset_events();
            let request = setup_created_spl_withdrawal(token_program, true);
            record(submitted_spl_withdrawal(100), 40);
            let balance_before = read_state(|state| state.balance());

            let replayed = finalize(EventType::SucceededTransaction {
                signature: minter_signature(),
            });

            assert_eq!(
                replayed.successful_spl_withdrawal_requests()[&100.into()].request,
                accepted_request(100, 7, &request)
            );
            assert!(replayed.failed_spl_withdrawal_requests().is_empty());
            assert_eq!(replayed.balance(), balance_before);
        }
    }

    #[test]
    fn should_return_the_unspent_rent_of_a_failed_spl_withdrawal() {
        for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
            for create_destination_account in [false, true] {
                reset_state();
                reset_events();
                let request =
                    setup_created_spl_withdrawal(token_program, create_destination_account);
                record(submitted_spl_withdrawal(100), 40);

                let replayed = finalize(EventType::FailedTransaction {
                    signature: minter_signature(),
                });

                assert_eq!(
                    replayed.failed_spl_withdrawal_requests()[&100.into()].request,
                    accepted_request(100, 7, &request)
                );
                assert!(replayed.successful_spl_withdrawal_requests().is_empty());
                assert_eq!(replayed.balance(), FUNDED_BALANCE - FEE_PER_SIGNATURE);
            }
        }
    }

    #[test]
    fn should_replay_the_actual_spend_of_a_successful_spl_withdrawal() {
        for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
            for spent_rent in [false, true] {
                reset_state();
                reset_events();
                setup_created_spl_withdrawal(token_program, true);
                record(submitted_spl_withdrawal(100), 40);
                let lamports_spent = FEE_PER_SIGNATURE
                    + if spent_rent {
                        token_program.account_rent_exemption()
                    } else {
                        0
                    };

                let replayed = finalize(EventType::SucceededSplWithdrawalTransaction {
                    signature: minter_signature(),
                    lamports_spent,
                });

                assert_eq!(replayed.balance(), FUNDED_BALANCE - lamports_spent);
                assert!(
                    replayed
                        .successful_spl_withdrawal_requests()
                        .contains_key(&100.into())
                );
            }
        }
    }

    #[test]
    fn should_reject_spl_success_with_spend_above_the_reservation() {
        setup_created_spl_withdrawal(TokenProgram::Classic, true);
        record(submitted_spl_withdrawal(100), 40);
        let original = read_state(Clone::clone);
        let reserved = original
            .spl_withdrawal_reserved_cost(&minter_signature())
            .unwrap();
        for lamports_spent in [reserved + 1, u64::MAX] {
            assert!(
                catch_unwind(AssertUnwindSafe(|| record(
                    EventType::SucceededSplWithdrawalTransaction {
                        signature: minter_signature(),
                        lamports_spent,
                    },
                    50,
                )))
                .is_err()
            );
            assert_eq!(assert_replay_matches(), original);
        }
    }

    #[test]
    fn should_not_reuse_the_index_of_a_finalized_spl_withdrawal() {
        let request = setup_created_spl_withdrawal(TokenProgram::Classic, false);
        record(submitted_spl_withdrawal(100), 40);
        finalize(EventType::SucceededTransaction {
            signature: minter_signature(),
        });
        assert!(catch_unwind(AssertUnwindSafe(|| accept(100, 8, &request))).is_err());
        assert!(
            catch_unwind(AssertUnwindSafe(|| reimburse_fee(
                100,
                request.fee,
                request.account
            )))
            .is_err()
        );
        assert_replay_matches();
    }

    /// Records the terminal event once, rejects a repeat, and checks the shared outcome:
    /// the request left the sent map and the nonce account is free again.
    fn finalize(terminal: EventType) -> State {
        record(terminal.clone(), 50);
        assert!(catch_unwind(AssertUnwindSafe(|| record(terminal, 51))).is_err());
        let replayed = assert_replay_matches();
        assert!(replayed.sent_spl_withdrawal_requests().is_empty());
        assert!(
            replayed
                .submitted_transactions()
                .get(&minter_signature())
                .is_none()
        );
        assert_eq!(replayed.nonce_pool().num_free_accounts(), 1);
        replayed
    }

    const FUNDED_BALANCE: Lamport = 500_000_000;

    /// Inits with a nonce account and funds the balance through a swept deposit.
    fn setup_with_nonce() -> SplWithdrawalRequestTerms {
        let request = setup_with(init_args_with_nonce_account());
        record(minter_public_key_fetched_event(), 3);
        init_balance_to(FUNDED_BALANCE);
        request
    }

    #[test]
    fn should_replay_spl_withdrawal_bound_to_nonce() {
        let request = setup_with_nonce();
        accept(100, 7, &request);
        accept(101, 8, &request);
        record(spl_transaction_created(100), 30);

        let replayed = assert_replay_matches();
        let nonce_account = NONCE_ACCOUNT;
        assert_eq!(replayed.pending_spl_withdrawal_requests().len(), 1);
        assert!(
            replayed
                .pending_spl_withdrawal_requests()
                .contains_key(&101.into())
        );
        let created = &replayed.created_spl_withdrawal_txs()[&nonce_account];
        assert_eq!(created.cksol_burn_block_index, 100.into());
        assert_eq!(replayed.created_spl_withdrawal_requests().len(), 1);
        let withdrawal =
            &replayed.created_spl_withdrawal_requests()[&created.cksol_burn_block_index];
        assert_eq!(withdrawal.request, accepted_request(100, 7, &request));
        assert_eq!(withdrawal.created_at, 20);
        assert_eq!(created.nonce_value, Hash::new_from_array([18; 32]));
        assert_eq!(replayed.nonce_pool().num_free_accounts(), 0);
        assert!(
            replayed
                .nonce_pool()
                .has_seen(&nonce_account, &created.nonce_value)
        );
    }

    #[test]
    fn should_rebuild_the_bound_spl_withdrawal_message_after_replay() {
        for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
            for create_destination_account in [false, true] {
                reset_state();
                reset_events();
                let mut request = setup_with_nonce();
                let mut token = spl_token([4; 32].into(), token_program);
                token.decimals = 9;
                record(EventType::AddedSplToken(token.clone()), 10);
                request.mint = token.mint;
                request.ledger_id = token.ledger_id;
                request.amount = 123_456_789;
                request.create_destination_account = create_destination_account;
                accept(100, 7, &request);
                record(spl_transaction_created(100), 30);
                let instructions = build_spl_withdrawal_instructions(
                    &MINTER_ADDRESS,
                    &token,
                    &request.address,
                    request.amount,
                    create_destination_account,
                );
                let expected = build_spl_withdrawal_message(
                    &MINTER_ADDRESS,
                    &NONCE_ACCOUNT,
                    Hash::new_from_array([18; 32]),
                    &instructions,
                )
                .unwrap();
                let message = read_state(bound_message);
                assert_eq!(message, expected);

                let replayed = assert_replay_matches();
                assert_eq!(bound_message(&replayed), message);
            }
        }
    }

    #[test]
    fn should_reserve_the_signature_fee_from_the_balance() {
        assert_reserves(false, FEE_PER_SIGNATURE);
    }

    #[test]
    fn should_reserve_the_destination_rent_with_the_signature_fee() {
        assert_reserves(
            true,
            FEE_PER_SIGNATURE + TokenProgram::Classic.account_rent_exemption(),
        );
    }

    fn assert_reserves(create_destination_account: bool, cost: Lamport) {
        let mut request = setup_with_nonce();
        request.create_destination_account = create_destination_account;
        accept(100, 7, &request);
        let balance_before = read_state(|state| state.balance());
        record(spl_transaction_created(100), 30);
        assert_eq!(assert_replay_matches().balance(), balance_before - cost);
    }

    #[test]
    fn should_reject_spl_binding_the_balance_cannot_pay() {
        let request = setup_with(init_args_with_nonce_account());
        accept(100, 7, &request);
        assert!(
            catch_unwind(AssertUnwindSafe(|| record(
                spl_transaction_created(100),
                30
            )))
            .is_err()
        );
        let replayed = assert_replay_matches();
        assert_eq!(replayed.balance(), 0);
        assert!(replayed.created_spl_withdrawal_txs().is_empty());
        assert_eq!(replayed.nonce_pool().num_free_accounts(), 1);
    }

    #[test]
    fn should_not_rebind_or_refund_a_created_spl_withdrawal() {
        let request = setup_with_nonce();
        accept(100, 7, &request);
        record(spl_transaction_created(100), 30);
        for event in [
            spl_transaction_created(100),
            EventType::AcceptedSplWithdrawalRequest(accepted_request(100, 8, &request)),
            EventType::FailedSplWithdrawalRequest(SplWithdrawalFeeReimbursementRequest {
                cksol_burn_block_index: 100.into(),
                reimbursed_amount: request.fee,
                to: request.account,
            }),
        ] {
            assert!(catch_unwind(AssertUnwindSafe(|| record(event, 40))).is_err());
            assert_eq!(
                assert_replay_matches().created_spl_withdrawal_txs().len(),
                1
            );
        }
    }

    #[test]
    fn should_reject_spl_binding_without_pending_request_or_available_nonce() {
        let request = setup_with_nonce();
        assert!(
            catch_unwind(AssertUnwindSafe(|| record(
                spl_transaction_created(100),
                30
            )))
            .is_err()
        );
        assert_eq!(assert_replay_matches().nonce_pool().num_free_accounts(), 1);
        accept(100, 7, &request);
        accept(101, 8, &request);
        record(spl_transaction_created(100), 30);
        assert!(
            catch_unwind(AssertUnwindSafe(|| record(
                spl_transaction_created(101),
                40
            )))
            .is_err()
        );
        let replayed = assert_replay_matches();
        assert!(
            replayed
                .pending_spl_withdrawal_requests()
                .contains_key(&101.into())
        );
        assert_eq!(replayed.created_spl_withdrawal_txs().len(), 1);
    }

    #[test]
    fn should_not_bind_a_sol_transaction_to_an_spl_nonce() {
        let request = setup_with_nonce();
        accept(100, 7, &request);
        record(spl_transaction_created(100), 30);
        record(
            EventType::AcceptedWithdrawalRequest(WithdrawalRequest {
                account: request.account,
                solana_address: request.address.to_bytes(),
                burn_block_index: 200.into(),
                burned_amount: 20_000,
                amount_to_transfer: 10_000,
            }),
            31,
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| record(
                EventType::CreatedWithdrawalTransaction {
                    burn_indices: vec![200.into()],
                    nonce_account: NONCE_ACCOUNT,
                    nonce_value: Hash::new_from_array([19; 32]),
                },
                40,
            )))
            .is_err()
        );
        let replayed = assert_replay_matches();
        assert!(
            replayed
                .pending_withdrawal_requests()
                .contains_key(&200.into())
        );
        assert_eq!(replayed.created_spl_withdrawal_txs().len(), 1);
        assert!(replayed.created_withdrawal_txs().is_empty());
    }

    fn accept(id: u64, token_burn: u64, terms: &SplWithdrawalRequestTerms) {
        record(
            EventType::AcceptedSplWithdrawalRequest(accepted_request(id, token_burn, terms)),
            20,
        );
    }

    fn reimburse_fee(id: u64, amount: u64, to: icrc_ledger_types::icrc1::account::Account) {
        record(
            EventType::FailedSplWithdrawalRequest(SplWithdrawalFeeReimbursementRequest {
                cksol_burn_block_index: id.into(),
                reimbursed_amount: amount,
                to,
            }),
            30,
        );
    }

    fn assert_replay_matches() -> State {
        let events = with_event_iter(|events| {
            events
                .map(|event| Event::from_bytes(event.to_bytes()))
                .collect::<Vec<_>>()
        });
        let replayed = replay_events(events);
        read_state(|state| assert_eq!(state, &replayed));
        replayed
    }

    #[test]
    fn should_replay_an_accepted_withdrawal_into_pending() {
        let request = setup();
        accept(100, 7, &request);
        let replayed = assert_replay_matches();
        assert_eq!(
            replayed
                .pending_spl_withdrawal_requests()
                .get(&LedgerBurnIndex::new(100)),
            Some(&PendingSplWithdrawalRequest {
                request: accepted_request(100, 7, &request),
                created_at: 20,
            })
        );
        assert!(
            replayed
                .pending_spl_withdrawal_fee_reimbursements()
                .is_empty()
        );
    }

    #[test]
    fn should_reject_a_zero_fee_reimbursement() {
        let request = setup();
        assert!(catch_unwind(AssertUnwindSafe(|| reimburse_fee(100, 0, request.account))).is_err());
        assert!(
            assert_replay_matches()
                .pending_spl_withdrawal_fee_reimbursements()
                .is_empty()
        );
    }

    #[test]
    fn should_reject_invalid_terms_on_acceptance() {
        let request = setup();
        let changes: [fn(&mut SplWithdrawalRequestTerms); 4] = [
            |r| r.mint = [3; 32].into(),
            |r| r.ledger_id = Principal::from_slice(&[99]),
            |r| r.amount = 0,
            |r| r.fee = 0,
        ];
        for change in changes {
            let mut changed = request.clone();
            change(&mut changed);
            assert!(catch_unwind(AssertUnwindSafe(|| accept(100, 7, &changed))).is_err());
            assert!(
                assert_replay_matches()
                    .pending_spl_withdrawal_requests()
                    .is_empty()
            );
        }
    }

    #[test]
    fn should_replay_an_older_log_without_spl_withdrawals() {
        setup();
        let replayed = assert_replay_matches();
        assert!(replayed.pending_spl_withdrawal_requests().is_empty());
        assert!(
            replayed
                .pending_spl_withdrawal_fee_reimbursements()
                .is_empty()
        );
    }

    #[test]
    fn should_replay_a_full_or_partial_fee_reimbursement_to_the_original_subaccount() {
        let mut request = setup();
        request.account.subaccount = Some([42; 32]);
        for (index, refund) in [1, 999_990, 1_000_000].into_iter().enumerate() {
            let burn_index = 100 + index as u64;
            reimburse_fee(burn_index, refund, request.account);

            let replayed = assert_replay_matches();
            assert!(replayed.pending_spl_withdrawal_requests().is_empty());
            assert_eq!(
                replayed.pending_spl_withdrawal_fee_reimbursements().len(),
                index + 1
            );
            assert_eq!(
                replayed
                    .pending_spl_withdrawal_fee_reimbursements()
                    .get(&burn_index.into()),
                Some(&PendingSplWithdrawalFeeReimbursement {
                    request: SplWithdrawalFeeReimbursementRequest {
                        cksol_burn_block_index: burn_index.into(),
                        reimbursed_amount: refund,
                        to: request.account,
                    },
                    created_at: 30,
                }),
            );
        }
    }

    #[test]
    fn should_not_reimburse_an_accepted_withdrawal() {
        let request = setup();
        accept(100, 7, &request);
        assert!(
            catch_unwind(AssertUnwindSafe(|| reimburse_fee(
                100,
                request.fee,
                request.account
            )))
            .is_err()
        );
        let replayed = assert_replay_matches();
        assert_eq!(replayed.pending_spl_withdrawal_requests().len(), 1);
        assert!(
            replayed
                .pending_spl_withdrawal_fee_reimbursements()
                .is_empty()
        );
    }

    #[test]
    fn should_not_accept_a_withdrawal_scheduled_for_fee_reimbursement() {
        let request = setup();
        reimburse_fee(100, request.fee, request.account);
        assert!(catch_unwind(AssertUnwindSafe(|| accept(100, 7, &request))).is_err());
        let replayed = assert_replay_matches();
        assert!(replayed.pending_spl_withdrawal_requests().is_empty());
        assert_eq!(
            replayed.pending_spl_withdrawal_fee_reimbursements().len(),
            1
        );
    }

    #[test]
    fn should_not_replace_an_existing_fee_reimbursement() {
        let request = setup();
        reimburse_fee(100, request.fee, request.account);
        assert!(
            catch_unwind(AssertUnwindSafe(|| reimburse_fee(
                100,
                request.fee - 1,
                request.account
            )))
            .is_err()
        );
        let replayed = assert_replay_matches();
        assert_eq!(
            replayed.pending_spl_withdrawal_fee_reimbursements()[&100.into()]
                .request
                .reimbursed_amount,
            request.fee
        );
    }

    #[test]
    fn should_only_move_the_failed_withdrawal_into_the_reimbursement_queue() {
        let request = setup();
        reimburse_fee(100, request.fee, request.account);
        accept(101, 7, &request);
        let replayed = assert_replay_matches();
        assert_eq!(replayed.pending_spl_withdrawal_requests().len(), 1);
        assert!(
            replayed
                .pending_spl_withdrawal_requests()
                .contains_key(&101.into())
        );
        assert_eq!(
            replayed.pending_spl_withdrawal_fee_reimbursements().len(),
            1
        );
        assert!(
            replayed
                .pending_spl_withdrawal_fee_reimbursements()
                .contains_key(&100.into())
        );
    }

    #[test]
    fn should_preserve_the_existing_top_up_event_encoding() {
        let bytes = hex::decode("8207821818820b0d").unwrap();
        let event = Event::from_bytes(bytes.as_slice().into());
        assert_eq!(
            event,
            Event {
                timestamp: 7,
                payload: EventType::ToppedUpOperatingBalance {
                    burn_block_index: LedgerBurnIndex::new(11),
                    amount: 13,
                },
            }
        );
        assert_eq!(event.to_bytes().as_ref(), bytes.as_slice());
    }

    #[test]
    #[should_panic(expected = "already accepted SPL withdrawal request")]
    fn should_reject_accepting_a_withdrawal_twice() {
        let request = setup();
        accept(100, 7, &request);
        accept(100, 8, &request);
    }

    #[test]
    #[should_panic(expected = "already belongs to a withdrawal")]
    fn should_reject_reusing_a_token_burn_on_the_same_ledger() {
        let request = setup();
        accept(100, 7, &request);
        accept(101, 7, &request);
    }

    #[test]
    fn should_keep_equal_token_burn_indexes_on_different_ledgers_separate() {
        let first = setup();
        let second = request(3);
        record(
            EventType::AddedSplToken(spl_token(second.mint, TokenProgram::Classic)),
            3,
        );
        accept(100, 7, &first);
        accept(101, 7, &second);
        let replayed = assert_replay_matches();
        assert_eq!(replayed.pending_spl_withdrawal_requests().len(), 2);
        assert_eq!(
            replayed.pending_spl_withdrawal_requests()[&LedgerBurnIndex::new(100)].request,
            accepted_request(100, 7, &first)
        );
        assert_eq!(
            replayed.pending_spl_withdrawal_requests()[&LedgerBurnIndex::new(101)].request,
            accepted_request(101, 7, &second)
        );
    }
}
