use crate::{
    address::{account_address, minter_address},
    constants::{FEE_PER_SIGNATURE, GET_BALANCE_CYCLES, RENT_EXEMPTION_THRESHOLD},
    ledger::client::LedgerClient,
    numeric::{LedgerBurnIndex, LedgerMintIndex},
    rpc::BlockHeight,
    sol_transfer::{
        BATCH_WITHDRAWAL_TX_FEE, CreateTransferError, MAX_SIGNATURES, MAX_WITHDRAWALS_PER_NONCE_TX,
        build_batch_withdrawal_message,
    },
    spl_transfer::{build_spl_withdrawal_instructions, build_spl_withdrawal_message},
    state::event::{
        CreditedDeposit, Signer, SplWithdrawalFeeReimbursementRequest, SplWithdrawalRequest,
        SplWithdrawalRequestTerms, TransactionPurpose, VersionedMessage, WithdrawalRequest,
    },
    utils::insertion_ordered_map::InsertionOrderedMap,
};
use candid::Principal;
use cksol_types::{
    AddSplTokenError, DepositSolId, DepositSplId, TxFinalizedStatus, WithdrawSolStatus,
};
use cksol_types_internal::SolanaNetwork;
use cksol_types_internal::{Ed25519KeyName, InitArgs, UpgradeArgs};
use ic_canister_runtime::Runtime;
use ic_ed25519::PublicKey;
use icrc_ledger_types::icrc1::account::Account;
use sol_rpc_client::{ClientBuilder, SolRpcClient};
use sol_rpc_types::{ConsensusStrategy, Lamport, RpcSources, SolanaCluster};
use solana_address::Address;
use solana_hash::Hash;
use solana_message::Message;
use solana_signature::Signature;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, btree_map},
    iter::Peekable,
    str::FromStr,
};

#[cfg(test)]
mod tests;

pub mod audit;
mod deposits;
pub mod event;
mod nonce_pool;
mod spl;
mod spl_deposits;
mod sweep;
use sweep::validate_sweep_transaction;

pub use spl::{SupportedSplToken, TokenProgram};
pub use spl_deposits::{
    InFlightSplDeposit, MintedSplSweep, PendingSplFeeMint, PendingSplMint, QueuedSplDeposit,
    SettledSplSweep, SplDeposits, SplSweep, SplSweepRecoveryError, SplSweeps, SplTransfer,
    SweptSplDeposit,
};
mod watchlist;

pub use deposits::{
    DepositBalance, Deposits, InFlightDeposit, MintedSweep, PendingMint, QuarantineCause,
    QuarantinedDeposit, QueuedDeposit, SettledSweep, Sweep, SweepMismatch, SweepRecoveryError,
    SweepSettlementError, Sweeps, SweptDeposit, Transfer, UnreadableOutcome,
};
pub use nonce_pool::{DurableNoncePool, NoncePoolError, NonceRead};
pub use watchlist::{
    MAX_WATCHED_ADDRESSES, WATCH_WINDOW, WatchedAddress, Watchlist, WatchlistFull,
};

thread_local! {
    static STATE: RefCell<Option<State>> = RefCell::default();
}

pub fn read_state<R>(f: impl FnOnce(&State) -> R) -> R {
    STATE.with(|s| f(s.borrow().as_ref().expect("BUG: state is not initialized")))
}

pub fn init_once_state(state: State) {
    STATE.with(|s| {
        if s.borrow().is_some() {
            panic!("BUG: state is already initialized");
        }
        *s.borrow_mut() = Some(state);
    });
}

#[cfg(any(test, feature = "canbench-rs"))]
pub fn reset_state() {
    STATE.with(|s| {
        *s.borrow_mut() = None;
    });
    crate::deposit::reset_cursors();
    crate::withdraw::spl::reset_fee_reimbursement_cursor();
}

pub fn mutate_state<F, R>(f: F) -> R
where
    F: FnOnce(&mut State) -> R,
{
    STATE.with(|s| {
        f(s.borrow_mut()
            .as_mut()
            .expect("BUG: state is not initialized"))
    })
}

/// State of the minter.
///
/// # Design
///
/// The state is transient and not preserved across canister upgrades.
/// Relevant state changes are recorded in an append-only event log
/// (see [`crate::state::audit::process_event`]),
/// and replaying this log upon canister upgrade will re-create an equivalent state.
///
/// That means in particular:
/// * Methods mutating the state should generally not be accessible outside the state crate,
///   to ensure that the state is only mutating through events.
/// * Having public methods mutating the state may be acceptable for transient data (e.g. guards)
///   that do not need to be preserved across canister upgrades.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    minter_public_key: Option<SchnorrPublicKey>,
    master_key_name: Ed25519KeyName,
    ledger_canister_id: Principal,
    ledger_suite_orchestrator_id: Option<Principal>,
    sol_rpc_canister_id: Principal,
    solana_network: SolanaNetwork,
    withdrawal_fee: Lamport,
    minimum_withdrawal_amount: Lamport,
    minimum_deposit_amount: Lamport,
    deposit_sol_required_cycles: u128,
    deposit_sol_fee: u128,
    automatic_deposit_fee: Option<Lamport>,
    pending_deposit_sol_request_guards: BTreeSet<Account>,
    pending_deposit_spl_request_guards: BTreeSet<(Account, Address)>,
    pending_withdrawal_request_guards: BTreeSet<Account>,
    pending_operating_top_up_guards: BTreeSet<Account>,
    deposits: Deposits,
    spl_deposits: SplDeposits,
    supported_spl_tokens: BTreeMap<Address, SupportedSplToken>,
    pending_withdrawal_requests: BTreeMap<LedgerBurnIndex, PendingWithdrawalRequest>,
    created_withdrawal_requests: BTreeMap<LedgerBurnIndex, PendingWithdrawalRequest>,
    sent_withdrawal_requests: BTreeMap<LedgerBurnIndex, SentWithdrawalRequest>,
    successful_withdrawal_requests: BTreeMap<LedgerBurnIndex, SentWithdrawalRequest>,
    failed_withdrawal_requests: BTreeMap<LedgerBurnIndex, SentWithdrawalRequest>,
    submitted_transactions: InsertionOrderedMap<Signature, MinterTransaction>,
    created_withdrawal_txs: BTreeMap<Address, CreatedWithdrawalTransaction>,
    succeeded_transactions: BTreeSet<Signature>,
    failed_transactions: InsertionOrderedMap<Signature, MinterTransaction>,
    nonce_pool: DurableNoncePool,
    active_tasks: BTreeSet<TaskType>,
    balance: Lamport,
    spl_sweep_reservations: BTreeMap<Signature, Lamport>,
    operating_balance: Lamport,
    pending_spl_withdrawal_requests: BTreeMap<LedgerBurnIndex, PendingSplWithdrawalRequest>,
    created_spl_withdrawal_requests: BTreeMap<LedgerBurnIndex, PendingSplWithdrawalRequest>,
    created_spl_withdrawal_txs: BTreeMap<Address, CreatedSplWithdrawalTransaction>,
    sent_spl_withdrawal_requests: BTreeMap<LedgerBurnIndex, SentSplWithdrawalRequest>,
    successful_spl_withdrawal_requests: BTreeMap<LedgerBurnIndex, SentSplWithdrawalRequest>,
    failed_spl_withdrawal_requests: BTreeMap<LedgerBurnIndex, SentSplWithdrawalRequest>,
    spl_withdrawal_token_burns: BTreeSet<(Principal, LedgerBurnIndex)>,
    pending_spl_withdrawal_fee_reimbursements:
        BTreeMap<LedgerBurnIndex, PendingSplWithdrawalFeeReimbursement>,
    completed_spl_withdrawal_fee_reimbursements:
        BTreeMap<LedgerBurnIndex, CompletedSplWithdrawalFeeReimbursement>,
    quarantined_spl_withdrawal_fee_reimbursements:
        BTreeMap<LedgerBurnIndex, PendingSplWithdrawalFeeReimbursement>,
}

