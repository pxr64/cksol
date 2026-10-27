use super::*;
use crate::test_fixtures::{
    EventsAssert, account, init_state, ledger_canister_id, runtime::TestCanisterRuntime,
};
use candid::Nat;

#[tokio::test]
async fn should_burn_the_cksol_fee_with_the_supplied_spl_withdrawal_memo() {
    init_state();
    let runtime = TestCanisterRuntime::new()
        .add_stub_response(Ok::<Nat, TransferFromError>(Nat::from(42_u64)));
    let from = Account {
        subaccount: Some([7; 32]),
        ..account(1)
    };
    let minter_account: Account = runtime.canister_self().into();
    let fee = 3_039_280;
    let memo = Memo::spl_withdrawal_fee([0xAB; 32].into(), u64::MAX, [0xCD; 32].into());

    assert_eq!(
        burn(&runtime, minter_account, from, fee, memo.clone()).await,
        Ok(42)
    );

    let calls = runtime.sent_update_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].canister_id, ledger_canister_id());
    assert_eq!(calls[0].method, "icrc2_transfer_from");
    let args = calls[0].single_arg::<TransferFromArgs>();
    assert_eq!(
        args,
        TransferFromArgs {
            spender_subaccount: None,
            from,
            to: minter_account,
            amount: Nat::from(fee),
            fee: None,
            created_at_time: None,
            memo: Some(memo.clone().into()),
        }
    );
    let sent_memo = args.memo.unwrap();
    assert_eq!(sent_memo.0.len(), 80);
    assert_eq!(minicbor::decode::<Memo>(&sent_memo.0).unwrap(), memo);
    EventsAssert::assert_no_events_recorded();
}
