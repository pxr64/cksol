use super::*;
use crate::test_fixtures::signer::{sign_as_minter, sign_for};
use crate::{
    address::{derivation_path, derive_public_key},
    constants::FEE_PER_SIGNATURE,
    state::{event::VersionedMessage, read_state},
    test_fixtures::{
        MINTER_ADDRESS, account_signature, init_schnorr_master_key, init_state, minter_signature,
        runtime::TestCanisterRuntime,
    },
};
use assert_matches::assert_matches;
use candid::Principal;
use ic_cdk::call::CallRejected;
use ic_cdk_management_canister::SignCallError;
use solana_address::Address;

fn setup() {
    init_state();
    init_schnorr_master_key();
}

fn derive_address(account: &Account) -> Address {
    let master_key = read_state(|s| s.minter_public_key().cloned().unwrap());
    Address::from(derive_public_key(&master_key, derivation_path(account)).serialize_raw())
}

fn minter_signing_once() -> TestCanisterRuntime {
    TestCanisterRuntime::new().add_signer(sign_as_minter())
}

/// Extracts the transfer amount (in lamports) from a compiled system program
/// transfer instruction. The data layout is:
///   [u32 LE instruction index = 2, u64 LE amount]
fn transfer_amount_from_instruction(instruction: &solana_transaction::CompiledInstruction) -> u64 {
    assert_eq!(instruction.data.len(), 12);
    u64::from_le_bytes(instruction.data[4..12].try_into().unwrap())
}

mod spl_sweep_tests {
    use super::*;
    use crate::state::TokenProgram;
    use solana_transaction::AccountMeta;

    fn queued_spl_deposit(token_program: TokenProgram) -> (QueuedSplDeposit, SupportedSplToken) {
        let account = Account {
            owner: Principal::from_slice(&[5]),
            subaccount: None,
        };
        let token = SupportedSplToken {
            mint: Address::from([3; 32]),
            token_program,
            decimals: 6,
            ledger_id: Principal::from_slice(&[4]),
            minimum_deposit_amount: 100,
            paused: false,
        };
        let deposit = QueuedSplDeposit {
            account,
            mint: token.mint,
            address: spl_deposit_address(
                &derive_address(&account),
                &token.mint,
                &token_program.id(),
            ),
            balance: 1_000_000,
        };
        (deposit, token)
    }

