use super::*;
use crate::sol_transfer::{MAX_SIGNATURES, MAX_TX_SIZE};
use crate::{
    lifecycle,
    state::{TokenProgram, audit::process_event, event::EventType, mutate_state, read_state},
    test_fixtures::{
        account, queued_spl_deposit, runtime::TestCanisterRuntime, schnorr_master_key, spl_token,
        valid_init_args,
    },
};
use solana_hash::Hash;
use solana_transaction::Transaction;

fn queue_deposits(
    count: usize,
    same_owner: bool,
    same_mint: bool,
    token_program: Option<TokenProgram>,
) {
    let runtime = TestCanisterRuntime::new().with_increasing_time();
    lifecycle::init(valid_init_args(), runtime.clone());
    for id in 0..count {
        let mint: solana_address::Address = if same_mint {
            [1; 32]
        } else {
            [id as u8 + 1; 32]
        }
        .into();
        let token_program = token_program.unwrap_or(if id % 2 == 0 {
            TokenProgram::Classic
        } else {
            TokenProgram::Token2022
        });
        let account = account(if same_owner { 1 } else { id + 1 });
        let deposit = queued_spl_deposit(account, mint, token_program);
        mutate_state(|state| {
            if state.supported_spl_token(&mint).is_none() {
                process_event(
                    state,
                    EventType::AddedSplToken(spl_token(mint, token_program)),
                    &runtime,
                );
            }
            process_event(
                state,
                EventType::QueuedSplDeposit {
                    deposit_id: id as u64,
                    account,
                    mint,
                    address: deposit.address,
                    balance: deposit.balance,
                },
                &runtime,
            );
        });
    }
}

fn assert_batches_are_maximal(round: &SweepRound) {
    for pair in round.batches.windows(2) {
        let mut deposits = pair[0].deposits().clone();
        let (id, next) = pair[1].deposits().first_key_value().unwrap();
        deposits.insert(*id, next.clone());
        let tokens = read_state(|state| {
            deposits
                .values()
                .map(|deposit| {
                    (
                        deposit.mint,
                        state.supported_spl_token(&deposit.mint).cloned().unwrap(),
                    )
                })
                .collect()
        });
        let candidate = SplSweep::plan(deposits, &tokens, &schnorr_master_key());
        let signatures = candidate
            .sweep_message(Hash::default())
            .header
            .num_required_signatures as u64;
        assert!(wire_size(&candidate) > MAX_TX_SIZE || signatures > MAX_SIGNATURES);
    }
}

fn wire_size(sweep: &SplSweep) -> usize {
    // An independent bincode measurement, to cross-check `transaction_size`.
    bincode::serialize(&Transaction::new_unsigned(
        sweep.sweep_message(Hash::default()),
    ))
    .unwrap()
    .len()
}

#[test]
fn should_return_empty_round_when_no_deposits_are_queued() {
    queue_deposits(0, false, true, Some(TokenProgram::Classic));
    let round = read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()));

    assert!(round.batches.is_empty());
    assert!(!round.leaves_deposits_queued);
}

#[test]
fn should_select_one_deposit_without_changing_the_queue() {
    queue_deposits(1, false, true, Some(TokenProgram::Token2022));
    let round = read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()));

    assert_eq!(round.batches.len(), 1);
    assert_eq!(
        round.batches[0]
            .deposits()
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![0]
    );
    assert!(wire_size(&round.batches[0]) <= MAX_TX_SIZE);
    assert!(!round.leaves_deposits_queued);
    read_state(|state| assert_eq!(state.spl_deposits().queued().len(), 1));
}

#[test]
fn should_fit_more_than_four_deposits_when_the_mint_is_shared() {
    queue_deposits(20, false, true, Some(TokenProgram::Classic));
    let round = read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()));

    assert!(round.batches.len() > 1);
    assert!(!round.leaves_deposits_queued);
    assert!(round.batches[0].deposits().len() > 4);
    assert_batches_are_maximal(&round);
    for batch in &round.batches {
        assert!(wire_size(batch) <= MAX_TX_SIZE);
        assert!(
            batch
                .sweep_message(Hash::default())
                .header
                .num_required_signatures as u64
                <= MAX_SIGNATURES
        );
    }
    let selected: Vec<_> = round
        .batches
        .iter()
        .flat_map(|batch| batch.deposits().keys().copied())
        .collect();
    assert_eq!(selected, (0..20).collect::<Vec<_>>());
    read_state(|state| assert_eq!(state.spl_deposits().queued().len(), 20));
}

