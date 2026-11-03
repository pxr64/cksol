use crate::withdraw::round::{
    WithdrawalPlan, WithdrawalRound, can_create_withdrawal_transaction, select_spl_withdrawal_batch,
};
use crate::withdraw::spl::fees::spl_withdrawal_batch_budget;
use crate::{
    constants::{FEE_PER_SIGNATURE, RENT_EXEMPTION_THRESHOLD},
    numeric::LedgerBurnIndex,
    sol_transfer::BATCH_WITHDRAWAL_TX_FEE,
    state::{
        event::{EventType, SplWithdrawalRequest, SplWithdrawalRequestTerms, WithdrawalRequest},
        mutate_state, read_state, reset_state,
        tests::helpers::*,
    },
    storage::reset_events,
    test_fixtures::NONCE_ACCOUNT,
};
use solana_hash::Hash;

/// Calls the read-only batch selector with fresh requests and the state's available funds.
fn select_pending_spl_batch(next_sol: Option<u64>) -> Option<Vec<SplWithdrawalRequest>> {
    read_state(|state| {
        let candidates: Vec<_> = state
            .pending_spl_withdrawal_requests()
            .values()
            .map(|pending| pending.request.clone())
            .collect();
        let before = state.clone();
        let selected = select_spl_withdrawal_batch(
            state,
            &candidates,
            next_sol.map(LedgerBurnIndex::new),
            state.spendable_balance(),
            state.spl_treasury_balances(),
        );
        assert_eq!(state, &before);
        selected
    })
}

mod batch_selection {
    use super::*;

    #[test]
    fn should_select_at_most_five_fresh_spl_requests_across_mints_and_token_programs() {
        let [classic, mut token_2022] = setup_pending_spl_batch();
        token_2022.amount = 1_000_000;
        accept(102, 9, &classic);
        accept(103, 10, &token_2022);
        accept(104, 11, &classic);
        accept(105, 12, &token_2022);
        let selected = select_pending_spl_batch(None).unwrap();
        assert_eq!(
            selected,
            vec![
                accepted_request(100, 7, &classic),
                read_state(|state| state.pending_spl_withdrawal_requests()[&101.into()]
                    .request
                    .clone()),
                accepted_request(102, 9, &classic),
                accepted_request(103, 10, &token_2022),
                accepted_request(104, 11, &classic),
            ]
        );
        let replayed = assert_replay_matches();
        assert_eq!(replayed.pending_spl_withdrawal_requests().len(), 6);
        assert!(replayed.created_spl_withdrawal_requests().is_empty());
        assert!(
            spl_withdrawal_batch_budget(&replayed, &selected).unwrap()
                <= replayed.spendable_balance()
        );
    }

    #[test]
    fn should_select_a_single_available_spl_withdrawal_without_waiting_for_a_batch() {
        let terms = setup_with_nonce();
        assert_eq!(select_pending_spl_batch(None), None);
        accept(100, 7, &terms);
        assert_eq!(
            select_pending_spl_batch(None),
            Some(vec![accepted_request(100, 7, &terms)])
        );
        assert_replay_matches();
    }

    #[test]
    fn should_stop_an_spl_batch_before_the_next_waiting_sol_withdrawal() {
        let terms = setup_with_nonce();
        for id in [100, 102, 104] {
            accept(id, id, &terms);
        }
        assert_eq!(select_pending_spl_batch(Some(99)), None);
        assert_eq!(
            select_pending_spl_batch(Some(103)),
            Some(vec![
                accepted_request(100, 100, &terms),
                accepted_request(102, 102, &terms),
            ])
        );
        assert_replay_matches();
    }

    #[test]
    fn should_send_low_fee_spl_requests_alone_without_bypassing_them() {
        for low_fee_first in [true, false] {
            reset_state();
            reset_events();
            let terms = setup_with_nonce();
            let low_fee = SplWithdrawalRequestTerms {
                fee: FEE_PER_SIGNATURE,
                ..terms.clone()
            };
            let first = if low_fee_first { &low_fee } else { &terms };
            let second = if low_fee_first { &terms } else { &low_fee };
            accept(100, 7, first);
            accept(101, 8, second);
            accept(102, 9, &terms);
            // The batch cannot include both 100 and 101. Do not jump over 101 to batch 100 with 102.
            assert_eq!(
                select_pending_spl_batch(None),
                Some(vec![accepted_request(100, 7, first)])
            );
            assert_replay_matches();
        }
    }

