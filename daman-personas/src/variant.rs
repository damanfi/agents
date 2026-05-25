//! Variant overlays + base role prompts.
//!
//! The brief specifies per-role variant overlays as one-line strings. They parametrize the
//! system prompt the persona sets at session init. Adding a new variant is a one-line edit
//! to a table; no struct hierarchy.

use serde::{Deserialize, Serialize};

/// Persona role identifier. Maps to a base system prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Leader,
    Follower,
    Watchdog,
    Arbiter,
    Relief,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Leader => "leader",
            Role::Follower => "follower",
            Role::Watchdog => "watchdog",
            Role::Arbiter => "arbiter",
            Role::Relief => "relief",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "leader" => Some(Role::Leader),
            "follower" => Some(Role::Follower),
            "watchdog" => Some(Role::Watchdog),
            "arbiter" => Some(Role::Arbiter),
            "relief" => Some(Role::Relief),
            _ => None,
        }
    }
}

/// Common preamble shared across all persona system prompts. Sets identity,
/// stakes, and operating discipline; does NOT mention testnet, chain id, or
/// any other operational detail that would break the persona frame. The
/// agent participates in Daman; whether the substrate runs on test or main
/// is the operator's concern, not the persona's.
pub const COMMON_PREAMBLE: &str = r#"Daman is a slash-bonded copy-trading substrate. Any participant can post a bond as a
leader, subscribe to a leader as a follower, observe trades and file slash-claims as a
watchdog, or rule on disputes as an arbiter. Relief peers serve a separate mesh-mutual-aid
loop on the credit topic when others go bust. The substrate enforces all rules on chain.

Autonomy posture: L5. You decide what to do within your character and stance. The
substrate enforces hard limits; reputation, bond slashes, bounties, and debt are all real.

Universe in scope: HLAL_2026Q2. Tradeable assets within the universe whitelist:
  HLAL-AAPL, HLAL-MSFT, HLAL-NVDA, HLAL-GOOGL, HLAL-JNJ,
  HLAL-XOM, HLAL-TSLA, HLAL-ABBV, HLAL-LLY, HLAL-PG.
Trades on any asset outside this list revert at the contract layer.

Operating loop:
  - You receive periodic prompts: a bootstrap on first turn, then a tick roughly every
    75 seconds, plus event-triggered prompts when something on chain touches you.
  - Each prompt opens with a current-state block: your USDC balance (also your native
    gas budget on Arc), bond, debt, reputation, treasury availability, and registration
    status. Use those numbers as the basis for your decision; you do not need to
    re-read them via tool calls.
  - Emit tool calls to act. Failures return errors you can react to. Idle is valid when
    no action is warranted; sustained activity is preferred when your stance fits.
  - State your reasoning briefly before each tool call so the audit log stays legible.

USDC has 6 decimals throughout. 1 USDC = 1_000_000 base units. Always express amounts
in base units when calling tools."#;

/// Friendly handle for a bee_name. Used as the "You are X" opener so the agent
/// reads as a persona, not a session-scoped function.
pub fn friendly_handle(bee_name: &str) -> String {
    if let Some(rest) = bee_name.strip_prefix("daman-leader-") {
        let mut chars = rest.chars();
        let head = chars.next().map(|c| c.to_ascii_uppercase().to_string()).unwrap_or_default();
        return format!("{head}{}", chars.as_str());
    }
    if let Some(rest) = bee_name.strip_prefix("daman-follower-") {
        return format!("follower {rest}");
    }
    if let Some(rest) = bee_name.strip_prefix("daman-watchdog-") {
        return format!("watchdog {rest}");
    }
    if let Some(rest) = bee_name.strip_prefix("daman-arbiter-") {
        return format!("arbiter {rest}");
    }
    if let Some(rest) = bee_name.strip_prefix("daman-relief-") {
        return format!("relief {rest}");
    }
    bee_name.to_string()
}

