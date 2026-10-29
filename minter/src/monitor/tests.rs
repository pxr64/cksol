use super::{
    FINALIZE_TRANSACTIONS_RETRY_DELAY, MAX_BLOCKHASH_AGE_IN_BLOCKS,
    MAX_SIGNATURES_PER_STATUS_CHECK, MIN_REBROADCAST_AGE, finalize_transactions,
};
use crate::{
    address::minter_address,
    constants::{FEE_PER_SIGNATURE, MAX_CONCURRENT_RPC_CALLS},
    rpc::BlockHeight,
    state::{
        MinterTransaction, TaskType, TokenProgram,
        audit::process_event,
        event::{
            EventType, Signer, SplWithdrawalRequest, SplWithdrawalRequestTerms, TransactionPurpose,
            VersionedMessage,
        },
        mutate_state, read_state, reset_state,
    },
    storage::{UndecidedWithdrawalReason, reset_events, undecided_withdrawal_transaction_count},
    test_fixtures::{
        EventsAssert, GetTransactionResult, MINIMUM_WITHDRAWAL_AMOUNT, NONCE_ACCOUNT, account,
        confirmed_block_at_height, devnet_sweep, durable_nonce, events, failed_withdrawal_response,
        finalized_status, init_balance, init_schnorr_master_key, init_state, nonce_account_info,
        runtime::TestCanisterRuntime, signature, spl_token, succeeded_transaction_response,
        succeeded_withdrawal_response,
    },
};
use sol_rpc_types::{
    CommitmentLevel, ConfirmedBlock, GetBlockCommitmentLevel, GetBlockParams, GetSlotParams,
    GetSlotRpcConfig, MultiRpcResult, RpcConfig, RpcError, RpcSources, SendTransactionParams, Slot,
    TransactionConfirmationStatus, TransactionError, TransactionStatus,
};
use solana_transaction::Transaction;

type SlotResult = MultiRpcResult<Slot>;
type BlockResult = MultiRpcResult<ConfirmedBlock>;
type SignatureStatusesResult = MultiRpcResult<Vec<Option<TransactionStatus>>>;
type SendTransactionResult = MultiRpcResult<sol_rpc_types::Signature>;
type GetAccountInfoResult = MultiRpcResult<Option<sol_rpc_types::AccountInfo>>;

const CURRENT_SLOT: Slot = 408_807_102;
const SUBMISSION_SLOT: Slot = CURRENT_SLOT - 10;
const CURRENT_BLOCK_HEIGHT: BlockHeight = BlockHeight::new(CURRENT_SLOT - 1_000);
const OLDEST_VALID_BLOCK_HEIGHT: BlockHeight =
    BlockHeight::new(CURRENT_BLOCK_HEIGHT.get() - MAX_BLOCKHASH_AGE_IN_BLOCKS.get());
const EXPIRED_BLOCK_HEIGHT: BlockHeight = BlockHeight::new(OLDEST_VALID_BLOCK_HEIGHT.get() - 1);

mod finalization {
    use super::*;

    #[tokio::test]
    async fn should_return_early_if_no_submitted_transactions() {
        setup();
        let events_before = EventsAssert::from_recorded();

        finalize_transactions(TestCanisterRuntime::new().with_increasing_time()).await;

        assert_eq!(EventsAssert::from_recorded(), events_before);
    }

    #[tokio::test]
    async fn should_return_early_if_task_already_active() {
        setup();
        submit_sweep_transaction(CURRENT_BLOCK_HEIGHT);

        mutate_state(|s| {
            s.active_tasks_mut().insert(TaskType::FinalizeTransactions);
        });

        let events_before = EventsAssert::from_recorded();

        finalize_transactions(TestCanisterRuntime::new()).await;

        let events_after = EventsAssert::from_recorded();
        assert_eq!(events_before, events_after);
    }

