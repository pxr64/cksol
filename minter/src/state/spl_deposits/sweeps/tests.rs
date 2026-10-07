use super::*;
use crate::test_fixtures::{
    account, planned_spl_sweep, queued_spl_deposit, schnorr_master_key, signature, spl_token,
};
use assert_matches::assert_matches;

#[test]
fn should_recover_mixed_program_sweep_without_cached_master_key() {
    let classic = spl_token([1; 32].into(), TokenProgram::Classic);
    let token_2022 = spl_token([2; 32].into(), TokenProgram::Token2022);
    let deposits = vec![
        (
            0,
            queued_spl_deposit(account(1), classic.mint, classic.token_program),
        ),
        (
            1,
            queued_spl_deposit(account(2), classic.mint, classic.token_program),
        ),
        (
            2,
            queued_spl_deposit(account(1), token_2022.mint, token_2022.token_program),
        ),
    ];
    let tokens = BTreeMap::from([(classic.mint, classic), (token_2022.mint, token_2022)]);
    let sweep = SplSweep::plan(deposits.clone(), &tokens, &schnorr_master_key());
    let message = sweep.sweep_message(Hash::default());

    let recovered = SplSweep::recover(
        deposits,
        &tokens,
        &message.clone().into(),
        &sweep.signers(&message),
    )
    .unwrap();

    assert_eq!(recovered, sweep);
    assert_eq!(recovered.sweep_message(Hash::default()), message);
}

#[test]
fn should_reject_modified_submitted_sweep_message() {
    let token = spl_token([1; 32].into(), TokenProgram::Classic);
    let deposit = queued_spl_deposit(account(1), token.mint, token.token_program);
    let tokens = BTreeMap::from([(token.mint, token)]);
    let sweep = SplSweep::plan([(0, deposit.clone())], &tokens, &schnorr_master_key());
    let message = sweep.sweep_message(Hash::default());
    let recorded_signers = sweep.signers(&message);
    for index in 0..6 {
        let mut changed = message.clone();
        match index {
            0 => {
                changed.instructions[1].data =
                    spl_token_2022_interface::instruction::TokenInstruction::TransferChecked {
                        amount: deposit.balance + 1,
                        decimals: 6,
                    }
                    .pack()
            }
            1 => {
                changed.instructions[1].data =
                    spl_token_2022_interface::instruction::TokenInstruction::TransferChecked {
                        amount: deposit.balance,
                        decimals: 9,
                    }
                    .pack()
            }
            2 => changed.instructions[1].accounts[2] = changed.instructions[1].accounts[0],
            3 => {
                changed.instructions[1].program_id_index = changed.instructions[0].program_id_index
            }
            4 => {
                changed.instructions.remove(0);
            }
            5 => changed.instructions.push(changed.instructions[1].clone()),
            _ => unreachable!(),
        }
        assert_matches!(
            SplSweep::recover(
                [(0, deposit.clone())],
                &tokens,
                &changed.into(),
                &recorded_signers
            ),
            Err(SplSweepRecoveryError::UnexpectedMessage { .. })
        );
    }
}

#[test]
fn should_preserve_full_token_balances_in_sweep_messages() {
    for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
        let token = spl_token([1; 32].into(), token_program);
        let tokens = BTreeMap::from([(token.mint, token.clone())]);
        for balance in [1, 1_000_000, u64::MAX] {
            let deposit = QueuedSplDeposit {
                balance,
                ..queued_spl_deposit(account(1), token.mint, token_program)
            };
            let sweep = SplSweep::plan([(0, deposit)], &tokens, &schnorr_master_key());
            let message = sweep.sweep_message(Hash::default());
            assert_matches!(
                spl_token_2022_interface::instruction::TokenInstruction::unpack(
                    &message.instructions[1].data
                ).unwrap(),
                spl_token_2022_interface::instruction::TokenInstruction::TransferChecked {
                    amount, decimals: 6,
                } if amount == balance
            );
        }
    }
}

#[test]
fn should_reject_incorrect_recorded_signers() {
    let token = spl_token([1; 32].into(), TokenProgram::Classic);
    let deposit = queued_spl_deposit(account(1), token.mint, token.token_program);
    let tokens = BTreeMap::from([(token.mint, token)]);
    let sweep = SplSweep::plan([(0, deposit.clone())], &tokens, &schnorr_master_key());
    let message = sweep.sweep_message(Hash::default()).into();
    for recorded in [
        vec![],
        vec![Signer::Minter],
        vec![Signer::Account(account(1)), Signer::Minter],
        vec![Signer::Minter, Signer::Account(account(2))],
    ] {
        assert_matches!(
            SplSweep::recover([(0, deposit.clone())], &tokens, &message, &recorded),
            Err(SplSweepRecoveryError::InvalidSigners)
        );
    }
}