impl State {
    pub fn ledger_suite_orchestrator_id(&self) -> Option<Principal> {
        self.ledger_suite_orchestrator_id
    }

    pub fn supported_spl_token(&self, mint: &Address) -> Option<&SupportedSplToken> {
        self.supported_spl_tokens.get(mint)
    }

    pub fn validate_spl_token_registration(
        &self,
        token: &SupportedSplToken,
    ) -> Result<(), AddSplTokenError> {
        if let Some(existing) = self.supported_spl_tokens.get(&token.mint) {
            if existing.token_symbol.is_none() && token.token_symbol.is_some() {
                let mut updated = existing.clone();
                updated.token_symbol = token.token_symbol.clone();
                if updated == *token {
                    return Ok(());
                }
            }
            return Err(AddSplTokenError::AlreadySupported {
                mint: token.mint.into(),
            });
        }
        if token.ledger_id == self.ledger_canister_id
            || self
                .supported_spl_tokens
                .values()
                .any(|t| t.ledger_id == token.ledger_id)
        {
            return Err(AddSplTokenError::LedgerAlreadyUsed {
                ledger_id: token.ledger_id,
            });
        }
        Ok(())
    }

    fn process_added_spl_token(&mut self, token: &SupportedSplToken) {
        self.validate_spl_token_registration(token)
            .expect("BUG: invalid SPL token registration event");
        self.supported_spl_tokens.insert(token.mint, token.clone());
    }

    pub fn minter_public_key(&self) -> Option<&SchnorrPublicKey> {
        self.minter_public_key.as_ref()
    }

    /// Cache the minter public key.
    ///
    /// Concurrent calls may each fetch the key before either one caches it.
    /// All of them fetch with identical arguments and thus obtain the same key,
    /// so caching the same key again is a no-op.
    ///
    /// # Panics
    /// This method will panic if a different public key is already cached,
    /// since the minter public key must never change.
    pub fn cache_minter_public_key(&mut self, public_key: SchnorrPublicKey) {
        match &self.minter_public_key {
            None => self.minter_public_key = Some(public_key),
            Some(cached) if *cached == public_key => {}
            Some(_) => panic!("BUG: attempt to overwrite the minter public key"),
        }
    }

    pub fn sol_rpc_canister_id(&self) -> Principal {
        self.sol_rpc_canister_id
    }

    pub fn ledger_canister_id(&self) -> Principal {
        self.ledger_canister_id
    }

    pub fn master_key_name(&self) -> Ed25519KeyName {
        self.master_key_name
    }

    pub fn deposit_sol_fee(&self) -> u128 {
        self.deposit_sol_fee
    }

    pub fn withdrawal_fee(&self) -> u64 {
        self.withdrawal_fee
    }

    pub fn minimum_withdrawal_amount(&self) -> u64 {
        self.minimum_withdrawal_amount
    }

    pub fn minimum_deposit_amount(&self) -> u64 {
        self.minimum_deposit_amount
    }

    pub fn solana_network(&self) -> SolanaNetwork {
        self.solana_network
    }

    pub fn deposit_sol_required_cycles(&self) -> u128 {
        self.deposit_sol_required_cycles
    }

    pub fn automatic_deposit_fee(&self) -> Option<Lamport> {
        self.automatic_deposit_fee
    }

    pub fn deposits(&self) -> &Deposits {
        &self.deposits
    }

    pub fn spl_deposits(&self) -> &SplDeposits {
        &self.spl_deposits
    }

    pub fn watchlist(&self) -> &Watchlist {
        self.deposits.watchlist()
    }

    pub fn watchlist_mut(&mut self) -> &mut Watchlist {
        self.deposits.watchlist_mut()
    }

    pub fn sent_withdrawal_requests(&self) -> &BTreeMap<LedgerBurnIndex, SentWithdrawalRequest> {
        &self.sent_withdrawal_requests
    }

    pub fn successful_withdrawal_requests(
        &self,
    ) -> &BTreeMap<LedgerBurnIndex, SentWithdrawalRequest> {
        &self.successful_withdrawal_requests
    }

    pub fn failed_withdrawal_requests(&self) -> &BTreeMap<LedgerBurnIndex, SentWithdrawalRequest> {
        &self.failed_withdrawal_requests
    }

    pub fn submitted_transactions(&self) -> &InsertionOrderedMap<Signature, MinterTransaction> {
        &self.submitted_transactions
    }

    pub fn created_withdrawal_txs(&self) -> &BTreeMap<Address, CreatedWithdrawalTransaction> {
        &self.created_withdrawal_txs
    }

    pub fn process_transaction_expired(&mut self, signature: &Signature) {
        assert!(
            !self.succeeded_transactions.contains(signature),
            "BUG: cannot mark already succeeded transaction {signature} as expired"
        );
        assert!(
            !self.failed_transactions.contains_key(signature),
            "BUG: cannot mark already failed transaction {signature} as expired"
        );
        let transaction = self
            .submitted_transactions
            .remove(signature)
            .unwrap_or_else(|| {
                panic!("BUG: cannot mark non-submitted transaction {signature} as expired")
            });
        match transaction {
            MinterTransaction::SweepDeposit { .. } => self.deposits.drop_swept(signature),
            MinterTransaction::SweepSplDeposit { .. } => {
                self.spl_deposits.drop_swept(signature);
                self.release_spl_sweep_cost(signature, 0);
            }
            MinterTransaction::Withdrawal { .. } => {
                panic!("BUG: durable-nonce withdrawal transaction {signature} cannot expire")
            }
        }
    }

    pub fn succeeded_transactions(&self) -> &BTreeSet<Signature> {
        &self.succeeded_transactions
    }

    pub fn failed_transactions(&self) -> &InsertionOrderedMap<Signature, MinterTransaction> {
        &self.failed_transactions
    }

    pub fn balance(&self) -> Lamport {
        self.balance
    }

    pub fn nonce_pool(&self) -> &DurableNoncePool {
        &self.nonce_pool
    }

    pub fn nonce_pool_addresses(&self) -> BTreeSet<Address> {
        self.nonce_pool.addresses().copied().collect()
    }

    /// The lamports of the balance set aside for the in-flight SPL sweeps, by signature.
    pub fn spl_sweep_reservations(&self) -> &BTreeMap<Signature, Lamport> {
        &self.spl_sweep_reservations
    }

    pub fn sol_rpc_client<R: Runtime>(&self, runtime: R) -> SolRpcClient<R> {
        self.sol_rpc_client_builder(runtime).build()
    }

    pub fn sol_rpc_client_builder<R: Runtime>(&self, runtime: R) -> ClientBuilder<R> {
        SolRpcClient::builder(runtime, self.sol_rpc_canister_id)
            .with_rpc_sources(RpcSources::Default(SolanaCluster::from(
                self.solana_network,
            )))
            .with_consensus_strategy(ConsensusStrategy::Threshold {
                min: 3,
                total: Some(4),
            })
    }

    pub fn ledger_client<R: Runtime>(&self, runtime: R) -> LedgerClient<R> {
        LedgerClient::new(runtime, self.ledger_canister_id)
    }

    pub fn pending_deposit_sol_request_guards_mut(&mut self) -> &mut BTreeSet<Account> {
        &mut self.pending_deposit_sol_request_guards
    }

    pub fn pending_deposit_spl_request_guards_mut(&mut self) -> &mut BTreeSet<(Account, Address)> {
        &mut self.pending_deposit_spl_request_guards
    }

    pub fn pending_withdrawal_request_guards_mut(&mut self) -> &mut BTreeSet<Account> {
        &mut self.pending_withdrawal_request_guards
    }

    pub fn pending_operating_top_up_guards_mut(&mut self) -> &mut BTreeSet<Account> {
        &mut self.pending_operating_top_up_guards
    }

    pub fn active_tasks_mut(&mut self) -> &mut BTreeSet<TaskType> {
        &mut self.active_tasks
    }