    #[tokio::test]
    async fn should_finalize_but_not_expire_transactions_if_fetching_current_block_fails() {
        setup();
        let finalized = submit_sweep_transaction_with_signature(1, EXPIRED_BLOCK_HEIGHT);
        let not_found = submit_sweep_transaction_with_signature(2, EXPIRED_BLOCK_HEIGHT);

        let runtime = TestCanisterRuntime::new()
            .with_increasing_time()
            .add_recent_block(Err(RpcError::ValidationError("Error".to_string())))
            .add_stub_response(SignatureStatusesResult::Consistent(Ok(vec![
                Some(finalized_status()),
                None,
            ])))
            .add_stub_response(GetTransactionResult::Consistent(Ok(None)));

        finalize_transactions(runtime).await;

        let events = EventsAssert::from_recorded().expect_contains_event_eq(
            EventType::SucceededTransaction {
                signature: finalized,
            },
        );
        assert!(!events.contains_event(&EventType::ExpiredTransaction {
            signature: not_found
        }));
        read_state(|s| assert!(s.submitted_transactions().contains_key(&not_found)));
    }

    #[tokio::test]
    async fn should_reschedule_while_more_sweeps_are_in_flight_than_a_round_checks() {
        setup();

        let num = MAX_CONCURRENT_RPC_CALLS * MAX_SIGNATURES_PER_STATUS_CHECK + 1;
        for i in 0..num {
            submit_sweep_transaction_with_signature(i, CURRENT_BLOCK_HEIGHT);
        }

        let mut runtime = TestCanisterRuntime::new()
            .with_increasing_time()
            .add_stub_response(SlotResult::Consistent(Ok(CURRENT_SLOT)))
            .add_stub_response(BlockResult::Consistent(Ok(current_block())));
        for _ in 0..MAX_CONCURRENT_RPC_CALLS {
            runtime = runtime.add_stub_response(SignatureStatusesResult::Consistent(Ok(vec![
                    None;
                    MAX_SIGNATURES_PER_STATUS_CHECK
                ])));
        }

        finalize_transactions(runtime.clone()).await;

        assert_eq!(
            called_methods(&runtime)
                .iter()
                .filter(|method| *method == "getSignatureStatuses")
                .count(),
            MAX_CONCURRENT_RPC_CALLS
        );
        assert_eq!(
            runtime.set_timer_delays(),
            vec![FINALIZE_TRANSACTIONS_RETRY_DELAY]
        );
    }

    #[tokio::test]
    async fn should_finalize_transaction_with_finalized_status() {
        setup();

        let signature = submit_sweep_transaction(CURRENT_BLOCK_HEIGHT);

        let runtime = TestCanisterRuntime::new()
            .with_increasing_time()
            .add_stub_response(SlotResult::Consistent(Ok(CURRENT_SLOT)))
            .add_stub_response(BlockResult::Consistent(Ok(current_block())))
            .add_stub_response(SignatureStatusesResult::Consistent(Ok(vec![Some(
                finalized_status(),
            )])))
            .add_stub_response(GetTransactionResult::Consistent(Ok(None)));

        finalize_transactions(runtime).await;

        EventsAssert::from_recorded()
            .expect_contains_event_eq(EventType::SucceededTransaction { signature });

        read_state(|s| {
            assert!(s.submitted_transactions().is_empty());
            assert!(s.succeeded_transactions().contains(&signature));
        });
    }

    #[tokio::test]
    async fn should_not_finalize_transaction() {
        for status in [processed_status(), confirmed_status()] {
            for block_height in [CURRENT_BLOCK_HEIGHT, EXPIRED_BLOCK_HEIGHT] {
                should_not_finalize(block_height, Some(status.clone())).await;
            }
        }
        should_not_finalize(CURRENT_BLOCK_HEIGHT, None).await;
    }

    async fn should_not_finalize(block_height: BlockHeight, status: Option<TransactionStatus>) {
        reset_state();
        reset_events();
        setup();

        submit_sweep_transaction(block_height);

        let runtime = TestCanisterRuntime::new()
            .with_increasing_time()
            .add_stub_response(SlotResult::Consistent(Ok(CURRENT_SLOT)))
            .add_stub_response(BlockResult::Consistent(Ok(current_block())))
            .add_stub_response(SignatureStatusesResult::Consistent(Ok(vec![status])));

        let events_before = EventsAssert::from_recorded();

        finalize_transactions(runtime).await;

        let events_after = EventsAssert::from_recorded();
        assert_eq!(events_before, events_after);

        read_state(|s| assert_eq!(s.submitted_transactions().len(), 1));
    }

