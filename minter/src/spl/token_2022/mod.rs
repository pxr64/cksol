//! Token-2022 layout helpers adapted from `spl-token-2022-interface` 2.1.0.
//!
//! Upstream source: <https://docs.rs/crate/spl-token-2022-interface/2.1.0/source/src/extension/mod.rs>.
//! These follow `StateWithExtensions::unpack`, `type_and_tlv_indices`, `unpack_tlv_data`,
//! `get_tlv_indices`, and `get_tlv_data_info`, with raw types, values, and string errors.
//! The official crate stays a dev dependency for comparison tests: its `solana-zk-sdk`
//! dependency imports browser functions in wasm32 builds that the IC cannot install.

use solana_program_pack::Pack;
use spl_token_interface::state::{Account as TokenAccount, Multisig};

#[cfg(test)]
mod tests;

/// Token-2022 pads the classic layout to the classic account length, then stores one
/// account type byte and the TLV extension entries.
const TOKEN_2022_ACCOUNT_TYPE_INDEX: usize = TokenAccount::LEN;
pub const TOKEN_2022_ACCOUNT_TYPE_MINT: u8 = 1;
pub const TOKEN_2022_ACCOUNT_TYPE_ACCOUNT: u8 = 2;
const TOKEN_2022_EXTENSION_UNINITIALIZED: u16 = 0;
pub const TOKEN_2022_EXTENSION_IMMUTABLE_OWNER: u16 = 7;
pub const TOKEN_2022_EXTENSION_MEMO_TRANSFER: u16 = 8;

/// Splits a Token-2022 account into its classic base and its TLV extension data.
/// Follows `StateWithExtensions::unpack` in spl-token-2022-interface 2.1.0; callers decode the base.
pub fn split_token_2022_account(
    data: &[u8],
    base_len: usize,
    account_type: u8,
) -> Result<(&[u8], &[u8]), String> {
    if data.len() == Multisig::LEN {
        return Err("Account data has the length of a multisig".to_string());
    }
    if data.len() < base_len {
        return Err("Account data is too short".to_string());
    }
    let (base_data, rest) = data.split_at(base_len);
    let tlv_data = unpack_tlv_data(rest, base_len, account_type)?;
    Ok((base_data, tlv_data))
}

fn type_and_tlv_indices(
    rest_input: &[u8],
    base_len: usize,
) -> Result<Option<(usize, usize)>, String> {
    if rest_input.is_empty() {
        Ok(None)
    } else {
        let account_type_index = TOKEN_2022_ACCOUNT_TYPE_INDEX.saturating_sub(base_len);
        let tlv_start_index = account_type_index.saturating_add(size_of::<u8>());
        if rest_input.len() < tlv_start_index {
            return Err("Account data is too short for a Token-2022 account type".to_string());
        }
        if rest_input[..account_type_index] != vec![0; account_type_index] {
            Err("Token-2022 padding must be zero".to_string())
        } else {
            Ok(Some((account_type_index, tlv_start_index)))
        }
    }
}

fn unpack_tlv_data(rest: &[u8], base_len: usize, account_type: u8) -> Result<&[u8], String> {
    if let Some((account_type_index, tlv_start_index)) = type_and_tlv_indices(rest, base_len)? {
        if rest[account_type_index] != account_type {
            return Err("Token-2022 account type does not match".to_string());
        }
        Ok(&rest[tlv_start_index..])
    } else {
        Ok(&[])
    }
}

struct TlvIndices {
    type_start: usize,
    length_start: usize,
    value_start: usize,
}

fn get_tlv_indices(type_start: usize) -> TlvIndices {
    let length_start = type_start.saturating_add(size_of::<u16>());
    let value_start = length_start.saturating_add(size_of::<u16>());
    TlvIndices {
        type_start,
        length_start,
        value_start,
    }
}

/// Returns each TLV entry as its extension type and value.
/// Follows `get_tlv_data_info` in spl-token-2022-interface 2.1.0, returning raw types and values.
pub fn token_2022_extensions(tlv_data: &[u8]) -> Result<Vec<(u16, &[u8])>, String> {
    let mut extensions = vec![];
    let mut start_index = 0;
    while start_index < tlv_data.len() {
        let tlv_indices = get_tlv_indices(start_index);
        if tlv_data.len() < tlv_indices.length_start {
            // The last byte can be padding used during a realloc.
            return Ok(extensions);
        }
        let extension_type = u16::from_le_bytes(
            tlv_data[tlv_indices.type_start..tlv_indices.length_start]
                .try_into()
                .map_err(|_| "Truncated Token-2022 extension type".to_string())?,
        );
        if extension_type == TOKEN_2022_EXTENSION_UNINITIALIZED {
            return Ok(extensions);
        } else {
            if tlv_data.len() < tlv_indices.value_start {
                return Err("Truncated Token-2022 extension length".to_string());
            }
            let length = u16::from_le_bytes(
                tlv_data[tlv_indices.length_start..tlv_indices.value_start]
                    .try_into()
                    .map_err(|_| "Truncated Token-2022 extension length".to_string())?,
            );

            let value_end_index = tlv_indices.value_start.saturating_add(usize::from(length));
            if value_end_index > tlv_data.len() {
                return Err("Truncated Token-2022 extension value".to_string());
            }
            extensions.push((
                extension_type,
                &tlv_data[tlv_indices.value_start..value_end_index],
            ));
            start_index = value_end_index;
        }
    }
    Ok(extensions)
}