    #[test]
    fn should_select_three_spl_requests_when_their_quotes_cannot_cover_a_two_request_batch() {
        let mut terms = setup_with_nonce();
        // Each quote covers a 5,000 solo retry plus a rounded-up 1,667 share of a three-request batch.
        terms.fee = 6_667;
        for id in 100..103 {
            accept(id, id, &terms);
        }
        assert_eq!(
            select_pending_spl_batch(None),
            Some(vec![
                accepted_request(100, 100, &terms),
                accepted_request(101, 101, &terms),
                accepted_request(102, 102, &terms),
            ])
        );
        assert_replay_matches();
    }

    #[test]
    fn should_use_a_single_spl_request_when_sol_cannot_cover_a_batch_and_its_retries() {
        let terms = setup_with_nonce();
        accept(100, 7, &terms);
        accept(101, 8, &terms);
        mutate_state(|state| {
            set_balance(state, RENT_EXEMPTION_THRESHOLD + 2 * FEE_PER_SIGNATURE);
        });
        assert_eq!(
            select_pending_spl_batch(None),
            Some(vec![accepted_request(100, 7, &terms)])
        );
        mutate_state(|state| {
            set_balance(state, RENT_EXEMPTION_THRESHOLD + FEE_PER_SIGNATURE - 1);
        });
        assert_eq!(select_pending_spl_batch(None), None);
    }

    #[test]
    fn should_skip_unfunded_mints_while_selecting_an_spl_batch() {
        let [classic, mut token_2022] = setup_pending_spl_batch();
        token_2022.amount = 1_000_000;
        accept(102, 9, &classic);
        accept(103, 10, &token_2022);
        let original = read_state(|state| {
            state.pending_spl_withdrawal_requests()[&101.into()]
                .request
                .clone()
        });
        mutate_state(|state| {
            set_spl_treasury_balance(state, classic.mint, 0);
        });
        assert_eq!(
            select_pending_spl_batch(None),
            Some(vec![original, accepted_request(103, 10, &token_2022)])
        );
    }

    #[test]
    fn should_count_all_same_mint_transfers_against_one_spl_batch_token_budget() {
        let mut terms = setup_with_nonce();
        terms.amount = 6_000_000;
        accept(100, 7, &terms);
        accept(101, 8, &terms);
        let smaller = SplWithdrawalRequestTerms {
            amount: 1_000_000,
            ..terms.clone()
        };
        accept(102, 9, &smaller);
        // After selecting 100, the remaining four million tokens cannot pay 101, but can pay 102.
        assert_eq!(
            select_pending_spl_batch(None),
            Some(vec![
                accepted_request(100, 7, &terms),
                accepted_request(102, 9, &smaller),
            ])
        );
        assert_replay_matches();
    }

    #[test]
    fn should_skip_unaffordable_ata_creation_when_a_later_spl_request_needs_no_rent() {
        let [mut classic, _] = setup_pending_spl_batch();
        classic.create_destination_account = false;
        accept(102, 9, &classic);
        mutate_state(|state| {
            set_balance(state, RENT_EXEMPTION_THRESHOLD + FEE_PER_SIGNATURE);
        });
        assert_eq!(
            select_pending_spl_batch(None),
            Some(vec![accepted_request(102, 9, &classic)])
        );
    }

    #[test]
    fn should_exclude_prepaid_retries_from_fresh_spl_batch_selection() {
        let mut candidates = setup_spl_withdrawal_retries(false);
        let fresh = accepted_request(200, 200, &candidates[0].terms);
        accept(200, 200, &fresh.terms);
        candidates.push(fresh.clone());
        read_state(|state| {
            assert_eq!(
                select_spl_withdrawal_batch(
                    state,
                    &candidates,
                    None,
                    state.spendable_balance(),
                    state.spl_treasury_balances(),
                ),
                Some(vec![fresh])
            );
        });
        assert_replay_matches();
    }
}

mod ordering {
    use super::*;

