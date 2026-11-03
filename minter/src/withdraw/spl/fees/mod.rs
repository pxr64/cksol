use crate::{
    constants::{FEE_PER_SIGNATURE, MAX_SPL_WITHDRAWALS_PER_NONCE_TX},
    state::{
        State, TokenProgram,
        event::{SplWithdrawalRequest, SplWithdrawalRequestTerms},
    },
};
use cksol_types::WithdrawSplError;
use sol_rpc_types::Lamport;
use solana_signature::Signature;
use std::collections::BTreeSet;

#[cfg(test)]
mod tests;

/// Quotes the ckSOL fee in lamports using the configured withdrawal fee.
/// Allowing destination ATA creation includes its rent even if it already exists.
pub fn quote_withdrawal_fee(
    withdrawal_fee: Lamport,
    token_program: TokenProgram,
    create_destination_account: bool,
) -> Result<Lamport, WithdrawSplError> {
    let rent = if create_destination_account {
        token_program.account_rent_exemption()
    } else {
        0
    };
    withdrawal_fee.checked_add(rent).ok_or_else(|| {
        WithdrawSplError::TemporarilyUnavailable(
            "SPL withdrawal fee exceeds the maximum lamport amount".to_string(),
        )
    })
}

/// One signature fee plus rent for each distinct destination ATA the transaction may create.
pub(crate) fn spl_withdrawal_transaction_cost<'a>(
    state: &State,
    requests: impl IntoIterator<Item = &'a SplWithdrawalRequestTerms>,
) -> Lamport {
    let mut cost = FEE_PER_SIGNATURE;
    let mut destinations_to_create = BTreeSet::new();
    for terms in requests {
        if !terms.create_destination_account {
            continue;
        }
        let token = state
            .supported_spl_token(&terms.mint)
            .expect("BUG: an accepted SPL withdrawal has a registered mint");
        if destinations_to_create.insert((terms.address, terms.mint, token.token_program.id())) {
            cost = cost
                .checked_add(token.token_program.account_rent_exemption())
                .expect("BUG: SPL withdrawal transaction cost overflows");
        }
    }
    cost
}

/// SOL budget for a batch attempt and one individual retry per request if it fails.
/// Returns `None` when the size or an individual request's quoted fee disallows batching.
pub(crate) fn spl_withdrawal_batch_budget(
    state: &State,
    requests: &[SplWithdrawalRequest],
) -> Option<Lamport> {
    if !(2..=MAX_SPL_WITHDRAWALS_PER_NONCE_TX).contains(&requests.len()) {
        return None;
    }
    // Round up so every request covers its share without borrowing another user's fee.
    let batch_fee_share = FEE_PER_SIGNATURE.div_ceil(requests.len() as Lamport);
    // A failed batch only spends its signature fee; its ATA creation is rolled back.
    let mut budget = FEE_PER_SIGNATURE;
    for request in requests {
        let retry_cost = spl_withdrawal_transaction_cost(state, std::iter::once(&request.terms));
        if request.terms.fee < retry_cost.checked_add(batch_fee_share)? {
            return None;
        }
        // Each individual retry reserves its own rent, including when ATAs are shared.
        budget = budget.checked_add(retry_cost)?;
    }
    Some(budget)
}

pub(crate) fn spl_withdrawal_reservation(
    state: &State,
    requests: &[SplWithdrawalRequest],
) -> Lamport {
    if let [request] = requests {
        spl_withdrawal_transaction_cost(state, std::iter::once(&request.terms))
    } else {
        spl_withdrawal_batch_budget(state, requests)
            .expect("Attempted to reserve an SPL batch its quoted fees cannot cover")
    }
}

/// The in-flight transaction's fee and rent limit, excluding funds held for fallback retries.
pub(crate) fn spl_withdrawal_reserved_cost(
    state: &State,
    signature: &Signature,
) -> Option<Lamport> {
    let mut requests = state
        .sent_spl_withdrawal_requests()
        .values()
        .filter(|sent| &sent.signature == signature)
        .map(|sent| &sent.request.terms)
        .peekable();
    requests.peek()?;
    Some(spl_withdrawal_transaction_cost(state, requests))
}

/// Includes the fallback funds while a batch is in flight; single transactions have no fallback.
pub(crate) fn spl_withdrawal_reserved_budget(
    state: &State,
    signature: &Signature,
) -> Option<Lamport> {
    let requests: Vec<_> = state
        .sent_spl_withdrawal_requests()
        .values()
        .filter(|sent| &sent.signature == signature)
        .map(|sent| sent.request.clone())
        .collect();
    if requests.is_empty() {
        None
    } else {
        Some(spl_withdrawal_reservation(state, &requests))
    }
}
