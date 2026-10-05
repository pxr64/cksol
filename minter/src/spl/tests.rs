use super::*;
use crate::{
    lifecycle,
    state::{audit::replay_events, event::Event},
    storage::{total_event_count, with_event_iter},
    test_fixtures::{runtime::TestCanisterRuntime, valid_init_args},
};
use assert_matches::assert_matches;
use base64::{Engine, engine::general_purpose::STANDARD};
use ic_stable_structures::Storable;
use sol_rpc_types::{AccountData, AccountEncoding, AccountInfo};
use spl_token_2022_interface::extension::{
    BaseStateWithExtensionsMut, ExtensionType, StateWithExtensionsMut,
    mint_close_authority::MintCloseAuthority,
};

fn args() -> AddSplTokenArgs {
    AddSplTokenArgs {
        mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
            .parse()
            .unwrap(),
        token_program: CLASSIC_TOKEN_PROGRAM.parse().unwrap(),
        decimals: 6,
        ledger_id: Principal::from_slice(&[42, 1]),
        minimum_deposit_amount: 1_000_000,
        paused: false,
    }
}

fn mint_data() -> Vec<u8> {
    let mut data = vec![0; 82];
    data[44] = 6;
    data[45] = 1;
    data
}

fn mint_account(data: &[u8]) -> AccountInfo {
    AccountInfo {
        lamports: 1_461_600,
        data: AccountData::Binary(STANDARD.encode(data), AccountEncoding::Base64),
        owner: CLASSIC_TOKEN_PROGRAM.to_string(),
        executable: false,
        rent_epoch: 0,
        space: data.len() as u64,
    }
}

fn init() -> TestCanisterRuntime {
    let runtime = TestCanisterRuntime::new().with_increasing_time();
    lifecycle::init(valid_init_args(), runtime.clone());
    runtime
}

fn with_mint(runtime: TestCanisterRuntime, account: Option<AccountInfo>) -> TestCanisterRuntime {
    runtime.add_stub_response(MultiRpcResult::Consistent(Ok(account)))
}

fn assert_not_registered(mint: &cksol_types::Address) {
    assert!(read_state(|state| state
        .supported_spl_token(mint)
        .is_none()));
    assert_eq!(total_event_count(), 1); // Init only.
}

#[tokio::test]
async fn should_register_and_replay_the_token_from_stable_events() {
    let runtime = with_mint(init(), Some(mint_account(&mint_data())));
    let args = args();
    assert_eq!(add_spl_token(&runtime, args.clone()).await, Ok(()));
    assert_eq!(
        read_state(|state| state.supported_spl_token(&args.mint).cloned()),
        Some(SupportedSplToken::from(args.clone()))
    );
    let events = with_event_iter(|events| {
        events
            .map(|event| Event::from_bytes(event.to_bytes()))
            .collect::<Vec<_>>()
    });
    assert_eq!(events.len(), 2);
    assert_matches!(&events[1].payload, EventType::AddedSplToken(token) if token.mint == args.mint);
    let replayed = replay_events(events);
    read_state(|state| assert_eq!(state, &replayed));
    assert_eq!(runtime.sent_update_calls().len(), 1);
    assert_eq!(runtime.sent_update_calls()[0].method, "getAccountInfo");
}

#[tokio::test]
async fn should_preserve_initial_paused_flag() {
    let runtime = with_mint(init(), Some(mint_account(&mint_data())));
    let args = AddSplTokenArgs {
        paused: true,
        ..args()
    };
    assert_eq!(add_spl_token(&runtime, args.clone()).await, Ok(()));
    assert!(read_state(|state| state
        .supported_spl_token(&args.mint)
        .unwrap()
        .paused));
}