    #[test]
    fn should_plan_withdrawals_of_both_kinds_in_burn_index_order_within_one_budget() {
        let request = setup_with_nonce();
        let sol = WithdrawalRequest {
            account: request.account,
            solana_address: request.address.to_bytes(),
            burn_block_index: 50.into(),
            burned_amount: 20_000,
            amount_to_transfer: 10_000,
        };
        record(EventType::AcceptedWithdrawalRequest(sol.clone()), 15);
        accept(100, 7, &request);
        accept(101, 8, &request);
        let plans: Vec<_> =
            read_state(|state| WithdrawalRound::take_from_queue(state, usize::MAX).collect());
        assert_eq!(
            plans,
            vec![
                WithdrawalPlan::Sol(vec![sol.clone()]),
                WithdrawalPlan::Spl(accepted_request(100, 7, &request)),
                WithdrawalPlan::Spl(accepted_request(101, 8, &request)),
            ]
        );

        mutate_state(|state| {
            set_balance(
                state,
                RENT_EXEMPTION_THRESHOLD
                    + sol.amount_to_transfer
                    + BATCH_WITHDRAWAL_TX_FEE
                    + FEE_PER_SIGNATURE,
            );
        });
        let plans: Vec<_> =
            read_state(|state| WithdrawalRound::take_from_queue(state, usize::MAX).collect());
        assert_eq!(
            plans,
            vec![
                WithdrawalPlan::Sol(vec![sol]),
                WithdrawalPlan::Spl(accepted_request(100, 7, &request)),
            ]
        );
    }

    #[test]
    fn should_not_batch_a_sol_request_newer_than_a_waiting_spl_request() {
        let request = setup_with_nonce();
        let sol = |burn_block_index: u64| WithdrawalRequest {
            account: request.account,
            solana_address: request.address.to_bytes(),
            burn_block_index: burn_block_index.into(),
            burned_amount: 20_000,
            amount_to_transfer: 10_000,
        };
        record(EventType::AcceptedWithdrawalRequest(sol(90)), 10);
        accept(100, 7, &request);
        record(EventType::AcceptedWithdrawalRequest(sol(110)), 30);
        let plans: Vec<_> =
            read_state(|state| WithdrawalRound::take_from_queue(state, usize::MAX).collect());
        assert_eq!(
            plans,
            vec![
                WithdrawalPlan::Sol(vec![sol(90)]),
                WithdrawalPlan::Spl(accepted_request(100, 7, &request)),
                WithdrawalPlan::Sol(vec![sol(110)]),
            ]
        );

        // The budget covers the first SOL request and the SPL request, not the later SOL one.
        mutate_state(|state| {
            set_balance(
                state,
                RENT_EXEMPTION_THRESHOLD + 10_000 + BATCH_WITHDRAWAL_TX_FEE + FEE_PER_SIGNATURE,
            );
        });
        let plans: Vec<_> =
            read_state(|state| WithdrawalRound::take_from_queue(state, usize::MAX).collect());
        assert_eq!(
            plans,
            vec![
                WithdrawalPlan::Sol(vec![sol(90)]),
                WithdrawalPlan::Spl(accepted_request(100, 7, &request)),
            ]
        );
    }
}

mod fairness {
    use super::*;

    #[test]
    fn should_alternate_individual_spl_retries_with_fresh_sol_and_spl_transactions() {
        let retries = setup_spl_withdrawal_retries(true);
        let terms = &retries[0].terms;
        let sol = WithdrawalRequest {
            account: terms.account,
            solana_address: terms.address.to_bytes(),
            burn_block_index: 150.into(),
            burned_amount: 20_000,
            amount_to_transfer: 10_000,
        };
        record(EventType::AcceptedWithdrawalRequest(sol.clone()), 85);
        for id in 200..204 {
            accept(id, id, terms);
        }
        let before = read_state(Clone::clone);
        let plans: Vec<_> = WithdrawalRound::take_from_queue(&before, usize::MAX).collect();
        assert_eq!(
            plans,
            vec![
                WithdrawalPlan::Spl(retries[0].clone()),
                WithdrawalPlan::Sol(vec![sol]),
                WithdrawalPlan::Spl(retries[1].clone()),
                WithdrawalPlan::Spl(accepted_request(200, 200, terms)),
                WithdrawalPlan::Spl(retries[2].clone()),
                WithdrawalPlan::Spl(accepted_request(201, 201, terms)),
                WithdrawalPlan::Spl(retries[3].clone()),
                WithdrawalPlan::Spl(accepted_request(202, 202, terms)),
                WithdrawalPlan::Spl(retries[4].clone()),
                WithdrawalPlan::Spl(accepted_request(203, 203, terms)),
            ]
        );
        // A round may only select as many transactions as the caller has slots for.
        for slots in [0, 1, 2, 3, 10] {
            assert_eq!(
                WithdrawalRound::take_from_queue(&before, slots).collect::<Vec<_>>(),
                plans[..slots]
            );
        }
        read_state(|state| assert_eq!(state, &before));
        assert_replay_matches();
    }