#[test]
fn should_reject_duplicated_recorded_signer() {
    let token = spl_token([1; 32].into(), TokenProgram::Classic);
    let deposits = [
        (
            0,
            queued_spl_deposit(account(1), token.mint, token.token_program),
        ),
        (
            1,
            queued_spl_deposit(account(2), token.mint, token.token_program),
        ),
    ];
    let tokens = BTreeMap::from([(token.mint, token)]);
    let sweep = SplSweep::plan(deposits.clone(), &tokens, &schnorr_master_key());
    let message = sweep.sweep_message(Hash::default()).into();
    let duplicated = vec![
        Signer::Minter,
        Signer::Account(account(1)),
        Signer::Account(account(1)),
    ];

    assert_matches!(
        SplSweep::recover(deposits, &tokens, &message, &duplicated),
        Err(SplSweepRecoveryError::InvalidSigners)
    );
}

#[test]
fn should_index_sweeps_by_signature_and_deposit_id() {
    let first = queued_spl_deposit(account(1), [1; 32].into(), TokenProgram::Classic);
    let second = queued_spl_deposit(account(2), first.mint, TokenProgram::Classic);
    let mut sweeps = SplSweeps::default();
    sweeps.insert(
        signature(1),
        planned_spl_sweep([(0, first.clone()), (1, second.clone())]),
    );

    assert_eq!(sweeps.len(), 1);
    assert_eq!(sweeps.deposit_count(), 2);
    assert_eq!(sweeps.deposit(0), Some((&signature(1), &first)));
    assert_eq!(sweeps.deposit(1), Some((&signature(1), &second)));
    assert_eq!(
        sweeps.signatures().copied().collect::<Vec<_>>(),
        vec![signature(1)]
    );
    assert_eq!(sweeps.iter().count(), 1);
    assert!(sweeps.remove(&signature(1)).is_some());
    assert!(sweeps.is_empty());
}

#[test]
#[should_panic(expected = "another sweep holds it")]
fn should_reject_deposit_in_two_sweeps() {
    let deposit = queued_spl_deposit(account(1), [1; 32].into(), TokenProgram::Classic);
    let sweep = planned_spl_sweep([(0, deposit)]);
    let mut sweeps = SplSweeps::default();
    sweeps.insert(signature(1), sweep.clone());
    sweeps.insert(signature(2), sweep);
}

#[test]
#[should_panic(expected = "Attempted to record SPL sweep")]
fn should_reject_duplicate_sweep_signature() {
    let mut sweeps = SplSweeps::default();
    sweeps.insert(
        signature(1),
        planned_spl_sweep([(
            0,
            queued_spl_deposit(account(1), [1; 32].into(), TokenProgram::Classic),
        )]),
    );
    sweeps.insert(
        signature(1),
        planned_spl_sweep([(
            1,
            queued_spl_deposit(account(2), [1; 32].into(), TokenProgram::Classic),
        )]),
    );
}

#[test]
#[should_panic(expected = "must be registered")]
fn should_reject_unregistered_mint_in_plan() {
    SplSweep::plan(
        [(
            0,
            queued_spl_deposit(account(1), [1; 32].into(), TokenProgram::Classic),
        )],
        &BTreeMap::new(),
        &schnorr_master_key(),
    );
}

#[test]
#[should_panic(expected = "SPL sweep source mismatch")]
fn should_reject_source_for_another_owner() {
    let mut deposit = queued_spl_deposit(account(1), [1; 32].into(), TokenProgram::Classic);
    deposit.address = queued_spl_deposit(account(2), deposit.mint, TokenProgram::Classic).address;
    planned_spl_sweep([(0, deposit)]);
}

#[test]
#[should_panic(expected = "SPL sweep mint mismatch")]
fn should_reject_mint_configuration_for_another_mint() {
    let deposit = queued_spl_deposit(account(1), [1; 32].into(), TokenProgram::Classic);
    let tokens = BTreeMap::from([(
        deposit.mint,
        spl_token([2; 32].into(), TokenProgram::Classic),
    )]);
    SplSweep::plan([(0, deposit)], &tokens, &schnorr_master_key());
}
