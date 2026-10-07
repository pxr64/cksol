use super::*;
use crate::test_fixtures::{
    account, planned_spl_sweep, queued_spl_deposit, schnorr_master_key, signature, spl_token,
};
use assert_matches::assert_matches;

mod settle {
    use super::*;
    use crate::test_fixtures::spl_sweep::{balanced_outcome, transaction_outcome as outcome};
    use solana_transaction::TransactionError;
    use solana_transaction_status_client_types::{
        EncodedTransaction, TransactionBinaryEncoding, UiTransactionError,
    };

    fn sweep() -> SplSweep {
        let tokens = BTreeMap::from([
            (
                [1; 32].into(),
                spl_token([1; 32].into(), TokenProgram::Classic),
            ),
            (
                [2; 32].into(),
                spl_token([2; 32].into(), TokenProgram::Token2022),
            ),
        ]);
        SplSweep::plan(
            [
                (
                    0,
                    queued_spl_deposit(account(1), [1; 32].into(), TokenProgram::Classic),
                ),
                (
                    1,
                    queued_spl_deposit(account(2), [1; 32].into(), TokenProgram::Classic),
                ),
                (
                    2,
                    queued_spl_deposit(account(1), [2; 32].into(), TokenProgram::Token2022),
                ),
            ],
            &tokens,
            &schnorr_master_key(),
        )
    }

    fn post_balances(
        outcome: &mut EncodedConfirmedTransactionWithStatusMeta,
    ) -> &mut Vec<UiTransactionTokenBalance> {
        let OptionSerializer::Some(balances) = &mut outcome
            .transaction
            .meta
            .as_mut()
            .unwrap()
            .post_token_balances
        else {
            panic!("Expected token balances")
        };
        balances
    }

