use assert_matches::assert_matches;
use assert2::check;
use base64::{Engine, engine::general_purpose::STANDARD};
use candid::Principal;
use cksol_int_tests::{
    Setup, SetupBuilder,
    fixtures::{
        FINALIZE_TRANSACTIONS_DELAY, MINTER_ADDRESS, MockBuilder, PROCESS_PENDING_MINTS_DELAY,
        SUBMISSION_BLOCK_HEIGHT, SWEEP_DEPOSITS_DELAY,
    },
    validator::FEE_PER_SIGNATURE,
};
use cksol_types::{
    AddSplTokenArgs, AddSplTokenError, DepositSplArgs, DepositSplId, GetDepositAddressArgs,
    GetSplDepositAddressArgs, Lamport, Signature,
};
use cksol_types_internal::event::{EventType, TransactionPurpose, VersionedTransactionMessage};
use ic_pocket_canister_runtime::JsonRpcResponse;
use icrc_ledger_types::icrc1::account::Account;
use serde_json::json;
use sol_rpc_types::{AccountData, AccountEncoding, AccountInfo};
use solana_address::{Address, address};
use solana_program_pack::Pack;
use spl_associated_token_account_interface::address::get_associated_token_address_with_program_id;
use spl_token_2022_interface::{
    extension::{
        BaseStateWithExtensionsMut, ExtensionType, StateWithExtensionsMut,
        immutable_owner::ImmutableOwner,
    },
    state::{Account as TokenAccount, AccountState},
};
use spl_token_interface::state::Mint;

pub const MINT: Address = address!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
pub const OTHER_MINT: Address = address!("So11111111111111111111111111111111111111112");
pub const DECIMALS: u8 = 6;
pub const MINIMUM_DEPOSIT_AMOUNT: u64 = 1_000_000;
/// Fee in the token's smallest units, quoted from the mocked exchange prices.
pub const DEPOSIT_FEE: u64 = 1;
pub const BALANCE_ABOVE_MINIMUM: u64 = MINIMUM_DEPOSIT_AMOUNT + DEPOSIT_FEE;
pub const RENT_EXEMPT_LAMPORTS: Lamport = 2_039_280;

pub fn associated_token_address(owner: &Address, token_program: &Address) -> Address {
    get_associated_token_address_with_program_id(
        &owner.to_bytes().into(),
        &MINT.to_bytes().into(),
        &token_program.to_bytes().into(),
    )
    .to_bytes()
    .into()
}

pub fn account_info(data: Vec<u8>, token_program: Address) -> AccountInfo {
    AccountInfo {
        lamports: RENT_EXEMPT_LAMPORTS,
        space: data.len() as u64,
        data: AccountData::Binary(STANDARD.encode(data), AccountEncoding::Base64),
        owner: token_program.to_string(),
        executable: false,
        rent_epoch: 0,
    }
}

fn mint_data() -> Vec<u8> {
    let mut data = vec![0; Mint::LEN];
    Mint::pack(
        Mint {
            supply: BALANCE_ABOVE_MINIMUM,
            decimals: DECIMALS,
            is_initialized: true,
            ..Mint::default()
        },
        &mut data,
    )
    .unwrap();
    data
}

pub fn token_account_data(owner: &Address, extensions: &[ExtensionType]) -> Vec<u8> {
    token_account(
        owner,
        extensions,
        BALANCE_ABOVE_MINIMUM,
        AccountState::Initialized,
    )
}

pub fn token_account_data_with_amount(owner: &Address, amount: u64) -> Vec<u8> {
    token_account(owner, &[], amount, AccountState::Initialized)
}

pub fn frozen_token_account_data(owner: &Address) -> Vec<u8> {
    token_account(owner, &[], BALANCE_ABOVE_MINIMUM, AccountState::Frozen)
}

