//! Live financial-state snapshot for the per-persona prompt-prepend path.
//!
//! Every chi:prompt the persona binary emits opens with a current-state block
//! produced by `render_state_block(&fetch_bee_state(&ctx).await)`. The block
//! tells claude its USDC balance (also its native gas budget on Arc), its
//! bond, its debt, its reputation, the treasury headroom, and whether the
//! agent-registry anchor is in place. With that block inline, claude does not
//! have to spend a tool call on every tick to re-discover its own balance.
//!
//! Failure mode: any individual read failure degrades gracefully. The snapshot
//! still returns; the failing field renders as `unknown` and the underlying
//! reason rides on `BeeState.errors`. That keeps the loop forward-moving even
//! if one of the eight contracts is temporarily flaky.

use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, I256, U256};
use alloy::providers::{Provider, ProviderBuilder};

use crate::contracts::{AgentRegistry, Benevolence, CopyBond, Erc20, ReputationRegistry};
use crate::factories::DamanCtx;

/// Compact financial-state snapshot for one bee.
#[derive(Debug, Clone)]
pub struct BeeState {
    pub bee_name: String,
    pub eoa: Address,

    /// USDC balance in base units. On Arc this IS the native gas budget;
    /// `with_recommended_fillers` pre-deducts `gas_limit * max_fee_per_gas`
    /// before any tx executes.
    pub usdc_balance: U256,
    pub usdc_balance_pretty: String,

    /// Posted bond on DamanCopyBond. Zero unless registered as a leader.
    pub bond_amount: U256,
    pub bond_amount_pretty: String,
    pub tier: Option<u8>,
    pub leader_active: bool,

    /// Reputation score (signed; can be negative).
    pub reputation_score: I256,

    /// Outstanding mesh-mutual-aid debt to the benevolence treasury.
    pub debt: U256,
    pub debt_pretty: String,

    /// Headroom in the benevolence treasury for new loans.
    pub treasury_available: U256,
    pub treasury_pretty: String,

    /// Did the boot-time register_agent call land on DamanAgentRegistry?
    pub agent_registered: bool,

    /// Latest block height observed during the snapshot fetch.
    pub block_height: u64,

    /// Unix seconds at which the snapshot was assembled.
    pub fetched_at_ts: u64,

    /// Per-field error notes, one line each. Empty on a clean fetch.
    pub errors: Vec<String>,
}

