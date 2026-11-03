use crate::{
    constants::{FEE_PER_SIGNATURE, MAX_SPL_WITHDRAWALS_PER_NONCE_TX},
    numeric::LedgerBurnIndex,
    sol_transfer::{BATCH_WITHDRAWAL_TX_FEE, MAX_WITHDRAWALS_PER_NONCE_TX},
    state::{
        PendingSplWithdrawalRequest, PendingWithdrawalRequest, State,
        event::{SplWithdrawalRequest, SplWithdrawalRequestTerms, WithdrawalRequest},
    },
    withdraw::spl::fees::{spl_withdrawal_batch_budget, spl_withdrawal_transaction_cost},
};
use sol_rpc_types::Lamport;
use solana_address::Address;
use std::{
    collections::{BTreeMap, VecDeque, btree_map},
    iter::Peekable,
};

#[cfg(test)]
mod tests;

/// One withdrawal transaction to create, containing only SOL transfers or only SPL transfers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WithdrawalPlan {
    Sol(Vec<WithdrawalRequest>),
    Spl(Vec<SplWithdrawalRequest>),
}

/// Selects fresh SPL requests in the supplied burn-index order, before the next SOL request.
/// Unaffordable requests are skipped. Every selected group, including one request, funds its retries.
/// This only calculates a selection; the caller must reserve its SOL and tokens afterward.
pub fn select_spl_withdrawal_batch(
    state: &State,
    candidates: &[SplWithdrawalRequest],
    next_sol: Option<LedgerBurnIndex>,
    available_balance: Lamport,
    available_tokens: &BTreeMap<Address, u64>,
) -> Option<Vec<SplWithdrawalRequest>> {
    let mut selected = Vec::new();
    let mut remaining_tokens = available_tokens.clone();
    for request in candidates {
        if selected.len() == MAX_SPL_WITHDRAWALS_PER_NONCE_TX
            || next_sol.is_some_and(|sol| sol <= request.cksol_burn_block_index)
        {
            break;
        }
        if state
            .retrying_spl_withdrawal_requests()
            .contains_key(&request.cksol_burn_block_index)
        {
            continue;
        }
        let terms = &request.terms;
        let tokens = remaining_tokens.get(&terms.mint).copied().unwrap_or(0);
        let Some(tokens_after) = tokens.checked_sub(terms.amount) else {
            continue;
        };
        // A group containing this request must cover its retry and the fresh signature fee.
        let Some(minimum_fresh_cost) =
            spl_withdrawal_transaction_cost(state, std::iter::once(terms))
                .checked_add(FEE_PER_SIGNATURE)
        else {
            continue;
        };
        if minimum_fresh_cost > available_balance {
            continue;
        }
        selected.push(request.clone());
        remaining_tokens.insert(terms.mint, tokens_after);
    }
    // Check the largest prefix first: a larger batch gives each request a smaller fee share.
    // Removing only from the end preserves the order of eligible requests.
    while !selected.is_empty() {
        if spl_withdrawal_batch_budget(state, &selected)
            .is_some_and(|cost| cost <= available_balance)
        {
            return Some(selected);
        }
        selected.pop();
    }
    None
}

pub fn can_create_withdrawal_transaction(state: &State) -> bool {
    state.nonce_pool().num_free_accounts() > 0
        && WithdrawalRound::take_from_queue(state, 1).next().is_some()
}

pub fn covers_withdrawal_batch(state: &State, requests: &[WithdrawalRequest]) -> bool {
    let cost = requests
        .iter()
        .fold(BATCH_WITHDRAWAL_TX_FEE, |cost, request| {
            cost.saturating_add(request.amount_to_transfer)
        });
    state.spendable_balance() >= cost
}

/// A bounded selection of fresh transactions and individual retries, in processing order.
/// Selection alternates between the two queues using one shared budget; either may use spare slots.
pub struct WithdrawalRound {
    plans: VecDeque<WithdrawalPlan>,
}

impl WithdrawalRound {
    pub fn take_from_queue(state: &State, slots: usize) -> Self {
        let mut round = Self {
            plans: VecDeque::new(),
        };
        if slots == 0 {
            return round;
        }
        let mut budget = WithdrawalBudget {
            state,
            available_balance: state.spendable_balance(),
            available_tokens: state.spl_treasury_balances().clone(),
        };
        let mut fresh = FreshWithdrawals {
            pending_sol: state.pending_withdrawal_requests().iter().peekable(),
            pending_spl: state.pending_spl_withdrawal_requests().iter().peekable(),
            sol_blocked: false,
            spl_blocked: false,
        };
        let mut retrying = state.retrying_spl_withdrawal_requests().iter();
        let mut next_is_retry = state.next_withdrawal_is_retry();
        // Reserve in processing order, so neither queue spends funds promised to the other.
        while round.plans.len() < slots {
            if next_is_retry && round.take_spl_retry(&mut retrying, &mut budget) {
                next_is_retry = false;
                continue;
            }
            if let Some(plan) = fresh.take_next(&mut budget) {
                round.plans.push_back(plan);
                next_is_retry = true;
                continue;
            }
            if round.take_spl_retry(&mut retrying, &mut budget) {
                next_is_retry = false;
                continue;
            }
            break;
        }
        round
    }