/// Both token programs share the base layout, so one builder serves both.
fn token_account(
    owner: &Address,
    extensions: &[ExtensionType],
    amount: u64,
    state: AccountState,
) -> Vec<u8> {
    let mut data =
        vec![0; ExtensionType::try_calculate_account_len::<TokenAccount>(extensions).unwrap()];
    let mut account =
        StateWithExtensionsMut::<TokenAccount>::unpack_uninitialized(&mut data).unwrap();
    for extension in extensions {
        match extension {
            ExtensionType::ImmutableOwner => {
                account.init_extension::<ImmutableOwner>(false).unwrap();
            }
            _ => unimplemented!("{extension:?}"),
        }
    }
    account.base = TokenAccount {
        mint: MINT.to_bytes().into(),
        owner: owner.to_bytes().into(),
        amount,
        state,
        ..TokenAccount::default()
    };
    account.pack_base();
    account.init_account_type().unwrap();
    data
}

/// Installs a token ledger and registers the mint on it. The mint account read uses
/// RPC IDs 0 to 3.
pub async fn setup_with_registered_mint(token_program: Address) -> Setup {
    let setup = SetupBuilder::new()
        .with_proxy_canister()
        .with_ledger_suite_orchestrator(Setup::DEFAULT_CALLER)
        .build()
        .await;
    setup.fund_minter().await;
    setup.top_up_operating_balance().await;
    let ledger = setup.install_spl_ledger(DECIMALS).await;
    register_token(&setup, ledger.canister_id(), token_program, 32).await;
    setup
}

pub fn add_spl_token_args(ledger_id: Principal, token_program: Address) -> AddSplTokenArgs {
    AddSplTokenArgs {
        mint: MINT.into(),
        token_program: token_program.into(),
        decimals: DECIMALS,
        ledger_id,
        minimum_deposit_amount: MINIMUM_DEPOSIT_AMOUNT,
        paused: false,
        xrc_symbol: "SPL".to_string(),
    }
}

/// Registers [`MINT`] on the ledger. The mint account read uses RPC IDs `start_id` to
/// `start_id + 3`.
pub async fn register_token(
    setup: &Setup,
    ledger_id: Principal,
    token_program: Address,
    start_id: u64,
) {
    try_register_token(
        setup,
        add_spl_token_args(ledger_id, token_program),
        start_id,
    )
    .await
    .expect("add_spl_token should register the token");
}

/// Registers the token of the arguments, answering the read of its mint account with
/// RPC IDs `start_id` to `start_id + 3`. The mint account has [`DECIMALS`] decimals.
pub async fn try_register_token(
    setup: &Setup,
    args: AddSplTokenArgs,
    start_id: u64,
) -> Result<(), AddSplTokenError> {
    let token_program = Address::from(args.token_program.clone());
    let mint = Address::from(args.mint.clone());
    setup
        .minter()
        .with_http_mocks(
            MockBuilder::with_start_id(start_id)
                .get_account_info(&mint, Some(account_info(mint_data(), token_program)))
                .build(),
        )
        .add_spl_token(args)
        .await
}