    #[test]
    fn should_use_spare_capacity_when_fresh_work_or_spl_retries_run_out() {
        for shared_destination in [false, true] {
            let retries = setup_spl_withdrawal_retries(shared_destination);
            let terms = &retries[0].terms;
            accept(200, 200, terms);
            let mut expected = vec![
                WithdrawalPlan::Spl(retries[0].clone()),
                WithdrawalPlan::Spl(accepted_request(200, 200, terms)),
            ];
            expected.extend(retries[1..].iter().cloned().map(WithdrawalPlan::Spl));
            assert_eq!(
                read_state(
                    |state| WithdrawalRound::take_from_queue(state, usize::MAX).collect::<Vec<_>>()
                ),
                expected
            );
        }
        // Once both retries are planned, fresh transactions can use every remaining slot.
        let retries = setup_spl_withdrawal_retries(false);
        let terms = &retries[0].terms;
        for id in 200..204 {
            accept(id, id, terms);
        }
        assert_eq!(
            read_state(
                |state| WithdrawalRound::take_from_queue(state, usize::MAX).collect::<Vec<_>>()
            ),
            vec![
                WithdrawalPlan::Spl(retries[0].clone()),
                WithdrawalPlan::Spl(accepted_request(200, 200, terms)),
                WithdrawalPlan::Spl(retries[1].clone()),
                WithdrawalPlan::Spl(accepted_request(201, 201, terms)),
                WithdrawalPlan::Spl(accepted_request(202, 202, terms)),
                WithdrawalPlan::Spl(accepted_request(203, 203, terms)),
            ]
        );
    }

    #[test]
    fn should_replay_the_fair_turn_when_only_one_nonce_is_available() {
        for fresh_is_sol in [false, true] {
            let retries = setup_spl_withdrawal_retries(false);
            let terms = &retries[0].terms;
            let fresh = if fresh_is_sol {
                let request = WithdrawalRequest {
                    account: terms.account,
                    solana_address: terms.address.to_bytes(),
                    burn_block_index: 50.into(),
                    burned_amount: 20_000,
                    amount_to_transfer: 10_000,
                };
                record(EventType::AcceptedWithdrawalRequest(request.clone()), 85);
                WithdrawalPlan::Sol(vec![request])
            } else {
                accept(200, 200, terms);
                WithdrawalPlan::Spl(accepted_request(200, 200, terms))
            };
            assert_eq!(
                read_state(|state| WithdrawalRound::take_from_queue(state, usize::MAX).next()),
                Some(WithdrawalPlan::Spl(retries[0].clone()))
            );
            let pending = create_individual_spl_retry(100, Hash::new_from_array([19; 32]), 90);
            let signature = [30; 64].into();
            submit_individual_spl_retry(&pending, signature, 100);
            finalize_individual_spl_retry(&pending, signature, true, 110);
            // The next round must serve fresh work, even after replay, rather than restarting at retries.
            let replayed = assert_replay_matches();
            assert!(!replayed.next_withdrawal_is_retry());
            assert_eq!(
                WithdrawalRound::take_from_queue(&replayed, usize::MAX).next(),
                Some(fresh)
            );
            let nonce_value = Hash::new_from_array([20; 32]);
            let event = if fresh_is_sol {
                EventType::CreatedWithdrawalTransaction {
                    burn_indices: vec![50.into()],
                    nonce_account: NONCE_ACCOUNT,
                    nonce_value,
                }
            } else {
                EventType::CreatedSplWithdrawalTransaction {
                    burn_indices: vec![200.into()],
                    nonce_account: NONCE_ACCOUNT,
                    nonce_value,
                }
            };
            record(event, 120);
            // A successful fresh binding gives the following turn back to the remaining retry.
            let replayed = assert_replay_matches();
            assert!(replayed.next_withdrawal_is_retry());
            assert_eq!(
                WithdrawalRound::take_from_queue(&replayed, usize::MAX).next(),
                Some(WithdrawalPlan::Spl(retries[1].clone()))
            );
        }
    }
}

