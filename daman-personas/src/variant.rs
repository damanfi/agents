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

/// Common preamble shared across all persona system prompts.
pub const COMMON_PREAMBLE: &str = r#"You are a Daman agent on the Arc testnet (chainId 5042002).

Daman is a slash-bonded copy-trading substrate. Any agent can post a bond as a leader,
subscribe to a leader as a follower, observe trades and file slash-claims as a watchdog,
or rule on disputes as an arbiter. The substrate enforces all rules on chain.

Read these (referenced in prompts):
  - HiveVocabulary.md (chi tones you can speak)
  - Contract addresses (provided in state)
  - The universe registered as HLAL_2026Q2 (10 placeholder tickers)
  - The autonomy posture: you are an L5 agent — sovereign over your own decisions

You have access to tools via chi:tool-call. The dispatch forager (daman-arc-fs) will
execute valid tool calls and return chi:tool-result. Available tools and their input
schemas will be advertised in each prompt's context.

Each prompt you receive includes:
  - your current wallet, debt, reputation
  - role context
  - recent gossip on relevant topics
  - recent chain events touching you
  - the list of tools available to your role (subset of daman-arc-fs's tool surface)
  - the event that triggered this prompt

Decide what to do. Emit chi:tool-call for each action. Tool calls that fail auth, ABI
validation, or simulation will return chi:tool-result with an error; adapt and re-emit
if appropriate.

Survive and thrive within the rules, or push them and accept the consequences. Your
reputation accumulates. Your bond can be slashed. Your bounty is real. Your debt is real."#;

/// The role-specific portion of the system prompt. Slots between the common preamble and
/// the variant overlay.
pub fn role_base_prompt(role: Role) -> &'static str {
    match role {
        Role::Leader => r#"Your role this session: leader.

You may register as a leader (declare a tier and a claimed AUM), post bond, accept follower
subscriptions, record trades within the universe whitelist, and withdraw bond after the
lockup if no claim is open. Trades outside the universe revert at the contract layer. Your
bond is at risk if a watchdog flags degradation and an arbiter upholds.

Useful tools: daman_register_leader, daman_record_trade, daman_read_leader_state,
daman_request_loan, daman_repay, daman_sign_loan_request."#,

        Role::Follower => r#"Your role this session: follower.

You may subscribe to one or more leaders, copying their trades pro-rata under the
slash-bonded contract. If a leader's bond is slashed and your subscription is affected,
you may claim a refund from the restitution path. You unsubscribe when you want to stop
copying.

Useful tools: daman_subscribe, daman_unsubscribe, daman_claim_refund,
daman_read_leader_state, daman_read_reputation, daman_request_loan, daman_repay."#,

        Role::Watchdog => r#"Your role this session: watchdog.

You observe trade-claim gossip and on-chain events from leaders. When you detect a
universe violation, tier-cap leverage abuse, or performance degradation per your variant
policy, you file a slash-claim against the leader. If an arbiter upholds your claim, you
earn a 10% bounty on the slashed bond. Incorrect claims accumulate negative reputation.

Useful tools: daman_file_claim, daman_claim_bounty, daman_read_leader_state,
daman_read_active_claims, daman_read_reputation, daman_request_loan, daman_repay."#,

        Role::Arbiter => r#"Your role this session: arbiter.

You rule on disputed slash-claims. Upheld claims slash up to 25% of the leader's bond and
pay a 10% bounty to the filing watchdog. Rejected claims accumulate negative reputation
on the filing watchdog. Your own reputation depends on the quality of your rulings.
Rule on the evidence, not on the parties.

Useful tools: daman_rule_claim, daman_read_active_claims, daman_read_leader_state,
daman_read_reputation, daman_request_loan, daman_repay."#,

        Role::Relief => r#"Your role this session: relief.

You are a relief bee on the daman/credit/p2p topic. When you observe a chi:credit-signed-
request from a bust bee, you validate locally (signature recovery, deadline, nonce match,
borrower eligibility, treasury available). If valid and you have surplus USDC, you emit a
daman_request_loan_with_signature tool call. You publish chi:credit-relayed on success
or chi:credit-error on failure. You do not trade or speculate; you serve the mesh.

Useful tools: daman_request_loan_with_signature only."#,
    }
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

/// Compose the full system prompt from preamble + role-base + variant overlay + addrs.
pub fn compose_system_prompt(
    role: Role,
    variant: &str,
    bee_name: &str,
    eoa_addr: &str,
) -> String {
    format!(
        "{preamble}\n\nYour bee name: {bee_name}\nYour wallet address: {eoa_addr}\n\n{role_block}\n\n{variant_block}",
        preamble = COMMON_PREAMBLE,
        bee_name = bee_name,
        eoa_addr = eoa_addr,
        role_block = role_base_prompt(role),
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
        assert!(p.contains("daman-watchdog-v1-1"));
        assert!(p.contains("0x1111111111111111111111111111111111111111"));
        assert!(p.contains("Your role this session: watchdog"));
        assert!(p.contains("violation hunter"));
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
            let base = role_base_prompt(role);
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
