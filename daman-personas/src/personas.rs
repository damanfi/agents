//! Five role personas implementing `persona_base::PersonaBee`.
//!
//! Each struct is the same shape: a `bee_name`, a `variant`, an `eoa_addr`, and a `sid`.
//! Only the trait-method bodies differ (which topics + event filters to subscribe to, and
//! which kind of incoming event triggers a prompt). The actual decision logic lives in the
//! local claude-cli worker's cell; the persona only assembles state and forwards.

use async_trait::async_trait;
use persona_base::persona::{Decision, Event, EventFilter, PersonaBee};
use serde_json::Value;

use crate::variant::{compose_system_prompt, Role};

/// Common construction shape for all five personas.
#[derive(Debug, Clone)]
pub struct PersonaConfig {
    pub bee_name: String,
    pub variant: String,
    pub eoa_addr: String,
    pub sid: String,
}

impl PersonaConfig {
    pub fn new(
        bee_name: impl Into<String>,
        variant: impl Into<String>,
        eoa_addr: impl Into<String>,
        sid: impl Into<String>,
    ) -> Self {
        Self {
            bee_name: bee_name.into(),
            variant: variant.into(),
            eoa_addr: eoa_addr.into(),
            sid: sid.into(),
        }
    }
}

fn format_user_prompt(event: &Event) -> String {
    let event_block = match event {
        Event::Gossip { topic, body } => format!(
            "Gossip event observed on topic `{topic}`:\n```json\n{}\n```",
            serde_json::to_string_pretty(body).unwrap_or_else(|_| body.to_string())
        ),
        Event::ChainEvent { contract, event, data } => format!(
            "Chain event `{event}` from contract `{contract}`:\n```json\n{}\n```",
            serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string())
        ),
    };
    format!(
        "Triggering event:\n{event_block}\n\n\
         Assemble your view of the world from this event and any state you already hold. \
         Decide what action to take, if any. Tool calls go through chi:tool-call against \
         the daman-arc-fs forager. Reasoning summary in chi:finish."
    )
}

// =============================================================================
// LeaderPersona
// =============================================================================

#[derive(Debug, Clone)]
pub struct LeaderPersona(pub PersonaConfig);

#[async_trait]
impl PersonaBee for LeaderPersona {
    fn bee_name(&self) -> &str {
        &self.0.bee_name
    }

    fn subscribe_topics(&self) -> Vec<String> {
        vec![
            "daman/slash/observability".into(),
            "daman/credit/observability".into(),
        ]
    }

    fn subscribe_chain_events(&self) -> Vec<EventFilter> {
        vec![
            EventFilter {
                contract: "0x493085c71f3CaceB8373db6e6ffeF43EacbC3e02".into(), // CopyBond
                event_signature: "FollowerSubscribed(address,address,uint256)".into(),
                from_block: None,
            },
            EventFilter {
                contract: "0x493085c71f3CaceB8373db6e6ffeF43EacbC3e02".into(),
                event_signature: "DegradationFlagged(uint256,address,address,bytes32,bytes32)".into(),
                from_block: None,
            },
        ]
    }

    fn persona_system_prompt(&self) -> Option<String> {
        Some(compose_system_prompt(
            Role::Leader,
            &self.0.variant,
            &self.0.bee_name,
            &self.0.eoa_addr,
        ))
    }

    async fn on_event(&self, event: Event) -> Decision {
        if !should_react_leader(&event) {
            return Decision::Skip { reason: "leader: event not actionable".into() };
        }
        Decision::Prompt {
            sid: self.0.sid.clone(),
            system_prompt: self.persona_system_prompt().unwrap(),
            user_prompt: format_user_prompt(&event),
        }
    }
}

fn should_react_leader(event: &Event) -> bool {
    match event {
        Event::Gossip { .. } => true,
        Event::ChainEvent { event, .. } => {
            event.contains("FollowerSubscribed") || event.contains("DegradationFlagged")
        }
    }
}

// =============================================================================
// FollowerPersona
// =============================================================================

#[derive(Debug, Clone)]
pub struct FollowerPersona(pub PersonaConfig);

#[async_trait]
impl PersonaBee for FollowerPersona {
    fn bee_name(&self) -> &str {
        &self.0.bee_name
    }

    fn subscribe_topics(&self) -> Vec<String> {
        vec![
            "daman/slash/observability".into(),
            "daman/credit/observability".into(),
        ]
    }

    fn subscribe_chain_events(&self) -> Vec<EventFilter> {
        vec![
            EventFilter {
                contract: "0x493085c71f3CaceB8373db6e6ffeF43EacbC3e02".into(),
                event_signature: "LeaderRegistered(address,uint8,uint256)".into(),
                from_block: None,
            },
            EventFilter {
                contract: "0xAA1a021215322FbB775c6Cc08d81347864a7Ac94".into(), // ReputationRegistry
                event_signature: "ReputationUpdated(address,int256,bool)".into(),
                from_block: None,
            },
            EventFilter {
                contract: "0x493085c71f3CaceB8373db6e6ffeF43EacbC3e02".into(),
                event_signature: "BondSlashed(address,uint256,uint256)".into(),
                from_block: None,
            },
        ]
    }