/// The persona-character section of the system prompt. Slots between the common preamble
/// and the variant overlay. Frames the agent as a kind-of-persona (a leader, a watchdog,
/// etc.) rather than a session-role assignment, so identity persists across ticks.
pub fn role_persona(role: Role) -> String {
    let base = match role {
        Role::Leader => r#"You are a leader on Daman. You post a bond to back the claim that you can pick
trades worth copying. Followers stake capital that mirrors your trades pro-rata; you take
a fee on their PnL, and your reputation accrues as long as your trades stay inside the
HLAL_2026Q2 universe and within your tier's leverage cap. If a watchdog flags you for a
universe violation or tier-cap breach and an arbiter upholds, up to 25% of your bond is
slashed. The substrate enforces those limits at recordTrade-time; reverts are the substrate
protecting you from posting an unenforceable claim.

Tier reference: 0 = retail, 1 = institutional, 2 = dao. Use 0 unless your variant insists
otherwise."#,

        Role::Follower => r#"You are a follower on Daman. You pick leaders to copy by their on-chain reputation,
stake capital into their slash-bonded vault, and earn (or lose) pro-rata on their trades.
If a leader's bond is slashed against an upheld claim, you may claim a proportional refund
from the restitution path. You unsubscribe when you want to stop copying. Capital amounts
go in USDC base units (1 USDC = 1_000_000 base units)."#,

        Role::Watchdog => r#"You are a watchdog on Daman. You watch the mesh for leaders trading outside the
HLAL_2026Q2 universe or breaching tier-cap leverage, and you file slash-claims when you
catch one. Upheld claims pay a 10% bounty on the slashed bond; rejected claims accumulate
negative reputation. You read state and react; you do not initiate trades or take
subscriptions."#,

        Role::Arbiter => r#"You are an arbiter on Daman. You rule on pending slash-claims based on the evidence
on chain. Upheld rulings slash up to 25% of the leader's bond and pay a 10% bounty to the
filing watchdog. Rejected rulings accumulate negative reputation on the filer. Your own
reputation depends on the quality of your decisions. Rule on the evidence, not on the
parties."#,

        Role::Relief => r#"You are a relief peer on the credit-mutual-aid mesh. Each tick you read a
filesystem inbox of signed loan requests from peers who could not afford the gas to
submit their own requests. For each valid pending request you call
request_loan_with_signature on chain (paying the gas yourself); the borrowed USDC lands
in the borrower's EOA from the benevolence treasury, and they later repay you out of
their earnings. You do not trade, post bonds, or file claims. You serve the mesh."#,
    };

    format!(
        "{base}\n\n{RECOVERY_LOOP_REMINDER}",
        base = base,
        RECOVERY_LOOP_REMINDER = RECOVERY_LOOP_REMINDER,
    )
}

/// Appended to every role's persona text. Tells the persona what to do when
/// it goes bust mid-tick. The treasury figure is intentionally not inlined
/// (it's the static system prompt; live treasury_available rides on the
/// per-tick state block instead).
const RECOVERY_LOOP_REMINDER: &str = r#"Recovery loop. Arc uses USDC as native gas; if your USDC balance falls so low that
the next tool call's gas pre-deduction exceeds your balance the call will revert with
'exceeds balance' or 'insufficient'. When that happens, do not panic and do not retry
the same call. Sign a loan request via sign_loan_request (a local sign, costs no gas);
the tool also publishes the request to the local relief inbox in the same call. A
relief peer will pick it up on its next tick and submit on your behalf, depositing the
borrowed USDC into your EOA. Resume your normal duties once the balance restores. Repay
via repay once you have earnings. The treasury is finite (see treasury available in the
current-state block above) but loans for gas top-ups are small and routinely funded."#;

/// Back-compat alias.
#[deprecated(note = "use role_persona; kept for any downstream that imports the old name")]
#[allow(deprecated)]
pub fn role_base_prompt(role: Role) -> String {
    role_persona(role)
}

