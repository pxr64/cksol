use crate::{address::associated_token_address, state::SplSweep};
use base64::{Engine, prelude::BASE64_STANDARD};
use serde_json::json;
use solana_hash::Hash;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_status_client_types::{
    EncodedConfirmedTransactionWithStatusMeta, option_serializer::OptionSerializer,
};
use std::collections::BTreeMap;

pub fn transaction_outcome(
    message: solana_message::VersionedMessage,
) -> EncodedConfirmedTransactionWithStatusMeta {
    let transaction = VersionedTransaction {
        signatures: vec![Signature::default(); message.header().num_required_signatures as usize],
        message,
    };
    serde_json::from_value(json!({
        "slot": 1,
        "transaction": [BASE64_STANDARD.encode(bincode::serialize(&transaction).unwrap()), "base64"],
        "meta": {
            "err": null,
            "status": {"Ok": null},
            "fee": 5000,
            "preBalances": [],
            "postBalances": [],
        },
    }))
    .unwrap()
}

pub fn balanced_outcome(
    sweep: &SplSweep,
    blockhash: Hash,
    new_destinations: bool,
) -> EncodedConfirmedTransactionWithStatusMeta {
    let message = sweep.sweep_message(blockhash);
    let mut outcome =
        transaction_outcome(solana_message::VersionedMessage::Legacy(message.clone()));
    let meta = outcome.transaction.meta.as_mut().unwrap();
    meta.pre_balances = vec![1_000_000; message.account_keys.len()];
    meta.post_balances = meta.pre_balances.clone();
    meta.post_balances[0] -= meta.fee;
    let mut balances = BTreeMap::new();
    for transfer in sweep.transfers() {
        let deposit = &sweep.deposits()[&transfer.deposit_id];
        balances.insert(
            deposit.address,
            (
                deposit.mint,
                transfer.owner,
                transfer.token_program.id(),
                transfer.decimals,
                deposit.balance.saturating_add(100),
                deposit.balance.saturating_add(100) - deposit.balance,
            ),
        );
        let destination = associated_token_address(
            &sweep.minter_address(),
            &deposit.mint,
            &transfer.token_program.id(),
        );
        let initial = if new_destinations { 0 } else { 42 };
        balances
            .entry(destination)
            .or_insert((
                deposit.mint,
                sweep.minter_address(),
                transfer.token_program.id(),
                transfer.decimals,
                initial,
                initial,
            ))
            .5 += deposit.balance;
    }
    let mut pre = vec![];
    let mut post = vec![];
    for (address, (mint, owner, program, decimals, before, after)) in balances {
        let index = message
            .account_keys
            .iter()
            .position(|key| *key == address)
            .unwrap();
        let balance = |amount: u64| {
            serde_json::from_value(json!({
            "accountIndex": index,
            "mint": mint.to_string(),
            "owner": owner.to_string(),
            "programId": program.to_string(),
            "uiTokenAmount": { "amount": amount.to_string(), "decimals": decimals, "uiAmount": null, "uiAmountString": "ignored" },
        })).unwrap()
        };
        if new_destinations && before == 0 {
            meta.pre_balances[index] = 0;
        } else {
            pre.push(balance(before));
        }
        post.push(balance(after));
    }
    meta.pre_token_balances = OptionSerializer::Some(pre);
    meta.post_token_balances = OptionSerializer::Some(post);
    outcome
}