    #[tokio::test]
    async fn should_record_failed_transaction_event_on_error() {
        setup();

        let signature = submit_sweep_transaction(CURRENT_BLOCK_HEIGHT);

        let runtime = TestCanisterRuntime::new()
            .with_increasing_time()
            .add_stub_response(SlotResult::Consistent(Ok(CURRENT_SLOT)))
            .add_stub_response(BlockResult::Consistent(Ok(current_block())))
            .add_stub_response(SignatureStatusesResult::Consistent(Ok(vec![Some(
                TransactionStatus {
                    slot: SUBMISSION_SLOT,
                    status: Err(TransactionError::InsufficientFundsForFee),
                    err: Some(TransactionError::InsufficientFundsForFee),
                    confirmation_status: Some(TransactionConfirmationStatus::Finalized),
                },
            )])));

        finalize_transactions(runtime).await;

        EventsAssert::from_recorded()
            .expect_contains_event_eq(EventType::FailedTransaction { signature });

        read_state(|s| {
            assert!(s.submitted_transactions().is_empty());
            assert_eq!(s.failed_transactions().len(), 1);
            assert!(s.failed_transactions().contains_key(&signature));
        });
    }

    #[tokio::test]
    async fn should_finalize_multiple_transactions_in_one_batch() {
        setup();

        let sig_a = 0x01;
        let sig_b = 0x02;
        let sig_c = 0x03;
        submit_sweep_transaction_with_signature(sig_a, CURRENT_BLOCK_HEIGHT);
        submit_sweep_transaction_with_signature(sig_b, CURRENT_BLOCK_HEIGHT);
        submit_sweep_transaction_with_signature(sig_c, CURRENT_BLOCK_HEIGHT);

        let runtime = TestCanisterRuntime::new()
            .with_increasing_time()
            .add_stub_response(SlotResult::Consistent(Ok(CURRENT_SLOT)))
            .add_stub_response(BlockResult::Consistent(Ok(current_block())))
            .add_stub_response(SignatureStatusesResult::Consistent(Ok(vec![
                Some(finalized_status()),
                None,
                Some(finalized_status()),
            ])))
            .add_stub_response(GetTransactionResult::Consistent(Ok(None)))
            .add_stub_response(GetTransactionResult::Consistent(Ok(None)));

        finalize_transactions(runtime).await;

        EventsAssert::from_recorded()
            .expect_contains_event_eq(EventType::SucceededTransaction {
                signature: signature(sig_a),
            })
            .expect_contains_event_eq(EventType::SucceededTransaction {
                signature: signature(sig_c),
            });

        read_state(|s| {
            assert_eq!(s.submitted_transactions().len(), 1);
            assert!(s.submitted_transactions().contains_key(&signature(sig_b)));
        });
    }

    #[tokio::test]
    async fn should_never_expire_nonce_withdrawal_with_unchanged_nonce() {
        setup();
        let sweep = submit_sweep_transaction_with_signature(1, EXPIRED_BLOCK_HEIGHT);
        let nonce_withdrawal = submit_withdrawal_bound_to(2, BOUND_NONCE_SEED);

        let runtime = TestCanisterRuntime::new()
            .with_increasing_time()
            .add_stub_response(SlotResult::Consistent(Ok(CURRENT_SLOT)))
            .add_stub_response(BlockResult::Consistent(Ok(current_block())))
            .add_stub_response(SignatureStatusesResult::Consistent(Ok(vec![None])))
            .add_stub_response(nonce_read(BOUND_NONCE_SEED));

        finalize_transactions(runtime).await;

        let events = EventsAssert::from_recorded()
            .expect_contains_event_eq(EventType::ExpiredTransaction { signature: sweep });
        assert!(!events.contains_event(&EventType::ExpiredTransaction {
            signature: nonce_withdrawal
        }));
        read_state(|s| assert!(s.submitted_transactions().contains_key(&nonce_withdrawal)));
    }

    #[tokio::test]
    async fn should_not_expire_transaction_if_status_check_fails() {
        setup();

        submit_sweep_transaction(EXPIRED_BLOCK_HEIGHT);

        let events_before = EventsAssert::from_recorded();

        let finalize_runtime = TestCanisterRuntime::new()
            .with_increasing_time()
            .add_stub_response(SlotResult::Consistent(Ok(CURRENT_SLOT)))
            .add_stub_response(BlockResult::Consistent(Ok(current_block())))
            .add_stub_response(SignatureStatusesResult::Consistent(Err(
                RpcError::ValidationError("Error".to_string()),
            )));

        finalize_transactions(finalize_runtime).await;

        let events_after = EventsAssert::from_recorded();
        assert_eq!(events_before, events_after);

        read_state(|s| {
            assert_eq!(s.submitted_transactions().len(), 1);
        });
    }

