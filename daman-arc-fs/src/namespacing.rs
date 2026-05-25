//! Per-bee namespacing convention for tool names.
//!
//! Each persona process advertises ONLY its own tools, prefixed by a short alias of its
//! bee_name. humd routes each `chi:"tool-call"` uniquely to the persona that owns it.
//! Worker's claude in each persona's sid sees only that persona's namespaced tools —
//! claude literally cannot emit a tool-call for another persona's tools because they
//! don't exist in its advertised list.
//!
//! Convention (from BRIEF_PERSONA_AS_FORAGER):
//! | persona bee_name        | namespace |
//! | daman-leader-alpha      | alpha     |
//! | daman-leader-bravo      | bravo     |
//! | daman-follower-v1-1     | fol_v1_1  |
//! | daman-watchdog-v1-1     | wd_v1_1   |
//! | daman-arbiter-v1        | arb_v1    |
//! | daman-relief-1          | relief1   |

/// Derive the namespace prefix from a persona bee_name. Falls back to `b_<sanitized>`
/// for any bee_name not matching a known role pattern; the operator should pick one
/// explicitly via the `--namespace` CLI flag on the persona binary to override.
pub fn namespace_for_bee(bee_name: &str) -> String {
    let lower = bee_name.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("daman-leader-") {
        // e.g. "daman-leader-alpha" -> "alpha"
        return rest.replace('-', "_");
    }
    if let Some(rest) = lower.strip_prefix("daman-follower-") {
        // e.g. "daman-follower-v1-1" -> "fol_v1_1"
        return format!("fol_{}", rest.replace('-', "_"));
    }
    if let Some(rest) = lower.strip_prefix("daman-watchdog-") {
        // e.g. "daman-watchdog-v1-1" -> "wd_v1_1"
        return format!("wd_{}", rest.replace('-', "_"));
    }
    if let Some(rest) = lower.strip_prefix("daman-arbiter-") {
        // e.g. "daman-arbiter-v1" -> "arb_v1"
        return format!("arb_{}", rest.replace('-', "_"));
    }
    if let Some(rest) = lower.strip_prefix("daman-relief-") {
        // e.g. "daman-relief-1" -> "relief1"
        return format!("relief{}", rest.replace('-', "_"));
    }
    // Fallback: sanitize the whole name.
    format!(
        "b_{}",
        lower.replace('-', "_").chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_role_patterns_normalize_correctly() {
        assert_eq!(namespace_for_bee("daman-leader-alpha"), "alpha");
        assert_eq!(namespace_for_bee("daman-leader-bravo"), "bravo");
        assert_eq!(namespace_for_bee("daman-leader-echo"), "echo");
        assert_eq!(namespace_for_bee("daman-follower-v1-1"), "fol_v1_1");
        assert_eq!(namespace_for_bee("daman-follower-v2-5"), "fol_v2_5");
        assert_eq!(namespace_for_bee("daman-follower-v3-3"), "fol_v3_3");
        assert_eq!(namespace_for_bee("daman-watchdog-v1-1"), "wd_v1_1");
        assert_eq!(namespace_for_bee("daman-watchdog-v2"), "wd_v2");
        assert_eq!(namespace_for_bee("daman-arbiter-v1"), "arb_v1");
        assert_eq!(namespace_for_bee("daman-arbiter-v2"), "arb_v2");
        assert_eq!(namespace_for_bee("daman-relief-1"), "relief1");
        assert_eq!(namespace_for_bee("daman-relief-2"), "relief2");
    }

    #[test]
    fn unknown_bee_pattern_falls_back_to_safe_prefix() {
        assert_eq!(namespace_for_bee("some-other-bee"), "b_some_other_bee");
        assert_eq!(namespace_for_bee("weird!chars"), "b_weirdchars");
    }

    #[test]
    fn all_27_canonical_namespaces_are_unique() {
        let bees: Vec<&str> = vec![
            "daman-leader-alpha", "daman-leader-bravo", "daman-leader-charlie",
            "daman-leader-delta", "daman-leader-echo",
            "daman-follower-v1-1", "daman-follower-v1-2", "daman-follower-v1-3",
            "daman-follower-v1-4", "daman-follower-v1-5",
            "daman-follower-v2-1", "daman-follower-v2-2", "daman-follower-v2-3",
            "daman-follower-v2-4", "daman-follower-v2-5",
            "daman-follower-v3-1", "daman-follower-v3-2", "daman-follower-v3-3",
            "daman-follower-v3-4", "daman-follower-v3-5",
            "daman-watchdog-v1-1", "daman-watchdog-v1-2", "daman-watchdog-v2",
            "daman-arbiter-v1", "daman-arbiter-v2",
            "daman-relief-1", "daman-relief-2",
        ];
        let ns: std::collections::HashSet<String> = bees.iter().map(|b| namespace_for_bee(b)).collect();
        assert_eq!(ns.len(), bees.len(), "namespace collision across the 27 personas");
    }
}
