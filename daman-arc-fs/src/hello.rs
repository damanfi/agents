//! Hello manifest for `daman-arc-fs`. Extends the substrate's `reverb-arc-fs` base manifest
//! with Daman-specific chi vocabulary and tool surface.

use reverb_arc_fs::manifest::Hello;

/// Daman-specific chis the forager emits on `gossip-publish`. The persona bees subscribe to
/// these on the gossip layer to drive their tick. The treasury chis (`credit-*`, `loan-*`)
/// match the `damanfi/protocol/src/HiveVocabulary.md` extension shipped with the benevolence
/// brief.
pub const DAMAN_CHIS: &[&str] = &[
    // Slash flow.
    "trade-claim",
    "slash-claim",
    "ruling",
    "bounty-claimed",
    // Treasury / credit flow.
    "credit-need",
    "credit-signed-request",
    "credit-relayed",
    "credit-error",
    "loan-requested",
    "loan-repaid",
    "loan-blocked",
];

/// 17 daman-specific tools surfaced by this forager. Each composes the base `arc_*`
/// primitives from `reverb-arc-fs` plus the Daman contract ABI shipped in
/// `damanfi/copy-bond` and `damanfi/universe`.
pub const DAMAN_TOOLS: &[&str] = &[
    // Leader path.
    "daman_register_leader",
    "daman_record_trade",
    // Follower path.
    "daman_subscribe",
    "daman_unsubscribe",
    "daman_claim_refund",
    // Watchdog + arbiter path.
    "daman_file_claim",
    "daman_rule_claim",
    "daman_claim_bounty",
    // Benevolence credit path.
    "daman_request_loan",
    "daman_request_loan_with_signature",
    "daman_repay",
    "daman_sign_loan_request",
    // Read-only / state.
    "daman_read_leader_state",
    "daman_read_subscription_state",
    "daman_read_reputation",
    "daman_read_active_claims",
    "daman_subscribe_to_role_events",
];

/// Build the `daman-arc-fs` hello manifest. Extends the base `reverb-arc-fs` manifest with
/// the daman-specific chi + tool surface, the `daman/arc-fs` wire namespace, and the
/// `damanfi/agents/daman-arc-fs` source pointer.
pub fn build_hello(version: impl Into<String>) -> Hello {
    Hello::base("daman-arc-fs", version)
        .with_wire("daman/arc-fs")
        .with_source("https://github.com/damanfi/agents/tree/main/daman-arc-fs")
        .extend(
            DAMAN_CHIS.iter().map(|s| s.to_string()),
            DAMAN_TOOLS.iter().map(|s| s.to_string()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use reverb_arc_fs::manifest::{BASE_CHIS, BASE_TOOLS};

    #[test]
    fn hello_extends_base_with_daman_surface() {
        let hello = build_hello("0.1.0");

        assert_eq!(hello.bee, "daman-arc-fs");
        assert_eq!(hello.propensity.wire, "daman/arc-fs");
        assert!(hello.source.contains("damanfi/agents"));
        assert!(hello.source.contains("daman-arc-fs"));

        // Base tools preserved.
        for tool in BASE_TOOLS {
            assert!(
                hello.tools.contains(&tool.to_string()),
                "missing base tool {tool}"
            );
        }
        // Base chis preserved.
        for chi in BASE_CHIS {
            assert!(
                hello.chis.contains(&chi.to_string()),
                "missing base chi {chi}"
            );
        }
        // Daman tools added.
        for tool in DAMAN_TOOLS {
            assert!(
                hello.tools.contains(&tool.to_string()),
                "missing daman tool {tool}"
            );
        }
        // Daman chis added.
        for chi in DAMAN_CHIS {
            assert!(
                hello.chis.contains(&chi.to_string()),
                "missing daman chi {chi}"
            );
        }
    }

    #[test]
    fn tool_count_matches_brief() {
        // Brief specifies 17 daman-specific tools.
        assert_eq!(DAMAN_TOOLS.len(), 17);
    }

    #[test]
    fn chi_count_matches_brief() {
        // 4 slash-flow chis + 7 treasury/credit chis = 11.
        assert_eq!(DAMAN_CHIS.len(), 11);
    }

    #[test]
    fn hello_serializes_to_camelcase_proto_version() {
        let hello = build_hello("0.1.0");
        let json = serde_json::to_string(&hello).unwrap();
        assert!(json.contains("\"protoVersion\":\"0.7.0\""));
        assert!(json.contains("\"bee\":\"daman-arc-fs\""));
        assert!(json.contains("\"wire\":\"daman/arc-fs\""));
    }
}
