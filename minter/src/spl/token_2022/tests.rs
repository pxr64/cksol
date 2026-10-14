use super::*;
use proptest::prelude::*;
use spl_token_2022_interface::{
    extension::{BaseState, BaseStateWithExtensions, ExtensionType, StateWithExtensions},
    state::{Account as Account2022, Mint as Mint2022},
};
use spl_token_interface::state::{AccountState, Mint};

fn account_data() -> Vec<u8> {
    let mut data = vec![0; TokenAccount::LEN];
    TokenAccount::pack(
        TokenAccount {
            state: AccountState::Initialized,
            ..TokenAccount::default()
        },
        &mut data,
    )
    .unwrap();
    data
}

fn mint_data() -> Vec<u8> {
    let mut data = vec![0; Mint::LEN];
    Mint::pack(
        Mint {
            decimals: 6,
            is_initialized: true,
            ..Mint::default()
        },
        &mut data,
    )
    .unwrap();
    data
}

fn reference_extensions<S: BaseState + Pack>(data: &[u8]) -> Option<Vec<u16>> {
    StateWithExtensions::<S>::unpack(data)
        .and_then(|state| state.get_extension_types())
        .ok()
        .map(|extensions| extensions.into_iter().map(u16::from).collect())
}

/// The reference rejects unknown extension types while it walks the entries. The callers of
/// the helpers reject them too, so they count as a rejection here.
fn parsed_extensions(data: &[u8], base_len: usize, account_type: u8) -> Option<Vec<u16>> {
    let extensions: Vec<u16> = split_token_2022_account(data, base_len, account_type)
        .and_then(|(_, tlv)| token_2022_extensions(tlv))
        .ok()?
        .into_iter()
        .map(|(extension, _)| extension)
        .collect();
    extensions
        .iter()
        .all(|extension| ExtensionType::try_from(*extension).is_ok())
        .then_some(extensions)
}

#[test]
fn should_parse_account_layouts_like_the_reference_crate() {
    let base = account_data();
    let tails: [&[u8]; 9] = [
        &[],
        &[2],
        &[2, 7, 0, 0, 0],
        &[2, 7, 0, 0, 0, 0],
        &[2, 8, 0, 1, 0, 1, 0, 0, 0, 0],
        &[2, 7, 0, 0],
        &[2, 8, 0, 1, 0],
        &[1],
        &[1, 7, 0, 0, 0],
    ];
    for tail in tails {
        let data = [base.as_slice(), tail].concat();

        assert_eq!(
            parsed_extensions(&data, TokenAccount::LEN, TOKEN_2022_ACCOUNT_TYPE_ACCOUNT),
            reference_extensions::<Account2022>(&data),
            "{tail:?}"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]
    #[test]
    fn should_parse_random_tails_like_the_reference_crate(
        tail in proptest::collection::vec(
            prop_oneof![Just(0u8), Just(1), Just(2), Just(3), Just(7), Just(8), Just(255)],
            0..14,
        ),
        is_mint in any::<bool>(),
    ) {
        if is_mint {
            let data = [mint_data().as_slice(), &tail].concat();
            prop_assert_eq!(
                parsed_extensions(&data, Mint::LEN, TOKEN_2022_ACCOUNT_TYPE_MINT),
                reference_extensions::<Mint2022>(&data),
                "{:?}",
                tail
            );
        } else {
            let data = [account_data().as_slice(), &tail].concat();
            prop_assert_eq!(
                parsed_extensions(&data, TokenAccount::LEN, TOKEN_2022_ACCOUNT_TYPE_ACCOUNT),
                reference_extensions::<Account2022>(&data),
                "{:?}",
                tail
            );
        }
    }
}