    fn persona_system_prompt(&self) -> Option<String> {
        Some(compose_system_prompt(
            Role::Follower,
            &self.0.variant,
            &self.0.bee_name,
            &self.0.eoa_addr,
        ))
    }

    async fn on_event(&self, event: Event) -> Decision {
        Decision::Prompt {
            sid: self.0.sid.clone(),
            system_prompt: self.persona_system_prompt().unwrap(),
            user_prompt: format_user_prompt(&event),
        }
    }
}

// =============================================================================
// WatchdogPersona
// =============================================================================

#[derive(Debug, Clone)]
pub struct WatchdogPersona(pub PersonaConfig);

#[async_trait]
impl PersonaBee for WatchdogPersona {
    fn bee_name(&self) -> &str {
        &self.0.bee_name
    }

    fn subscribe_topics(&self) -> Vec<String> {
        vec![
            "daman/slash/observability".into(),
            "daman/trade-claim".into(),
        ]
    }

    fn subscribe_chain_events(&self) -> Vec<EventFilter> {
        vec![
            EventFilter {
                contract: "0x493085c71f3CaceB8373db6e6ffeF43EacbC3e02".into(),
                event_signature: "TradeExecuted(uint256,address,address,uint256,bool,bytes32)"
                    .into(),
                from_block: None,
            },
            EventFilter {
                contract: "0x493085c71f3CaceB8373db6e6ffeF43EacbC3e02".into(),
                event_signature: "SettlementCompleted(uint256,address,int256,uint256)".into(),
                from_block: None,
            },
        ]
    }

    fn persona_system_prompt(&self) -> Option<String> {
        Some(compose_system_prompt(
            Role::Watchdog,
            &self.0.variant,
            &self.0.bee_name,
            &self.0.eoa_addr,
        ))
    }

    async fn on_event(&self, event: Event) -> Decision {
        Decision::Prompt {
            sid: self.0.sid.clone(),
            system_prompt: self.persona_system_prompt().unwrap(),
            user_prompt: format_user_prompt(&event),
        }
    }
}

// =============================================================================
// ArbiterPersona
// =============================================================================

#[derive(Debug, Clone)]
pub struct ArbiterPersona(pub PersonaConfig);

#[async_trait]
impl PersonaBee for ArbiterPersona {
    fn bee_name(&self) -> &str {
        &self.0.bee_name
    }

    fn subscribe_topics(&self) -> Vec<String> {
        vec!["daman/slash/observability".into()]
    }

    fn subscribe_chain_events(&self) -> Vec<EventFilter> {
        vec![
            EventFilter {
                contract: "0x493085c71f3CaceB8373db6e6ffeF43EacbC3e02".into(),
                event_signature: "DegradationFlagged(uint256,address,address,bytes32,bytes32)"
                    .into(),
                from_block: None,
            },
            EventFilter {
                contract: "0x493085c71f3CaceB8373db6e6ffeF43EacbC3e02".into(),
                event_signature: "DisputeOpened(uint256,address)".into(),
                from_block: None,
            },
        ]
    }

    fn persona_system_prompt(&self) -> Option<String> {
        Some(compose_system_prompt(
            Role::Arbiter,
            &self.0.variant,
            &self.0.bee_name,
            &self.0.eoa_addr,
        ))
    }

    async fn on_event(&self, event: Event) -> Decision {
        let actionable = match &event {
            Event::Gossip { body, .. } => body.get("claimId").is_some(),
            Event::ChainEvent { event, .. } => {
                event.contains("DegradationFlagged") || event.contains("DisputeOpened")
            }
        };
        if !actionable {
            return Decision::Skip { reason: "arbiter: event carries no actionable claim".into() };
        }
        Decision::Prompt {
            sid: self.0.sid.clone(),
            system_prompt: self.persona_system_prompt().unwrap(),
            user_prompt: format_user_prompt(&event),
        }
    }
}

// =============================================================================
// ReliefPersona
// =============================================================================

#[derive(Debug, Clone)]
pub struct ReliefPersona(pub PersonaConfig);

#[async_trait]
impl PersonaBee for ReliefPersona {
    fn bee_name(&self) -> &str {
        &self.0.bee_name
    }

    fn subscribe_topics(&self) -> Vec<String> {
        vec!["daman/credit/p2p".into()]
    }

    fn subscribe_chain_events(&self) -> Vec<EventFilter> {
        vec![]
    }