    fn validate(&self) -> Result<(), InvalidStateError> {
        if matches!(self.ledger_suite_orchestrator_id, Some(id)
            if id == Principal::anonymous() || id == Principal::management_canister())
        {
            return Err(InvalidStateError::InvalidCanisterId(
                "ERROR: ledger suite orchestrator must be a non-anonymous, non-management principal".to_string(),
            ));
        }
        let mut canister_ids = vec![self.sol_rpc_canister_id, self.ledger_canister_id];
        canister_ids.extend(self.ledger_suite_orchestrator_id);
        let distinct_ids: BTreeSet<_> = canister_ids.iter().copied().collect();
        if distinct_ids.contains(&Principal::anonymous()) {
            return Err(InvalidStateError::InvalidCanisterId(
                "ERROR: anonymous principal is not accepted!".to_string(),
            ));
        }
        if distinct_ids.len() < canister_ids.len() {
            return Err(InvalidStateError::InvalidCanisterId(
                "ERROR: provided canister IDs are not distinct!".to_string(),
            ));
        }
        let maximum_sweep_fee = MAX_SIGNATURES * FEE_PER_SIGNATURE;
        if self.minimum_deposit_amount < maximum_sweep_fee + RENT_EXEMPTION_THRESHOLD {
            return Err(InvalidStateError::InvalidMinimumDepositAmount {
                minimum_deposit_amount: self.minimum_deposit_amount,
                maximum_sweep_fee,
                rent_exemption_threshold: RENT_EXEMPTION_THRESHOLD,
            });
        }
        if self.minimum_deposit_amount < 2 * RENT_EXEMPTION_THRESHOLD + FEE_PER_SIGNATURE {
            return Err(
                InvalidStateError::MinimumDepositAmountLeavesMainAddressBelowRent {
                    minimum_deposit_amount: self.minimum_deposit_amount,
                    rent_exemption_threshold: RENT_EXEMPTION_THRESHOLD,
                    fee_per_signature: FEE_PER_SIGNATURE,
                },
            );
        }
        if self.minimum_withdrawal_amount < self.withdrawal_fee + RENT_EXEMPTION_THRESHOLD {
            return Err(InvalidStateError::InvalidMinimumWithdrawalAmount {
                minimum_withdrawal_amount: self.minimum_withdrawal_amount,
                withdrawal_fee: self.withdrawal_fee,
                rent_exemption_threshold: RENT_EXEMPTION_THRESHOLD,
            });
        }
        if self.deposit_sol_required_cycles < GET_BALANCE_CYCLES + self.deposit_sol_fee {
            return Err(InvalidStateError::DepositSolRequiredCyclesTooLow {
                required_cycles: self.deposit_sol_required_cycles,
                get_balance_cycles: GET_BALANCE_CYCLES,
                deposit_sol_fee: self.deposit_sol_fee,
            });
        }
        if let Some(automatic_deposit_fee) = self.automatic_deposit_fee
            && automatic_deposit_fee > self.max_automatic_deposit_fee()
        {
            return Err(InvalidStateError::AutomaticDepositFeeTooHigh {
                automatic_deposit_fee,
                max_automatic_deposit_fee: self.max_automatic_deposit_fee(),
            });
        }
        Ok(())
    }

    fn max_automatic_deposit_fee(&self) -> Lamport {
        self.minimum_deposit_amount
            .saturating_sub(RENT_EXEMPTION_THRESHOLD)
            .saturating_sub(FEE_PER_SIGNATURE)
            .saturating_sub(1)
    }

    fn upgrade(
        &mut self,
        UpgradeArgs {
            ledger_suite_orchestrator_id,
            sol_rpc_canister_id,
            minimum_withdrawal_amount,
            minimum_deposit_amount,
            withdrawal_fee,
            deposit_sol_required_cycles,
            deposit_sol_fee,
            nonce_accounts_to_add,
            automatic_deposit_fee,
        }: UpgradeArgs,
    ) -> Result<(), InvalidStateError> {
        if let Some(id) = ledger_suite_orchestrator_id {
            self.ledger_suite_orchestrator_id = Some(id);
        }
        if let Some(sol_rpc_canister_id) = sol_rpc_canister_id {
            self.sol_rpc_canister_id = sol_rpc_canister_id;
        }
        if let Some(withdrawal_fee) = withdrawal_fee {
            self.withdrawal_fee = withdrawal_fee;
        }
        if let Some(minimum_withdrawal_amount) = minimum_withdrawal_amount {
            self.minimum_withdrawal_amount = minimum_withdrawal_amount;
        }
        if let Some(minimum_deposit_amount) = minimum_deposit_amount {
            self.minimum_deposit_amount = minimum_deposit_amount;
        }
        if let Some(deposit_sol_required_cycles) = deposit_sol_required_cycles {
            self.deposit_sol_required_cycles = deposit_sol_required_cycles as u128;
        }
        if let Some(deposit_sol_fee) = deposit_sol_fee {
            self.deposit_sol_fee = deposit_sol_fee as u128;
        }
        if let Some(automatic_deposit_fee) = automatic_deposit_fee {
            self.automatic_deposit_fee = Some(automatic_deposit_fee);
        }
        if let Some(nonce_accounts) = nonce_accounts_to_add {
            let nonce_accounts = parse_nonce_accounts(nonce_accounts)?;
            self.ensure_no_incomplete_withdrawal_to(&nonce_accounts)?;
            self.nonce_pool.add_accounts(nonce_accounts)?;
        }
        self.validate()
    }

    /// An incomplete withdrawal survives an upgrade, so an address may only
    /// join the nonce pool once no queued or in-flight transfer targets it;
    /// otherwise the withdrawal would credit a minter-controlled account.
    fn ensure_no_incomplete_withdrawal_to(
        &self,
        nonce_accounts: &[Address],
    ) -> Result<(), InvalidStateError> {
        let incomplete_destinations: BTreeSet<Address> = self
            .pending_withdrawal_requests
            .values()
            .chain(self.created_withdrawal_requests.values())
            .map(|pending| &pending.request)
            .chain(
                self.sent_withdrawal_requests
                    .values()
                    .map(|sent| &sent.request),
            )
            .map(|request| Address::from(request.solana_address))
            .collect();
        match nonce_accounts
            .iter()
            .find(|address| incomplete_destinations.contains(address))
        {
            Some(address) => Err(InvalidStateError::NonceAccountIsWithdrawalDestination(
                *address,
            )),
            None => Ok(()),
        }
    }

    fn process_queued_deposit(
        &mut self,
        deposit_id: DepositSolId,
        deposit: QueuedDeposit,
        queued_at: u64,
    ) {
        debug_assert_eq!(
            deposit.address,
            account_address(
                self.minter_public_key
                    .as_ref()
                    .expect("BUG: a deposit was queued before the minter public key was recorded"),
                &deposit.account,
            ),
            "Attempted to queue deposit {deposit_id} with address {} not derived from account {:?}",
            deposit.address,
            deposit.account,
        );
        self.deposits.queue(deposit_id, deposit, queued_at);
    }

    fn process_queued_spl_deposit(&mut self, deposit_id: DepositSplId, deposit: QueuedSplDeposit) {
        assert!(
            self.supported_spl_tokens.contains_key(&deposit.mint),
            "Attempted to queue an unsupported SPL mint {}",
            deposit.mint
        );
        self.spl_deposits.queue(deposit_id, deposit);
    }

    fn process_credited_sweep(
        &mut self,
        signature: &Signature,
        amount_received: Lamport,
        mints: &[CreditedDeposit],
        timestamp: u64,
    ) {
        let amount_to_mint: Lamport = mints.iter().map(|mint| mint.amount_to_mint).sum();
        assert!(
            amount_to_mint <= amount_received,
            "Attempted to credit sweep {signature} with mints of {amount_to_mint} lamports exceeding the {amount_received} lamports received"
        );
        self.deposits.credit_sweep(signature, mints, timestamp);
        self.balance += amount_received;
    }