    #[tokio::test]
    async fn should_fetch_the_current_block_height_at_finalized_commitment() {
        setup();
        submit_sweep_transaction(OLDEST_VALID_BLOCK_HEIGHT);
        let runtime = TestCanisterRuntime::new()
            .with_increasing_time()
            .add_stub_response(SlotResult::Consistent(Ok(CURRENT_SLOT)))
            .add_stub_response(BlockResult::Consistent(Ok(current_block())))
            .add_stub_response(SignatureStatusesResult::Consistent(Ok(vec![None])));

        finalize_transactions(runtime.clone()).await;

        let calls = runtime.sent_update_calls();
        let get_slot = calls.iter().find(|call| call.method == "getSlot").unwrap();
        let (_sources, _config, slot_params): (
            RpcSources,
            Option<GetSlotRpcConfig>,
            Option<GetSlotParams>,
        ) = get_slot.args();
        assert_eq!(
            slot_params.and_then(|params| params.commitment),
            Some(CommitmentLevel::Finalized)
        );
        let get_block = calls.iter().find(|call| call.method == "getBlock").unwrap();
        let (_sources, _config, block_params): (RpcSources, Option<RpcConfig>, GetBlockParams) =
            get_block.args();
        assert_eq!(
            block_params.commitment,
            Some(GetBlockCommitmentLevel::Finalized)
        );
    }

    struct ExpiryCase {
        name: &'static str,
        transaction_block_height: BlockHeight,
        should_expire: bool,
    }

    #[tokio::test]
    async fn should_expire_transaction_past_the_blockhash_age_limit() {
        let cases = [
            ExpiryCase {
                name: "at the age limit",
                transaction_block_height: OLDEST_VALID_BLOCK_HEIGHT,
                should_expire: false,
            },
            ExpiryCase {
                name: "past the age limit",
                transaction_block_height: BlockHeight::new(OLDEST_VALID_BLOCK_HEIGHT.get() - 1),
                should_expire: true,
            },
            ExpiryCase {
                name: "ahead of the current block height",
                transaction_block_height: BlockHeight::new(u64::MAX),
                should_expire: false,
            },
        ];

        for case in cases {
            reset_state();
            reset_events();
            setup();
            let signature = submit_sweep_transaction(case.transaction_block_height);
            let runtime = TestCanisterRuntime::new()
                .with_increasing_time()
                .add_stub_response(SlotResult::Consistent(Ok(CURRENT_SLOT)))
                .add_stub_response(BlockResult::Consistent(Ok(current_block())))
                .add_stub_response(SignatureStatusesResult::Consistent(Ok(vec![None])));

            finalize_transactions(runtime).await;

            let expired = EventsAssert::from_recorded()
                .contains_event(&EventType::ExpiredTransaction { signature });
            assert_eq!(expired, case.should_expire, "{}", case.name);
            read_state(|s| {
                assert_eq!(
                    s.submitted_transactions().contains_key(&signature),
                    !case.should_expire,
                    "{}",
                    case.name
                );
            });
        }
    }

    fn confirmed_status() -> TransactionStatus {
        TransactionStatus {
            slot: 0,
            status: Ok(()),
            err: None,
            confirmation_status: Some(TransactionConfirmationStatus::Confirmed),
        }
    }

    fn processed_status() -> TransactionStatus {
        TransactionStatus {
            slot: 0,
            status: Ok(()),
            err: None,
            confirmation_status: Some(TransactionConfirmationStatus::Processed),
        }
    }
}

mod withdrawal_finalization {
    use super::*;