#[test]
fn should_include_signatures_and_destinations_in_shared_owner_batches() {
    queue_deposits(20, true, false, Some(TokenProgram::Token2022));
    let round = read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()));

    assert!(round.batches.len() > 1);
    assert!(!round.leaves_deposits_queued);
    assert_batches_are_maximal(&round);
    for batch in &round.batches {
        let message = batch.sweep_message(Hash::default());
        assert_eq!(message.header.num_required_signatures, 2);
        assert_eq!(message.instructions.len(), batch.deposits().len() * 2);
        assert!(wire_size(batch) <= MAX_TX_SIZE);
    }
    let selected: Vec<_> = round
        .batches
        .iter()
        .flat_map(|batch| batch.deposits().keys().copied())
        .collect();
    assert_eq!(selected, (0..20).collect::<Vec<_>>());
}

#[test]
fn should_leave_deposits_queued_when_the_round_limit_is_reached() {
    let count = 100;
    queue_deposits(count, false, true, Some(TokenProgram::Classic));
    let round = read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()));

    assert_eq!(round.batches.len(), MAX_CONCURRENT_RPC_CALLS);
    assert!(round.leaves_deposits_queued);
    let selected: Vec<_> = round
        .batches
        .iter()
        .flat_map(|batch| batch.deposits().keys().copied())
        .collect();
    assert!(selected.len() < count);
    assert_eq!(selected, (0..selected.len() as u64).collect::<Vec<_>>());
    for batch in &round.batches {
        assert!(wire_size(batch) <= MAX_TX_SIZE);
    }
    read_state(|state| assert_eq!(state.spl_deposits().queued().len(), count));
}

#[test]
fn should_limit_distinct_owners_and_mints_by_actual_size() {
    queue_deposits(20, false, false, None);
    let round = read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()));

    assert_eq!(round.batches[0].deposits().len(), 4);
    assert!(!round.leaves_deposits_queued);
    assert_batches_are_maximal(&round);
    for batch in &round.batches {
        assert!(wire_size(batch) <= MAX_TX_SIZE);
    }
}

#[test]
fn should_pack_more_deposits_when_owners_and_mints_are_shared() {
    let runtime = TestCanisterRuntime::new().with_increasing_time();
    lifecycle::init(valid_init_args(), runtime.clone());
    for mint_index in 0..4 {
        let mint = [mint_index as u8 + 1; 32].into();
        let token_program = if mint_index % 2 == 0 {
            TokenProgram::Classic
        } else {
            TokenProgram::Token2022
        };
        mutate_state(|state| {
            process_event(
                state,
                EventType::AddedSplToken(spl_token(mint, token_program)),
                &runtime,
            )
        });
        for owner_index in 0..3 {
            let account = account(owner_index + 1);
            let deposit = queued_spl_deposit(account, mint, token_program);
            mutate_state(|state| {
                process_event(
                    state,
                    EventType::QueuedSplDeposit {
                        deposit_id: (mint_index * 3 + owner_index) as u64,
                        account,
                        mint,
                        address: deposit.address,
                        balance: deposit.balance,
                    },
                    &runtime,
                )
            });
        }
    }

    let round = read_state(|state| SweepRound::take_from_queue(state, &schnorr_master_key()));

    assert_eq!(round.batches.len(), 2);
    assert_eq!(round.batches[0].deposits().len(), 9);
    assert_eq!(round.batches[1].deposits().len(), 3);
    assert!(!round.leaves_deposits_queued);
    assert_batches_are_maximal(&round);
    for batch in &round.batches {
        assert!(wire_size(batch) <= MAX_TX_SIZE);
    }
}

#[test]
fn should_fit_a_single_deposit_in_a_sweep_of_its_own() {
    for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
        let mint = [1; 32].into();
        let tokens = BTreeMap::from([(mint, spl_token(mint, token_program))]);
        let sweep = SplSweep::plan(
            [(0, queued_spl_deposit(account(1), mint, token_program))],
            &tokens,
            &schnorr_master_key(),
        );
        let message = sweep.sweep_message(Hash::default());

        assert_eq!(transaction_size(&message), wire_size(&sweep));
        assert!(transaction_size(&message) <= MAX_TX_SIZE);
        assert!(u64::from(message.header.num_required_signatures) <= MAX_SIGNATURES);
    }
}