#[tokio::test]
async fn should_reject_invalid_configuration_before_rpc() {
    let runtime = init();
    let base = args();
    let cases = [
        AddSplTokenArgs {
            minimum_deposit_amount: 0,
            ..base.clone()
        },
        AddSplTokenArgs {
            token_program: cksol_types::Address::default(),
            ..base.clone()
        },
        AddSplTokenArgs {
            ledger_id: Principal::anonymous(),
            ..base.clone()
        },
        AddSplTokenArgs {
            ledger_id: Principal::management_canister(),
            ..base.clone()
        },
        AddSplTokenArgs {
            ledger_id: runtime.canister_self(),
            ..base.clone()
        },
    ];
    for args in cases {
        assert_matches!(
            add_spl_token(&runtime, args).await,
            Err(AddSplTokenError::InvalidToken(_))
        );
    }
    let native_ledger = AddSplTokenArgs {
        ledger_id: valid_init_args().ledger_canister_id,
        ..base.clone()
    };
    assert_eq!(
        add_spl_token(&runtime, native_ledger.clone()).await,
        Err(AddSplTokenError::LedgerAlreadyUsed {
            ledger_id: native_ledger.ledger_id
        })
    );
    assert!(runtime.sent_update_calls().is_empty());
    assert_not_registered(&base.mint);
}

#[tokio::test]
async fn should_reject_duplicate_mint_and_ledger_without_overwriting_config() {
    let runtime = with_mint(init(), Some(mint_account(&mint_data())));
    let original = args();
    assert_eq!(add_spl_token(&runtime, original.clone()).await, Ok(()));
    let changed = AddSplTokenArgs {
        decimals: 9,
        paused: true,
        ..original.clone()
    };
    assert_eq!(
        add_spl_token(&runtime, changed).await,
        Err(AddSplTokenError::AlreadySupported {
            mint: original.mint.clone()
        })
    );
    let other_mint = AddSplTokenArgs {
        mint: cksol_types::Address::default(),
        ..original.clone()
    };
    assert_eq!(
        add_spl_token(&runtime, other_mint).await,
        Err(AddSplTokenError::LedgerAlreadyUsed {
            ledger_id: original.ledger_id
        })
    );
    assert_eq!(runtime.sent_update_calls().len(), 1);
    assert_eq!(total_event_count(), 2);
    assert_eq!(
        read_state(|state| state.supported_spl_token(&original.mint).cloned()),
        Some(SupportedSplToken::from(original))
    );
}

#[tokio::test]
async fn should_recheck_registration_after_concurrent_rpc_calls() {
    let runtime = init();
    let first = with_mint(runtime.clone(), Some(mint_account(&mint_data())));
    let second = with_mint(runtime, Some(mint_account(&mint_data())));
    let (a, b) = futures::join!(
        add_spl_token(&first, args()),
        add_spl_token(&second, args())
    );
    assert_matches!(
        (&a, &b),
        (Ok(()), Err(AddSplTokenError::AlreadySupported { .. }))
            | (Err(AddSplTokenError::AlreadySupported { .. }), Ok(()))
    );
    assert_eq!(total_event_count(), 2);
}

#[tokio::test]
async fn should_reject_non_mint_accounts_and_wrong_metadata() {
    let runtime = init();
    let mut uninitialized = mint_data();
    uninitialized[45] = 0;
    let mut wrong_decimals = mint_data();
    wrong_decimals[44] = 9;
    let mut bad_authority = mint_data();
    bad_authority[0] = 2;
    let mut wrong_owner = mint_account(&mint_data());
    wrong_owner.owner = "11111111111111111111111111111111".to_string();
    let mut executable = mint_account(&mint_data());
    executable.executable = true;
    let mut bad_encoding = mint_account(&mint_data());
    bad_encoding.data = AccountData::Binary("invalid base64".to_string(), AccountEncoding::Base64);
    let cases = [
        None,
        Some(mint_account(&[])),
        Some(mint_account(&[0; 165])),
        Some(mint_account(&uninitialized)),
        Some(mint_account(&wrong_decimals)),
        Some(mint_account(&bad_authority)),
        Some(wrong_owner),
        Some(executable),
        Some(bad_encoding),
    ];
    for account in cases {
        let runtime = with_mint(runtime.clone(), account);
        assert_matches!(
            add_spl_token(&runtime, args()).await,
            Err(AddSplTokenError::InvalidToken(_))
        );
        assert_not_registered(&args().mint);
    }
}

