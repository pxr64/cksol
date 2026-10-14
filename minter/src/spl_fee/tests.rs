use super::*;

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
        assert_eq!(estimator.estimate_fee(mint).await, Ok(0));
    }
}