/// Uses the recorded message, with an outcome where the given amount moved from the
/// source to the treasury ATA. `destination_balance` is `None` when the sweep creates it.
pub fn sweep_transaction_response(
    signature: &Signature,
    transaction: VersionedTransactionMessage,
    owner: &Address,
    token_program: &Address,
    amount: u64,
    destination_balance: Option<u64>,
) -> JsonRpcResponse {
    let bytes = assert_matches!(transaction, VersionedTransactionMessage::Legacy(bytes) => bytes);
    let message: solana_message::Message = bincode::deserialize(&bytes).unwrap();
    let source = associated_token_address(owner, token_program);
    let destination = associated_token_address(&MINTER_ADDRESS, token_program);
    let index = |address: &Address| {
        message
            .account_keys
            .iter()
            .position(|key| key == address)
            .unwrap()
    };
    let source_index = index(&source);
    let destination_index = index(&destination);
    let fee = u64::from(message.header.num_required_signatures) * FEE_PER_SIGNATURE;
    let mut pre_balances = vec![1; message.account_keys.len()];
    pre_balances[0] = 5_000_000_000;
    pre_balances[source_index] = RENT_EXEMPT_LAMPORTS;
    pre_balances[destination_index] = if destination_balance.is_some() {
        RENT_EXEMPT_LAMPORTS
    } else {
        0
    };
    let mut post_balances = pre_balances.clone();
    post_balances[0] -= fee + RENT_EXEMPT_LAMPORTS - pre_balances[destination_index];
    post_balances[destination_index] = RENT_EXEMPT_LAMPORTS;
    let token_balance = |account_index: usize, owner: &Address, amount: u64| {
        json!({
            "accountIndex": account_index,
            "mint": MINT.to_string(),
            "owner": owner.to_string(),
            "programId": token_program.to_string(),
            "uiTokenAmount": {
                "amount": amount.to_string(),
                "decimals": DECIMALS,
                "uiAmount": null,
                "uiAmountString": solana_account_decoder_client_types::token::real_number_string_trimmed(amount, DECIMALS)
            }
        })
    };
    let mut transaction = solana_transaction::Transaction::new_unsigned(message);
    transaction.signatures[0] = signature.clone().into();
    let encoded = STANDARD.encode(bincode::serialize(&transaction).unwrap());
    let mut pre_token_balances = vec![token_balance(source_index, owner, amount)];
    if let Some(balance) = destination_balance {
        pre_token_balances.push(token_balance(destination_index, &MINTER_ADDRESS, balance));
    }
    JsonRpcResponse::from(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "slot": 350_000_000_u64,
            "blockTime": 1700000000_i64,
            "transaction": [encoded, "base64"],
            "meta": {
                "err": null,
                "status": {"Ok": null},
                "fee": fee,
                "preBalances": pre_balances,
                "postBalances": post_balances,
                "preTokenBalances": pre_token_balances,
                "postTokenBalances": [
                    token_balance(source_index, owner, 0),
                    token_balance(destination_index, &MINTER_ADDRESS, destination_balance.unwrap_or(0) + amount)
                ],
                "innerInstructions": [],
                "logMessages": [],
                "loadedAddresses": {"readonly": [], "writable": []},
                "rewards": []
            }
        }
    }))
}

/// Registers the mint on a fresh SPL ledger, then queues a deposit from the caller's
/// associated token account and checks the recorded events.
pub async fn queue_deposit_for_caller(token_program: Address, extensions: &[ExtensionType]) {
    let setup = SetupBuilder::new()
        .with_proxy_canister()
        .with_ledger_suite_orchestrator(Setup::DEFAULT_CALLER)
        .build()
        .await;
    let ledger = setup.install_spl_ledger(DECIMALS).await;
    let subaccount = Some([1; 32]);
    let owner: Address = setup
        .minter()
        .get_deposit_address(GetDepositAddressArgs {
            owner: None,
            subaccount,
        })
        .await
        .into();
    let source = associated_token_address(&owner, &token_program);
    let destination = associated_token_address(&MINTER_ADDRESS, &token_program);
    register_token(&setup, ledger.canister_id(), token_program, 0).await;
    assert_eq!(
        setup
            .minter()
            .get_spl_deposit_address(GetSplDepositAddressArgs {
                owner: None,
                subaccount,
                mint: MINT.into(),
            })
            .await,
        Ok(source.into())
    );

    let deposit_id = setup
        .minter()
        .with_http_mocks(
            MockBuilder::with_start_id(4)
                .get_account_info(
                    &source,
                    Some(account_info(
                        token_account_data(&owner, extensions),
                        token_program,
                    )),
                )
                .get_account_info(&destination, None)
                .build(),
        )
        .deposit_spl(DepositSplArgs {
            owner: None,
            subaccount,
            mint: MINT.into(),
        })
        .await
        .expect("deposit_spl should queue a sweep");

    assert_eq!(deposit_id, 0);
    setup.minter().assert_that_events().await.satisfy(|events| {
        check!(
            events[2..]
                == [
                    EventType::AddedSplToken {
                        mint: MINT.into(),
                        token_program: token_program.into(),
                        decimals: DECIMALS,
                        ledger_id: ledger.canister_id(),
                        minimum_deposit_amount: MINIMUM_DEPOSIT_AMOUNT,
                        paused: false,
                        xrc_symbol: "SPL".to_string(),
                    },
                    EventType::QueuedSplDeposit {
                        deposit_id,
                        account: Account {
                            owner: setup.proxy_canister_id(),
                            subaccount,
                        },
                        mint: MINT.into(),
                        address: source.into(),
                        balance: BALANCE_ABOVE_MINIMUM,
                        fee: DEPOSIT_FEE,
                    },
                ]
        );
    });

    setup.drop().await;
}

