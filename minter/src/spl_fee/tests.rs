use super::*;
use crate::constants::{FEE_PER_SIGNATURE, TOKEN_ACCOUNT_RENT_EXEMPTION};

#[test]
fn should_estimate_sol_cost_with_and_without_rent() {
    for (transaction_fee, rent, expected) in [
        (0, 0, 0),
        (FEE_PER_SIGNATURE, 0, 6_000),
        (
            2 * FEE_PER_SIGNATURE,
            TOKEN_ACCOUNT_RENT_EXEMPTION,
            2_500_896,
        ),
    ] {
        assert_eq!(estimate_sol_cost(transaction_fee, rent), expected);
    }
}

#[test]
fn should_round_up_the_sol_cost_after_adding_the_fee_and_rent() {
    for (transaction_fee, rent, expected) in [(1, 0, 2), (4, 0, 5), (5, 0, 6), (1, 1, 3)] {
        assert_eq!(estimate_sol_cost(transaction_fee, rent), expected);
    }
}

#[test]
fn should_accept_the_largest_sol_cost_that_fits_after_the_margin() {
    assert_eq!(
        estimate_sol_cost(15_372_286_728_091_293_012, 0),
        Lamport::MAX
    );
}

#[test]
#[should_panic(expected = "BUG: the SPL sweep cost exceeds the lamport supply")]
fn should_panic_when_the_sol_cost_overflows_after_the_margin() {
    estimate_sol_cost(15_372_286_728_091_293_013, 0);
}

struct UnexpectedOracle;

impl PriceOracle for UnexpectedOracle {
    async fn sol_to_spl(&self, _amount: Lamport, _mint: Address) -> Result<u64, String> {
        panic!("Zero-fee estimation must not query the oracle");
    }
}

#[tokio::test]
async fn should_return_zero_without_querying_the_oracle() {
    let estimator = FeeEstimator::new(UnexpectedOracle);
    for mint in [Address::default(), Address::new_from_array([1; 32])] {
        assert_eq!(estimator.estimate_fee(mint, 0).await, Ok(0));
    }
}

#[tokio::test]
async fn should_fail_a_nonzero_cost_without_a_price_oracle() {
    let estimator = FeeEstimator::new(NoPriceOracle);

    assert_eq!(
        estimator.estimate_fee(Address::default(), 5_000).await,
        Err("No price oracle is configured".to_string())
    );
}

struct MockPriceOracle {
    amount: Lamport,
    mint: Address,
    result: Result<u64, String>,
}

impl PriceOracle for MockPriceOracle {
    async fn sol_to_spl(&self, amount: Lamport, mint: Address) -> Result<u64, String> {
        assert_eq!(amount, self.amount);
        assert_eq!(mint, self.mint);
        self.result.clone()
    }
}

#[tokio::test]
async fn should_forward_the_sol_cost_and_mint_to_the_oracle() {
    let mint = Address::new_from_array([1; 32]);
    let amount = 5_000;
    let estimator = FeeEstimator::new(MockPriceOracle {
        amount,
        mint,
        result: Ok(42),
    });

    assert_eq!(estimator.estimate_fee(mint, amount).await, Ok(42));
}

#[tokio::test]
async fn should_return_the_oracle_error() {
    let mint = Address::new_from_array([2; 32]);
    let amount = 10_000;
    let error = "Price oracle is unavailable".to_string();
    let estimator = FeeEstimator::new(MockPriceOracle {
        amount,
        mint,
        result: Err(error.clone()),
    });

    assert_eq!(estimator.estimate_fee(mint, amount).await, Err(error));
}
