//! Concrete tool definitions for the daman-arc-fs forager hello manifest.
//!
//! Each tool def carries a name, a description claude reads when deciding whether to
//! call, and an inputSchema claude reads to construct args. The pattern mirrors the
//! humfs forager in `hum/hives/humfs/src/tools/read.rs`: properties + required + type
//! per arg, prose description per tool.
//!
//! `as_bee` is the auth field every write tool requires: the bee_name of the caller.
//! Must equal `chi.from` per `reverb_arc_fs::safety::check_auth`. claude is told to
//! set `as_bee` to its own bee identity in the persona system prompt.

use serde_json::{json, Value};

pub fn tools_array() -> Vec<Value> {
    vec![
        // ─────────── Leader path ───────────
        json!({
            "name": "daman_register_leader",
            "description": "Register the calling bee as a Daman leader. Declares a tier (0=Retail, 1=Mid, 2=Institutional) and a claimedAum (the AUM the leader claims to manage, in USDC's smallest unit; Arc USDC is 6-decimal so 10000 USDC = 10000000000). The contract enforces a tier-proportional bond requirement at postBond time. Call this once per session before any other leader action.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "as_bee": {"type": "string", "description": "The caller's bee_name. Must match chi.from for auth."},
                    "tier": {"type": "integer", "enum": [0, 1, 2], "description": "0=Retail (10% bond), 1=Mid (5% bond), 2=Institutional (2.5% bond)."},
                    "claimedAum": {"type": "string", "description": "Claimed AUM in atomic USDC units as a decimal string. Example: 10000 USDC = \"10000000000\"."}
                },
                "required": ["as_bee", "tier", "claimedAum"]
            }
        }),
        json!({
            "name": "daman_record_trade",
            "description": "Record an on-platform trade by the calling leader. The substrate's UniverseRegistry must list the traded asset or the call reverts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "as_bee": {"type": "string"},
                    "asset": {"type": "string", "description": "20-byte hex address of the traded asset."},
                    "amount": {"type": "string", "description": "Trade amount in atomic units."},
                    "isLong": {"type": "boolean"}
                },
                "required": ["as_bee", "asset", "amount", "isLong"]
            }
        }),

        // ─────────── Follower path ───────────
        json!({
            "name": "daman_subscribe",
            "description": "Subscribe to a registered leader. Delegates `capital` USDC to copy that leader's trades pro-rata. The leader must already be registered and have an active bond. Call daman_read_reputation on the leader first to gauge fit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "as_bee": {"type": "string"},
                    "leader": {"type": "string", "description": "20-byte hex address of the leader."},
                    "capital": {"type": "string", "description": "Capital to delegate, in atomic USDC units."},
                    "builder": {"type": "string", "description": "Optional bytes32 builder-attribution tag; default 0x000...000."}
                },
                "required": ["as_bee", "leader", "capital"]
            }
        }),
        json!({
            "name": "daman_unsubscribe",
            "description": "Exit a subscription. Reverts if any active claim against the leader is open.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "as_bee": {"type": "string"},
                    "leader": {"type": "string"}
                },
                "required": ["as_bee", "leader"]
            }
        }),
        json!({
            "name": "daman_claim_refund",
            "description": "Claim restitution from a slashed leader's bond via RefundProtocolFixed. The paymentId comes from the SettlementCompleted or BondSlashed event the follower is owed under.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "as_bee": {"type": "string"},
                    "paymentId": {"type": "string", "description": "bytes32 payment id from the relevant slash settlement event."}
                },
                "required": ["as_bee", "paymentId"]
            }
        }),

        // ─────────── Watchdog + arbiter path ───────────
        json!({
            "name": "daman_file_claim",
            "description": "File a slash claim against a leader for a universe violation, tier-cap leverage abuse, or performance degradation. The arbiter rules upheld or rejected within the dispute window. Upheld claims pay a 10% bounty on the slashed bond to the calling watchdog. Rejected claims accumulate negative reputation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "as_bee": {"type": "string"},
                    "leader": {"type": "string", "description": "20-byte hex address of the suspect leader."},
                    "evidenceHash": {"type": "string", "description": "keccak256 hash of the evidence bundle, as a 0x-prefixed bytes32 hex string."},
                    "builder": {"type": "string", "description": "Optional bytes32 builder tag; default 0x000...000."}
                },
                "required": ["as_bee", "leader", "evidenceHash"]
            }
        }),
        json!({
            "name": "daman_rule_claim",
            "description": "Arbiter rules on an open slash claim. `upheld=true` slashes up to slashAmount (capped at 25% of leader's bond by BondEconomics) and triggers the bounty + restitution payout. `upheld=false` rejects, no slash, accumulates negative reputation on the filing watchdog.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "as_bee": {"type": "string"},
                    "claimId": {"type": "string", "description": "uint256 claim id from the DegradationFlagged event."},
                    "slashAmount": {"type": "string", "description": "Amount to slash in atomic USDC; ignored if upheld=false."},
                    "upheld": {"type": "boolean"},
                    "builder": {"type": "string", "description": "Optional bytes32 builder tag; default 0x000...000."},
                    "traceCid": {"type": "string", "description": "Optional bytes32 IPFS CID of the arbiter's reasoning trace."}
                },
                "required": ["as_bee", "claimId", "slashAmount", "upheld"]
            }
        }),
        json!({
            "name": "daman_claim_bounty",
            "description": "Claim the 10% bounty owed to a watchdog for an upheld slash claim. Call this only after observing the corresponding arbiterRule(upheld=true) event for the claim.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "as_bee": {"type": "string"},
                    "claimId": {"type": "string"}
                },
                "required": ["as_bee", "claimId"]
            }
        }),

        // ─────────── Benevolence credit path ───────────
        json!({
            "name": "daman_request_loan",
            "description": "Borrow USDC from the Benevolence treasury. Zero interest, capped at 5 USDC per borrower. Eligibility: registered against the agent registry AND either a fresh entrant (no prior loans) OR active within the last 24h with current balance below 1 USDC. Direct path; requires caller to pay gas.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "as_bee": {"type": "string"},
                    "amount": {"type": "string", "description": "Amount to borrow in atomic USDC. Max 5000000 (5 USDC at 6 decimals)."}
                },
                "required": ["as_bee", "amount"]
            }
        }),
        json!({
            "name": "daman_request_loan_with_signature",
            "description": "Relief-bee submits an EIP-712 signed LoanRequest on behalf of a bust borrower. The borrower signed off-chain; the relief bee pays only gas. Debt anchors to req.borrower, never to msg.sender.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "as_bee": {"type": "string", "description": "The relief bee's name."},
                    "request": {
                        "type": "object",
                        "properties": {
                            "borrower": {"type": "string"},
                            "amount": {"type": "string"},
                            "nonce": {"type": "string"},
                            "deadline": {"type": "string"}
                        },
                        "required": ["borrower", "amount", "nonce", "deadline"]
                    },
                    "signature": {"type": "string", "description": "Borrower's EIP-712 signature hex."}
                },
                "required": ["as_bee", "request", "signature"]
            }
        }),
        json!({
            "name": "daman_repay",
            "description": "Repay outstanding benevolence debt. Approves USDC then calls Benevolence.repay. Amount must be at most current debtOf(as_bee).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "as_bee": {"type": "string"},
                    "amount": {"type": "string"}
                },
                "required": ["as_bee", "amount"]
            }
        }),
        json!({
            "name": "daman_sign_loan_request",
            "description": "Sign an EIP-712 LoanRequest off-chain. Pure-cpu, no gas. Use this when the bee is bust (cannot afford its own tx) and needs a relief bee to relay daman_request_loan_with_signature. Returns a signed body the bee then gossips on daman/credit/p2p.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "as_bee": {"type": "string"},
                    "amount": {"type": "string"},
                    "nonce": {"type": "string", "description": "Per-borrower nonce; read via daman_read_* helpers or via nonceOf at the contract."},
                    "deadline": {"type": "string", "description": "Unix timestamp (seconds) after which the signed request expires."}
                },
                "required": ["as_bee", "amount", "nonce", "deadline"]
            }
        }),

        // ─────────── Read-only ───────────
        json!({
            "name": "daman_read_leader_state",
            "description": "Read on-chain leader state: address, tier, bondAmount, claimedAum, active flag, registeredAt, bondLockedUntil. Use before subscribing or filing a claim.",
            "inputSchema": {
                "type": "object",
                "properties": {"leader": {"type": "string"}},
                "required": ["leader"]
            }
        }),
        json!({
            "name": "daman_read_subscription_state",
            "description": "Read on-chain subscription state for a (follower, leader) pair.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "follower": {"type": "string"},
                    "leader": {"type": "string"}
                },
                "required": ["follower", "leader"]
            }
        }),
        json!({
            "name": "daman_read_reputation",
            "description": "Read an agent's reputation: signed score plus cumulativeUpheld and cumulativeRejected counts. Use to gauge a leader's trustworthiness or a watchdog/arbiter's track record before acting.",
            "inputSchema": {
                "type": "object",
                "properties": {"agent": {"type": "string"}},
                "required": ["agent"]
            }
        }),
        json!({
            "name": "daman_read_active_claims",
            "description": "Read the set of open slash claims against a leader. Returns claimId, watchdog, evidenceHash, filedAt, disputeWindowEnds, status for each.",
            "inputSchema": {
                "type": "object",
                "properties": {"leader": {"type": "string"}},
                "required": ["leader"]
            }
        }),
        json!({
            "name": "daman_subscribe_to_role_events",
            "description": "Open a streaming subscription for events relevant to a role. Watchdog: TradeExecuted + SettlementCompleted. Arbiter: DegradationFlagged + DisputeOpened. Relief: chi:credit-signed-request gossip. Call once at session start, then idle and react to incoming events.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "as_bee": {"type": "string"},
                    "role": {"type": "string", "enum": ["leader", "follower", "watchdog", "arbiter", "relief"]}
                },
                "required": ["as_bee", "role"]
            }
        }),
    ]
}

pub fn tool_names() -> Vec<&'static str> {
    vec![
        "daman_register_leader",
        "daman_record_trade",
        "daman_subscribe",
        "daman_unsubscribe",
        "daman_claim_refund",
        "daman_file_claim",
        "daman_rule_claim",
        "daman_claim_bounty",
        "daman_request_loan",
        "daman_request_loan_with_signature",
        "daman_repay",
        "daman_sign_loan_request",
        "daman_read_leader_state",
        "daman_read_subscription_state",
        "daman_read_reputation",
        "daman_read_active_claims",
        "daman_subscribe_to_role_events",
    ]
}