mod affordability {
    use super::*;

    #[test]
    fn should_skip_unaffordable_spl_retries_without_blocking_other_work() {
        let retries = setup_spl_withdrawal_retries(false);
        let mut fresh = retries[1].terms.clone();
        fresh.amount = 1_000_000;
        fresh.create_destination_account = false;
        accept(200, 200, &fresh);
        // The classic treasury cannot cover retry 100; Token-2022 retry 101 can still proceed.
        mutate_state(|state| {
            set_spl_treasury_balance(state, retries[0].terms.mint, 0);
        });
        assert_eq!(
            read_state(
                |state| WithdrawalRound::take_from_queue(state, usize::MAX).collect::<Vec<_>>()
            ),
            vec![
                WithdrawalPlan::Spl(retries[1].clone()),
                WithdrawalPlan::Spl(accepted_request(200, 200, &fresh)),
            ]
        );
        // Retry SOL is prepaid, so reducing free SOL only limits the fresh request.
        mutate_state(|state| {
            set_balance(state, RENT_EXEMPTION_THRESHOLD + FEE_PER_SIGNATURE);
        });
        assert_eq!(
            read_state(
                |state| WithdrawalRound::take_from_queue(state, usize::MAX).collect::<Vec<_>>()
            ),
            vec![
                WithdrawalPlan::Spl(retries[1].clone()),
                WithdrawalPlan::Spl(accepted_request(200, 200, &fresh)),
            ]
        );
        mutate_state(|state| {
            set_balance(state, RENT_EXEMPTION_THRESHOLD);
        });
        assert_eq!(
            read_state(
                |state| WithdrawalRound::take_from_queue(state, usize::MAX).collect::<Vec<_>>()
            ),
            vec![WithdrawalPlan::Spl(retries[1].clone()),]
        );
        assert!(read_state(can_create_withdrawal_transaction));
    }

    #[test]
    fn should_share_treasury_tokens_between_prepaid_retries_and_fresh_requests() {
        let retries = setup_spl_withdrawal_retries(true);
        let terms = &retries[0].terms;
        accept(200, 200, terms);
        mutate_state(|state| {
            set_spl_treasury_balance(state, terms.mint, 2_000_000);
        });
        let before = read_state(Clone::clone);
        assert_eq!(
            WithdrawalRound::take_from_queue(&before, usize::MAX).collect::<Vec<_>>(),
            vec![
                WithdrawalPlan::Spl(retries[0].clone()),
                WithdrawalPlan::Spl(accepted_request(200, 200, terms)),
            ]
        );
        read_state(|state| assert_eq!(state, &before));
    }

    #[test]
    fn should_skip_spl_plans_the_treasury_cannot_pay_and_keep_sol_going() {
        let request = setup_with_nonce();
        let mut unfunded = request.clone();
        unfunded.amount = FUNDED_TOKENS + 1;
        accept(100, 7, &unfunded);
        accept(101, 8, &request);
        let sol = WithdrawalRequest {
            account: request.account,
            solana_address: request.address.to_bytes(),
            burn_block_index: 200.into(),
            burned_amount: 20_000,
            amount_to_transfer: 10_000,
        };
        record(EventType::AcceptedWithdrawalRequest(sol.clone()), 15);
        let plans: Vec<_> =
            read_state(|state| WithdrawalRound::take_from_queue(state, usize::MAX).collect());
        assert_eq!(plans, vec![WithdrawalPlan::Sol(vec![sol])]);
    }

    #[test]
    fn should_skip_sol_plans_the_balance_cannot_pay_and_keep_spl_going() {
        let request = setup_with_nonce();
        let sol = WithdrawalRequest {
            account: request.account,
            solana_address: request.address.to_bytes(),
            burn_block_index: 90.into(),
            burned_amount: FUNDED_BALANCE,
            amount_to_transfer: FUNDED_BALANCE,
        };
        record(EventType::AcceptedWithdrawalRequest(sol), 15);
        accept(100, 7, &request);
        let plans: Vec<_> =
            read_state(|state| WithdrawalRound::take_from_queue(state, usize::MAX).collect());
        assert_eq!(
            plans,
            vec![WithdrawalPlan::Spl(accepted_request(100, 7, &request))]
        );
    }
}

