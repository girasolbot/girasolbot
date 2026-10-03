//! Copy-trade replay: derive our order size from the target transaction's real
//! amounts instead of a fixed buy_amount, and select mirror vs replay mode.

/// Order size derived from a target transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplaySize {
    /// SOL to spend, in lamports.
    pub sol_lamports: u64,
    /// Token amount to request, in base units.
    pub token_amount: u64,
}

/// True when replay mode should be used for this detection.
/// `mirror` always falls back to the legacy buyer path.
pub fn should_use_replay(copy_mode: &str) -> bool {
    copy_mode.eq_ignore_ascii_case("replay")
}

/// Compute our replay order size from the target's real trade amounts.
///
/// * `proportional` — spend `mirror_percent`% of the target's SOL when
///   `mirror_percent > 0`, otherwise fall back to the operator's `buy_amount_sol`.
/// * `one_to_one`   — spend exactly what the target spent.
///
/// The result is capped by `max_position_sol` (when > 0) and never below 1 lamport.
/// Returns `None` when the target amount is zero/unknown.
pub fn compute_replay_size(
    target_sol_lamports: u64,
    target_token_amount: u64,
    mirror_percent: f64,
    buy_amount_sol: f64,
    max_position_sol: f64,
    size_mode: &str,
) -> Option<ReplaySize> {
    if target_sol_lamports == 0 {
        return None;
    }
    let target_sol = target_sol_lamports as f64 / 1e9;
    let mut our_sol = match size_mode {
        "one_to_one" => target_sol,
        "proportional" => {
            if mirror_percent > 0.0 {
                target_sol * mirror_percent / 100.0
            } else {
                buy_amount_sol
            }
        }
        _ => return None,
    };
    if max_position_sol > 0.0 {
        our_sol = our_sol.min(max_position_sol);
    }
    let our_sol = our_sol.max(0.000_000_001);
    let our_sol_lamports = (our_sol * 1e9).round() as u64;
    let token_amount = if target_token_amount == 0 {
        1
    } else {
        let scaled = (target_token_amount as f64 * our_sol_lamports as f64
            / target_sol_lamports as f64)
            .round();
        (scaled as u64).max(1)
    };
    Some(ReplaySize {
        sol_lamports: our_sol_lamports.max(1),
        token_amount,
    })
}

/// Latency from the target's WebSocket notification to the moment we start our buy,
/// in milliseconds. Used for first-block race diagnostics.
pub fn replay_latency_ms(target_ws_received_at_ms: u128, our_buy_start_ms: u128) -> u128 {
    our_buy_start_ms.saturating_sub(target_ws_received_at_ms)
}

#[cfg(test)]
mod tests {
    use super::{compute_replay_size, replay_latency_ms, should_use_replay};

    #[test]
    fn replay_mode_is_selected_case_insensitively() {
        assert!(should_use_replay("replay"));
        assert!(should_use_replay("REPLAY"));
        assert!(!should_use_replay("mirror"));
    }

    #[test]
    fn proportional_mode_uses_mirror_percent() {
        let size =
            compute_replay_size(1_000_000_000, 1_000, 10.0, 0.5, 0.0, "proportional").unwrap();
        assert_eq!(size.sol_lamports, 100_000_000);
    }

    #[test]
    fn proportional_mode_falls_back_to_buy_amount() {
        let size =
            compute_replay_size(1_000_000_000, 1_000, 0.0, 0.25, 0.0, "proportional").unwrap();
        assert_eq!(size.sol_lamports, 250_000_000);
    }

    #[test]
    fn one_to_one_mode_uses_target_amount() {
        let size = compute_replay_size(750_000_000, 1_000, 10.0, 0.25, 0.0, "one_to_one").unwrap();
        assert_eq!(size.sol_lamports, 750_000_000);
    }

    #[test]
    fn max_position_caps_replay_size() {
        let size =
            compute_replay_size(1_000_000_000, 1_000, 100.0, 0.25, 0.2, "proportional").unwrap();
        assert_eq!(size.sol_lamports, 200_000_000);
    }

    #[test]
    fn unknown_size_mode_returns_none() {
        assert_eq!(
            compute_replay_size(1_000_000_000, 1_000, 10.0, 0.25, 0.0, "unknown"),
            None
        );
    }

    #[test]
    fn zero_target_sol_returns_none() {
        assert_eq!(
            compute_replay_size(0, 1_000, 10.0, 0.25, 0.0, "proportional"),
            None
        );
    }

    #[test]
    fn token_amount_scales_proportionally_and_stays_positive() {
        let scaled =
            compute_replay_size(1_000_000_000, 10_000, 10.0, 0.25, 0.0, "proportional").unwrap();
        assert_eq!(scaled.token_amount, 1_000);

        let minimum =
            compute_replay_size(1_000_000_000, 0, 10.0, 0.25, 0.0, "proportional").unwrap();
        assert_eq!(minimum.token_amount, 1);
    }

    #[test]
    fn replay_latency_saturates_at_zero() {
        assert_eq!(replay_latency_ms(200, 100), 0);
    }
}
