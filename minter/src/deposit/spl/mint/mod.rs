use crate::{
    constants::SPL_FEE_SUBACCOUNT,
    deposit::mint::{MintQueue, mint_pending_deposit, process_pending_mints_round},
    ledger::client::LedgerClient,
    runtime::CanisterRuntime,
    state::{PendingSplFeeMint, PendingSplMint, event::EventType, read_state},
};
use cksol_types::{DepositSplId, Memo, MintMemo};
use ic_canister_runtime::Runtime;
use icrc_ledger_types::icrc1::{
    account::Account,
    transfer::{NumTokens, TransferArg},
};
use solana_address::Address;
use std::time::Duration;

#[cfg(test)]
mod tests;

pub async fn process_pending_spl_mints<R: CanisterRuntime>(runtime: R) {
    let run_again = process_pending_mints_round(
        &runtime,
        MintQueue::Spl,
        |state| state.spl_deposits().pending_mints(),
        process_pending_spl_mint,
    )
    .await;
    let fee_run_again = process_pending_mints_round(
        &runtime,
        MintQueue::SplFee,
        |state| state.spl_deposits().pending_fee_mints(),
        process_pending_spl_fee_mint,
    )
    .await;
    if run_again || fee_run_again {
        runtime.set_timer(Duration::ZERO, process_pending_spl_mints);
    }
}

async fn process_pending_spl_mint<R: CanisterRuntime>(
    runtime: &R,
    deposit_id: DepositSplId,
    pending: PendingSplMint,
) {
    mint_pending_deposit(
        runtime,
        spl_ledger_client(runtime, &pending.deposit.deposit.mint),
        mint_arg(
            pending.account(),
            pending.created_at_time,
            MintMemo::sweep(pending.sweep_signature(), deposit_id),
            pending.amount_to_mint,
        ),
        |mint_block_index| EventType::MintedSweptSplDeposit {
            deposit_id,
            mint_block_index,
        },
        EventType::QuarantinedPendingSplMint { deposit_id },
        &format!("SPL deposit {deposit_id}"),
    )
    .await;
}

async fn process_pending_spl_fee_mint<R: CanisterRuntime>(
    runtime: &R,
    deposit_id: DepositSplId,
    pending: PendingSplFeeMint,
) {
    mint_pending_deposit(
        runtime,
        spl_ledger_client(runtime, &pending.mint),
        mint_arg(
            Account {
                owner: runtime.canister_self(),
                subaccount: Some(SPL_FEE_SUBACCOUNT),
            },
            pending.created_at_time,
            MintMemo::sweep_fee(pending.signature, deposit_id),
            pending.amount,
        ),
        |mint_block_index| EventType::MintedSplDepositFee {
            deposit_id,
            mint_block_index,
        },
        EventType::QuarantinedPendingSplFeeMint { deposit_id },
        &format!("SPL deposit {deposit_id} fee"),
    )
    .await;
}

fn spl_ledger_client<R: CanisterRuntime>(
    runtime: &R,
    mint: &Address,
) -> LedgerClient<impl Runtime> {
    read_state(|state| {
        let token = state
            .supported_spl_token(mint)
            .expect("BUG: a pending SPL mint must have a registered token");
        LedgerClient::new(runtime.inter_canister_call_runtime(), token.ledger_id)
    })
}

fn mint_arg(to: Account, created_at_time: u64, memo: MintMemo, amount: u64) -> TransferArg {
    TransferArg {
        from_subaccount: None,
        to,
        fee: None,
        created_at_time: Some(created_at_time),
        memo: Some(Memo::from(memo).into()),
        amount: NumTokens::from(amount),
    }
}
