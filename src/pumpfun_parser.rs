//! Synchronous PumpFun account-data parsers (P0-2, ported from zero-block-sniper).
//!
//! These are pure, allocation-light parsers that read PumpFun on-chain account
//! data without any RPC round-trips. They are a NEW code path: existing async
//! fetchers in `rpc.rs` remain untouched and can optionally delegate here.
//!
//! Anti-pattern guard (see ANALYSIS.md): offsets are never hardcoded without a
//! fallback. Each parser first tries the fast fixed-offset read, and if that
//! yields an invalid / zero `Pubkey`, it falls back to a borsh structured
//! decode of the account layout before giving up.

use borsh::BorshDeserialize;
use solana_program::pubkey::Pubkey;
use std::str::FromStr;

/// Size of a Solana `Pubkey` in bytes.
const PUBKEY_SIZE: usize = 32;

/// Length of the Anchor account discriminator prefix.
const DISCRIMINATOR_LEN: usize = 8;

// ---- Global PDA layout (fast-path offsets, ABSOLUTE, include discriminator) ----

/// Absolute offset of the legacy `fee_recipient` field in the Global account:
/// discriminator(8) + initialized(1) + authority(32) = 41.
const GLOBAL_FEE_RECIPIENT_OFFSET: usize = 41;

/// Absolute offset of the `fee_recipients[7]` array in the Global account:
/// discriminator(8) + GLOBAL_FEE_RECIPIENTS_REL_OFFSET(154) = 162.
const GLOBAL_FEE_RECIPIENTS_OFFSET: usize = DISCRIMINATOR_LEN + 154;

/// Number of entries in the `fee_recipients` array.
const GLOBAL_FEE_RECIPIENTS_COUNT: usize = 7;

/// Canonical authorized fee recipient for standard pump.fun tokens.
/// Final fallback when nothing else parses.
const PUMP_FEE_RECIPIENT: &str = "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM";

// ---- BondingCurve layout (fast-path offset, ABSOLUTE, includes discriminator) ----

/// Absolute offset of the `creator` field in a bonding curve account:
/// discriminator(8) + 5×u64(40) + bool(1) = 49.
const BONDING_CURVE_CREATOR_OFFSET: usize = DISCRIMINATOR_LEN + 41;

/// Minimum bonding curve account length required to read the creator:
/// discriminator(8) + 5×u64(40) + bool(1) + creator(32) = 81.
const BONDING_CURVE_MIN_LEN: usize = BONDING_CURVE_CREATOR_OFFSET + PUBKEY_SIZE;

/// Errors that can occur while parsing PumpFun account data.
#[derive(Debug, thiserror::Error)]
pub enum ParserError {
    /// Account data was too short for the expected layout.
    #[error("account data too short: need at least {need} bytes, got {got}")]
    TooShort { need: usize, got: usize },
    /// Neither the fast-path offset read nor the borsh fallback produced a
    /// valid (non-default) pubkey.
    #[error("no valid pubkey found via offset or borsh fallback")]
    NoValidPubkey,
}

/// Borsh mirror of the Global account (fields up to `fee_recipients`).
/// Pubkeys are decoded as raw byte arrays to avoid depending on the optional
/// `borsh` feature of `solana-program`.
#[derive(BorshDeserialize)]
struct GlobalBorsh {
    _initialized: u8,
    _authority: [u8; 32],
    fee_recipient: [u8; 32],
    _initial_virtual_token_reserves: u64,
    _initial_virtual_sol_reserves: u64,
    _initial_real_token_reserves: u64,
    _token_total_supply: u64,
    _fee_basis_points: u64,
    _withdraw_authority: [u8; 32],
    _enable_migrate: u8,
    _pool_migration_fee: u64,
    _creator_fee_basis_points: u64,
    fee_recipients: [[u8; 32]; GLOBAL_FEE_RECIPIENTS_COUNT],
}