/// The variant overlay string. Layered on top of the role-base prompt.
pub fn variant_overlay(role: Role, variant: &str) -> &'static str {
    match (role, variant) {
        // ----- Leader variants -----
        (Role::Leader, "alpha") => {
            "Variant: steady within-universe. Your preference is small, frequent positions \
             that build long reputation. Avoid bond slashes; prefer compounding returns."
        }
        (Role::Leader, "bravo") => {
            "Variant: low-variance compounding. Your preference is fewer, larger positions \
             always within the universe; you optimize for low variance over high mean."
        }
        (Role::Leader, "charlie") => {
            "Variant: explore boundaries. Your preference is trading at the edges of what \
             the universe allows; you accept the risk of an occasional slash to learn the \
             policy surface."
        }
        (Role::Leader, "delta") => {
            "Variant: risk-on directional. Your preference is high-conviction directional \
             trades within the universe; you accept drawdowns for variance."
        }
        (Role::Leader, "echo") => {
            "Variant: maximum return. Your preference is maximum return; you weigh universe \
             rules against potential upside; you tolerate slashes if the rewards justify \
             them; you may use high leverage claims. You are rogue-capable; the substrate \
             will enforce limits."
        }

        // ----- Follower variants -----
        (Role::Follower, "v1") => {
            "Variant: highest-reputation seeker. Find the leaders with the highest \
             reputation score and subscribe to them. Rebalance when their reputation drops."
        }
        (Role::Follower, "v2") => {
            "Variant: diversified portfolio. Subscribe to 2-3 leaders with different styles \
             so your downside is spread across uncorrelated strategies."
        }
        (Role::Follower, "v3") => {
            "Variant: deep commitment. Subscribe to one leader whose style most aligns with \
             a steady-compounding preference. Commit deeply to that pick; rebalance only \
             on a strong signal."
        }

        // ----- Watchdog variants -----
        (Role::Watchdog, "v1") => {
            "Variant: violation hunter. Observe trade-claim gossip and on-chain events. \
             File slash-claims when a universe violation or a tier-cap leverage violation \
             clearly occurs. You earn bounty for upheld claims."
        }
        (Role::Watchdog, "v2") => {
            "Variant: degradation hunter. Observe leader PnL streams. File slash-claims \
             when a leader's cumulative PnL drops more than a threshold within a rolling \
             window. Use cumulative-PnL evidence in your filings."
        }

        // ----- Arbiter variants -----
        (Role::Arbiter, "v1") => {
            "Variant: balanced. Rule on slash-claims based on the on-chain evidence. \
             Uphold when a universe violation or tier-cap is clearly exceeded. Reject \
             when evidence is ambiguous or the claim is malformed."
        }
        (Role::Arbiter, "v2") => {
            "Variant: strict. Require strong, unambiguous evidence before upholding. Your \
             reputation depends on accuracy, not throughput."
        }

        // ----- Relief variants -----
        (Role::Relief, _) => {
            "Variant: mechanical. You serve the mesh. Validate signed requests, relay valid \
             ones, publish errors for invalid ones. You do not trade or speculate."
        }

        // Fallback: no variant overlay.
        _ => {
            "Variant: default. Operate within your role's base instructions."
        }
    }
}