    #[tokio::test]
    async fn should_reconcile_successful_spl_withdrawals_from_the_payer_balance() {
        for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
            for new_destination in [false, true] {
                reset_state();
                reset_events();
                let signature = submit_signed_spl_withdrawal(token_program);
                let balance_before = read_state(|state| state.balance());
                let reserved =
                    read_state(|state| state.spl_withdrawal_reserved_cost(&signature).unwrap());
                let lamports_spent = FEE_PER_SIGNATURE
                    + if new_destination {
                        token_program.account_rent_exemption()
                    } else {
                        0
                    };
                let runtime = TestCanisterRuntime::new()
                    .with_increasing_time()
                    .add_stub_response(nonce_read(UNSEEN_NONCE_SEED))
                    .add_stub_response(spl_withdrawal_response(&signature, lamports_spent));

                finalize_transactions(runtime).await;

                EventsAssert::from_recorded().expect_contains_event_eq(
                    EventType::SucceededSplWithdrawalTransaction {
                        signature,
                        lamports_spent,
                    },
                );
                read_state(|state| {
                    assert_eq!(state.balance(), balance_before + reserved - lamports_spent);
                    assert!(
                        state
                            .successful_spl_withdrawal_requests()
                            .contains_key(&100.into())
                    );
                    assert!(state.submitted_transactions().is_empty());
                });
                assert_nonce_account_is_free();
            }
        }
    }

    #[tokio::test]
    async fn should_reconcile_spl_withdrawals_whose_fee_differs_from_the_constant() {
        for (fee, lamports_spent) in [
            (FEE_PER_SIGNATURE / 2, FEE_PER_SIGNATURE / 2),
            (FEE_PER_SIGNATURE + 1, FEE_PER_SIGNATURE + 1),
        ] {
            reset_state();
            reset_events();
            let signature = submit_signed_spl_withdrawal(TokenProgram::Classic);
            let balance_before = read_state(|state| state.balance());
            let reserved =
                read_state(|state| state.spl_withdrawal_reserved_cost(&signature).unwrap());
            let mut response = spl_withdrawal_response(&signature, lamports_spent);
            let GetTransactionResult::Consistent(Ok(Some(outcome))) = &mut response else {
                unreachable!()
            };
            outcome.transaction.meta.as_mut().unwrap().fee = fee;
            let runtime = TestCanisterRuntime::new()
                .with_increasing_time()
                .add_stub_response(nonce_read(UNSEEN_NONCE_SEED))
                .add_stub_response(response);

            finalize_transactions(runtime).await;

            EventsAssert::from_recorded().expect_contains_event_eq(
                EventType::SucceededSplWithdrawalTransaction {
                    signature,
                    lamports_spent,
                },
            );
            read_state(|state| {
                assert_eq!(state.balance(), balance_before + reserved - lamports_spent);
                assert!(
                    state
                        .successful_spl_withdrawal_requests()
                        .contains_key(&100.into())
                );
            });
            assert_nonce_account_is_free();
        }
    }

    #[tokio::test]
    async fn should_keep_spl_withdrawals_unresolved_with_inconsistent_balance_metadata() {
        for change in 0..6 {
            reset_state();
            reset_events();
            let signature = submit_signed_spl_withdrawal(TokenProgram::Classic);
            let mut response = spl_withdrawal_response(&signature, FEE_PER_SIGNATURE);
            let GetTransactionResult::Consistent(Ok(Some(outcome))) = &mut response else {
                unreachable!()
            };
            let meta = outcome.transaction.meta.as_mut().unwrap();
            match change {
                0 => meta.pre_balances.clear(),
                1 => {
                    meta.post_balances.pop();
                }
                2 => meta.fee += 1,
                3 => meta.post_balances[0] = meta.pre_balances[0] + 1,
                4 => {
                    meta.post_balances[0] = meta.pre_balances[0]
                        - FEE_PER_SIGNATURE
                        - TokenProgram::Classic.account_rent_exemption()
                        - 1
                }
                5 => meta.post_balances[0] = meta.pre_balances[0] - FEE_PER_SIGNATURE + 1,
                _ => unreachable!(),
            }
            let events_before = EventsAssert::from_recorded();
            let original = read_state(Clone::clone);
            let runtime = TestCanisterRuntime::new()
                .with_increasing_time()
                .add_stub_response(nonce_read(UNSEEN_NONCE_SEED))
                .add_stub_response(response);

            finalize_transactions(runtime).await;

            assert_eq!(EventsAssert::from_recorded(), events_before);
            read_state(|state| assert_eq!(state, &original));
            assert_eq!(
                undecided_withdrawal_transaction_count(
                    UndecidedWithdrawalReason::UnresolvedOutcome
                ),
                1
            );
        }
    }

    #[tokio::test]
    async fn should_record_a_withdrawal_as_succeeded_once_its_nonce_advanced() {
        setup_with_signing_key();
        let signature = submit_signed_withdrawal_bound_to(1, BOUND_NONCE_SEED);

        let runtime = TestCanisterRuntime::new()
            .with_increasing_time()
            .add_stub_response(nonce_read(UNSEEN_NONCE_SEED))
            .add_stub_response(succeeded_withdrawal_response(&signature));

        finalize_transactions(runtime.clone()).await;

        EventsAssert::from_recorded()
            .expect_contains_event_eq(EventType::SucceededTransaction { signature });
        read_state(|s| {
            assert!(s.submitted_transactions().is_empty());
            assert!(s.succeeded_transactions().contains(&signature));
        });
        assert_nonce_account_is_free();
        assert_eq!(
            called_methods(&runtime),
            vec!["getAccountInfo", "getTransaction"]
        );
    }

    #[tokio::test]
    async fn should_record_a_withdrawal_as_failed_once_its_nonce_advanced_with_an_error() {
        setup_with_signing_key();
        let signature = submit_signed_withdrawal_bound_to(1, BOUND_NONCE_SEED);

        let runtime = TestCanisterRuntime::new()
            .with_increasing_time()
            .add_stub_response(nonce_read(UNSEEN_NONCE_SEED))
            .add_stub_response(failed_withdrawal_response(&signature));

        finalize_transactions(runtime).await;

        EventsAssert::from_recorded()
            .expect_contains_event_eq(EventType::FailedTransaction { signature });
        read_state(|s| {
            assert!(s.submitted_transactions().is_empty());
            assert!(s.failed_transactions().contains_key(&signature));
        });
        assert_nonce_account_is_free();
    }

    #[tokio::test]
    async fn should_rebroadcast_a_withdrawal_with_an_unchanged_nonce_unchanged() {
        setup_with_signing_key();
        let signature = submit_signed_withdrawal_bound_to(1, BOUND_NONCE_SEED);
        let events_before = EventsAssert::from_recorded();

        let runtime = TestCanisterRuntime::new()
            .with_increasing_time_from(min_rebroadcast_age_nanos())
            .add_stub_response(nonce_read(BOUND_NONCE_SEED))
            .add_stub_response(SendTransactionResult::Consistent(Ok(signature.into())));

        finalize_transactions(runtime.clone()).await;

        let sent = runtime.sent_transactions();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].get_transaction(),
            encoded_submitted_transaction(&signature)
        );
        assert_eq!(sent[0].skip_preflight, Some(true));
        assert_eq!(EventsAssert::from_recorded(), events_before);
        read_state(|s| assert!(s.submitted_transactions().contains_key(&signature)));
    }

    #[tokio::test]
    async fn should_rebroadcast_a_withdrawal_with_an_unchanged_nonce_only_after_the_minimum_age() {
        setup_with_signing_key();
        let signature = submit_signed_withdrawal_bound_to(1, BOUND_NONCE_SEED);

        let too_early = TestCanisterRuntime::new()
            .with_increasing_time()
            .add_stub_response(nonce_read(BOUND_NONCE_SEED));

        finalize_transactions(too_early.clone()).await;

        assert!(too_early.sent_transactions().is_empty());

        let old_enough = TestCanisterRuntime::new()
            .with_increasing_time_from(min_rebroadcast_age_nanos())
            .add_stub_response(nonce_read(BOUND_NONCE_SEED))
            .add_stub_response(SendTransactionResult::Consistent(Ok(signature.into())));

        finalize_transactions(old_enough.clone()).await;

        assert_eq!(old_enough.sent_transactions().len(), 1);
        read_state(|s| assert!(s.submitted_transactions().contains_key(&signature)));
    }

    #[tokio::test]
    async fn should_take_no_decision_on_a_stale_nonce_read() {
        setup_with_signing_key();
        let landed = submit_signed_withdrawal_bound_to(1, STALE_NONCE_SEED);
        events::succeed_transaction(landed);
        let signature = submit_signed_withdrawal_bound_to(2, BOUND_NONCE_SEED);
        let events_before = EventsAssert::from_recorded();

        let runtime = TestCanisterRuntime::new()
            .with_increasing_time_from(min_rebroadcast_age_nanos())
            .add_stub_response(nonce_read(STALE_NONCE_SEED));

        finalize_transactions(runtime.clone()).await;

        assert_eq!(EventsAssert::from_recorded(), events_before);
        assert!(runtime.sent_transactions().is_empty());
        read_state(|s| assert!(s.submitted_transactions().contains_key(&signature)));
        assert_eq!(
            undecided_withdrawal_transaction_count(UndecidedWithdrawalReason::StaleNonce),
            1
        );
    }

    #[tokio::test]
    async fn should_keep_the_nonce_account_bound_until_the_outcome_is_known() {
        setup_with_signing_key();
        let signature = submit_signed_withdrawal_bound_to(1, BOUND_NONCE_SEED);
        let events_before = EventsAssert::from_recorded();

        let runtime = TestCanisterRuntime::new()
            .with_increasing_time_from(min_rebroadcast_age_nanos())
            .add_stub_response(nonce_read(UNSEEN_NONCE_SEED))
            .add_stub_response(GetTransactionResult::Consistent(Ok(None)));

        finalize_transactions(runtime.clone()).await;

        assert_eq!(EventsAssert::from_recorded(), events_before);
        assert!(runtime.sent_transactions().is_empty());
        read_state(|s| {
            assert!(s.submitted_transactions().contains_key(&signature));
            assert_eq!(s.nonce_pool().num_free_accounts(), 0);
        });
        assert_eq!(
            undecided_withdrawal_transaction_count(UndecidedWithdrawalReason::UnresolvedOutcome),
            1
        );
    }

    #[tokio::test]
    async fn should_not_finalize_a_withdrawal_fetched_with_another_message() {
        setup_with_signing_key();
        let other_message =
            solana_message::Message::new(&[], Some(&devnet_sweep::minter_main_address()));
        let signature = devnet_sweep::minter_signature_of(&other_message);
        events::accept_withdrawal(account(1), 1, MINIMUM_WITHDRAWAL_AMOUNT);
        events::create_withdrawal_batch_transaction(durable_nonce(BOUND_NONCE_SEED), vec![1]);
        events::submit_signed_withdrawal_batch_transaction_under(
            signature,
            durable_nonce(BOUND_NONCE_SEED),
            vec![1],
        );
        let events_before = EventsAssert::from_recorded();

        let runtime = TestCanisterRuntime::new()
            .with_increasing_time()
            .add_stub_response(nonce_read(UNSEEN_NONCE_SEED))
            .add_stub_response(succeeded_transaction_response(&signature, other_message));

        finalize_transactions(runtime).await;

        assert_eq!(EventsAssert::from_recorded(), events_before);
        read_state(|s| {
            assert!(s.submitted_transactions().contains_key(&signature));
            assert_eq!(s.nonce_pool().num_free_accounts(), 0);
        });
    }

    fn assert_nonce_account_is_free() {
        read_state(|s| {
            assert!(
                s.nonce_pool()
                    .free_accounts()
                    .any(|account| *account == NONCE_ACCOUNT)
            )
        });
    }
}