fn format_usdc(amount: U256) -> String {
    let ten_pow_6 = U256::from(1_000_000u64);
    let whole = amount / ten_pow_6;
    let frac = (amount % ten_pow_6).to::<u64>();
    format!("{whole}.{frac:06} USDC")
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Fetch the bee's live financial state in one provider session. Each
/// contract read is independent; one failure does not abort the snapshot, it
/// records a note on `errors` and falls back to a zero/default value for that
/// field.
pub async fn fetch_bee_state(ctx: &DamanCtx) -> BeeState {
    let mut errors: Vec<String> = Vec::new();

    let eoa = match Address::from_str(&ctx.eoa_addr) {
        Ok(a) => a,
        Err(e) => {
            errors.push(format!("eoa parse: {e}"));
            Address::ZERO
        }
    };

    // Single provider for the whole snapshot. No wallet (these are all
    // read-only) so the signer nonce isn't bumped.
    let url = match reqwest::Url::parse(&ctx.rpc_url) {
        Ok(u) => u,
        Err(e) => {
            errors.push(format!("rpc url: {e}"));
            return empty_state(ctx, eoa, errors);
        }
    };
    let provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .on_http(url);

    let block_height = match provider.get_block_number().await {
        Ok(n) => n,
        Err(e) => {
            errors.push(format!("get_block_number: {e}"));
            0
        }
    };

    // USDC balance (also native gas on Arc).
    let (usdc_balance, usdc_balance_pretty) =
        match Address::from_str(&ctx.addrs.usdc) {
            Ok(usdc_addr) => {
                let token = Erc20::new(usdc_addr, &provider);
                match token.balanceOf(eoa).call().await {
                    Ok(r) => (r._0, format_usdc(r._0)),
                    Err(e) => {
                        errors.push(format!("usdc.balanceOf: {e}"));
                        (U256::ZERO, "unknown".into())
                    }
                }
            }
            Err(e) => {
                errors.push(format!("usdc addr: {e}"));
                (U256::ZERO, "unknown".into())
            }
        };

    // Leader state from DamanCopyBond. Most bees aren't leaders, so the
    // `tier=None, active=false, bond=0` fallback IS the steady state, not an
    // error. Genuine RPC failures still record on `errors`.
    let (bond_amount, tier, leader_active) =
        match Address::from_str(&ctx.addrs.copy_bond) {
            Ok(cb_addr) => {
                let cb = CopyBond::new(cb_addr, &provider);
                match cb.getLeader(eoa).call().await {
                    Ok(r) => (r.bondAmount, Some(r.tier), r.active),
                    Err(e) => {
                        // CopyBond.getLeader reverts NotLeader for unregistered
                        // addresses; that's expected and not an error.
                        let msg = e.to_string();
                        if !msg.contains("NotLeader") && !msg.contains("revert") {
                            errors.push(format!("copyBond.getLeader: {msg}"));
                        }
                        (U256::ZERO, None, false)
                    }
                }
            }
            Err(e) => {
                errors.push(format!("copy_bond addr: {e}"));
                (U256::ZERO, None, false)
            }
        };
    let bond_amount_pretty = format_usdc(bond_amount);

    // Reputation score.
    let reputation_score = match Address::from_str(&ctx.addrs.reputation_registry) {
        Ok(rep_addr) => {
            let rep = ReputationRegistry::new(rep_addr, &provider);
            match rep.reputationScore(eoa).call().await {
                Ok(r) => r._0,
                Err(e) => {
                    errors.push(format!("reputation.reputationScore: {e}"));
                    I256::ZERO
                }
            }
        }
        Err(e) => {
            errors.push(format!("reputation addr: {e}"));
            I256::ZERO
        }
    };

    // Debt + treasury headroom from DamanBenevolence.
    let (debt, treasury_available) = match Address::from_str(&ctx.addrs.benevolence) {
        Ok(benev_addr) => {
            let benev = Benevolence::new(benev_addr, &provider);
            let debt = match benev.debtOf(eoa).call().await {
                Ok(r) => r._0,
                Err(e) => {
                    errors.push(format!("benevolence.debtOf: {e}"));
                    U256::ZERO
                }
            };
            let treasury = match benev.treasuryAvailable().call().await {
                Ok(r) => r._0,
                Err(e) => {
                    errors.push(format!("benevolence.treasuryAvailable: {e}"));
                    U256::ZERO
                }
            };
            (debt, treasury)
        }
        Err(e) => {
            errors.push(format!("benevolence addr: {e}"));
            (U256::ZERO, U256::ZERO)
        }
    };
    let debt_pretty = format_usdc(debt);
    let treasury_pretty = format_usdc(treasury_available);

    // Agent registry anchor.
    let agent_registered = match Address::from_str(&ctx.addrs.agent_registry) {
        Ok(reg_addr) => {
            let reg = AgentRegistry::new(reg_addr, &provider);
            match reg.isRegistered(eoa).call().await {
                Ok(r) => r._0,
                Err(e) => {
                    errors.push(format!("agentRegistry.isRegistered: {e}"));
                    false
                }
            }
        }
        Err(e) => {
            errors.push(format!("agent_registry addr: {e}"));
            false
        }
    };

    BeeState {
        bee_name: ctx.bee_name.as_ref().clone(),
        eoa,
        usdc_balance,
        usdc_balance_pretty,
        bond_amount,
        bond_amount_pretty,
        tier,
        leader_active,
        reputation_score,
        debt,
        debt_pretty,
        treasury_available,
        treasury_pretty,
        agent_registered,
        block_height,
        fetched_at_ts: now_unix_secs(),
        errors,
    }
}

fn empty_state(ctx: &DamanCtx, eoa: Address, errors: Vec<String>) -> BeeState {
    BeeState {
        bee_name: ctx.bee_name.as_ref().clone(),
        eoa,
        usdc_balance: U256::ZERO,
        usdc_balance_pretty: "unknown".into(),
        bond_amount: U256::ZERO,
        bond_amount_pretty: "unknown".into(),
        tier: None,
        leader_active: false,
        reputation_score: I256::ZERO,
        debt: U256::ZERO,
        debt_pretty: "unknown".into(),
        treasury_available: U256::ZERO,
        treasury_pretty: "unknown".into(),
        agent_registered: false,
        block_height: 0,
        fetched_at_ts: now_unix_secs(),
        errors,
    }
}

/// Render the snapshot as the inline block prepended to every chi:prompt.
/// Format is intentionally compact and consistent so claude pattern-matches
/// the numeric positions rather than re-parsing layout each tick.
pub fn render_state_block(s: &BeeState) -> String {
    let now = now_unix_secs();
    let age = now.saturating_sub(s.fetched_at_ts);
    let tier_str = match (s.tier, s.leader_active) {
        (Some(0), active) => format!("tier 0 retail, active={active}"),
        (Some(1), active) => format!("tier 1 mid, active={active}"),
        (Some(2), active) => format!("tier 2 institutional, active={active}"),
        (Some(t), active) => format!("tier {t}, active={active}"),
        (None, _) => "not registered as leader".into(),
    };

    let mut out = String::new();
    out.push_str(&format!(
        "Current state (block {block}, fetched {age}s ago):\n",
        block = s.block_height,
        age = age
    ));
    out.push_str(&format!(
        "- USDC balance: {bal} (also your native gas; every tx pre-deducts gas_limit * max_fee_per_gas from this)\n",
        bal = s.usdc_balance_pretty
    ));
    out.push_str(&format!(
        "- bond posted: {bond} ({tier_str})\n",
        bond = s.bond_amount_pretty,
        tier_str = tier_str,
    ));
    out.push_str(&format!(
        "- reputation score: {score}\n",
        score = s.reputation_score
    ));
    out.push_str(&format!(
        "- debt to benevolence treasury: {debt}\n",
        debt = s.debt_pretty
    ));
    out.push_str(&format!(
        "- treasury available for new loans: {avail}\n",
        avail = s.treasury_pretty
    ));
    out.push_str(&format!(
        "- agent registered on DamanAgentRegistry: {yes}\n",
        yes = if s.agent_registered { "yes" } else { "no" }
    ));
    if !s.errors.is_empty() {
        out.push_str("- snapshot warnings:\n");
        for e in &s.errors {
            out.push_str(&format!("    * {e}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> BeeState {
        BeeState {
            bee_name: "daman-leader-alpha".into(),
            eoa: Address::ZERO,
            usdc_balance: U256::from(2_850_000u64),
            usdc_balance_pretty: format_usdc(U256::from(2_850_000u64)),
            bond_amount: U256::from(100_000u64),
            bond_amount_pretty: format_usdc(U256::from(100_000u64)),
            tier: Some(0),
            leader_active: true,
            reputation_score: I256::ZERO,
            debt: U256::ZERO,
            debt_pretty: format_usdc(U256::ZERO),
            treasury_available: U256::from(45_000_000u64),
            treasury_pretty: format_usdc(U256::from(45_000_000u64)),
            agent_registered: true,
            block_height: 44_002_317,
            fetched_at_ts: now_unix_secs(),
            errors: Vec::new(),
        }
    }

    #[test]
    fn format_usdc_matches_six_decimals() {
        assert_eq!(format_usdc(U256::from(2_850_000u64)), "2.850000 USDC");
        assert_eq!(format_usdc(U256::from(0u64)), "0.000000 USDC");
        assert_eq!(format_usdc(U256::from(1_000_000u64)), "1.000000 USDC");
        assert_eq!(format_usdc(U256::from(45_000_000u64)), "45.000000 USDC");
    }

    #[test]
    fn render_block_has_every_required_field() {
        let s = sample_state();
        let block = render_state_block(&s);
        assert!(block.contains("Current state (block 44002317"));
        assert!(block.contains("USDC balance: 2.850000 USDC"));
        assert!(block.contains("bond posted: 0.100000 USDC"));
        assert!(block.contains("tier 0 retail"));
        assert!(block.contains("reputation score: 0"));
        assert!(block.contains("debt to benevolence treasury: 0.000000 USDC"));
        assert!(block.contains("treasury available for new loans: 45.000000 USDC"));
        assert!(block.contains("agent registered on DamanAgentRegistry: yes"));
    }

    #[test]
    fn render_block_reflects_unregistered_bee() {
        let mut s = sample_state();
        s.tier = None;
        s.leader_active = false;
        s.bond_amount = U256::ZERO;
        s.bond_amount_pretty = format_usdc(U256::ZERO);
        s.agent_registered = false;
        let block = render_state_block(&s);
        assert!(block.contains("not registered as leader"));
        assert!(block.contains("agent registered on DamanAgentRegistry: no"));
    }

    #[test]
    fn render_block_emits_warnings_when_errors_present() {
        let mut s = sample_state();
        s.errors.push("benevolence.debtOf: rpc timeout".into());
        let block = render_state_block(&s);
        assert!(block.contains("snapshot warnings"));
        assert!(block.contains("benevolence.debtOf: rpc timeout"));
    }
}