/// Borsh mirror of the BondingCurve account (fields up to `creator`).
#[derive(BorshDeserialize)]
struct BondingCurveBorsh {
    _virtual_token_reserves: u64,
    _virtual_sol_reserves: u64,
    _real_token_reserves: u64,
    _real_sol_reserves: u64,
    _token_total_supply: u64,
    _complete: u8,
    creator: [u8; 32],
}

/// Read a `Pubkey` from `data[offset..offset+32]`, returning `None` if the
/// slice is too short or the bytes form the default (all-zero) pubkey.
fn read_pubkey_at(data: &[u8], offset: usize) -> Option<Pubkey> {
    let end = offset.checked_add(PUBKEY_SIZE)?;
    let bytes = data.get(offset..end)?;
    let pk = Pubkey::try_from(bytes).ok()?;
    if pk == Pubkey::default() {
        None
    } else {
        Some(pk)
    }
}

/// Convert a raw 32-byte array into a non-default `Pubkey`, or `None`.
fn pubkey_from_bytes(bytes: [u8; 32]) -> Option<Pubkey> {
    let pk = Pubkey::new_from_array(bytes);
    if pk == Pubkey::default() {
        None
    } else {
        Some(pk)
    }
}

/// Parse the authorized fee recipient from a PumpFun Global account.
///
/// `data` is the full account data (including the 8-byte discriminator).
///
/// Resolution order:
/// 1. Fast path: legacy `fee_recipient` at absolute offset 41.
/// 2. Fast path: first non-zero entry of the `fee_recipients[7]` array
///    (absolute offset 162).
/// 3. Borsh fallback: structurally decode the account and reuse the same
///    field priority (anti-pattern guard — never trust offsets alone).
/// 4. Final fallback: the hardcoded canonical `PUMP_FEE_RECIPIENT`.
pub fn parse_fee_recipient_from_global(data: &[u8]) -> Result<Pubkey, ParserError> {
    // 1. Fast path — legacy fee_recipient field.
    if let Some(pk) = read_pubkey_at(data, GLOBAL_FEE_RECIPIENT_OFFSET) {
        return Ok(pk);
    }

    // 2. Fast path — first non-zero fee_recipients[] entry.
    for i in 0..GLOBAL_FEE_RECIPIENTS_COUNT {
        let offset = GLOBAL_FEE_RECIPIENTS_OFFSET + i * PUBKEY_SIZE;
        if let Some(pk) = read_pubkey_at(data, offset) {
            return Ok(pk);
        }
    }

    // 3. Borsh fallback — decode the layout structurally (skip discriminator).
    if data.len() > DISCRIMINATOR_LEN {
        if let Ok(global) = GlobalBorsh::try_from_slice(&data[DISCRIMINATOR_LEN..]) {
            if let Some(pk) = pubkey_from_bytes(global.fee_recipient) {
                return Ok(pk);
            }
            for entry in global.fee_recipients.iter() {
                if let Some(pk) = pubkey_from_bytes(*entry) {
                    return Ok(pk);
                }
            }
        }
    }

    // 4. Final fallback — canonical hardcoded recipient.
    Pubkey::from_str(PUMP_FEE_RECIPIENT).map_err(|_| ParserError::NoValidPubkey)
}