const BOUND_NONCE_SEED: usize = 1;
const STALE_NONCE_SEED: usize = 2;
const UNSEEN_NONCE_SEED: usize = 3;

fn min_rebroadcast_age_nanos() -> u64 {
    MIN_REBROADCAST_AGE.as_nanos() as u64
}

fn encoded_submitted_transaction(signature: &solana_signature::Signature) -> String {
    let message = read_state(|s| {
        match s
            .submitted_transactions()
            .get(signature)
            .expect("the transaction is submitted")
        {
            MinterTransaction::Withdrawal {
                message: VersionedMessage::Legacy(message),
                ..
            } => message.clone(),
            other => panic!("expected a withdrawal transaction, got {other:?}"),
        }
    });
    let transaction = Transaction {
        signatures: vec![*signature],
        message,
    };
    SendTransactionParams::try_from(transaction)
        .expect("the transaction is serializable")
        .get_transaction()
        .to_string()
}

/// Submits a withdrawal transaction under `signature(i)` bound to [`NONCE_ACCOUNT`]
/// and to the nonce value that [`nonce_read`] returns for `nonce_seed`.
fn submit_withdrawal_bound_to(i: usize, nonce_seed: usize) -> solana_signature::Signature {
    let signature = signature(i);
    events::accept_withdrawal(account(i), i as u64, MINIMUM_WITHDRAWAL_AMOUNT);
    events::create_withdrawal_batch_transaction(durable_nonce(nonce_seed), vec![i as u64]);
    events::submit_withdrawal_batch_transaction(
        signature,
        durable_nonce(nonce_seed),
        vec![i as u64],
    );
    signature
}

