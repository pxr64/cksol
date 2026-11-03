use super::*;
use crate::state::{read_state, tests::helpers::*};

mod fee_quote {
    use super::*;

    #[test]
    fn should_quote_only_the_configured_fee_when_creation_is_not_allowed() {
        for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
            for fee in [12_345, 1_000_000, u64::MAX] {
                assert_eq!(quote_withdrawal_fee(fee, token_program, false), Ok(fee));
            }
        }
    }

    #[test]
    fn should_include_the_token_programs_ata_rent_when_creation_is_allowed() {
        assert_eq!(
            quote_withdrawal_fee(1_000_000, TokenProgram::Classic, true),
            Ok(3_039_280)
        );
        assert_eq!(
            quote_withdrawal_fee(1_000_000, TokenProgram::Token2022, true),
            Ok(3_074_080)
        );
        assert_eq!(
            quote_withdrawal_fee(2_000_000, TokenProgram::Classic, true),
            Ok(4_039_280)
        );
    }

    #[test]
    fn should_accept_the_largest_fee_that_can_include_rent() {
        for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
            let fee = u64::MAX - token_program.account_rent_exemption();
            assert_eq!(quote_withdrawal_fee(fee, token_program, true), Ok(u64::MAX));
        }
    }

    #[test]
    fn should_reject_a_fee_that_overflows_when_rent_is_added() {
        for token_program in [TokenProgram::Classic, TokenProgram::Token2022] {
            let fee = u64::MAX - token_program.account_rent_exemption() + 1;
            assert!(matches!(
                quote_withdrawal_fee(fee, token_program, true),
                Err(WithdrawSplError::TemporarilyUnavailable(_))
            ));
        }
    }
}

mod batch_budget {
    use super::*;

    #[test]
    fn should_budget_one_batch_fee_and_one_individual_retry_per_spl_withdrawal() {
        let mut requests = setup_spl_batch_budget_requests();
        read_state(|state| {
            assert_eq!(
                spl_withdrawal_batch_budget(state, &requests),
                Some(3 * FEE_PER_SIGNATURE)
            );
        });
        for request in &mut requests {
            request.terms.create_destination_account = true;
            request.terms.fee = u64::MAX;
        }
        // Both fallback transactions reserve their program's ATA rent, plus their signature fees.
        let expected = 3 * FEE_PER_SIGNATURE
            + TokenProgram::Classic.account_rent_exemption()
            + TokenProgram::Token2022.account_rent_exemption();
        let before = read_state(Clone::clone);
        assert_eq!(
            spl_withdrawal_batch_budget(&before, &requests),
            Some(expected)
        );
        read_state(|state| assert_eq!(state, &before));
    }

    #[test]
    fn should_budget_each_individual_retry_even_when_spl_requests_share_an_ata() {
        let requests = setup_spl_batch_budget_requests();
        let mut request = requests[0].clone();
        request.terms.create_destination_account = true;
        request.terms.fee = u64::MAX;
        let mut second = request.clone();
        second.cksol_burn_block_index = 102.into();
        second.ckspl_burn_block_index = 9.into();
        let shared_ata = vec![request, second];
        read_state(|state| {
            let rent = TokenProgram::Classic.account_rent_exemption();
            assert_eq!(
                spl_withdrawal_transaction_cost(
                    state,
                    shared_ata.iter().map(|request| &request.terms)
                ),
                FEE_PER_SIGNATURE + rent
            );
            // The batch only creates one ATA, but two concurrent solo retries each reserve rent.
            assert_eq!(
                spl_withdrawal_batch_budget(state, &shared_ata),
                Some(3 * FEE_PER_SIGNATURE + 2 * rent)
            );
        });
    }

    #[test]
    fn should_require_each_spl_fee_to_cover_its_retry_and_share_of_the_batch_fee() {
        let mut requests = setup_spl_batch_budget_requests();
        for request in &mut requests {
            request.terms.create_destination_account = true;
            let minimum = read_state(|state| {
                spl_withdrawal_transaction_cost(state, std::iter::once(&request.terms))
            }) + FEE_PER_SIGNATURE / 2;
            request.terms.fee = minimum;
        }
        assert!(read_state(|state| spl_withdrawal_batch_budget(state, &requests)).is_some());
        // One large fee must not subsidize a request whose fee is one lamport short.
        let mut too_little_for_classic = requests.clone();
        too_little_for_classic[0].terms.fee -= 1;
        too_little_for_classic[1].terms.fee = u64::MAX;
        assert_eq!(
            read_state(|state| spl_withdrawal_batch_budget(state, &too_little_for_classic)),
            None
        );
        requests[0].terms.fee = u64::MAX;
        requests[1].terms.fee -= 1;
        assert_eq!(
            read_state(|state| spl_withdrawal_batch_budget(state, &requests)),
            None
        );
    }

    #[test]
    fn should_round_up_the_batch_fee_share_for_three_spl_withdrawals() {
        let mut requests = setup_spl_batch_budget_requests();
        let mut third = requests[0].clone();
        third.cksol_burn_block_index = 102.into();
        third.ckspl_burn_block_index = 9.into();
        requests.push(third);
        // 5,000 / 3 rounds up to 1,667 for each request; its solo retry needs another 5,000.
        for request in &mut requests {
            request.terms.fee = 6_667;
        }
        assert_eq!(
            read_state(|state| spl_withdrawal_batch_budget(state, &requests)),
            Some(20_000)
        );
        requests[0].terms.fee = 6_666;
        assert_eq!(
            read_state(|state| spl_withdrawal_batch_budget(state, &requests)),
            None
        );
    }

    #[test]
    fn should_only_quote_spl_batch_budgets_for_two_to_five_requests() {
        let requests = setup_spl_batch_budget_requests();
        let mut five = requests.clone();
        for id in 102..105 {
            let mut request = requests[0].clone();
            request.cksol_burn_block_index = id.into();
            request.ckspl_burn_block_index = id.into();
            five.push(request);
        }
        read_state(|state| {
            assert_eq!(spl_withdrawal_batch_budget(state, &[]), None);
            assert_eq!(spl_withdrawal_batch_budget(state, &requests[..1]), None);
            assert_eq!(
                spl_withdrawal_batch_budget(state, &five),
                Some(6 * FEE_PER_SIGNATURE)
            );
        });
        five.push(requests[0].clone());
        assert_eq!(
            read_state(|state| spl_withdrawal_batch_budget(state, &five)),
            None
        );
    }
}