    fn process_credited_spl_sweep(
        &mut self,
        signature: &Signature,
        lamports_spent: Lamport,
        mints: &[event::CreditedSplDeposit],
        timestamp: u64,
    ) {
        self.spl_deposits.credit_sweep(signature, mints, timestamp);
        self.release_spl_sweep_cost(signature, lamports_spent);
    }

    /// Sets the most the sweep can cost aside, so that withdrawals cannot spend it meanwhile.
    /// The timer submits only a covered sweep, so the cap on the balance never traps a replay.
    fn reserve_spl_sweep_cost(&mut self, signature: &Signature) {
        let reserved = self
            .submitted_spl_sweep(signature)
            .max_lamports_spent()
            .min(self.balance);
        self.balance -= reserved;
        self.operating_balance = self.operating_balance.saturating_sub(reserved);
        self.spl_sweep_reservations.insert(*signature, reserved);
    }

    /// Returns the reservation to the balance and debits the spend, so that a spend above a
    /// partial reservation still leaves the balance.
    fn release_spl_sweep_cost(&mut self, signature: &Signature, spent: Lamport) {
        let reserved = self
            .spl_sweep_reservations
            .remove(signature)
            .unwrap_or_default();
        self.balance = self.balance.saturating_add(reserved).saturating_sub(spent);
        self.operating_balance = self
            .operating_balance
            .saturating_add(reserved)
            .saturating_sub(spent);
    }

    fn process_operating_top_up(&mut self, amount: Lamport) {
        self.operating_balance = self.operating_balance.saturating_add(amount);
    }

    /// A failed sweep paid the fee of its signatures on chain, never the rent.
    fn release_failed_spl_sweep_cost(&mut self, signature: &Signature) {
        let fee = self.submitted_spl_sweep(signature).transaction_fee();
        self.release_spl_sweep_cost(signature, fee);
    }

    fn submitted_spl_sweep(&self, signature: &Signature) -> &SplSweep {
        self.spl_deposits
            .swept()
            .get(signature)
            .unwrap_or_else(|| panic!("BUG: SPL sweep {signature} is not submitted"))
    }

    fn process_minted_swept_deposit(
        &mut self,
        deposit_id: DepositSolId,
        mint_block_index: &LedgerMintIndex,
    ) {
        self.deposits.mint(deposit_id, *mint_block_index);
    }

    fn process_quarantined_pending_mint(&mut self, deposit_id: DepositSolId) {
        self.deposits.quarantine_pending_mint(deposit_id);
    }

    fn process_minted_swept_spl_deposit(
        &mut self,
        deposit_id: DepositSplId,
        mint_block_index: &LedgerMintIndex,
        timestamp: u64,
    ) {
        self.spl_deposits
            .mint(deposit_id, *mint_block_index, timestamp);
    }

    fn process_minted_spl_deposit_fee(
        &mut self,
        deposit_id: DepositSplId,
        mint_block_index: &LedgerMintIndex,
    ) {
        self.spl_deposits.mint_fee(deposit_id, *mint_block_index);
    }

    fn process_quarantined_pending_spl_fee_mint(&mut self, deposit_id: DepositSplId) {
        self.spl_deposits.quarantine_fee_mint(deposit_id);
    }

    fn process_quarantined_pending_spl_mint(&mut self, deposit_id: DepositSplId) {
        self.spl_deposits.quarantine_pending_mint(deposit_id);
    }

    fn process_quarantined_sweep(&mut self, signature: &Signature) {
        self.deposits.quarantine_sweep(signature);
    }

    fn process_quarantined_spl_sweep(&mut self, signature: &Signature) {
        self.spl_deposits.quarantine_sweep(signature);
        // The spend is unknown, so the reservation stays out of the balance.
        self.spl_sweep_reservations.remove(signature);
    }

    pub fn withdrawal_status(&self, block_index: u64) -> WithdrawSolStatus {
        let burn_index = LedgerBurnIndex::from(block_index);
        if self.pending_withdrawal_requests.contains_key(&burn_index)
            || self.created_withdrawal_requests.contains_key(&burn_index)
        {
            return WithdrawSolStatus::Pending;
        }
        if let Some(sent) = self.sent_withdrawal_requests.get(&burn_index) {
            return WithdrawSolStatus::TxSent {
                transaction_id: sent.signature.into(),
            };
        }
        if let Some(sent) = self.successful_withdrawal_requests.get(&burn_index) {
            return WithdrawSolStatus::TxFinalized(TxFinalizedStatus::Success {
                transaction_id: sent.signature.into(),
            });
        }
        if let Some(sent) = self.failed_withdrawal_requests.get(&burn_index) {
            return WithdrawSolStatus::TxFinalized(TxFinalizedStatus::Failure {
                transaction_id: sent.signature.into(),
            });
        }
        WithdrawSolStatus::NotFound
    }

    pub fn pending_withdrawal_requests(
        &self,
    ) -> &BTreeMap<LedgerBurnIndex, PendingWithdrawalRequest> {
        &self.pending_withdrawal_requests
    }

    pub fn created_withdrawal_requests(
        &self,
    ) -> &BTreeMap<LedgerBurnIndex, PendingWithdrawalRequest> {
        &self.created_withdrawal_requests
    }

    /// The balance above the rent exemption of the main account, which withdrawals and SPL
    /// sweeps may spend.
    pub fn spendable_balance(&self) -> Lamport {
        self.balance.saturating_sub(RENT_EXEMPTION_THRESHOLD)
    }

    /// The part of the balance that no ckSOL backs, so the minter may spend it on its own costs.
    pub fn operating_balance(&self) -> Lamport {
        self.operating_balance
    }

    /// The lamports that SPL sweeps may spend: held by the minter and backed by no ckSOL.
    pub fn spl_sweep_allowance(&self) -> Lamport {
        self.spendable_balance().min(self.operating_balance())
    }

    pub fn covers_spl_sweep(&self, sweep: &SplSweep) -> bool {
        self.spl_sweep_allowance() >= sweep.max_lamports_spent()
    }

    pub fn covers_withdrawal_batch(&self, requests: &[WithdrawalRequest]) -> bool {
        let cost = requests
            .iter()
            .fold(BATCH_WITHDRAWAL_TX_FEE, |cost, request| {
                cost.saturating_add(request.amount_to_transfer)
            });
        self.spendable_balance() >= cost
    }