fn submit_signed_withdrawal_bound_to(i: usize, nonce_seed: usize) -> solana_signature::Signature {
    events::accept_withdrawal(account(i), i as u64, MINIMUM_WITHDRAWAL_AMOUNT);
    events::create_withdrawal_batch_transaction(durable_nonce(nonce_seed), vec![i as u64]);
    events::submit_signed_withdrawal_batch_transaction(durable_nonce(nonce_seed), vec![i as u64])
}

fn nonce_read(nonce_seed: usize) -> GetAccountInfoResult {
    let minter_address = read_state(|s| s.minter_public_key().map(minter_address))
        .expect("the minter public key is cached");
    GetAccountInfoResult::Consistent(Ok(Some(nonce_account_info(minter_address, nonce_seed))))
}

fn called_methods(runtime: &TestCanisterRuntime) -> Vec<String> {
    runtime
        .sent_update_calls()
        .into_iter()
        .map(|call| call.method)
        .collect()
}

fn setup() {
    init_state();
    init_balance();
    init_schnorr_master_key();
}

fn setup_with_signing_key() {
    init_state();
    devnet_sweep::init_balance();
}

fn submit_signed_spl_withdrawal(token_program: TokenProgram) -> solana_signature::Signature {
    setup_with_signing_key();
    let token = spl_token([4; 32].into(), token_program);
    let runtime = TestCanisterRuntime::new().with_time(20);
    mutate_state(|state| {
        process_event(state, EventType::AddedSplToken(token.clone()), &runtime);
        process_event(
            state,
            EventType::AcceptedSplWithdrawalRequest(SplWithdrawalRequest {
                terms: SplWithdrawalRequestTerms {
                    account: account(1),
                    mint: token.mint,
                    ledger_id: token.ledger_id,
                    address: [9; 32].into(),
                    amount: 100,
                    fee: state.withdrawal_fee() + token_program.account_rent_exemption(),
                    create_destination_account: true,
                },
                cksol_burn_block_index: 100.into(),
                ckspl_burn_block_index: 7.into(),
            }),
            &runtime,
        );
        process_event(
            state,
            EventType::CreatedSplWithdrawalTransaction {
                cksol_burn_block_index: 100.into(),
                nonce_account: NONCE_ACCOUNT,
                nonce_value: durable_nonce(BOUND_NONCE_SEED),
            },
            &runtime,
        );
    });
    let message = read_state(|state| {
        state
            .bound_spl_withdrawal_message(
                &NONCE_ACCOUNT,
                &state.created_spl_withdrawal_txs()[&NONCE_ACCOUNT],
            )
            .unwrap()
    });
    let signature = devnet_sweep::minter_signature_of(&message);
    mutate_state(|state| {
        process_event(
            state,
            EventType::SubmittedTransaction {
                signature,
                message: message.into(),
                signers: vec![Signer::Minter],
                purpose: TransactionPurpose::SplWithdrawal {
                    cksol_burn_block_index: 100.into(),
                },
            },
            &runtime,
        )
    });
    signature
}

