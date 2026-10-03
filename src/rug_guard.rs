use arc_swap::ArcSwap;
use log::{debug, error, info, warn};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct TokenSafetyReport {
    pub mint_authority: Option<Pubkey>,
    pub freeze_authority: Option<Pubkey>,
    pub is_mutable: Option<bool>,
    pub transfer_fee_bps: Option<u16>,
    pub is_token_2022: bool,
    pub decimals: u8,
    pub supply: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ScreenDecision {
    Allow,
    Warn(String),
    Reject(String),
}

pub fn parse_spl_mint(data: &[u8]) -> Result<(Option<Pubkey>, u8, u64, Option<Pubkey>), String> {
    if data.len() < 82 {
        return Err(format!(
            "SPL mint data too short: expected at least 82 bytes, got {}",
            data.len()
        ));
    }

    let mint_authority = parse_coption_pubkey(&data[0..36]);
    let supply = u64::from_le_bytes(data[36..44].try_into().unwrap());
    let decimals = data[44];
    let freeze_authority = parse_coption_pubkey(&data[46..82]);

    Ok((mint_authority, decimals, supply, freeze_authority))
}

fn parse_coption_pubkey(data: &[u8]) -> Option<Pubkey> {
    let tag = u32::from_le_bytes(data[0..4].try_into().unwrap());
    (tag == 1).then(|| Pubkey::new_from_array(data[4..36].try_into().unwrap()))
}

pub fn parse_token2022_transfer_fee_bps(data: &[u8]) -> Option<u16> {
    const TLV_START: usize = 166;
    const TRANSFER_FEE_CONFIG: u16 = 1;
    const NEWER_FEE_BPS_OFFSET: usize = 106;

    if data.len() < TLV_START || data[165] != 1 {
        return None;
    }

    let mut offset = TLV_START;
    while offset.checked_add(4)? <= data.len() {
        let extension_type = u16::from_le_bytes(data[offset..offset + 2].try_into().ok()?);
        let length = u16::from_le_bytes(data[offset + 2..offset + 4].try_into().ok()?) as usize;
        let value_start = offset.checked_add(4)?;
        let value_end = value_start.checked_add(length)?;
        if value_end > data.len() {
            return None;
        }

        if extension_type == TRANSFER_FEE_CONFIG {
            // 72 bytes of authorities/withheld amount + 18-byte older fee,
            // then 16 bytes into the newer fee: 72 + 18 + 16 = 106.
            let bps_start = value_start.checked_add(NEWER_FEE_BPS_OFFSET)?;
            let bps_end = bps_start.checked_add(2)?;
            if bps_end > value_end {
                return None;
            }
            return Some(u16::from_le_bytes(
                data[bps_start..bps_end].try_into().ok()?,
            ));
        }

        offset = value_end;
    }

    None
}

pub fn parse_metaplex_is_mutable(data: &[u8]) -> Option<bool> {
    if data.first().copied()? != 4 {
        return None;
    }

    let mut offset = 65;
    for _ in 0..3 {
        let string_len = read_u32(data, &mut offset)? as usize;
        offset = offset.checked_add(string_len)?;
        if offset > data.len() {
            return None;
        }
    }

    offset = offset.checked_add(2)?;
    let creators_tag = *data.get(offset)?;
    offset += 1;
    match creators_tag {
        0 => {}
        1 => {
            let count = read_u32(data, &mut offset)? as usize;
            let creators_len = count.checked_mul(34)?;
            offset = offset.checked_add(creators_len)?;
            if offset > data.len() {
                return None;
            }
        }
        _ => return None,
    }

    data.get(offset.checked_add(1)?).map(|value| *value != 0)
}

fn read_u32(data: &[u8], offset: &mut usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let value = u32::from_le_bytes(data.get(*offset..end)?.try_into().ok()?);
    *offset = end;
    Some(value)
}

pub fn evaluate_safety(
    report: &TokenSafetyReport,
    settings: &crate::settings::Settings,
) -> ScreenDecision {
    if settings.reject_mint_authority && report.mint_authority.is_some() {
        return ScreenDecision::Reject("mint authority still active".to_string());
    }
    if settings.reject_freeze_authority && report.freeze_authority.is_some() {
        return ScreenDecision::Reject("freeze authority still active".to_string());
    }

    let warning = if report.is_mutable == Some(true) {
        if settings.reject_mutable_metadata {
            return ScreenDecision::Reject("metadata is mutable".to_string());
        }
        Some("metadata is mutable".to_string())
    } else {
        None
    };

    if settings.reject_transfer_fee
        && report.transfer_fee_bps.unwrap_or(0) > settings.max_transfer_fee_bps as u16
    {
        return ScreenDecision::Reject(format!(
            "transfer fee {} bps exceeds maximum {} bps",
            report.transfer_fee_bps.unwrap_or(0),
            settings.max_transfer_fee_bps
        ));
    }

    warning.map_or(ScreenDecision::Allow, ScreenDecision::Warn)
}

pub fn check_safer_sniping_gates(
    sol_amount: f64,
    token_amount: u64,
    liquidity_sol: Option<f64>,
    price_sol_per_token: f64,
    settings: &crate::settings::Settings,
) -> ScreenDecision {
    if !settings.enable_safer_sniping {
        return ScreenDecision::Allow;
    }
    if settings.min_tokens_threshold > 0 && token_amount < settings.min_tokens_threshold {
        return ScreenDecision::Reject(format!(
            "token amount {} is below minimum {}",
            token_amount, settings.min_tokens_threshold
        ));
    }
    if settings.max_sol_per_token > 0.0 && price_sol_per_token > settings.max_sol_per_token {
        return ScreenDecision::Reject(format!(
            "price {} SOL per token exceeds maximum {}",
            price_sol_per_token, settings.max_sol_per_token
        ));
    }
    if let Some(liquidity) = liquidity_sol {
        if liquidity < settings.min_liquidity_sol {
            return ScreenDecision::Reject(format!(
                "liquidity {} SOL is below minimum {} SOL",
                liquidity, settings.min_liquidity_sol
            ));
        }
        if settings.max_liquidity_sol > 0.0 && liquidity > settings.max_liquidity_sol {
            return ScreenDecision::Reject(format!(
                "liquidity {} SOL exceeds maximum {} SOL",
                liquidity, settings.max_liquidity_sol
            ));
        }
    }
    if sol_amount <= 0.0 {
        return ScreenDecision::Reject(format!("SOL amount must be positive, got {}", sol_amount));
    }

    if liquidity_sol.is_none() {
        warn!("safer sniping liquidity is unknown");
        ScreenDecision::Warn("liquidity unknown".to_string())
    } else {
        ScreenDecision::Allow
    }
}

#[derive(Clone)]
pub struct Blacklist {
    entries: Arc<ArcSwap<HashSet<String>>>,
}

impl Blacklist {
    pub fn empty() -> Self {
        Self {
            entries: Arc::new(ArcSwap::from_pointee(HashSet::new())),
        }
    }

    pub fn load_from_file(path: &str) -> Self {
        let blacklist = Self::empty();
        blacklist.reload_from_file(path);
        blacklist
    }

    pub fn contains(&self, mint: &str) -> bool {
        let contains = self.entries.load().contains(mint);
        debug!("blacklist lookup for {}: {}", mint, contains);
        contains
    }

    pub fn len(&self) -> usize {
        self.entries.load().len()
    }

    pub fn reload_from_file(&self, path: &str) {
        match read_blacklist(path) {
            Ok(entries) => {
                let count = entries.len();
                self.entries.store(Arc::new(entries));
                info!("loaded {} blacklist entries from {}", count, path);
            }
            Err(message) => {
                warn!("failed to load blacklist from {}: {}", path, message);
                error!("blacklist reload failed; retaining {} entries", self.len());
            }
        }
    }
}

fn read_blacklist(path: &str) -> Result<HashSet<String>, String> {
    let contents = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let entries =
        serde_json::from_str::<Vec<String>>(&contents).map_err(|error| error.to_string())?;
    Ok(entries.into_iter().collect())
}

pub fn estimate_pumpfun_liquidity_sol(real_sol_reserves_lamports: u64) -> f64 {
    real_sol_reserves_lamports as f64 / 1e9
}

#[cfg(test)]
mod tests {
    use super::*;

    fn safety_report() -> TokenSafetyReport {
        TokenSafetyReport {
            mint_authority: None,
            freeze_authority: None,
            is_mutable: Some(false),
            transfer_fee_bps: None,
            is_token_2022: false,
            decimals: 6,
            supply: 12_345,
        }
    }

    #[test]
    fn parse_spl_mint_without_authorities() {
        let mut data = vec![0; 82];
        data[36..44].copy_from_slice(&12_345_u64.to_le_bytes());
        data[44] = 6;

        assert_eq!(parse_spl_mint(&data), Ok((None, 6, 12_345, None)));
    }

    #[test]
    fn parse_spl_mint_with_authorities() {
        let mint_authority = Pubkey::new_unique();
        let freeze_authority = Pubkey::new_unique();
        let mut data = vec![0; 82];
        data[0..4].copy_from_slice(&1_u32.to_le_bytes());
        data[4..36].copy_from_slice(mint_authority.as_ref());
        data[46..50].copy_from_slice(&1_u32.to_le_bytes());
        data[50..82].copy_from_slice(freeze_authority.as_ref());

        assert_eq!(
            parse_spl_mint(&data),
            Ok((Some(mint_authority), 0, 0, Some(freeze_authority)))
        );
    }

    #[test]
    fn parse_spl_mint_rejects_short_data() {
        assert!(parse_spl_mint(&[0; 81]).is_err());
    }

    #[test]
    fn parse_token2022_transfer_fee_extension() {
        let expected_bps = 275_u16;
        let mut data = vec![0; 166 + 4 + 108];
        data[165] = 1;
        data[166..168].copy_from_slice(&1_u16.to_le_bytes());
        data[168..170].copy_from_slice(&108_u16.to_le_bytes());
        data[170 + 106..170 + 108].copy_from_slice(&expected_bps.to_le_bytes());

        assert_eq!(parse_token2022_transfer_fee_bps(&data), Some(expected_bps));
        assert_eq!(parse_token2022_transfer_fee_bps(&vec![0; 166]), None);
    }

    fn metaplex_metadata(is_mutable: bool) -> Vec<u8> {
        let mut data = vec![4];
        data.extend_from_slice(&[0; 64]);
        for value in ["Token", "TKN", "https://example.test/token.json"] {
            data.extend_from_slice(&(value.len() as u32).to_le_bytes());
            data.extend_from_slice(value.as_bytes());
        }
        data.extend_from_slice(&0_u16.to_le_bytes());
        data.push(0);
        data.push(0);
        data.push(u8::from(is_mutable));
        data
    }

    #[test]
    fn parse_metaplex_mutability() {
        assert_eq!(
            parse_metaplex_is_mutable(&metaplex_metadata(true)),
            Some(true)
        );
        assert_eq!(
            parse_metaplex_is_mutable(&metaplex_metadata(false)),
            Some(false)
        );
    }

    #[test]
    fn evaluate_safety_mint_authority_respects_setting() {
        let mut report = safety_report();
        report.mint_authority = Some(Pubkey::new_unique());
        let mut settings = crate::settings::Settings::default();
        settings.reject_mint_authority = true;
        assert_eq!(
            evaluate_safety(&report, &settings),
            ScreenDecision::Reject("mint authority still active".to_string())
        );

        settings.reject_mint_authority = false;
        assert_eq!(evaluate_safety(&report, &settings), ScreenDecision::Allow);
    }

    #[test]
    fn evaluate_safety_mutable_metadata_warns_or_rejects() {
        let mut report = safety_report();
        report.is_mutable = Some(true);
        let mut settings = crate::settings::Settings::default();
        settings.reject_mutable_metadata = false;
        assert_eq!(
            evaluate_safety(&report, &settings),
            ScreenDecision::Warn("metadata is mutable".to_string())
        );

        settings.reject_mutable_metadata = true;
        assert_eq!(
            evaluate_safety(&report, &settings),
            ScreenDecision::Reject("metadata is mutable".to_string())
        );
    }

    #[test]
    fn evaluate_safety_transfer_fee_threshold() {
        let mut report = safety_report();
        report.transfer_fee_bps = Some(25);
        let mut settings = crate::settings::Settings::default();
        settings.reject_transfer_fee = true;
        settings.max_transfer_fee_bps = 20;
        assert!(matches!(
            evaluate_safety(&report, &settings),
            ScreenDecision::Reject(_)
        ));

        settings.max_transfer_fee_bps = 25;
        assert_eq!(evaluate_safety(&report, &settings), ScreenDecision::Allow);
    }

    #[test]
    fn safer_sniping_gates_respect_thresholds() {
        let mut settings = crate::settings::Settings::default();
        assert_eq!(
            check_safer_sniping_gates(1.0, 1, Some(1.0), 0.1, &settings),
            ScreenDecision::Allow
        );

        settings.enable_safer_sniping = true;
        settings.min_tokens_threshold = 10;
        settings.max_sol_per_token = 1.0;
        settings.min_liquidity_sol = 2.0;
        settings.max_liquidity_sol = 100.0;
        assert!(matches!(
            check_safer_sniping_gates(1.0, 9, Some(10.0), 0.1, &settings),
            ScreenDecision::Reject(_)
        ));
        assert!(matches!(
            check_safer_sniping_gates(1.0, 10, Some(1.0), 0.1, &settings),
            ScreenDecision::Reject(_)
        ));
    }

    #[test]
    fn blacklist_loads_and_checks_entries() {
        let mint = Pubkey::new_unique().to_string();
        assert!(Pubkey::from_str(&mint).is_ok());
        let path = std::env::temp_dir().join(format!(
            "same-block-sniper-blacklist-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, format!("[\"{}\"]", mint)).unwrap();

        let blacklist = Blacklist::load_from_file(path.to_str().unwrap());
        assert_eq!(blacklist.len(), 1);
        assert!(blacklist.contains(&mint));
        assert!(!blacklist.contains(&Pubkey::new_unique().to_string()));

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn estimates_pumpfun_liquidity_in_sol() {
        assert_eq!(estimate_pumpfun_liquidity_sol(1_500_000_000), 1.5);
    }
}