mod sol_batches {
    use crate::{
        state::State,
        test_fixtures::{
            MINIMUM_WITHDRAWAL_AMOUNT, WITHDRAWAL_FEE, account, address, valid_init_args,
        },
    };
    use proptest::prelude::*;
    use sol_rpc_types::Lamport;

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

        assert_eq!(
            WithdrawalRound::take_from_queue(&state, usize::MAX).next(),
            None
        );
    }

    proptest! {
        #[test]
        fn should_batch_single_request_when_balance_covers_amount_and_fee(
            amount_to_transfer in MINIMUM_WITHDRAWAL_AMOUNT..=MAX_AMOUNT_TO_TRANSFER
        ) {
            let mut state = state();
            set_balance(&mut state, amount_to_transfer + BATCH_WITHDRAWAL_TX_FEE + RENT_EXEMPTION_THRESHOLD);
            let requests = [withdrawal_request(0, amount_to_transfer)];
            accept_withdrawal_requests(&mut state, requests.clone());

            let batches: Vec<_> = WithdrawalRound::take_from_queue(&state, usize::MAX).collect();

            prop_assert_eq!(batches, vec![WithdrawalPlan::Sol(vec![requests[0].clone()])]);
        }

        #[test]
        fn should_be_empty_when_balance_does_not_cover_amount_and_fee(
            amount_to_transfer in MINIMUM_WITHDRAWAL_AMOUNT..=MAX_AMOUNT_TO_TRANSFER,
            shortfall in 1..=MINIMUM_WITHDRAWAL_AMOUNT + BATCH_WITHDRAWAL_TX_FEE
        ) {
            let mut state = state();
            set_balance(&mut state, amount_to_transfer + BATCH_WITHDRAWAL_TX_FEE + RENT_EXEMPTION_THRESHOLD
                - shortfall);
            let requests = [withdrawal_request(0, amount_to_transfer)];
            accept_withdrawal_requests(&mut state, requests);

            prop_assert_eq!(WithdrawalRound::take_from_queue(&state, usize::MAX).next(), None);
        }

        #[test]
        fn should_split_requests_into_batches_of_max_size_when_balance_covers_both_fees(
            balance in COST_OF_TWO_BATCHES..=u64::MAX
        ) {
            let mut state = state();
            set_balance(&mut state, balance);
            let requests = withdrawal_requests(NUM_REQUESTS_FOR_TWO_BATCHES);
            accept_withdrawal_requests(&mut state, requests.clone());

            let batches: Vec<_> = WithdrawalRound::take_from_queue(&state, usize::MAX).collect();

            prop_assert_eq!(
                batches,
                vec![
                    WithdrawalPlan::Sol(requests[..MAX_WITHDRAWALS_PER_NONCE_TX].to_vec()),
                    WithdrawalPlan::Sol(requests[MAX_WITHDRAWALS_PER_NONCE_TX..].to_vec())
                ]
            );
        }

        #[test]
        fn should_not_start_second_batch_when_balance_does_not_cover_its_amount_and_fee(
            shortfall in 1..=MINIMUM_WITHDRAWAL_AMOUNT + BATCH_WITHDRAWAL_TX_FEE
        ) {
            let mut state = state();
            set_balance(&mut state, COST_OF_TWO_BATCHES - shortfall);
            let requests = withdrawal_requests(NUM_REQUESTS_FOR_TWO_BATCHES);
            accept_withdrawal_requests(&mut state, requests.clone());

            let batches: Vec<_> = WithdrawalRound::take_from_queue(&state, usize::MAX).collect();

            prop_assert_eq!(
                batches,
                vec![WithdrawalPlan::Sol(requests[..MAX_WITHDRAWALS_PER_NONCE_TX].to_vec())]
            );
        }
    }

    #[test]
    fn should_hold_back_the_rent_exemption_threshold() {
        let mut state = state();
        let requests = [withdrawal_request(0, MINIMUM_WITHDRAWAL_AMOUNT)];
        accept_withdrawal_requests(&mut state, requests.clone());
        set_balance(
            &mut state,
            MINIMUM_WITHDRAWAL_AMOUNT + BATCH_WITHDRAWAL_TX_FEE + RENT_EXEMPTION_THRESHOLD - 1,
        );

        assert_eq!(
            WithdrawalRound::take_from_queue(&state, usize::MAX).next(),
            None
        );

        let next_balance = state.balance() + 1;
        set_balance(&mut state, next_balance);

        assert_eq!(
            WithdrawalRound::take_from_queue(&state, usize::MAX).next(),
            Some(WithdrawalPlan::Sol(requests.to_vec()))
        );
    }

    #[test]
    fn should_be_empty_when_cost_overflows() {
        let mut state = state();
        set_balance(&mut state, u64::MAX);
        let request = WithdrawalRequest {
            amount_to_transfer: u64::MAX,
            burned_amount: u64::MAX,
            ..withdrawal_request(0, MINIMUM_WITHDRAWAL_AMOUNT)
        };
        accept_withdrawal_requests(&mut state, [request]);

        assert_eq!(
            WithdrawalRound::take_from_queue(&state, usize::MAX).next(),
            None
        );
    }

    #[test]
    fn should_stop_at_first_unaffordable_request_without_skipping_it() {
        let mut state = state();
        set_balance(
            &mut state,
            2 * MINIMUM_WITHDRAWAL_AMOUNT + BATCH_WITHDRAWAL_TX_FEE + RENT_EXEMPTION_THRESHOLD,
        );
        let requests = [
            withdrawal_request(0, MINIMUM_WITHDRAWAL_AMOUNT),
            withdrawal_request(1, 2 * MINIMUM_WITHDRAWAL_AMOUNT),
            withdrawal_request(2, MINIMUM_WITHDRAWAL_AMOUNT),
        ];
        accept_withdrawal_requests(&mut state, requests.clone());

        let batches: Vec<_> = WithdrawalRound::take_from_queue(&state, usize::MAX).collect();

        assert_eq!(
            batches,
            vec![WithdrawalPlan::Sol(vec![requests[0].clone()])]
        );
    }

    #[test]
    fn should_not_create_a_transaction_without_pending_withdrawals() {
        let mut state = state();
        set_balance(&mut state, u64::MAX);
        nonce_pool_mut(&mut state)
            .add_accounts([address(1)])
            .unwrap();

        assert!(!can_create_withdrawal_transaction(&state));
    }

    #[test]
    fn should_not_create_a_transaction_without_a_free_nonce_account() {
        let mut state = state();
        set_balance(&mut state, u64::MAX);
        accept_withdrawal_requests(
            &mut state,
            [withdrawal_request(0, MINIMUM_WITHDRAWAL_AMOUNT)],
        );
        nonce_pool_mut(&mut state)
            .add_accounts([address(1)])
            .unwrap();
        bind_nonce(&mut state, &address(1), durable_nonce(1));

        assert!(!can_create_withdrawal_transaction(&state));
    }

    #[test]
    fn should_not_create_a_transaction_without_an_affordable_batch() {
        let mut state = state();
        set_balance(
            &mut state,
            MINIMUM_WITHDRAWAL_AMOUNT + BATCH_WITHDRAWAL_TX_FEE + RENT_EXEMPTION_THRESHOLD - 1,
        );
        accept_withdrawal_requests(
            &mut state,
            [withdrawal_request(0, MINIMUM_WITHDRAWAL_AMOUNT)],
        );
        nonce_pool_mut(&mut state)
            .add_accounts([address(1)])
            .unwrap();

        assert!(!can_create_withdrawal_transaction(&state));
    }

    #[test]
    fn should_create_a_transaction_with_an_affordable_batch_and_a_free_nonce_account() {
        let mut state = state();
        set_balance(
            &mut state,
            MINIMUM_WITHDRAWAL_AMOUNT + BATCH_WITHDRAWAL_TX_FEE + RENT_EXEMPTION_THRESHOLD,
        );
        accept_withdrawal_requests(
            &mut state,
            [withdrawal_request(0, MINIMUM_WITHDRAWAL_AMOUNT)],
        );
        nonce_pool_mut(&mut state)
            .add_accounts([address(1)])
            .unwrap();

        assert!(can_create_withdrawal_transaction(&state));
    }

    fn state() -> State {
        State::try_from(valid_init_args()).unwrap()
    }

    fn accept_withdrawal_requests<I: IntoIterator<Item = WithdrawalRequest>>(
        state: &mut State,
        requests: I,
    ) {
        for (index, request) in requests.into_iter().enumerate() {
            accept_sol_withdrawal(state, &request, index as u64);
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