fn spl_withdrawal_response(
    signature: &solana_signature::Signature,
    lamports_spent: u64,
) -> GetTransactionResult {
    let mut response = succeeded_withdrawal_response(signature);
    let count = read_state(
        |state| match state.submitted_transactions().get(signature).unwrap() {
            MinterTransaction::Withdrawal {
                message: VersionedMessage::Legacy(message),
                ..
            } => message.account_keys.len(),
            _ => unreachable!(),
        },
    );
    let GetTransactionResult::Consistent(Ok(Some(outcome))) = &mut response else {
        unreachable!()
    };
    let meta = outcome.transaction.meta.as_mut().unwrap();
    meta.pre_balances = vec![10_000_000; count];
    meta.post_balances = meta.pre_balances.clone();
    meta.post_balances[0] -= lamports_spent;
    response
}

fn current_block() -> ConfirmedBlock {
    confirmed_block_at_height(CURRENT_BLOCK_HEIGHT)
}

fn submit_sweep_transaction(block_height: BlockHeight) -> solana_signature::Signature {
    submit_sweep_transaction_with_signature(1, block_height)
}

fn submit_sweep_transaction_with_signature(
    i: usize,
    block_height: BlockHeight,
) -> solana_signature::Signature {
    let signature = signature(i);
    let deposit_id = crate::state::read_state(|state| state.deposits().next_id());
    events::queue_deposit(deposit_id, account(i), 1_000_000);
    events::submit_sweep_at_height(signature, vec![deposit_id], block_height);
    signature
}
