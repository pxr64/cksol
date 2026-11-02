use super::{PendingSplWithdrawalRequest, PendingWithdrawalRequest, State};
use crate::{
    numeric::LedgerBurnIndex,
    sol_transfer::{BATCH_WITHDRAWAL_TX_FEE, MAX_WITHDRAWALS_PER_NONCE_TX},
    state::event::{SplWithdrawalRequest, SplWithdrawalRequestTerms, WithdrawalRequest},
};
use sol_rpc_types::Lamport;
use solana_address::Address;
use std::{
    collections::{BTreeMap, VecDeque, btree_map},
    iter::Peekable,
};

/// One withdrawal transaction to create: a batch of SOL transfers or one SPL withdrawal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WithdrawalPlan {
    Sol(Vec<WithdrawalRequest>),
    Spl(SplWithdrawalRequest),
}

/// A bounded selection of fresh transactions and individual retries, sharing one budget.
/// Processing alternates between the two queues; either may use the other's spare slots.
pub struct WithdrawalRound {
    retries: VecDeque<SplWithdrawalRequest>,
    fresh: VecDeque<WithdrawalPlan>,
    next_is_retry: bool,
}

impl WithdrawalRound {
    pub(super) fn take_from_queue(state: &State, slots: usize) -> Self {
        let mut round = Self {
            retries: VecDeque::new(),
            fresh: VecDeque::new(),
            next_is_retry: state.next_withdrawal_is_retry,
        };
        if slots == 0 {
            return round;
        }
        let mut budget = WithdrawalBudget {
            state,
            available_balance: state.spendable_balance(),
            available_tokens: state.spl_treasury_balances.clone(),
        };
        let mut fresh = FreshWithdrawals {
            pending_sol: state.pending_withdrawal_requests.iter().peekable(),
            pending_spl: state.pending_spl_withdrawal_requests.iter().peekable(),
            sol_blocked: false,
            spl_blocked: false,
        };
        let mut retrying = state.retrying_spl_withdrawal_requests.iter();
        let mut next_is_retry = state.next_withdrawal_is_retry;
        // Reserve in processing order, so neither queue spends funds promised to the other.
        while round.retries.len() + round.fresh.len() < slots {
            if next_is_retry && round.take_retry(&mut retrying, &mut budget) {
                next_is_retry = false;
                continue;
            }
            if let Some(plan) = fresh.take_next(&mut budget) {
                round.fresh.push_back(plan);
                next_is_retry = true;
                continue;
            }
            if round.take_retry(&mut retrying, &mut budget) {
                next_is_retry = false;
                continue;
            }
            break;
        }
        round
    }

    /// Retries keep their ID order, skipping any the shared budget cannot cover.
    fn take_retry(
        &mut self,
        retrying: &mut btree_map::Iter<'_, LedgerBurnIndex, PendingSplWithdrawalRequest>,
        budget: &mut WithdrawalBudget<'_>,
    ) -> bool {
        for (_, pending) in retrying {
            if budget.reserve_spl(&pending.request.terms) {
                self.retries.push_back(pending.request.clone());
                return true;
            }
        }
        false
    }

    fn next_retry(&mut self) -> Option<WithdrawalPlan> {
        let request = self.retries.pop_front()?;
        self.next_is_retry = false;
        Some(WithdrawalPlan::Spl(request))
    }

    fn next_fresh(&mut self) -> Option<WithdrawalPlan> {
        let plan = self.fresh.pop_front()?;
        self.next_is_retry = true;
        Some(plan)
    }
}

impl Iterator for WithdrawalRound {
    type Item = WithdrawalPlan;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_is_retry {
            self.next_retry().or_else(|| self.next_fresh())
        } else {
            self.next_fresh().or_else(|| self.next_retry())
        }
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
        if !budget.reserve_spl(&pending.request.terms) {
            self.spl_blocked = true;
            return None;
        }
        let request = pending.request.clone();
        self.pending_spl.next();
        Some(WithdrawalPlan::Spl(request))
    }
}

struct WithdrawalBudget<'a> {
    state: &'a State,
    available_balance: Lamport,
    available_tokens: BTreeMap<Address, u64>,
}

impl WithdrawalBudget<'_> {
    /// Fresh SPL withdrawals and retries reserve from the same per-round budget.
    fn reserve_spl(&mut self, terms: &SplWithdrawalRequestTerms) -> bool {
        let affordable = self
            .available_balance
            .checked_sub(
                self.state
                    .spl_withdrawal_transaction_cost(std::iter::once(terms)),
            )
            .and_then(|balance| {
                let tokens = self.available_tokens.get(&terms.mint).copied().unwrap_or(0);
                Some((balance, tokens.checked_sub(terms.amount)?))
            });
        let Some((remaining_balance, remaining_tokens)) = affordable else {
            return false;
        };
        self.available_balance = remaining_balance;
        self.available_tokens.insert(terms.mint, remaining_tokens);
        true
    }
}