/// Parse the `creator` pubkey from a PumpFun bonding curve account.
///
/// `data` is the full account data (including the 8-byte discriminator).
///
/// Resolution order:
/// 1. Fast path: `creator` at absolute offset 49.
/// 2. Borsh fallback: structurally decode the account (anti-pattern guard).
///
/// Returns `None` when the account is missing / unreadable or the creator is
/// the default pubkey in both the offset read and the borsh decode.
pub fn parse_creator_from_bonding_curve(data: &[u8]) -> Option<Pubkey> {
    if data.len() < BONDING_CURVE_MIN_LEN {
        return None;
    }

    // 1. Fast path — fixed creator offset.
    if let Some(pk) = read_pubkey_at(data, BONDING_CURVE_CREATOR_OFFSET) {
        return Some(pk);
    }

    // 2. Borsh fallback — structural decode (skip discriminator).
    if let Ok(curve) = BondingCurveBorsh::try_from_slice(&data[DISCRIMINATOR_LEN..]) {
        if let Some(pk) = pubkey_from_bytes(curve.creator) {
            return Some(pk);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISCRIMINATOR: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    fn sample_pubkey(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    /// Build a Global account buffer with the given legacy fee_recipient and
    /// fee_recipients array, laid out at the documented absolute offsets.
    fn build_global(fee_recipient: Option<Pubkey>, recipients: &[Option<Pubkey>]) -> Vec<u8> {
        // Total size covers through reserved_fee_recipient (offset 475 rel + 32).
        let total = DISCRIMINATOR_LEN + 475 + PUBKEY_SIZE;
        let mut buf = vec![0u8; total];
        buf[..DISCRIMINATOR_LEN].copy_from_slice(&DISCRIMINATOR);
        if let Some(pk) = fee_recipient {
            buf[GLOBAL_FEE_RECIPIENT_OFFSET..GLOBAL_FEE_RECIPIENT_OFFSET + PUBKEY_SIZE]
                .copy_from_slice(pk.as_ref());
        }
        for (i, r) in recipients.iter().enumerate() {
            if let Some(pk) = r {
                let start = GLOBAL_FEE_RECIPIENTS_OFFSET + i * PUBKEY_SIZE;
                buf[start..start + PUBKEY_SIZE].copy_from_slice(pk.as_ref());
            }
        }
        buf
    }

    fn build_bonding_curve(creator: Option<Pubkey>) -> Vec<u8> {
        let mut buf = vec![0u8; BONDING_CURVE_MIN_LEN];
        buf[..DISCRIMINATOR_LEN].copy_from_slice(&DISCRIMINATOR);
        if let Some(pk) = creator {
            buf[BONDING_CURVE_CREATOR_OFFSET..BONDING_CURVE_CREATOR_OFFSET + PUBKEY_SIZE]
                .copy_from_slice(pk.as_ref());
        }
        buf
    }

    #[test]
    fn fee_recipient_fast_path_legacy_field() {
        let expected = sample_pubkey(0xAB);
        let buf = build_global(Some(expected), &[]);
        assert_eq!(parse_fee_recipient_from_global(&buf).unwrap(), expected);
    }

    #[test]
    fn fee_recipient_falls_back_to_recipients_array() {
        let expected = sample_pubkey(0xCD);
        // Legacy field zero -> should scan fee_recipients, pick first non-zero.
        let buf = build_global(None, &[None, Some(expected)]);
        assert_eq!(parse_fee_recipient_from_global(&buf).unwrap(), expected);
    }

    #[test]
    fn fee_recipient_final_hardcoded_fallback() {
        // All-zero account (valid discriminator, everything default) -> hardcoded.
        let buf = build_global(None, &[]);
        let got = parse_fee_recipient_from_global(&buf).unwrap();
        assert_eq!(got, Pubkey::from_str(PUMP_FEE_RECIPIENT).unwrap());
    }

    #[test]
    fn fee_recipient_empty_data_hits_final_fallback() {
        let got = parse_fee_recipient_from_global(&[]).unwrap();
        assert_eq!(got, Pubkey::from_str(PUMP_FEE_RECIPIENT).unwrap());
    }

    #[test]
    fn creator_fast_path() {
        let expected = sample_pubkey(0x11);
        let buf = build_bonding_curve(Some(expected));
        assert_eq!(parse_creator_from_bonding_curve(&buf), Some(expected));
    }

    #[test]
    fn creator_none_when_too_short() {
        let buf = vec![0u8; BONDING_CURVE_MIN_LEN - 1];
        assert_eq!(parse_creator_from_bonding_curve(&buf), None);
    }

    #[test]
    fn creator_none_when_default() {
        // Correct length but creator is all zeros and borsh decode also yields default.
        let buf = build_bonding_curve(None);
        assert_eq!(parse_creator_from_bonding_curve(&buf), None);
    }
}