#[tokio::test]
async fn should_leave_registry_unchanged_when_rpc_results_are_inconsistent() {
    let runtime =
        init().add_stub_response(MultiRpcResult::<Option<AccountInfo>>::Inconsistent(vec![]));
    assert_matches!(
        add_spl_token(&runtime, args()).await,
        Err(AddSplTokenError::TemporarilyUnavailable(_))
    );
    assert_not_registered(&args().mint);
}

fn token_2022_args() -> AddSplTokenArgs {
    AddSplTokenArgs {
        token_program: TOKEN_2022_PROGRAM.parse().unwrap(),
        ..args()
    }
}

fn token_2022_mint_account(data: &[u8]) -> AccountInfo {
    AccountInfo {
        owner: TOKEN_2022_PROGRAM.to_string(),
        ..mint_account(data)
    }
}

#[tokio::test]
async fn should_register_and_replay_token_2022_without_extensions() {
    let runtime = with_mint(init(), Some(token_2022_mint_account(&mint_data())));
    let args = token_2022_args();

    assert_eq!(add_spl_token(&runtime, args.clone()).await, Ok(()));

    assert_eq!(
        read_state(|state| state.supported_spl_token(&args.mint).cloned()),
        Some(SupportedSplToken::from(args))
    );
    let replayed = with_event_iter(|events| replay_events(events));
    read_state(|state| assert_eq!(state, &replayed));
    assert_eq!(total_event_count(), 2);
    assert_eq!(runtime.sent_update_calls().len(), 1);
}

#[tokio::test]
async fn should_reject_token_2022_mint_extensions() {
    let mut data = vec![
        0;
        ExtensionType::try_calculate_account_len::<Mint2022>(&[
            ExtensionType::MintCloseAuthority
        ],)
        .unwrap()
    ];
    let mut mint = StateWithExtensionsMut::<Mint2022>::unpack_uninitialized(&mut data).unwrap();
    mint.init_extension::<MintCloseAuthority>(false).unwrap();
    mint.base = Mint2022 {
        decimals: 6,
        is_initialized: true,
        ..Mint2022::default()
    };
    mint.pack_base();
    mint.init_account_type().unwrap();
    let runtime = with_mint(init(), Some(token_2022_mint_account(&data)));

    let result = add_spl_token(&runtime, token_2022_args()).await;

    assert_eq!(
        result,
        Err(AddSplTokenError::InvalidToken(
            "Token-2022 mint extensions are not supported".to_string(),
        ))
    );
    assert_not_registered(&token_2022_args().mint);
}

#[tokio::test]
async fn should_reject_invalid_token_2022_mint_accounts() {
    let runtime = init();
    let mut uninitialized = mint_data();
    uninitialized[45] = 0;
    let mut wrong_decimals = mint_data();
    wrong_decimals[44] = 9;
    let mut executable = token_2022_mint_account(&mint_data());
    executable.executable = true;
    let cases = [
        token_2022_mint_account(&[]),
        token_2022_mint_account(&[0; 165]),
        token_2022_mint_account(&uninitialized),
        token_2022_mint_account(&wrong_decimals),
        mint_account(&mint_data()),
        executable,
    ];
    for account in cases {
        let runtime = with_mint(runtime.clone(), Some(account));

        let result = add_spl_token(&runtime, token_2022_args()).await;

        assert_matches!(result, Err(AddSplTokenError::InvalidToken(_)));
        assert_not_registered(&token_2022_args().mint);
    }
}