    pub fn withdrawal_batches(&self) -> WithdrawalBatches<'_> {
        WithdrawalBatches {
            pending_requests: self.pending_withdrawal_requests.values().peekable(),
            available_balance: self.spendable_balance(),
        }
    }

    pub fn can_create_withdrawal_transaction(&self) -> bool {
        self.nonce_pool.num_free_accounts() > 0 && self.withdrawal_batches().next().is_some()
    }

    pub fn has_unsigned_withdrawal_transaction(&self) -> bool {
        !self.created_withdrawal_txs.is_empty()
    }

    /// Returns the creation timestamp (in nanoseconds) of the oldest incomplete withdrawal request.
    /// An incomplete withdrawal is one that has not yet been finalized (succeeded or failed).
    pub fn oldest_incomplete_withdrawal_created_at(&self) -> Option<u64> {
        let pending = self
            .pending_withdrawal_requests
            .values()
            .map(|r| r.created_at);
        let created = self
            .created_withdrawal_requests
            .values()
            .map(|r| r.created_at);
        let sent = self.sent_withdrawal_requests.values().map(|r| r.created_at);
        pending.chain(created).chain(sent).min()
    }

    pub fn pending_spl_withdrawal_requests(
        &self,
    ) -> &BTreeMap<LedgerBurnIndex, PendingSplWithdrawalRequest> {
        &self.pending_spl_withdrawal_requests
    }

    pub fn created_spl_withdrawal_requests(
        &self,
    ) -> &BTreeMap<LedgerBurnIndex, PendingSplWithdrawalRequest> {
        &self.created_spl_withdrawal_requests
    }

    pub fn created_spl_withdrawal_txs(
        &self,
    ) -> &BTreeMap<Address, CreatedSplWithdrawalTransaction> {
        &self.created_spl_withdrawal_txs
    }

    pub fn sent_spl_withdrawal_requests(
        &self,
    ) -> &BTreeMap<LedgerBurnIndex, SentSplWithdrawalRequest> {
        &self.sent_spl_withdrawal_requests
    }

    pub fn successful_spl_withdrawal_requests(
        &self,
    ) -> &BTreeMap<LedgerBurnIndex, SentSplWithdrawalRequest> {
        &self.successful_spl_withdrawal_requests
    }

    pub fn failed_spl_withdrawal_requests(
        &self,
    ) -> &BTreeMap<LedgerBurnIndex, SentSplWithdrawalRequest> {
        &self.failed_spl_withdrawal_requests
    }

    pub fn pending_spl_withdrawal_fee_reimbursements(
        &self,
    ) -> &BTreeMap<LedgerBurnIndex, PendingSplWithdrawalFeeReimbursement> {
        &self.pending_spl_withdrawal_fee_reimbursements
    }

    pub fn completed_spl_withdrawal_fee_reimbursements(
        &self,
    ) -> &BTreeMap<LedgerBurnIndex, CompletedSplWithdrawalFeeReimbursement> {
        &self.completed_spl_withdrawal_fee_reimbursements
    }

    pub fn quarantined_spl_withdrawal_fee_reimbursements(
        &self,
    ) -> &BTreeMap<LedgerBurnIndex, PendingSplWithdrawalFeeReimbursement> {
        &self.quarantined_spl_withdrawal_fee_reimbursements
    }

    fn process_quarantined_spl_withdrawal_fee_reimbursement(
        &mut self,
        burn_index: LedgerBurnIndex,
    ) {
        let pending = self
            .pending_spl_withdrawal_fee_reimbursements
            .remove(&burn_index)
            .unwrap_or_else(|| {
                panic!("Attempted to quarantine the fee reimbursement of SPL withdrawal {burn_index} that has no pending request")
            });
        self.quarantined_spl_withdrawal_fee_reimbursements
            .insert(burn_index, pending);
    }

    fn process_reimbursed_spl_withdrawal_fee(
        &mut self,
        burn_index: LedgerBurnIndex,
        mint_block_index: &LedgerMintIndex,
        reimbursed_at: u64,
    ) {
        let pending = self
            .pending_spl_withdrawal_fee_reimbursements
            .remove(&burn_index)
            .unwrap_or_else(|| {
                panic!("Attempted to complete the fee reimbursement of SPL withdrawal {burn_index} that has no pending request")
            });
        self.completed_spl_withdrawal_fee_reimbursements.insert(
            burn_index,
            CompletedSplWithdrawalFeeReimbursement {
                request: pending.request,
                mint_block_index: *mint_block_index,
                created_at: pending.created_at,
                reimbursed_at,
            },
        );
    }

    fn assert_valid_spl_withdrawal_terms(&self, request: &SplWithdrawalRequestTerms) {
        let token = self
            .supported_spl_tokens
            .get(&request.mint)
            .expect("Attempted to accept an SPL withdrawal for an unsupported mint");
        assert_eq!(
            token.ledger_id, request.ledger_id,
            "Attempted to accept an SPL withdrawal with a ledger different from the mint's registered ledger"
        );
        assert!(
            request.amount > 0,
            "Attempted to accept an SPL withdrawal for a zero token amount"
        );
        assert!(
            request.fee > 0,
            "Attempted to accept an SPL withdrawal with a zero fee burn"
        );
    }

    fn has_spl_withdrawal_outcome(&self, burn_index: &LedgerBurnIndex) -> bool {
        self.pending_spl_withdrawal_requests
            .contains_key(burn_index)
            || self
                .created_spl_withdrawal_requests
                .contains_key(burn_index)
            || self.sent_spl_withdrawal_requests.contains_key(burn_index)
            || self
                .successful_spl_withdrawal_requests
                .contains_key(burn_index)
            || self.failed_spl_withdrawal_requests.contains_key(burn_index)
            || self
                .pending_spl_withdrawal_fee_reimbursements
                .contains_key(burn_index)
            || self
                .completed_spl_withdrawal_fee_reimbursements
                .contains_key(burn_index)
            || self
                .quarantined_spl_withdrawal_fee_reimbursements
                .contains_key(burn_index)
    }

    fn process_accepted_spl_withdrawal(&mut self, request: &SplWithdrawalRequest, created_at: u64) {
        let cksol_burn_block_index = request.cksol_burn_block_index;
        self.assert_valid_spl_withdrawal_terms(&request.terms);
        assert!(
            !self.has_spl_withdrawal_outcome(&cksol_burn_block_index),
            "Attempted to accept an already accepted SPL withdrawal request: {cksol_burn_block_index}"
        );
        let token_burn = (request.terms.ledger_id, request.ckspl_burn_block_index);
        assert!(
            !self.spl_withdrawal_token_burns.contains(&token_burn),
            "Attempted to reuse a token burn that already belongs to a withdrawal: {token_burn:?}"
        );
        self.spl_withdrawal_token_burns.insert(token_burn);
        self.pending_spl_withdrawal_requests.insert(
            cksol_burn_block_index,
            PendingSplWithdrawalRequest {
                request: request.clone(),
                created_at,
            },
        );
    }

    fn process_failed_spl_withdrawal(
        &mut self,
        request: &SplWithdrawalFeeReimbursementRequest,
        created_at: u64,
    ) {
        let burn_index = request.cksol_burn_block_index;
        assert!(
            !self.has_spl_withdrawal_outcome(&burn_index),
            "Attempted to schedule an already scheduled SPL withdrawal fee reimbursement: {burn_index}"
        );
        assert!(
            request.reimbursed_amount > 0,
            "Attempted to reimburse an SPL withdrawal with a zero amount"
        );
        self.pending_spl_withdrawal_fee_reimbursements.insert(
            burn_index,
            PendingSplWithdrawalFeeReimbursement {
                request: request.clone(),
                created_at,
            },
        );
    }

    fn process_accepted_withdrawal(&mut self, request: &WithdrawalRequest, created_at: u64) {
        assert_eq!(
            self.pending_withdrawal_requests.insert(
                request.burn_block_index,
                PendingWithdrawalRequest {
                    request: request.clone(),
                    created_at,
                }
            ),
            None,
            "Attempted to accept an already accepted withdrawal request: {:?}",
            request.burn_block_index
        );
    }

    fn process_transaction_submitted(
        &mut self,
        signature: &Signature,
        transaction: &VersionedMessage,
        signers: &[Signer],
        purpose: &TransactionPurpose,
        timestamp: u64,
    ) {
        assert!(
            !self.succeeded_transactions.contains(signature),
            "Attempted to submit already succeeded transaction {signature:?}"
        );
        assert!(
            !self.failed_transactions.contains_key(signature),
            "Attempted to submit already failed transaction {signature:?}"
        );
        let submitted_transaction = match purpose {
            TransactionPurpose::SplWithdrawal {
                cksol_burn_block_index,
            } => self.send_spl_nonce_withdrawal(
                signature,
                transaction.clone(),
                signers,
                *cksol_burn_block_index,
                timestamp,
            ),
            TransactionPurpose::Withdrawal { burn_indices } => self.send_nonce_withdrawal(
                signature,
                transaction.clone(),
                signers,
                burn_indices,
                timestamp,
            ),
            TransactionPurpose::SweepDeposit {
                deposit_ids,
                block_height,
            } => {
                let sweep_destination = minter_address(self.minter_public_key.as_ref().expect(
                    "BUG: a sweep was submitted before the minter public key was recorded",
                ));
                self.deposits
                    .sweep(deposit_ids, sweep_destination, transaction, signature);
                MinterTransaction::SweepDeposit {
                    block_height: *block_height,
                }
            }
            TransactionPurpose::SweepSplDeposits {
                deposit_ids,
                block_height,
            } => {
                self.spl_deposits.sweep(
                    deposit_ids,
                    transaction,
                    signers,
                    &self.supported_spl_tokens,
                    signature,
                );
                self.reserve_spl_sweep_cost(signature);
                MinterTransaction::SweepSplDeposit {
                    block_height: *block_height,
                }
            }
            TransactionPurpose::RetrySplDeposit {
                deposit_id,
                block_height,
            } => {
                self.spl_deposits.retry(
                    *deposit_id,
                    transaction,
                    signers,
                    &self.supported_spl_tokens,
                    signature,
                );
                self.reserve_spl_sweep_cost(signature);
                MinterTransaction::SweepSplDeposit {
                    block_height: *block_height,
                }
            }
        };
        assert_eq!(
            self.submitted_transactions
                .insert(*signature, submitted_transaction),
            None,
            "Attempted to submit transaction with signature {signature:?} twice"
        );
    }

    /// The lamports the transaction spends: its signature fee, plus the destination
    /// account rent when the request allows creating it.
    fn spl_withdrawal_cost(&self, terms: &SplWithdrawalRequestTerms) -> Lamport {
        let rent = if terms.create_destination_account {
            self.supported_spl_token(&terms.mint)
                .expect("BUG: an accepted SPL withdrawal has a registered mint")
                .token_program
                .account_rent_exemption()
        } else {
            0
        };
        FEE_PER_SIGNATURE + rent
    }

    fn process_spl_transaction_created(
        &mut self,
        cksol_burn_block_index: LedgerBurnIndex,
        nonce_account: &Address,
        nonce_value: Hash,
    ) {
        let withdrawal = self
            .pending_spl_withdrawal_requests
            .remove(&cksol_burn_block_index)
            .expect("Attempted to create a transaction for an SPL withdrawal that is not pending");
        self.balance = self
            .balance
            .checked_sub(self.spl_withdrawal_cost(&withdrawal.request.terms))
            .expect("BUG: insufficient minter balance for SPL withdrawal");
        self.nonce_pool.bind(nonce_account, nonce_value);
        assert_eq!(
            self.created_spl_withdrawal_requests
                .insert(cksol_burn_block_index, withdrawal),
            None,
            "Attempted to create a transaction for an already created SPL withdrawal: {cksol_burn_block_index}"
        );
        assert_eq!(
            self.created_spl_withdrawal_txs.insert(
                *nonce_account,
                CreatedSplWithdrawalTransaction {
                    nonce_value,
                    cksol_burn_block_index,
                },
            ),
            None,
            "BUG: nonce account {nonce_account} already has a created SPL transaction"
        );
    }

    fn process_transaction_created(
        &mut self,
        burn_indices: &[LedgerBurnIndex],
        nonce_account: &Address,
        nonce_value: Hash,
    ) {
        self.nonce_pool.bind(nonce_account, nonce_value);
        let mut total: Lamport = 0;
        for burn_index in burn_indices {
            let pending = self
                .pending_withdrawal_requests
                .remove(burn_index)
                .unwrap_or_else(|| {
                    panic!(
                        "Attempted to create transaction for unknown withdrawal request: {burn_index:?}"
                    )
                });
            total = total
                .checked_add(pending.request.amount_to_transfer)
                .expect("BUG: total amount of a withdrawal transaction overflows");
            assert_eq!(
                self.created_withdrawal_requests
                    .insert(*burn_index, pending),
                None,
                "Attempted to create transaction for already created withdrawal request: {burn_index:?}"
            );
        }
        self.balance = self
            .balance
            .checked_sub(total + BATCH_WITHDRAWAL_TX_FEE)
            .expect("BUG: insufficient minter balance for withdrawal");
        assert_eq!(
            self.created_withdrawal_txs.insert(
                *nonce_account,
                CreatedWithdrawalTransaction {
                    nonce_value,
                    burn_indices: burn_indices.to_vec(),
                }
            ),
            None,
            "BUG: nonce account {nonce_account} already has a created transaction"
        );
    }

    fn send_spl_nonce_withdrawal(
        &mut self,
        signature: &Signature,
        message: VersionedMessage,
        signers: &[Signer],
        cksol_burn_block_index: LedgerBurnIndex,
        submitted_at: u64,
    ) -> MinterTransaction {
        let nonce_account = nonce_account_of(signature, &message);
        let created = self
            .created_spl_withdrawal_txs
            .remove(&nonce_account)
            .unwrap_or_else(|| {
                panic!(
                    "BUG: no SPL withdrawal transaction was created for nonce account {nonce_account}"
                )
            });
        assert_eq!(
            cksol_burn_block_index, created.cksol_burn_block_index,
            "BUG: SPL withdrawal transaction {signature} does not serve the request bound to nonce account {nonce_account}"
        );
        let bound = self
            .bound_spl_withdrawal_message(&nonce_account, &created)
            .unwrap_or_else(|e| {
                panic!("BUG: cannot rebuild the SPL withdrawal message bound to nonce account {nonce_account}: {e}")
            });
        assert_bound_withdrawal(signature, &nonce_account, &message, bound, signers);
        let pending = self
            .created_spl_withdrawal_requests
            .remove(&cksol_burn_block_index)
            .unwrap_or_else(|| {
                panic!(
                    "BUG: SPL withdrawal request {cksol_burn_block_index} of a created transaction is not in the created bucket"
                )
            });
        assert_eq!(
            self.sent_spl_withdrawal_requests.insert(
                cksol_burn_block_index,
                SentSplWithdrawalRequest {
                    request: pending.request,
                    signature: *signature,
                    created_at: pending.created_at,
                },
            ),
            None,
            "Attempted to send an already sent SPL withdrawal: {cksol_burn_block_index}"
        );
        MinterTransaction::Withdrawal {
            message,
            nonce_account,
            nonce_value: created.nonce_value,
            submitted_at,
        }
    }

    fn send_nonce_withdrawal(
        &mut self,
        signature: &Signature,
        message: VersionedMessage,
        signers: &[Signer],
        burn_indices: &[LedgerBurnIndex],
        submitted_at: u64,
    ) -> MinterTransaction {
        let nonce_account = nonce_account_of(signature, &message);
        let created = self
            .created_withdrawal_txs
            .remove(&nonce_account)
            .unwrap_or_else(|| {
                panic!(
                    "BUG: no withdrawal transaction was created for nonce account {nonce_account}"
                )
            });
        assert_eq!(
            burn_indices, created.burn_indices,
            "BUG: withdrawal transaction {signature} does not serve the withdrawal requests bound to nonce account {nonce_account}"
        );
        let bound = self.bound_withdrawal_message(&nonce_account, &created);
        assert_bound_withdrawal(signature, &nonce_account, &message, bound, signers);
        for burn_index in &created.burn_indices {
            let pending = self
                .created_withdrawal_requests
                .remove(burn_index)
                .unwrap_or_else(|| {
                    panic!(
                        "BUG: withdrawal request {burn_index:?} of a created transaction is not in the created bucket"
                    )
                });
            assert_eq!(
                self.sent_withdrawal_requests.insert(
                    *burn_index,
                    SentWithdrawalRequest {
                        request: pending.request,
                        signature: *signature,
                        created_at: pending.created_at,
                    },
                ),
                None,
                "Attempted to send transaction for already sent withdrawal request: {burn_index:?}"
            );
        }
        MinterTransaction::Withdrawal {
            message,
            nonce_account,
            nonce_value: created.nonce_value,
            submitted_at,
        }
    }

    /// Rebuilds the message of the SPL withdrawal bound to this nonce account from
    /// recorded state only, so retries and replay produce the same message.
    pub fn bound_spl_withdrawal_message(
        &self,
        nonce_account: &Address,
        created: &CreatedSplWithdrawalTransaction,
    ) -> Result<Message, CreateTransferError> {
        let request = &self
            .created_spl_withdrawal_requests
            .get(&created.cksol_burn_block_index)
            .unwrap_or_else(|| {
                panic!(
                    "BUG: SPL withdrawal request {} of a created transaction is not in the created bucket",
                    created.cksol_burn_block_index
                )
            })
            .request;
        let token = self
            .supported_spl_token(&request.terms.mint)
            .expect("BUG: an accepted SPL withdrawal has a registered mint");
        let minter_address = minter_address(self.minter_public_key.as_ref().expect(
            "BUG: an SPL withdrawal message was rebuilt before the minter public key was recorded",
        ));
        let instructions = build_spl_withdrawal_instructions(
            &minter_address,
            token,
            &request.terms.address,
            request.terms.amount,
            request.terms.create_destination_account,
        );
        build_spl_withdrawal_message(
            &minter_address,
            nonce_account,
            created.nonce_value,
            &instructions,
        )
    }

    fn bound_withdrawal_message(
        &self,
        nonce_account: &Address,
        created: &CreatedWithdrawalTransaction,
    ) -> Message {
        let minter_address =
            minter_address(self.minter_public_key.as_ref().expect(
                "BUG: a withdrawal was submitted before the minter public key was recorded",
            ));
        let transfers: Vec<(Address, Lamport)> = created
            .burn_indices
            .iter()
            .map(|burn_index| {
                let request = &self
                    .created_withdrawal_requests
                    .get(burn_index)
                    .unwrap_or_else(|| {
                        panic!(
                            "BUG: withdrawal request {burn_index:?} of a created transaction is not in the created bucket"
                        )
                    })
                    .request;
                (
                    Address::from(request.solana_address),
                    request.amount_to_transfer,
                )
            })
            .collect();
        build_batch_withdrawal_message(
            &minter_address,
            nonce_account,
            created.nonce_value,
            &transfers,
        )
        .unwrap_or_else(|e| {
            panic!("BUG: cannot rebuild the withdrawal message bound to nonce account {nonce_account}: {e}")
        })
    }

    fn process_transaction_succeeded(&mut self, signature: &Signature) {
        assert!(
            !self.failed_transactions.contains_key(signature),
            "Attempted to mark already failed transaction {signature:?} as succeeded"
        );
        let transaction = self
            .submitted_transactions
            .remove(signature)
            .unwrap_or_else(|| {
                panic!("Attempted to mark unknown transaction {signature:?} as succeeded")
            });
        match transaction {
            MinterTransaction::SweepDeposit { .. } => self.deposits.finalize_swept(signature),
            MinterTransaction::SweepSplDeposit { .. } => {
                self.spl_deposits.finalize_swept(signature)
            }
            MinterTransaction::Withdrawal { nonce_account, .. } => {
                self.nonce_pool.free(&nonce_account)
            }
        }
        assert!(
            self.succeeded_transactions.insert(*signature),
            "Attempted to mark transaction {signature:?} as succeeded twice"
        );
        self.sent_withdrawal_requests
            .extract_if(.., |_, sent| &sent.signature == signature)
            .for_each(|(burn_index, sent)| {
                self.successful_withdrawal_requests.insert(burn_index, sent);
            });
        self.sent_spl_withdrawal_requests
            .extract_if(.., |_, sent| &sent.signature == signature)
            .for_each(|(burn_index, sent)| {
                self.successful_spl_withdrawal_requests
                    .insert(burn_index, sent);
            });
    }

    fn process_transaction_failed(&mut self, signature: &Signature) {
        let transaction = self.take_failed_transaction(signature);
        match &transaction {
            MinterTransaction::SweepDeposit { .. } => self.deposits.drop_swept(signature),
            MinterTransaction::SweepSplDeposit { .. } => {
                self.release_failed_spl_sweep_cost(signature);
                self.spl_deposits.drop_swept(signature);
            }
            MinterTransaction::Withdrawal { nonce_account, .. } => {
                self.nonce_pool.free(nonce_account)
            }
        }
        self.record_failed_transaction(signature, transaction);
    }

    fn process_split_failed_spl_sweep(&mut self, signature: &Signature) {
        let transaction = self.take_failed_spl_batch(signature);
        self.release_failed_spl_sweep_cost(signature);
        self.spl_deposits.retry_individually(signature);
        self.record_failed_transaction(signature, transaction);
    }

    fn take_failed_transaction(&mut self, signature: &Signature) -> MinterTransaction {
        assert!(
            !self.succeeded_transactions.contains(signature),
            "Attempted to mark already succeeded transaction {signature:?} as failed"
        );
        self.submitted_transactions
            .remove(signature)
            .unwrap_or_else(|| {
                panic!("Attempted to mark unknown transaction {signature:?} as failed")
            })
    }

    fn take_failed_spl_batch(&mut self, signature: &Signature) -> MinterTransaction {
        let transaction = self.take_failed_transaction(signature);
        assert!(
            matches!(transaction, MinterTransaction::SweepSplDeposit { .. })
                && self.spl_deposits.is_batch(signature),
            "Attempted to split a transaction that is not an SPL batch"
        );
        transaction
    }

    fn record_failed_transaction(&mut self, signature: &Signature, transaction: MinterTransaction) {
        assert_eq!(
            self.failed_transactions.insert(*signature, transaction),
            None,
            "Attempted to fail transaction {signature:?} twice"
        );
        self.sent_withdrawal_requests
            .extract_if(.., |_, sent| &sent.signature == signature)
            .for_each(|(burn_index, sent)| {
                self.failed_withdrawal_requests.insert(burn_index, sent);
            });
        let failed_spl_withdrawals: Vec<_> = self
            .sent_spl_withdrawal_requests
            .extract_if(.., |_, sent| &sent.signature == signature)
            .collect();
        for (burn_index, sent) in failed_spl_withdrawals {
            // Failed Solana transactions pay their fee but create no token accounts.
            let unspent_rent = self.spl_withdrawal_cost(&sent.request.terms) - FEE_PER_SIGNATURE;
            self.balance = self
                .balance
                .checked_add(unspent_rent)
                .expect("BUG: returning unspent SPL withdrawal rent overflows the balance");
            self.failed_spl_withdrawal_requests.insert(burn_index, sent);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum InvalidStateError {
    InvalidCanisterId(String),
    InvalidMinimumDepositAmount {
        minimum_deposit_amount: u64,
        maximum_sweep_fee: u64,
        rent_exemption_threshold: u64,
    },
    MinimumDepositAmountLeavesMainAddressBelowRent {
        minimum_deposit_amount: u64,
        rent_exemption_threshold: u64,
        fee_per_signature: u64,
    },
    InvalidMinimumWithdrawalAmount {
        minimum_withdrawal_amount: u64,
        withdrawal_fee: u64,
        rent_exemption_threshold: u64,
    },
    DepositSolRequiredCyclesTooLow {
        required_cycles: u128,
        get_balance_cycles: u128,
        deposit_sol_fee: u128,
    },
    AutomaticDepositFeeTooHigh {
        automatic_deposit_fee: Lamport,
        max_automatic_deposit_fee: Lamport,
    },
    InvalidNonceAccount(String),
    DuplicateNonceAccount(Address),
    NonceAccountIsWithdrawalDestination(Address),
}

impl From<NoncePoolError> for InvalidStateError {
    fn from(error: NoncePoolError) -> Self {
        match error {
            NoncePoolError::DuplicateAccount(address) => Self::DuplicateNonceAccount(address),
        }
    }
}

fn parse_nonce_accounts(addresses: Vec<String>) -> Result<Vec<Address>, InvalidStateError> {
    addresses
        .into_iter()
        .map(|address| {
            Address::from_str(&address).map_err(|error| {
                InvalidStateError::InvalidNonceAccount(format!(
                    "ERROR: failed to parse nonce account {address}: {error}"
                ))
            })
        })
        .collect()
}

impl TryFrom<InitArgs> for State {
    type Error = InvalidStateError;

    fn try_from(
        InitArgs {
            ledger_suite_orchestrator_id,
            sol_rpc_canister_id,
            ledger_canister_id,
            master_key_name,
            minimum_withdrawal_amount,
            minimum_deposit_amount,
            withdrawal_fee,
            deposit_sol_required_cycles,
            solana_network,
            deposit_sol_fee,
            nonce_accounts,
            automatic_deposit_fee,
        }: InitArgs,
    ) -> Result<Self, Self::Error> {
        let nonce_pool = DurableNoncePool::new(parse_nonce_accounts(nonce_accounts)?)?;
        let state = Self {
            minter_public_key: None,
            master_key_name,
            ledger_canister_id,
            sol_rpc_canister_id,
            ledger_suite_orchestrator_id,
            solana_network,
            withdrawal_fee,
            minimum_withdrawal_amount,
            minimum_deposit_amount,
            deposit_sol_required_cycles: deposit_sol_required_cycles as u128,
            deposit_sol_fee: deposit_sol_fee as u128,
            automatic_deposit_fee,
            pending_deposit_sol_request_guards: BTreeSet::new(),
            pending_deposit_spl_request_guards: BTreeSet::new(),
            pending_withdrawal_request_guards: BTreeSet::new(),
            pending_operating_top_up_guards: BTreeSet::new(),
            deposits: Deposits::default(),
            spl_deposits: SplDeposits::default(),
            supported_spl_tokens: BTreeMap::new(),
            pending_withdrawal_requests: BTreeMap::new(),
            created_withdrawal_requests: BTreeMap::new(),
            sent_withdrawal_requests: BTreeMap::new(),
            successful_withdrawal_requests: BTreeMap::new(),
            failed_withdrawal_requests: BTreeMap::new(),
            submitted_transactions: InsertionOrderedMap::new(),
            created_withdrawal_txs: BTreeMap::new(),
            succeeded_transactions: BTreeSet::new(),
            failed_transactions: InsertionOrderedMap::new(),
            nonce_pool,
            active_tasks: BTreeSet::new(),
            balance: 0,
            spl_sweep_reservations: BTreeMap::new(),
            operating_balance: 0,
            pending_spl_withdrawal_requests: BTreeMap::new(),
            created_spl_withdrawal_requests: BTreeMap::new(),
            created_spl_withdrawal_txs: BTreeMap::new(),
            sent_spl_withdrawal_requests: BTreeMap::new(),
            successful_spl_withdrawal_requests: BTreeMap::new(),
            failed_spl_withdrawal_requests: BTreeMap::new(),
            spl_withdrawal_token_burns: BTreeSet::new(),
            pending_spl_withdrawal_fee_reimbursements: BTreeMap::new(),
            completed_spl_withdrawal_fee_reimbursements: BTreeMap::new(),
            quarantined_spl_withdrawal_fee_reimbursements: BTreeMap::new(),
        };
        state.validate()?;
        Ok(state)
    }
}

/// The nonce account a withdrawal transaction advances first.
fn nonce_account_of(signature: &Signature, message: &VersionedMessage) -> Address {
    message.advanced_nonce_account().unwrap_or_else(|| {
        panic!(
            "BUG: withdrawal transaction {signature} does not start with an AdvanceNonceAccount instruction"
        )
    })
}

/// Checks that a submitted withdrawal carries the message bound to its nonce account
/// and the minter's signature only.
fn assert_bound_withdrawal(
    signature: &Signature,
    nonce_account: &Address,
    message: &VersionedMessage,
    bound: Message,
    signers: &[Signer],
) {
    assert_eq!(
        *message,
        VersionedMessage::Legacy(bound),
        "BUG: withdrawal transaction {signature} does not carry the message bound to nonce account {nonce_account}"
    );
    assert_eq!(
        signers,
        [Signer::Minter],
        "BUG: withdrawal transaction {signature} must be signed by the minter only"
    );
}

/// An SPL withdrawal bound to a durable nonce, waiting to be signed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedSplWithdrawalTransaction {
    pub nonce_value: Hash,
    pub cksol_burn_block_index: LedgerBurnIndex,
}

/// An accepted SPL withdrawal with both burns recorded, waiting for processing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingSplWithdrawalRequest {
    pub request: SplWithdrawalRequest,
    pub created_at: u64,
}