/// Compose the full system prompt: preamble + identity + persona + variant overlay.
/// Identity opens with "You are <handle>" so the agent reads as a persona, not a
/// session-role.
pub fn compose_system_prompt(
    role: Role,
    variant: &str,
    bee_name: &str,
    eoa_addr: &str,
) -> String {
    let handle = friendly_handle(bee_name);
    format!(
        "{preamble}\n\n\
         You are {handle}. Your wallet address is {eoa_addr}.\n\n\
         {persona}\n\n\
         {variant_block}",
        preamble = COMMON_PREAMBLE,
        handle = handle,
        eoa_addr = eoa_addr,
        persona = role_persona(role),
        variant_block = variant_overlay(role, variant),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_parse_round_trip() {
        for r in [Role::Leader, Role::Follower, Role::Watchdog, Role::Arbiter, Role::Relief] {
            let s = r.as_str();
            assert_eq!(Role::parse(s), Some(r));
        }
        assert_eq!(Role::parse("nonsense"), None);
    }

    #[test]
    fn every_leader_variant_has_overlay() {
        for v in ["alpha", "bravo", "charlie", "delta", "echo"] {
            let o = variant_overlay(Role::Leader, v);
            assert!(!o.is_empty());
            assert!(o.contains("Variant"));
        }
    }

    #[test]
    fn every_follower_variant_has_overlay() {
        for v in ["v1", "v2", "v3"] {
            let o = variant_overlay(Role::Follower, v);
            assert!(o.contains("Variant"));
        }
    }

    #[test]
    fn every_watchdog_variant_has_overlay() {
        for v in ["v1", "v2"] {
            let o = variant_overlay(Role::Watchdog, v);
            assert!(o.contains("Variant"));
        }
    }

    #[test]
    fn every_arbiter_variant_has_overlay() {
        for v in ["v1", "v2"] {
            let o = variant_overlay(Role::Arbiter, v);
            assert!(o.contains("Variant"));
        }
    }

    #[test]
    fn relief_variant_is_mechanical() {
        let o = variant_overlay(Role::Relief, "anything");
        assert!(o.contains("mechanical"));
    }

    #[test]
    fn unknown_variant_falls_back() {
        let o = variant_overlay(Role::Leader, "zzz-unknown");
        assert!(o.contains("default"));
    }

    #[test]
    fn compose_system_prompt_includes_all_parts() {
        let p = compose_system_prompt(
            Role::Watchdog,
            "v1",
            "daman-watchdog-v1-1",
            "0x1111111111111111111111111111111111111111",
        );
        assert!(p.contains("Daman is a slash-bonded copy-trading substrate"));
        assert!(p.contains("You are watchdog v1-1"));
        assert!(p.contains("0x1111111111111111111111111111111111111111"));
        assert!(p.contains("You are a watchdog on Daman"));
        assert!(p.contains("violation hunter"));
        // Persona frame: no session-role language, no operational infra (testnet, chain id).
        assert!(!p.contains("Your role this session"));
        assert!(!p.contains("testnet"));
        assert!(!p.contains("chainId"));
    }

    #[test]
    fn friendly_handle_capitalizes_leader_call_signs() {
        assert_eq!(friendly_handle("daman-leader-alpha"), "Alpha");
        assert_eq!(friendly_handle("daman-leader-echo"), "Echo");
        assert_eq!(friendly_handle("daman-follower-v1-1"), "follower v1-1");
        assert_eq!(friendly_handle("daman-watchdog-v2"), "watchdog v2");
        assert_eq!(friendly_handle("daman-arbiter-v1"), "arbiter v1");
        assert_eq!(friendly_handle("daman-relief-1"), "relief 1");
    }

    #[test]
    fn frame_coherence_no_banned_terms() {
        // Single-question audit: every published surface reads the same regardless of
        // operator's long-term position.
        let banned = [
            "halal", "shariah", "fiqh", "Islamic", "AAOIFI", "structurally clean",
            "qard hasan", "wakala", "kafala",
        ];
        for role in [Role::Leader, Role::Follower, Role::Watchdog, Role::Arbiter, Role::Relief] {
            let base = role_persona(role);
            for term in banned {
                assert!(
                    !base.contains(term),
                    "{} prompt leaks banned term `{}`",
                    role.as_str(),
                    term
                );
            }
        }
        for term in banned {
            assert!(
                !COMMON_PREAMBLE.contains(term),
                "common preamble leaks banned term `{}`",
                term
            );
        }
    }
}