    /// Retries keep their ID order, skipping any the shared budget cannot cover.
    fn take_spl_retry(
        &mut self,
        retrying: &mut btree_map::Iter<'_, LedgerBurnIndex, PendingSplWithdrawalRequest>,
        budget: &mut WithdrawalBudget<'_>,
    ) -> bool {
        for (_, pending) in retrying {
            // The failed fresh transaction kept retry SOL reserved but released its tokens.
            // Reserve only tokens here so the retry's SOL is not counted twice.
            if budget.reserve_tokens(&pending.request.terms) {
                self.plans
                    .push_back(WithdrawalPlan::Spl(vec![pending.request.clone()]));
                return true;
            }
        }
        false
    }
}

impl Iterator for WithdrawalRound {
    type Item = WithdrawalPlan;

    fn next(&mut self) -> Option<Self::Item> {
        self.plans.pop_front()
    }
}

/// Fresh SOL and SPL requests retain their existing burn-index ordering and batching.
/// An unaffordable head stops its own kind; the other kind can keep filling the round.
struct FreshWithdrawals<'a> {
    pending_sol: Peekable<btree_map::Iter<'a, LedgerBurnIndex, PendingWithdrawalRequest>>,
    pending_spl: Peekable<btree_map::Iter<'a, LedgerBurnIndex, PendingSplWithdrawalRequest>>,
    sol_blocked: bool,
    spl_blocked: bool,
}

impl FreshWithdrawals<'_> {
    fn take_next(&mut self, budget: &mut WithdrawalBudget<'_>) -> Option<WithdrawalPlan> {
        loop {
            let next_sol = self.next_burn_index_of_sol();
            let next_spl = self.next_burn_index_of_spl();
            let plan = match (next_sol, next_spl) {
                (None, None) => return None,
                (Some(sol), Some(spl)) if spl < sol => self.next_spl(budget),
                (Some(_), _) => self.next_sol_batch(next_spl, budget),
                (None, Some(_)) => self.next_spl(budget),
            };
            if plan.is_some() {
                return plan;
            }
        }
    }

    fn next_burn_index_of_sol(&mut self) -> Option<LedgerBurnIndex> {
        if self.sol_blocked {
            return None;
        }
        self.pending_sol.peek().map(|(burn_index, _)| **burn_index)
    }

    fn next_burn_index_of_spl(&mut self) -> Option<LedgerBurnIndex> {
        if self.spl_blocked {
            return None;
        }
        self.pending_spl.peek().map(|(burn_index, _)| **burn_index)
    }

    /// Batches SOL requests up to the next SPL request, so a batch never serves a
    /// request newer than an SPL request still waiting.
    fn next_sol_batch(
        &mut self,
        next_spl: Option<LedgerBurnIndex>,
        budget: &mut WithdrawalBudget<'_>,
    ) -> Option<WithdrawalPlan> {
        let mut batch = Vec::new();
        while batch.len() < MAX_WITHDRAWALS_PER_NONCE_TX {
            let reserved_fee = if batch.is_empty() {
                BATCH_WITHDRAWAL_TX_FEE
            } else {
                0
            };
            let Some((burn_index, pending)) = self.pending_sol.peek() else {
                break;
            };
            if next_spl.is_some_and(|spl| spl < **burn_index) {
                break;
            }
            let Some(remaining_balance) = pending
                .request
                .amount_to_transfer
                .checked_add(reserved_fee)
                .and_then(|cost| budget.available_balance.checked_sub(cost))
            else {
                break;
            };
            budget.available_balance = remaining_balance;
            batch.push(pending.request.clone());
            self.pending_sol.next();
        }
        if batch.is_empty() {
            self.sol_blocked = true;
            None
        } else {
            Some(WithdrawalPlan::Sol(batch))
        }
    }

    fn next_spl(&mut self, budget: &mut WithdrawalBudget<'_>) -> Option<WithdrawalPlan> {
        let (_, pending) = self.pending_spl.peek().copied()?;
        if !budget.reserve_spl(&pending.request) {
            self.spl_blocked = true;
            return None;
        }
        let request = pending.request.clone();
        self.pending_spl.next();
        Some(WithdrawalPlan::Spl(vec![request]))
    }
}

struct WithdrawalBudget<'a> {
    state: &'a State,
    available_balance: Lamport,
    available_tokens: BTreeMap<Address, u64>,
}

impl WithdrawalBudget<'_> {
    /// Fresh withdrawals need free SOL; retry SOL is already held outside this budget.
    fn reserve_spl(&mut self, request: &SplWithdrawalRequest) -> bool {
        let Some(reserved) = spl_withdrawal_batch_budget(self.state, std::slice::from_ref(request))
        else {
            return false;
        };
        let Some(remaining_balance) = self.available_balance.checked_sub(reserved) else {
            return false;
        };
        if !self.reserve_tokens(&request.terms) {
            return false;
        }
        self.available_balance = remaining_balance;
        true
    }

    /// Both queues share the same available treasury tokens.
    fn reserve_tokens(&mut self, terms: &SplWithdrawalRequestTerms) -> bool {
        let tokens = self.available_tokens.get(&terms.mint).copied().unwrap_or(0);
        let Some(remaining) = tokens.checked_sub(terms.amount) else {
            return false;
        };
        self.available_tokens.insert(terms.mint, remaining);
        true
    }
}