/// An SPL withdrawal recorded as submitted to Solana.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentSplWithdrawalRequest {
    pub request: SplWithdrawalRequest,
    pub signature: Signature,
    pub created_at: u64,
}

/// A failed token burn awaiting reimbursement of its ckSOL fee.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingSplWithdrawalFeeReimbursement {
    pub request: SplWithdrawalFeeReimbursementRequest,
    pub created_at: u64,
}

/// A failed token burn whose ckSOL fee was minted back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedSplWithdrawalFeeReimbursement {
    pub request: SplWithdrawalFeeReimbursementRequest,
    pub mint_block_index: LedgerMintIndex,
    pub created_at: u64,
    pub reimbursed_at: u64,
}

/// A pending withdrawal request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingWithdrawalRequest {
    pub request: WithdrawalRequest,
    pub created_at: u64,
}

/// Groups pending withdrawal requests, oldest first, into batches that the
/// minter balance can pay for, including one transaction fee per batch and
/// holding back the rent exemption threshold so a withdrawal transaction can
/// never leave the main address below it.
///
/// Iteration stops at the first request the remaining balance cannot cover,
/// so requests are never reordered or skipped.
pub struct WithdrawalBatches<'a> {
    pending_requests: Peekable<btree_map::Values<'a, LedgerBurnIndex, PendingWithdrawalRequest>>,
    available_balance: Lamport,
}

