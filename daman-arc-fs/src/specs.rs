//! Tool spec table for the namespaced daman tool set.
//!
//! humd's prompt-forward path injects every registered forager tool into the
//! chi:"prompt" frame's `foragerTools` array as `{name, description, inputSchema}`
//! objects (see hum/humd/src/lib.rs around line 1167). The substrate's `Hello`
//! struct currently serializes `tools` as plain strings, which humd's hello parser
//! filter-maps away because it expects objects with a `name` field. The persona
//! binary uses this module to build the proper object shape and override the
//! hello's `tools` field before send, and to put the same shape on each chi:prompt
//! so claude reliably sees the tool surface.
//!
//! Schemas are deliberately minimal. Claude is fine with under-specified types
//! when the description is concrete enough to disambiguate the call site.

use serde_json::{json, Value};

/// Build the {name, description, inputSchema} object array for the namespaced
/// daman tools. The `ns` argument is the per-persona prefix (e.g. `alpha`,
/// `fol_v1_1`, `wd_v1_1`, `arb_v1`, `relief1`).
///
/// `record_trade` and `rule_claim` are intentionally absent. They are oracle /
/// arbiter-only on `DamanCopyBond` and live on the operator binary that
/// holds the privileged key.
pub fn daman_tool_specs(ns: &str) -> Vec<Value> {
    vec![
        // ---------- CopyBond writes ----------
        json!({
            "name": format!("{ns}_register_leader"),
            "description": "Register the bound EOA as a copy-trading leader on DamanCopyBond at the chosen tier and claimed AUM. Idempotent per address; second call reverts. Use this exactly once when a leader-role persona boots.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tier": { "type": "integer", "minimum": 0, "maximum": 2, "description": "0 = retail (~1000 bps bond on AUM), 1 = mid (500 bps), 2 = institutional (250 bps floor)" },
                    "claimedAum": { "type": "string", "description": "Claimed AUM in USDC base units (6 decimals), decimal string, e.g. \"10000000000\" for 10000 USDC" }
                },
                "required": ["tier", "claimedAum"]
            }
        }),
        json!({
            "name": format!("{ns}_post_bond"),
            "description": "Post additional USDC bond to your leader registration on DamanCopyBond. The tool handles the required USDC approve(copyBond, amount) inline; do not call a separate approve. Reverts NotLeader if you have not yet called register_leader.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "amount": { "type": "string", "description": "USDC base units, decimal string" }
                },
                "required": ["amount"]
            }
        }),
        json!({
            "name": format!("{ns}_withdraw_bond"),
            "description": "Withdraw a portion of your posted bond. Reverts BondLocked(unlocksAt) if called before the lockup window, InsufficientBond(req, posted) if the request exceeds posted bond. Withdrawing below the required-bond floor deactivates you as a leader until you post more.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "amount": { "type": "string", "description": "USDC base units, decimal string" }
                },
                "required": ["amount"]
            }
        }),
        json!({
            "name": format!("{ns}_subscribe"),
            "description": "Subscribe this EOA as a follower to a leader. Deposits `capital` USDC to the copy-trading vault and starts mirroring the leader's recorded trades. This tool handles the required USDC approve(copyBond, capital) inline before subscribe; do not call a separate approve.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "leader":  { "type": "string", "description": "Leader EOA address, 0x-prefixed hex" },
                    "capital": { "type": "string", "description": "Capital to commit, USDC base units, decimal string" },
                    "builder": { "type": "string", "description": "Optional bytes32 builder tag, 0x-prefixed hex" }
                },
                "required": ["leader", "capital"]
            }
        }),
        json!({
            "name": format!("{ns}_unsubscribe"),
            "description": "Unsubscribe from a leader and withdraw mirrored capital. Refunds full principal back to the follower; the current contract does not accrue PnL into the subscription record.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "leader": { "type": "string", "description": "Leader EOA address" }
                },
                "required": ["leader"]
            }
        }),
        json!({
            "name": format!("{ns}_file_claim"),
            "description": "Watchdog action: file a degradation claim against a leader. Permissionless. The tool parses the DegradationFlagged event from the receipt logs and returns the assigned claimId, so a follow-up dispute_claim (leader-side) or arbiter ruling can reference it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "leader":       { "type": "string", "description": "Leader EOA being challenged" },
                    "evidenceHash": { "type": "string", "description": "32-byte evidence hash, 0x-prefixed hex" },
                    "builder":      { "type": "string", "description": "Optional bytes32 builder tag" }
                },
                "required": ["leader", "evidenceHash"]
            }
        }),
        json!({
            "name": format!("{ns}_dispute_claim"),
            "description": "Dispute a degradation claim filed against you. Callable only by the leader named in the claim, only before disputeWindowEnds. Reverts ClaimNotFound, NotLeader, DisputeWindowClosed, or AlreadyDisputed. Flips the claim to Disputed status; an arbiter still has to rule.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "claimId": { "type": "string", "description": "Dispute claim id (uint256 decimal string)" }
                },
                "required": ["claimId"]
            }
        }),

        // ---------- Refund ----------
        json!({
            "name": format!("{ns}_claim_refund"),
            "description": "Recipient-side withdraw from RefundProtocol. Calls withdraw(uint256[]) against the deployed proxy for the listed payment ids (all owned by this bee). The tool preflights payments(id) for each id to surface clean errors when the lockup is still active, the payment is already refunded, or the caller is not the recipient. Accepts `paymentIds` (array) or `paymentId` (single) for back-compat with the legacy spec.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "paymentIds": { "type": "array", "items": { "type": "string" }, "description": "Payment ids as uint256 decimal strings" },
                    "paymentId":  { "type": "string", "description": "Single payment id (uint256 decimal string); equivalent to paymentIds with one entry" }
                }
            }
        }),
        json!({
            "name": format!("{ns}_read_payment"),
            "description": "Read a single payment record from RefundProtocol plus derived eligibility flags (paused, refunded, lockup elapsed, remaining principal, recipient balance / debt, withdrawable). Use this before calling claim_refund to avoid burning gas on a doomed withdraw.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "paymentId": { "type": "string", "description": "uint256 decimal string" },
                    "recipient": { "type": "string", "description": "Optional 0x-address to test eligibility against; defaults to this bee's EOA" }
                },
                "required": ["paymentId"]
            }
        }),

        // ---------- Bounty ----------
        json!({
            "name": format!("{ns}_claim_bounty"),
            "description": "Watchdog action: claim the bounty payout for an upheld slash-claim where this EOA is the recorded recipient on DamanBountyAccrual. The tool preflights bountyRecipient / bountyClaimed / bountyAmount and short-circuits with a clean ABI error if any precondition would revert.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "claimId": { "type": "string", "description": "Bounty claim id (uint256 decimal string); note this is a different namespace from the DamanCopyBond dispute claim id" }
                },
                "required": ["claimId"]
            }
        }),
        json!({
            "name": format!("{ns}_read_bounty_amount"),
            "description": "Read the payout amount associated with a bounty claim id. Returns 0 for unknown ids.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "claimId": { "type": "string", "description": "Bounty claim id" }
                },
                "required": ["claimId"]
            }
        }),
        json!({
            "name": format!("{ns}_read_bounty_recipient"),
            "description": "Read the recipient address for a bounty claim id. Returns 0x0 for unknown ids, which doubles as an existence check.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "claimId": { "type": "string", "description": "Bounty claim id" }
                },
                "required": ["claimId"]
            }
        }),
        json!({
            "name": format!("{ns}_read_bounty_claimed"),
            "description": "Has this bounty claim id been paid out yet? Returns false for unknown ids.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "claimId": { "type": "string", "description": "Bounty claim id" }
                },
                "required": ["claimId"]
            }
        }),

        // ---------- Benevolence ----------
        json!({
            "name": format!("{ns}_request_loan"),
            "description": "Direct loan request from the DamanBenevolence mesh-mutual-aid treasury. Use when this persona's EOA still has enough USDC to cover gas (Arc uses USDC as native gas). For bust personas with insufficient gas, use sign_loan_request and have a relief bee submit via request_loan_with_signature.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "amount": { "type": "string", "description": "Requested amount, USDC base units, decimal string. Per-borrower cap is 5_000_000 (5 USDC)." }
                },
                "required": ["amount"]
            }
        }),
        json!({
            "name": format!("{ns}_request_loan_with_signature"),
            "description": "Relief-side action: submit a signed LoanRequest on behalf of a bust persona that cannot afford gas. Args accepted top-level (borrower, amount, nonce, deadline, signature) or nested under `request`. The signature must have been produced by the borrower via sign_loan_request against the EIP-712 domain bound to chain id 5042002 and the benevolence proxy.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "borrower":  { "type": "string", "description": "Borrower EOA (0x-address)" },
                    "amount":    { "type": "string", "description": "Requested amount, USDC base units" },
                    "nonce":     { "type": "string", "description": "uint256 nonce; read benevolence.nonceOf(borrower)" },
                    "deadline":  { "type": "string", "description": "Unix seconds the signature expires at" },
                    "signature": { "type": "string", "description": "65-byte EIP-712 signature, 0x-prefixed hex" }
                },
                "required": ["borrower", "amount", "nonce", "deadline", "signature"]
            }
        }),
        json!({
            "name": format!("{ns}_repay"),
            "description": "Repay principal against your outstanding benevolence debt. The tool inlines the required USDC approve(benevolence, amount) before calling repay; do not chain a separate approve. Not pause-gated; repayment works during a treasury pause.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "amount": { "type": "string", "description": "Amount to repay, USDC base units; must be <= debtOf(borrower)" }
                },
                "required": ["amount"]
            }
        }),
        json!({
            "name": format!("{ns}_sign_loan_request"),
            "description": "Sign an EIP-712 LoanRequest with your bound key AND publish it to the local credit-mutual-aid inbox in one call. Use when your USDC balance is too low to cover gas (Arc uses USDC as native gas; every tx pre-deducts gas_limit * max_fee_per_gas). The relief peers poll that inbox each tick and submit on your behalf via request_loan_with_signature; the borrowed USDC lands in your EOA. Idempotent: same (amount, nonce, deadline) yields a recoverable signature, but each publish creates a new inbox entry, so do not loop.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "amount":   { "type": "string", "description": "Requested amount, USDC base units" },
                    "nonce":    { "type": "string", "description": "uint256 nonce; read benevolence.nonceOf(borrower) before signing" },
                    "deadline": { "type": "string", "description": "Unix seconds the signature expires at; recommend now + 3600" },
                    "reason":   { "type": "string", "description": "Optional short human-readable reason (e.g. 'gas top-up after subscribe revert')" }
                },
                "required": ["amount", "nonce", "deadline"]
            }
        }),
        json!({
            "name": format!("{ns}_publish_signed_request"),
            "description": "Publish an already-signed LoanRequest payload to the credit-mutual-aid inbox without re-signing. Useful only when you have a signed payload from a sibling channel; sign_loan_request already publishes inline, so most personas never call this directly. Accepts the same shape sign_loan_request returns: top-level borrower/amount/nonce/deadline/signature, or nested under `signed` or `request`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "borrower":  { "type": "string", "description": "Borrower EOA, 0x-prefixed" },
                    "amount":    { "type": "string", "description": "USDC base units, decimal string" },
                    "nonce":     { "type": "string", "description": "uint256 nonce, decimal string" },
                    "deadline":  { "type": "string", "description": "Unix seconds the signature expires at" },
                    "signature": { "type": "string", "description": "65-byte EIP-712 signature, 0x-prefixed hex" },
                    "reason":    { "type": "string", "description": "Optional reason string" }
                },
                "required": ["borrower", "amount", "nonce", "deadline", "signature"]
            }
        }),
        json!({
            "name": format!("{ns}_read_credit_inbox"),
            "description": "List unprocessed signed-loan-request entries in the local credit-mutual-aid inbox. Relief peers call this each tick and submit each valid entry via request_loan_with_signature, then call mark_credit_processed to retire the file. Returns an array of {filename, borrower, amount, nonce, deadline, signature, by_bee, reason, signed_at_ts, age_seconds}.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": format!("{ns}_mark_credit_processed"),
            "description": "Mark a credit-inbox entry as submitted. Renames `<filename>.signed.json` to `<filename>.submitted-<tx_hash>.json` so the next read_credit_inbox tick does not re-process it. Call this after request_loan_with_signature returns a receipt.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "filename": { "type": "string", "description": "Basename of the *.signed.json file, as returned by read_credit_inbox" },
                    "tx_hash":  { "type": "string", "description": "Receipt tx hash from request_loan_with_signature" }
                },
                "required": ["filename", "tx_hash"]
            }
        }),

        // ---------- USDC ----------
        json!({
            "name": format!("{ns}_approve_usdc"),
            "description": "Approve a contract to pull USDC from this bee's EOA. For subscribe / post_bond pass the DamanCopyBond address; for repay pass the DamanBenevolence address. amount=\"max\" sets an unlimited allowance (one approve covers all future pulls to that spender). Pass an exact base-units integer string for a one-shot tight approve. Without an allowance the dependent tools revert with ERC20InsufficientAllowance.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "spender": { "type": "string", "description": "Contract that will call transferFrom on USDC, 0x-prefixed" },
                    "amount":  { "type": "string", "description": "USDC base units as a decimal string, or the literal \"max\" for 2^256-1" }
                },
                "required": ["spender", "amount"]
            }
        }),

        // ---------- CopyBond reads ----------
        json!({
            "name": format!("{ns}_read_leader_state"),
            "description": "Read DamanCopyBond leader state: addr, tier (0=Retail, 1=Mid, 2=Institutional), bondAmount, claimedAum, derived requiredBond (computed from BondEconomics bps math so claude does not have to), registeredAt, bondLockedUntil, active.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "leader": { "type": "string", "description": "Leader EOA" }
                },
                "required": ["leader"]
            }
        }),
        json!({
            "name": format!("{ns}_read_subscription_state"),
            "description": "Read a follower's subscription against a given leader from DamanCopyBond.getSubscription: follower, leader, capital committed, since (unix seconds), builder tag.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "follower": { "type": "string", "description": "Follower EOA" },
                    "leader":   { "type": "string", "description": "Leader EOA" }
                },
                "required": ["follower", "leader"]
            }
        }),
        json!({
            "name": format!("{ns}_read_claim"),
            "description": "Read a single Claim record from DamanCopyBond.getClaim: id, leader, watchdog, evidenceHash, filedAt, disputeWindowEnds, status (0=None, 1=Filed, 2=Disputed, 3=Upheld, 4=Rejected), slashAmount, builder.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "claimId": { "type": "string", "description": "Dispute claim id" }
                },
                "required": ["claimId"]
            }
        }),
        json!({
            "name": format!("{ns}_read_bond_balance"),
            "description": "Read the leader's current bond balance via DamanCopyBond.bondBalance. Cheap single-uint lookup; useful for an arbiter sizing a slash or a leader checking headroom.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "leader": { "type": "string", "description": "Leader EOA" }
                },
                "required": ["leader"]
            }
        }),
        json!({
            "name": format!("{ns}_read_active_claims"),
            "description": "DamanCopyBond does not expose an enumerable claim view; this tool returns a structured error pointing at the daman-oracle event index (subscribed to DegradationFlagged) for the full active-claims list. If/when the contract adds nextClaimId() + getClaims(cursor, limit) this becomes a real on-chain read.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),

        // ---------- Reputation ----------
        json!({
            "name": format!("{ns}_read_reputation"),
            "description": "Read an agent's reputation score, cumulative upheld and cumulative rejected counts from DamanReputationRegistry. Useful for picking which leader to follow or whose claims to weight. The cumulative counters fall back to 0 on RPC error; treat 0 as ambiguous (either no rulings yet or transient read failure).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "description": "Agent EOA, 0x-prefixed" }
                },
                "required": ["agent"]
            }
        }),

        // ---------- Universe ----------
        json!({
            "name": format!("{ns}_universe_check"),
            "description": "Read whether a specific asset address is currently eligible in the active universe (HLAL_2026Q2). Use this before recording a trade to avoid AssetNotEligible reverts. Returns the active sourceTag alongside so the caller can verify the curation snapshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "asset": { "type": "string", "description": "Asset address, 0x-prefixed hex. For HLAL placeholders this is keccak256(ticker) truncated to 20 bytes." }
                },
                "required": ["asset"]
            }
        }),
        json!({
            "name": format!("{ns}_universe_list_eligible"),
            "description": "Enumerate the full whitelist of eligible asset addresses in the active universe, with the current sourceTag and last-updated timestamp. Use this at boot to reconcile the persona's claimed tradeable set against the on-chain truth.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),

        // ---------- USDC reads ----------
        json!({
            "name": format!("{ns}_read_usdc_balance"),
            "description": "Read a USDC balance in base units (6 decimals on Arc). Defaults to this bee's own EOA when addr is omitted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "addr": { "type": "string", "description": "Optional 0x-prefixed address; defaults to caller EOA" }
                }
            }
        }),
        json!({
            "name": format!("{ns}_read_usdc_allowance"),
            "description": "Read the current USDC allowance this bee has granted to a spender contract. Use this before deciding whether to issue a fresh approve_usdc.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "spender": { "type": "string", "description": "Spender contract address, 0x-prefixed" }
                },
                "required": ["spender"]
            }
        }),

        // ---------- Gossip ----------
        json!({
            "name": format!("{ns}_subscribe_to_role_events"),
            "description": "Declare interest in chi-gossip topics for the named role. Returns the canonical topic list and a delivery-pending marker. Note: hum does not yet ship a bee-facing gossip-subscribe chi; this tool records intent locally for observability. Real delivery lands once hum's per-bee gossip bridge is wired.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "role":       { "type": "string", "enum": ["leader", "follower", "watchdog", "arbiter", "relief"] },
                    "leaderAddr": { "type": "string", "description": "Optional follower-side leader to watch; if omitted the topic list uses wildcard `*`" }
                },
                "required": ["role"]
            }
        }),
    ]
}