/// Queues a deposit of the full test balance for the account, with an absent treasury
/// account. The two account reads use RPC IDs `start_id` to `start_id + 7`.
pub async fn queue_deposit(
    setup: &Setup,
    account: Account,
    token_program: Address,
    extensions: &[ExtensionType],
    start_id: u64,
) -> DepositSplId {
    let owner: Address = setup.minter().get_deposit_address(account).await.into();
    setup
        .minter()
        .with_http_mocks(
            MockBuilder::with_start_id(start_id)
                .get_account_info(
                    &associated_token_address(&owner, &token_program),
                    Some(account_info(
                        token_account_data(&owner, extensions),
                        token_program,
                    )),
                )
                .get_account_info(
                    &associated_token_address(&MINTER_ADDRESS, &token_program),
                    None,
                )
                .build(),
        )
        .deposit_spl(DepositSplArgs {
            owner: None,
            subaccount: account.subaccount,
            mint: MINT.into(),
        })
        .await
        .expect("deposit_spl should queue a sweep")
}

/// Runs the sweep timer and answers its getSlot, getBlock and sendTransaction calls with
/// RPC IDs `start_id` to `start_id + 11`. Returns the signature and message of the sweep.
pub async fn sweep_queued_deposit(
    setup: &Setup,
    deposit_id: DepositSplId,
    start_id: u64,
) -> (Signature, VersionedTransactionMessage) {
    setup.advance_time(SWEEP_DEPOSITS_DELAY).await;
    setup
        .execute_http_mocks(
            MockBuilder::with_start_id(start_id)
                .submit_transaction(SUBMISSION_BLOCK_HEIGHT)
                .build(),
        )
        .await;
    setup
        .minter()
        .get_all_events()
        .await
        .into_iter()
        .find_map(|event| match event.payload {
            EventType::SubmittedTransaction {
                signature,
                transaction,
                purpose: TransactionPurpose::SweepSplDeposits { deposit_ids, .. },
                ..
            } if deposit_ids == [deposit_id] => Some((signature, transaction)),
            _ => None,
        })
        .expect("the SPL sweep timer should submit the queued deposit")
}

/// Runs the finalization timer and reports the sweep as finalized with the full token
/// transfer. The finalization calls use RPC IDs `start_id` to `start_id + 15`.
pub async fn finalize_sweep(
    setup: &Setup,
    signature: &Signature,
    transaction: VersionedTransactionMessage,
    owner: &Address,
    token_program: &Address,
    start_id: u64,
) {
    finalize_sweep_with_amount(
        setup,
        signature,
        transaction,
        owner,
        token_program,
        BALANCE_ABOVE_MINIMUM,
        start_id,
    )
    .await;
}

/// Like [`finalize_sweep`], with the given amount moved instead of the recorded balance.
pub async fn finalize_sweep_with_amount(
    setup: &Setup,
    signature: &Signature,
    transaction: VersionedTransactionMessage,
    owner: &Address,
    token_program: &Address,
    amount: u64,
    start_id: u64,
) {
    setup.advance_time(FINALIZE_TRANSACTIONS_DELAY).await;
    setup
        .execute_http_mocks(
            MockBuilder::with_start_id(start_id)
                .finalize_transaction(signature, SUBMISSION_BLOCK_HEIGHT)
                .get_transaction(
                    signature,
                    sweep_transaction_response(
                        signature,
                        transaction,
                        owner,
                        token_program,
                        amount,
                        None,
                    ),
                )
                .build(),
        )
        .await;
}

/// Runs the mint timer and enough PocketIC rounds to complete the ledger calls.
pub async fn run_mint_timer(setup: &Setup) {
    setup.advance_time(PROCESS_PENDING_MINTS_DELAY).await;
    for _ in 0..10 {
        setup.tick().await;
    }
}
