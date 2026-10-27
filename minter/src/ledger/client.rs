use crate::numeric::LedgerMintIndex;
use candid::{Nat, Principal};
use ic_canister_runtime::{IcError, Runtime};
use icrc_ledger_types::{
    icrc1::{
        account::Account,
        transfer::{BlockIndex, TransferArg, TransferError},
    },
    icrc2::transfer_from::{TransferFromArgs, TransferFromError},
};
use num_traits::ToPrimitive;

pub struct LedgerClient<R> {
    pub runtime: R,
    pub ledger_canister_id: Principal,
}

impl<R> LedgerClient<R> {
    pub fn new(runtime: R, ledger_canister_id: Principal) -> Self {
        Self {
            runtime,
            ledger_canister_id,
        }
    }
}

impl<R: Runtime> LedgerClient<R> {
    pub async fn transfer(
        &self,
        args: TransferArg,
    ) -> Result<Result<LedgerMintIndex, TransferError>, IcError> {
        self.runtime
            .update_call::<_, Result<BlockIndex, TransferError>>(
                self.ledger_canister_id,
                "icrc1_transfer",
                (args,),
                0,
            )
            .await
            .map(|result| result.map(to_ledger_mint_index))
    }
}

impl<R: Runtime> LedgerClient<R> {
    pub async fn minting_account(&self) -> Result<Option<Account>, IcError> {
        self.runtime
            .update_call(self.ledger_canister_id, "icrc1_minting_account", (), 0)
            .await
    }

    pub async fn balance_of(&self, account: Account) -> Result<Nat, IcError> {
        self.runtime
            .update_call(self.ledger_canister_id, "icrc1_balance_of", (account,), 0)
            .await
    }
}

impl<R: Runtime> LedgerClient<R> {
    pub async fn transfer_from(
        &self,
        args: TransferFromArgs,
    ) -> Result<Result<BlockIndex, TransferFromError>, IcError> {
        self.runtime
            .update_call(self.ledger_canister_id, "icrc2_transfer_from", (args,), 0)
            .await
    }
}

pub(crate) fn to_ledger_mint_index(block_index: BlockIndex) -> LedgerMintIndex {
    LedgerMintIndex::from(
        block_index
            .0
            .to_u64()
            .expect("ledger block index does not fit into u64"),
    )
}