    #[test]
    fn should_validate_successful_mixed_program_transaction() {
        let sweep = sweep();
        for blockhash in [Hash::default(), Hash::from([1; 32])] {
            for new_destinations in [false, true] {
                let outcome = balanced_outcome(&sweep, blockhash, new_destinations);
                let settled = sweep.clone().settle(&outcome).unwrap();
                assert_eq!(settled.lamports_spent(), 5_000);
                assert_eq!(
                    settled.into_mints(),
                    sweep
                        .deposits()
                        .iter()
                        .map(|(deposit_id, deposit)| CreditedSplDeposit {
                            deposit_id: *deposit_id,
                            amount_to_mint: deposit.balance,
                        })
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn should_validate_full_u64_token_amount() {
        let sweep = planned_spl_sweep([(
            0,
            QueuedSplDeposit {
                balance: u64::MAX,
                ..queued_spl_deposit(account(1), [1; 32].into(), TokenProgram::Classic)
            },
        )]);
        let outcome = balanced_outcome(&sweep, Hash::default(), true);
        let settled = sweep.settle(&outcome).unwrap();
        assert_eq!(settled.lamports_spent(), 5_000);
        assert_eq!(
            settled.into_mints(),
            vec![CreditedSplDeposit {
                deposit_id: 0,
                amount_to_mint: u64::MAX
            }]
        );
    }

    #[test]
    fn should_reject_incomplete_lamport_metadata() {
        let sweep = sweep();
        let mut outcome = balanced_outcome(&sweep, Hash::default(), true);
        outcome
            .transaction
            .meta
            .as_mut()
            .unwrap()
            .pre_balances
            .pop();
        assert_eq!(
            sweep.clone().settle(&outcome),
            Err(SweepSettlementError::Unreadable(
                UnreadableOutcome::IncompleteBalances
            ))
        );
    }

    #[test]
    fn should_reject_unexpected_source_or_destination_balance_changes() {
        let sweep = sweep();
        let original = balanced_outcome(&sweep, Hash::default(), false);
        let count = original
            .transaction
            .meta
            .as_ref()
            .unwrap()
            .post_token_balances
            .as_ref()
            .unwrap()
            .len();
        for index in 0..count {
            for change in [-1i128, 1] {
                let mut outcome = balanced_outcome(&sweep, Hash::default(), false);
                let balance = &mut post_balances(&mut outcome)[index];
                let amount: u64 = balance.ui_token_amount.amount.parse().unwrap();
                balance.ui_token_amount.amount = (i128::from(amount) + change).to_string();
                assert_matches!(
                    sweep.clone().settle(&outcome),
                    Err(SweepSettlementError::Mismatch(
                        SweepMismatch::UnexpectedTokenBalanceChange { .. }
                    ))
                );
            }
        }
    }

    #[test]
    fn should_reject_token_identity_mismatch() {
        let sweep = sweep();
        for is_pre in [true, false] {
            for field in 0..4 {
                let mut outcome = balanced_outcome(&sweep, Hash::default(), false);
                let meta = outcome.transaction.meta.as_mut().unwrap();
                let balances = if is_pre {
                    &mut meta.pre_token_balances
                } else {
                    &mut meta.post_token_balances
                };
                let OptionSerializer::Some(balances) = balances else {
                    unreachable!()
                };
                let balance = &mut balances[0];
                match field {
                    0 => balance.mint = Address::default().to_string(),
                    1 => balance.owner = OptionSerializer::Some(Address::default().to_string()),
                    2 => {
                        balance.program_id = OptionSerializer::Some(Address::default().to_string())
                    }
                    3 => balance.ui_token_amount.decimals += 1,
                    _ => unreachable!(),
                }
                assert_matches!(
                    sweep.clone().settle(&outcome),
                    Err(SweepSettlementError::Mismatch(
                        SweepMismatch::UnexpectedTokenAccount { .. }
                    ))
                );
            }
        }
    }

    #[test]
    fn should_return_main_account_spend_including_destination_rent() {
        let sweep = sweep();
        for rent in [0, 2_039_280, 2 * 2_039_280] {
            let mut outcome = balanced_outcome(&sweep, Hash::default(), rent > 0);
            let meta = outcome.transaction.meta.as_mut().unwrap();
            meta.pre_balances[0] = 10_000_000;
            meta.post_balances[0] = meta.pre_balances[0] - meta.fee - rent;
            assert_eq!(
                sweep.clone().settle(&outcome).unwrap().lamports_spent(),
                5_000 + rent
            );
        }
    }

    #[test]
    fn should_reject_main_account_spend_below_transaction_fee() {
        let sweep = sweep();
        for change in [-4_999i128, 0, 1] {
            let mut outcome = balanced_outcome(&sweep, Hash::default(), false);
            let meta = outcome.transaction.meta.as_mut().unwrap();
            meta.post_balances[0] = (i128::from(meta.pre_balances[0]) + change) as u64;
            let expected = SweepMismatch::UnexpectedLamportsSpent {
                pre: meta.pre_balances[0],
                post: meta.post_balances[0],
                fee: meta.fee,
            };
            assert_eq!(
                sweep.clone().settle(&outcome),
                Err(SweepSettlementError::Mismatch(expected))
            );
        }
    }

    #[test]
    fn should_reject_incomplete_token_metadata() {
        let sweep = sweep();
        for deviation in 0..7 {
            let mut outcome = balanced_outcome(&sweep, Hash::default(), false);
            match deviation {
                0 => {
                    outcome
                        .transaction
                        .meta
                        .as_mut()
                        .unwrap()
                        .pre_token_balances = OptionSerializer::None
                }
                1 => {
                    outcome
                        .transaction
                        .meta
                        .as_mut()
                        .unwrap()
                        .post_token_balances = OptionSerializer::Skip
                }
                2 => {
                    post_balances(&mut outcome).pop();
                }
                3 => post_balances(&mut outcome)[0].owner = OptionSerializer::Skip,
                4 => post_balances(&mut outcome)[0].program_id = OptionSerializer::None,
                5 => {
                    let OptionSerializer::Some(pre) = &mut outcome
                        .transaction
                        .meta
                        .as_mut()
                        .unwrap()
                        .pre_token_balances
                    else {
                        unreachable!()
                    };
                    pre.pop();
                }
                6 => {
                    let source_index = sweep
                        .sweep_message(Hash::default())
                        .account_keys
                        .iter()
                        .position(|key| *key == sweep.deposits()[&0].address)
                        .unwrap() as u8;
                    let meta = outcome.transaction.meta.as_mut().unwrap();
                    meta.pre_balances[source_index as usize] = 0;
                    let OptionSerializer::Some(pre) = &mut meta.pre_token_balances else {
                        unreachable!()
                    };
                    pre.retain(|balance| balance.account_index != source_index);
                }
                _ => unreachable!(),
            }
            assert_eq!(
                sweep.clone().settle(&outcome),
                Err(SweepSettlementError::Unreadable(
                    UnreadableOutcome::IncompleteTokenBalances
                )),
                "deviation {deviation}"
            );
        }
    }

    #[test]
    fn should_reject_malformed_token_metadata() {
        let sweep = sweep();
        for deviation in 0..4 {
            let mut outcome = balanced_outcome(&sweep, Hash::default(), false);
            let balances = post_balances(&mut outcome);
            match deviation {
                0 => balances[0].ui_token_amount.amount = "1.5".to_string(),
                1 => balances[0].ui_token_amount.amount = "18446744073709551616".to_string(),
                2 => balances[0].account_index = u8::MAX,
                3 => balances.push(balances[0].clone()),
                _ => unreachable!(),
            }
            assert_matches!(
                sweep.clone().settle(&outcome),
                Err(SweepSettlementError::Unreadable(
                    UnreadableOutcome::InvalidTokenBalances { .. }
                ))
            );
        }
    }

    #[test]
    fn should_reject_unreadable_transaction_outcome() {
        let sweep = sweep();
        let mut unreadable = outcome(solana_message::VersionedMessage::Legacy(
            sweep.sweep_message(Hash::default()),
        ));
        unreadable.transaction.transaction = EncodedTransaction::Binary(
            "invalid base64!".to_string(),
            TransactionBinaryEncoding::Base64,
        );
        assert_eq!(
            sweep.clone().settle(&unreadable),
            Err(SweepSettlementError::Unreadable(
                UnreadableOutcome::TransactionDecodingFailed
            )),
        );
        let mut missing_meta = outcome(solana_message::VersionedMessage::Legacy(
            sweep.sweep_message(Hash::default()),
        ));
        missing_meta.transaction.meta = None;
        assert_eq!(
            sweep.clone().settle(&missing_meta),
            Err(SweepSettlementError::Unreadable(
                UnreadableOutcome::NoMetaField
            )),
        );
    }

    #[test]
    fn should_reject_failed_transaction() {
        let sweep = sweep();
        let mut outcome = outcome(solana_message::VersionedMessage::Legacy(
            sweep.sweep_message(Hash::default()),
        ));
        let error = UiTransactionError::from(TransactionError::AccountNotFound);
        let meta = outcome.transaction.meta.as_mut().unwrap();
        meta.err = Some(error.clone());
        meta.status = Err(error.clone());
        assert_eq!(
            sweep.clone().settle(&outcome),
            Err(SweepSettlementError::Mismatch(
                SweepMismatch::TransactionFailed {
                    error: error.to_string(),
                }
            )),
        );
    }

    #[test]
    fn should_reject_changed_executed_message() {
        let sweep = sweep();
        let planned = sweep.sweep_message(Hash::default());
        for index in 0..5 {
            let mut message = planned.clone();
            match index {
                0 => message.instructions[1].data[1] ^= 1,
                1 => message.instructions[1].accounts[2] = message.instructions[1].accounts[0],
                2 => {
                    message.instructions[1].program_id_index =
                        message.instructions[0].program_id_index
                }
                3 => {
                    message.instructions.remove(0);
                }
                4 => message.instructions.push(message.instructions[1].clone()),
                _ => unreachable!(),
            }
            let outcome = outcome(solana_message::VersionedMessage::Legacy(message));
            assert_eq!(
                sweep.clone().settle(&outcome),
                Err(SweepSettlementError::Mismatch(
                    SweepMismatch::UnexpectedMessage
                )),
                "deviation {index}",
            );
        }
    }

    #[test]
    fn should_reject_versioned_executed_message() {
        let sweep = sweep();
        let message = sweep.sweep_message(Hash::default());
        let outcome = outcome(solana_message::VersionedMessage::V0(
            solana_message::v0::Message {
                header: message.header,
                account_keys: message.account_keys,
                recent_blockhash: message.recent_blockhash,
                instructions: message.instructions,
                address_table_lookups: vec![],
            },
        ));
        assert_eq!(
            sweep.clone().settle(&outcome),
            Err(SweepSettlementError::Mismatch(
                SweepMismatch::UnexpectedMessage
            )),
        );
    }
}

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
