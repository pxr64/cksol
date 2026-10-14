use assert_matches::assert_matches;
use assert2::check;
use base64::{Engine, engine::general_purpose::STANDARD};
use candid::Principal;
use cksol_int_tests::{
    Setup, SetupBuilder,
    fixtures::{MINTER_ADDRESS, MockBuilder},
    validator::FEE_PER_SIGNATURE,
};
use cksol_types::{
    AddSplTokenArgs, DepositSplArgs, GetDepositAddressArgs, GetSplDepositAddressArgs, Lamport,
    Signature,
};
use cksol_types_internal::event::{EventType, VersionedTransactionMessage};
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
pub const DECIMALS: u8 = 6;
pub const MINIMUM_DEPOSIT_AMOUNT: u64 = 1_000_000;
pub const BALANCE_ABOVE_MINIMUM: u64 = MINIMUM_DEPOSIT_AMOUNT + 1;
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
    let ledger = setup.install_spl_ledger(DECIMALS).await;
    register_token(&setup, ledger.canister_id(), token_program).await;
    setup
}

pub async fn register_token(setup: &Setup, ledger_id: Principal, token_program: Address) {
    setup
        .minter()
        .with_http_mocks(
            MockBuilder::new()
                .get_account_info(&MINT, Some(account_info(mint_data(), token_program)))
                .build(),
        )
        .add_spl_token(AddSplTokenArgs {
            mint: MINT.into(),
            token_program: token_program.into(),
            decimals: DECIMALS,
            ledger_id,
            minimum_deposit_amount: MINIMUM_DEPOSIT_AMOUNT,
            paused: false,
        })
        .await
        .expect("add_spl_token should register the token");
}

/// Uses the recorded message, with an outcome where the full source balance reached
/// a newly created treasury ATA and the minter paid its fee and rent.
pub fn sweep_transaction_response(
    signature: &Signature,
    transaction: VersionedTransactionMessage,
    owner: &Address,
    token_program: &Address,
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
    pre_balances[destination_index] = 0;
    let mut post_balances = pre_balances.clone();
    post_balances[0] -= fee + RENT_EXEMPT_LAMPORTS;
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
                "preTokenBalances": [token_balance(source_index, owner, BALANCE_ABOVE_MINIMUM)],
                "postTokenBalances": [
                    token_balance(source_index, owner, 0),
                    token_balance(destination_index, &MINTER_ADDRESS, BALANCE_ABOVE_MINIMUM)
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
    register_token(&setup, ledger.canister_id(), token_program).await;
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
            events[1..]
                == [
                    EventType::AddedSplToken {
                        mint: MINT.into(),
                        token_program: token_program.into(),
                        decimals: DECIMALS,
                        ledger_id: ledger.canister_id(),
                        minimum_deposit_amount: MINIMUM_DEPOSIT_AMOUNT,
                        paused: false,
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
                        fee: 0,
                    },
                ]
        );
    });

    setup.drop().await;
}