impl Iterator for WithdrawalBatches<'_> {
    type Item = Vec<WithdrawalRequest>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut batch = Vec::new();
        while batch.len() < MAX_WITHDRAWALS_PER_NONCE_TX {
            let reserved_fee = if batch.is_empty() {
                BATCH_WITHDRAWAL_TX_FEE
            } else {
                0
            };
            let Some(pending) = self.pending_requests.peek() else {
                break;
            };
            let Some(remaining_balance) = pending
                .request
                .amount_to_transfer
                .checked_add(reserved_fee)
                .and_then(|cost| self.available_balance.checked_sub(cost))
            else {
                break;
            };
            self.available_balance = remaining_balance;
            batch.push(pending.request.clone());
            self.pending_requests.next();
        }
        if batch.is_empty() { None } else { Some(batch) }
    }
}

/// The binding of a durable nonce account and its nonce value to the
/// withdrawal requests of a transaction whose threshold signature was not yet
/// recorded. The binding determines the transaction message, so that a signing
/// failure leads to re-signing the identical message instead of building a new
/// one for the same nonce value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedWithdrawalTransaction {
    pub nonce_value: Hash,
    pub burn_indices: Vec<LedgerBurnIndex>,
}

/// A withdrawal request that has been submitted in a Solana transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentWithdrawalRequest {
    pub request: WithdrawalRequest,
    pub signature: Signature,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchnorrPublicKey {
    pub public_key: PublicKey,
    pub chain_code: [u8; 32],
}

#[derive(Copy, Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TaskType {
    SweepDeposits,
    SweepSplDeposits,
    Mint,
    MintSpl,
    FinalizeTransactions,
    WithdrawalProcessing,
    ReimburseSplWithdrawalFees,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MinterTransaction {
    SweepDeposit {
        /// The block height of the block whose blockhash the transaction uses.
        block_height: BlockHeight,
    },
    /// A sweep of SPL deposits, batched or retried alone, dropped once its blockhash expires.
    SweepSplDeposit {
        /// The block height of the block whose blockhash the transaction uses.
        block_height: BlockHeight,
    },
    /// A durable-nonce withdrawal transaction, which never expires: it stays
    /// in flight until it is finalized.
    Withdrawal {
        message: VersionedMessage,
        /// The durable nonce account whose nonce value the transaction uses.
        nonce_account: Address,
        /// The durable nonce value the transaction uses instead of a recent blockhash.
        nonce_value: Hash,
        /// The time, in nanoseconds since the epoch, at which the transaction was recorded as submitted.
        submitted_at: u64,
    },
}
