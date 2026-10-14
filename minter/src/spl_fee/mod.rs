use cksol_types::Lamport;
use solana_address::Address;

#[cfg(test)]
mod tests;

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
    /// Returns the fee in the mint's smallest token units.
    /// Fee estimation is disabled for now: return zero without querying the oracle.
    pub async fn estimate_fee(&self, _mint: Address) -> Result<u64, String> {
        Ok(0)
    }
}

/// Placeholder used until a price oracle is configured.
pub struct NoPriceOracle;

impl PriceOracle for NoPriceOracle {
    async fn sol_to_spl(&self, _amount: Lamport, _mint: Address) -> Result<u64, String> {
        Err("No price oracle is configured".to_string())
    }
}
