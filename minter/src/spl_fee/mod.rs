use cksol_types::Lamport;
use solana_address::Address;

#[cfg(test)]
mod tests;

mod xrc;
pub use xrc::{XrcAssetError, XrcPriceOracle, check_xrc_asset};

const FEE_MARGIN_PERCENT: u128 = 20;

/// Adds a 20% margin to the transaction fee and applicable account rent.
/// Rounds up to whole lamports; pass zero rent when no account creation is needed.
pub fn estimate_sol_cost(transaction_fee: Lamport, rent: Lamport) -> Lamport {
    let cost = u128::from(transaction_fee) + u128::from(rent);
    (cost * (100 + FEE_MARGIN_PERCENT))
        .div_ceil(100)
        .try_into()
        .expect("BUG: the SPL sweep cost exceeds the lamport supply")
}

pub trait PriceOracle {
    /// Converts a SOL amount in lamports into the mint's smallest token units.
    fn sol_to_spl(
        &self,
        amount: Lamport,
        mint: Address,
    ) -> impl Future<Output = Result<u64, String>>;
}

pub struct FeeEstimator<O> {
    pub oracle: O,
}

impl<O> FeeEstimator<O> {
    pub fn new(oracle: O) -> Self {
        Self { oracle }
    }
}

impl<O: PriceOracle> FeeEstimator<O> {
    /// Converts the estimated SOL cost into the mint's smallest token units.
    /// A zero cost returns zero without querying the oracle.
    pub async fn estimate_fee(&self, mint: Address, amount: Lamport) -> Result<u64, String> {
        if amount == 0 {
            return Ok(0);
        }
        self.oracle.sol_to_spl(amount, mint).await
    }
}

/// Placeholder used until a price oracle is configured.
pub struct NoPriceOracle;

impl PriceOracle for NoPriceOracle {
    async fn sol_to_spl(&self, _amount: Lamport, _mint: Address) -> Result<u64, String> {
        Err("No price oracle is configured".to_string())
    }
}