    #[tokio::test]
    async fn should_sign_spl_sweep_with_minter_and_deposit_owner() {
        setup();
        for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
            let (deposit, token) = queued_spl_deposit(token_program);
            let blockhash = Hash::new_from_array([0xBB; 32]);
            let runtime = TestCanisterRuntime::new()
                .add_signer(sign_as_minter())
                .add_signer(sign_for(&deposit.account));

            let (transaction, signers) =
                sign_spl_sweep_transaction(&runtime, &deposit, &token, blockhash)
                    .await
                    .expect("signing should succeed");

            assert_eq!(
                signers,
                vec![Signer::Minter, Signer::Account(deposit.account)]
            );
            assert_eq!(
                transaction.signatures,
                vec![minter_signature(), account_signature(&deposit.account)]
            );
            assert_eq!(
                transaction.message,
                create_spl_sweep_message(
                    &deposit,
                    &token,
                    &derive_address(&deposit.account),
                    &MINTER_ADDRESS,
                    blockhash,
                )
            );
        }
    }

    #[tokio::test]
    async fn should_fail_when_spl_sweep_signing_is_rejected() {
        setup();
        for minter_fails in [true, false] {
            let (deposit, token) = queued_spl_deposit(TokenProgram::Classic);
            let error = SignCallError::CallFailed(
                CallRejected::with_rejection(4, "signing service unavailable".to_string()).into(),
            );
            let runtime = if minter_fails {
                TestCanisterRuntime::new().add_signer(sign_as_minter().expect([Err(error)]))
            } else {
                TestCanisterRuntime::new()
                    .add_signer(sign_as_minter())
                    .add_signer(sign_for(&deposit.account).expect([Err(error)]))
            };

            let result = sign_spl_sweep_transaction(
                &runtime,
                &deposit,
                &token,
                Hash::new_from_array([0xBB; 32]),
            )
            .await;

            assert_matches!(result, Err(CreateTransferError::SigningFailed(_)));
        }
    }

    #[test]
    fn should_build_spl_sweep_message_with_minter_as_fee_payer() {
        for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
            let owner = Address::from([1; 32]);
            let minter = Address::from([2; 32]);
            let token = SupportedSplToken {
                mint: Address::from([3; 32]),
                token_program,
                decimals: 6,
                ledger_id: Principal::from_slice(&[4]),
                minimum_deposit_amount: 100,
                paused: false,
            };
            let deposit = QueuedSplDeposit {
                account: Account {
                    owner: Principal::from_slice(&[5]),
                    subaccount: None,
                },
                mint: token.mint,
                address: spl_deposit_address(&owner, &token.mint, &token_program.id()),
                balance: 1_000_000,
            };
            let blockhash = Hash::new_from_array([0xBB; 32]);

            let message = create_spl_sweep_message(&deposit, &token, &owner, &minter, blockhash);

            assert_eq!(message.account_keys[0], minter);
            assert_eq!(message.signer_keys(), vec![&minter, &owner]);
            assert_eq!(message.recent_blockhash, blockhash);
            assert_eq!(message.instructions.len(), 2);

            let create_destination =
                spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent(
                    &minter.to_bytes().into(),
                    &minter.to_bytes().into(),
                    &token.mint.to_bytes().into(),
                    &token_program.id().to_bytes().into(),
                );
            let transfer = create_spl_sweep_instruction(&deposit, &token, &owner, &minter);
            for (compiled, instruction) in message
                .instructions
                .iter()
                .zip([create_destination, transfer])
            {
                assert_eq!(
                    message.account_keys[compiled.program_id_index as usize],
                    instruction.program_id
                );
                assert_eq!(compiled.data, instruction.data);
                assert_eq!(
                    compiled
                        .accounts
                        .iter()
                        .map(|index| message.account_keys[*index as usize])
                        .collect::<Vec<_>>(),
                    instruction
                        .accounts
                        .iter()
                        .map(|account| account.pubkey)
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn should_build_checked_spl_sweep_transfer() {
        for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
            let owner = Address::from([1; 32]);
            let minter = Address::from([2; 32]);
            let mint = Address::from([3; 32]);
            let token = SupportedSplToken {
                mint,
                token_program,
                decimals: 6,
                ledger_id: Principal::from_slice(&[4]),
                minimum_deposit_amount: 100,
                paused: false,
            };
            let source = spl_deposit_address(&owner, &mint, &token_program.id());
            let destination = spl_deposit_address(&minter, &mint, &token_program.id());
            for balance in [100, 1_000_000, u64::MAX] {
                let deposit = QueuedSplDeposit {
                    account: Account {
                        owner: Principal::from_slice(&[5]),
                        subaccount: None,
                    },
                    mint,
                    address: source,
                    balance,
                };
                let instruction = create_spl_sweep_instruction(&deposit, &token, &owner, &minter);

                assert_eq!(instruction.program_id, token_program.id());
                assert_eq!(
                    instruction.accounts,
                    vec![
                        AccountMeta::new(source, false),
                        AccountMeta::new_readonly(mint, false),
                        AccountMeta::new(destination, false),
                        AccountMeta::new_readonly(owner, true),
                    ]
                );
                assert_matches!(
                    spl_token_2022_interface::instruction::TokenInstruction::unpack(
                        &instruction.data
                    )
                    .unwrap(),
                    spl_token_2022_interface::instruction::TokenInstruction::TransferChecked {
                        amount,
                        decimals: 6,
                    } if amount == balance
                );
            }
        }
    }
}

mod sweep_tests {
    use super::*;
    use crate::test_fixtures::{planned_sweep, queued_deposit_of};

    #[tokio::test]
    async fn should_sign_a_sweep_with_a_single_deposit() {
        setup();
        let account = Account {
            owner: Principal::from_slice(&[1, 2, 3]),
            subaccount: None,
        };
        let amount: Lamport = 500_000_000;
        let blockhash = Hash::new_from_array([0xBB; 32]);
        let sweep = planned_sweep([(0, queued_deposit_of(account, amount))]);
        let runtime = TestCanisterRuntime::new().add_signer(sign_for(&account));

        let (tx, signers) = sign_sweep_transaction(&runtime, &sweep, blockhash)
            .await
            .expect("signing should succeed");

        assert_eq!(signers, vec![Signer::Account(account)]);
        assert_eq!(tx.message.account_keys[0], derive_address(&account));
        assert!(tx.message.account_keys.contains(&MINTER_ADDRESS));
        assert_eq!(tx.message.instructions.len(), 1);
        assert_eq!(
            transfer_amount_from_instruction(&tx.message.instructions[0]),
            amount - FEE_PER_SIGNATURE
        );
        assert_eq!(tx.signatures, vec![account_signature(&account)]);
        assert_eq!(tx.message.recent_blockhash, blockhash);
    }

    #[tokio::test]
    async fn should_sign_with_the_deposits_in_the_order_of_the_message() {
        setup();
        let smaller = Account {
            owner: Principal::from_slice(&[1]),
            subaccount: None,
        };
        let larger = Account {
            owner: Principal::from_slice(&[2]),
            subaccount: None,
        };
        let sweep = planned_sweep([
            (0, queued_deposit_of(smaller, 100_000_000)),
            (1, queued_deposit_of(larger, 200_000_000)),
        ]);
        let runtime = TestCanisterRuntime::new()
            .add_signer(sign_for(&smaller))
            .add_signer(sign_for(&larger));

        let (tx, signers) =
            sign_sweep_transaction(&runtime, &sweep, Hash::new_from_array([0xDD; 32]))
                .await
                .expect("signing should succeed");

        assert_eq!(signers.len(), 2);
        assert_eq!(signers[0], Signer::Account(larger));
        let signer_addresses: Vec<Address> = signers
            .iter()
            .map(|signer| match signer {
                Signer::Account(account) => derive_address(account),
                Signer::Minter => panic!("BUG: the minter does not sign sweeps"),
            })
            .collect();
        assert_eq!(tx.message.account_keys[..2], signer_addresses);
        let signatures: Vec<_> = signers
            .iter()
            .map(|signer| match signer {
                Signer::Account(account) => account_signature(account),
                Signer::Minter => panic!("BUG: the minter does not sign sweeps"),
            })
            .collect();
        assert_eq!(tx.signatures, signatures);
    }

    #[tokio::test]
    async fn should_fail_when_signing_is_rejected() {
        setup();
        let account = Account {
            owner: Principal::from_slice(&[1]),
            subaccount: None,
        };
        let sweep = planned_sweep([(0, queued_deposit_of(account, 500_000_000))]);
        let runtime = TestCanisterRuntime::new().add_signer(sign_for(&account).expect([Err(
            SignCallError::CallFailed(
                CallRejected::with_rejection(4, "signing service unavailable".to_string()).into(),
            ),
        )]));

        let result =
            sign_sweep_transaction(&runtime, &sweep, Hash::new_from_array([0xBB; 32])).await;

        assert_matches!(result, Err(CreateTransferError::SigningFailed(_)));
    }
}

mod batch_withdrawal_tests {
    use super::*;

    #[tokio::test]
    async fn should_create_batch_withdrawal_with_single_target() {
        setup();
        let target = Address::new_from_array([0xAA; 32]);
        let amount: Lamport = 500_000_000;
        let blockhash = Hash::new_from_array([0xBB; 32]);

        let (tx, signers) = create_signed_batch_withdrawal_transaction(
            &minter_signing_once(),
            &[(target, amount)],
            blockhash,
        )
        .await
        .expect("transaction creation should succeed");

        assert_eq!(signers, vec![Signer::Minter]);
        assert_eq!(tx.signatures.len(), 1);
        assert_eq!(tx.signatures[0], minter_signature());
        assert_eq!(tx.message.account_keys[0], MINTER_ADDRESS);
        assert!(tx.message.account_keys.contains(&target));
        assert_eq!(tx.message.instructions.len(), 1);
        assert_eq!(tx.message.recent_blockhash, blockhash);
    }

    #[tokio::test]
    async fn should_create_batch_withdrawal_with_multiple_targets() {
        setup();
        let target_1 = Address::new_from_array([0xAA; 32]);
        let target_2 = Address::new_from_array([0xBB; 32]);
        let target_3 = Address::new_from_array([0xCC; 32]);
        let blockhash = Hash::new_from_array([0xDD; 32]);

        let (tx, signers) = create_signed_batch_withdrawal_transaction(
            &minter_signing_once(),
            &[(target_1, 100), (target_2, 200), (target_3, 300)],
            blockhash,
        )
        .await
        .expect("transaction creation should succeed");

        // Only the minter signs
        assert_eq!(signers, vec![Signer::Minter]);
        assert_eq!(tx.signatures.len(), 1);

        // Fee payer is at position 0
        assert_eq!(tx.message.account_keys[0], MINTER_ADDRESS);

        // All targets are in account keys
        assert!(tx.message.account_keys.contains(&target_1));
        assert!(tx.message.account_keys.contains(&target_2));
        assert!(tx.message.account_keys.contains(&target_3));

        // One instruction per target
        assert_eq!(tx.message.instructions.len(), 3);
    }

    #[tokio::test]
    async fn should_fail_when_signing_fails() {
        setup();
        let target = Address::new_from_array([0xAA; 32]);
        let blockhash = Hash::new_from_array([0xBB; 32]);

        let runtime = TestCanisterRuntime::new().add_signer(sign_as_minter().expect([Err(
            SignCallError::CallFailed(
                CallRejected::with_rejection(4, "signing service unavailable".to_string()).into(),
            ),
        )]));

        let result =
            create_signed_batch_withdrawal_transaction(&runtime, &[(target, 100)], blockhash).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn should_create_batch_withdrawal_at_max_capacity() {
        setup();
        let blockhash = Hash::new_from_array([0xDD; 32]);

        let targets: Vec<(Address, Lamport)> = (0..MAX_WITHDRAWALS_PER_TX)
            .map(|i| {
                let mut addr = [0u8; 32];
                addr[0] = i as u8;
                addr[1] = (i >> 8) as u8;
                (Address::new_from_array(addr), 1_000_000)
            })
            .collect();

        let (tx, signers) =
            create_signed_batch_withdrawal_transaction(&minter_signing_once(), &targets, blockhash)
                .await
                .expect("transaction creation should succeed at max capacity");

        assert_eq!(signers, vec![Signer::Minter]);
        assert_eq!(tx.signatures.len(), 1);
        assert_eq!(tx.message.instructions.len(), MAX_WITHDRAWALS_PER_TX);
    }

    #[tokio::test]
    async fn should_charge_the_fee_reserved_per_batch() {
        setup();
        let blockhash = Hash::new_from_array([0xDD; 32]);
        let targets: Vec<(Address, Lamport)> = (0..MAX_WITHDRAWALS_PER_TX)
            .map(|i| {
                let mut addr = [0u8; 32];
                addr[0] = i as u8;
                addr[1] = (i >> 8) as u8;
                (Address::new_from_array(addr), 1_000_000)
            })
            .collect();

        let (tx, _signers) =
            create_signed_batch_withdrawal_transaction(&minter_signing_once(), &targets, blockhash)
                .await
                .expect("transaction creation should succeed at max capacity");

        assert_eq!(
            VersionedMessage::Legacy(tx.message).transaction_fee(),
            BATCH_WITHDRAWAL_TX_FEE
        );
    }

    #[tokio::test]
    async fn should_return_error_when_exceeding_tx_size_limit() {
        setup();
        let blockhash = Hash::new_from_array([0xDD; 32]);

        // Each additional target adds ~49 bytes (32-byte key + 17-byte instruction).
        // With a base of ~166 bytes and MAX_TX_SIZE = 1232, the limit is around 21-22.
        // Use 25 targets to reliably exceed the limit.
        const NUM_TARGETS: usize = 25;
        let targets: Vec<(Address, Lamport)> = (0..NUM_TARGETS)
            .map(|i| {
                let mut addr = [0u8; 32];
                addr[0] = i as u8;
                (Address::new_from_array(addr), 1_000_000)
            })
            .collect();

        let result =
            create_signed_batch_withdrawal_transaction(&minter_signing_once(), &targets, blockhash)
                .await;

        assert_matches!(
            result,
            Err(CreateTransferError::TransactionTooLarge {
                max: MAX_TX_SIZE,
                ..
            })
        );
    }
}