    fn persona_system_prompt(&self) -> Option<String> {
        Some(compose_system_prompt(
            Role::Relief,
            &self.0.variant,
            &self.0.bee_name,
            &self.0.eoa_addr,
        ))
    }

    async fn on_event(&self, event: Event) -> Decision {
        match &event {
            Event::Gossip { topic, body } if topic == "daman/credit/p2p" => {
                if body.get("chi").and_then(Value::as_str) == Some("credit-signed-request") {
                    Decision::Prompt {
                        sid: self.0.sid.clone(),
                        system_prompt: self.persona_system_prompt().unwrap(),
                        user_prompt: format_user_prompt(&event),
                    }
                } else {
                    Decision::Skip { reason: "relief: gossip is not a signed-request".into() }
                }
            }
            _ => Decision::Skip { reason: "relief: event not on p2p topic".into() },
        }
    }
}

// =============================================================================
// Constructors
// =============================================================================

pub fn build(role: Role, cfg: PersonaConfig) -> Box<dyn PersonaBee> {
    match role {
        Role::Leader => Box::new(LeaderPersona(cfg)),
        Role::Follower => Box::new(FollowerPersona(cfg)),
        Role::Watchdog => Box::new(WatchdogPersona(cfg)),
        Role::Arbiter => Box::new(ArbiterPersona(cfg)),
        Role::Relief => Box::new(ReliefPersona(cfg)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(name: &str) -> PersonaConfig {
        PersonaConfig::new(
            name,
            "alpha",
            "0x1111111111111111111111111111111111111111",
            "sid-test",
        )
    }

    #[tokio::test]
    async fn leader_emits_prompt_on_gossip() {
        let p = LeaderPersona(cfg("daman-leader-alpha"));
        let d = p
            .on_event(Event::Gossip {
                topic: "daman/slash/observability".into(),
                body: json!({"leader": "0xabc"}),
            })
            .await;
        assert!(matches!(d, Decision::Prompt { .. }));
    }

    #[tokio::test]
    async fn relief_skips_off_topic() {
        let p = ReliefPersona(cfg("daman-relief-1"));
        let d = p
            .on_event(Event::Gossip {
                topic: "daman/slash/observability".into(),
                body: json!({}),
            })
            .await;
        assert!(matches!(d, Decision::Skip { .. }));
    }

    #[tokio::test]
    async fn relief_prompts_on_signed_request() {
        let p = ReliefPersona(cfg("daman-relief-1"));
        let d = p
            .on_event(Event::Gossip {
                topic: "daman/credit/p2p".into(),
                body: json!({"chi": "credit-signed-request", "request": {}, "signature": "0x"}),
            })
            .await;
        assert!(matches!(d, Decision::Prompt { .. }));
    }

    #[tokio::test]
    async fn relief_skips_non_signed_request_on_topic() {
        let p = ReliefPersona(cfg("daman-relief-1"));
        let d = p
            .on_event(Event::Gossip {
                topic: "daman/credit/p2p".into(),
                body: json!({"chi": "something-else"}),
            })
            .await;
        assert!(matches!(d, Decision::Skip { .. }));
    }

    #[tokio::test]
    async fn arbiter_skips_event_without_claim_id() {
        let p = ArbiterPersona(cfg("daman-arbiter-v1"));
        let d = p
            .on_event(Event::Gossip {
                topic: "daman/slash/observability".into(),
                body: json!({}),
            })
            .await;
        assert!(matches!(d, Decision::Skip { .. }));
    }

    #[tokio::test]
    async fn arbiter_prompts_on_claim_event() {
        let p = ArbiterPersona(cfg("daman-arbiter-v1"));
        let d = p
            .on_event(Event::Gossip {
                topic: "daman/slash/observability".into(),
                body: json!({"claimId": "42"}),
            })
            .await;
        assert!(matches!(d, Decision::Prompt { .. }));
    }

    #[test]
    fn build_dispatches_by_role() {
        let c = cfg("test-bee");
        for role in [Role::Leader, Role::Follower, Role::Watchdog, Role::Arbiter, Role::Relief] {
            let b = build(role, c.clone());
            assert_eq!(b.bee_name(), "test-bee");
        }
    }

    #[test]
    fn each_persona_advertises_chain_events_or_is_relief() {
        let c = cfg("x");
        // Relief has zero chain filters intentionally; the rest do.
        assert!(LeaderPersona(c.clone()).subscribe_chain_events().len() > 0);
        assert!(FollowerPersona(c.clone()).subscribe_chain_events().len() > 0);
        assert!(WatchdogPersona(c.clone()).subscribe_chain_events().len() > 0);
        assert!(ArbiterPersona(c.clone()).subscribe_chain_events().len() > 0);
        assert_eq!(ReliefPersona(c.clone()).subscribe_chain_events().len(), 0);
    }
}
