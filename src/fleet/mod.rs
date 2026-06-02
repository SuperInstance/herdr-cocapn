//! Fleet management for herdr agents using cocapn-core primitives.
//!
//! This module brings cocapn-core's tier escalation, deadband triggers, and
//! crossfade handoff into herdr's agent multiplexer. It treats each herdr
//! agent pane as a "device" in a CoCapn compute stripe, enabling:
//!
//! - **Tier escalation**: local (Reflex) agents → cloud (Cortex) agents on demand
//! - **Deadband triggers**: detect when an agent is overloaded or idle
//! - **Crossfade handoff**: smoothly transition between agents without cutting
//! - **Stripe rebalancing**: redistribute agent work when panes are added/removed
//! - **Push-down**: prefer cheaper local models when they can handle the task

#![allow(dead_code)]

use std::collections::HashMap;
use std::time::Duration;

use cocapn_core::{
    deadband::{Deadband, DeadbandDirection, DeadbandState},
    device::{Capability, Device, DeviceTier},
    handoff::Handoff,
    pushdown::{push_down, FeatureSpec, FeatureStatus},
    stripe::{Stripe, StripeEvent, StripeLayer},
};

#[cfg(test)]
use cocapn_core::pushdown::ComputeClass;

/// The kind of AI agent model running in a herdr pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentModel {
    /// Running a local model (Ollama, llama.cpp, etc.)
    Local,
    /// Running a cloud API agent (Claude Code, Codex, GPT, etc.)
    Cloud,
    /// Running a hybrid agent (pilot + local fallback)
    Hybrid,
    /// Unknown or unsupported model
    Unknown,
}

/// How an agent is currently performing in terms of throughput.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentHealth {
    /// Agent is handling work fine
    Healthy,
    /// Agent is slow but managing
    Strained,
    /// Agent is overloaded — needs escalation
    Overloaded,
    /// Agent is idle — unnecessary resource spend
    Idle,
}

/// Metrics tracked per agent pane for deadband evaluation.
#[derive(Debug, Clone)]
pub struct AgentMetrics {
    /// Number of pending/queued tasks
    pub pending_tasks: usize,
    /// Average task completion time in seconds (rolling window)
    pub avg_completion_secs: f64,
    /// Total tokens generated in the last interval
    pub tokens_last_interval: usize,
    /// Errors encountered in the last interval
    pub errors_last_interval: usize,
    /// Whether the agent is currently generating
    pub is_generating: bool,
    /// Wall clock time since last activity
    pub idle_seconds: f64,
}

impl Default for AgentMetrics {
    fn default() -> Self {
        Self {
            pending_tasks: 0,
            avg_completion_secs: 0.0,
            tokens_last_interval: 0,
            errors_last_interval: 0,
            is_generating: false,
            idle_seconds: 0.0,
        }
    }
}

/// A fleet-managed herdr agent pane.
#[derive(Debug, Clone)]
pub struct FleetAgent {
    /// The pane/terminal identifier (herdr's pane ID)
    pub pane_id: String,
    /// Human-readable agent name
    pub name: String,
    /// Agent model type
    pub model: AgentModel,
    /// cocapn-core device for this agent
    pub device: Device,
    /// Current metrics snapshot
    pub metrics: AgentMetrics,
    /// Last known health status
    pub health: AgentHealth,
    /// The CoCapn tier this agent maps to
    pub tier: DeviceTier,
}

impl FleetAgent {
    /// Create a new fleet agent from a herdr pane.
    pub fn new(pane_id: impl Into<String>, name: impl Into<String>, model: AgentModel) -> Self {
        let pane_id: String = pane_id.into();
        let name: String = name.into();
        let tier = model_to_tier(model);
        let mut device = Device::new(pane_id.clone(), name.clone(), tier);

        // Map capabilities based on model type
        let caps = match model {
            AgentModel::Local => {
                vec![Capability::Sense, Capability::Act, Capability::Route]
            }
            AgentModel::Cloud => {
                vec![
                    Capability::Sense,
                    Capability::Act,
                    Capability::Route,
                    Capability::Predict,
                    Capability::Communicate,
                ]
            }
            AgentModel::Hybrid => {
                vec![
                    Capability::Sense,
                    Capability::Act,
                    Capability::Route,
                    Capability::Predict,
                ]
            }
            AgentModel::Unknown => {
                vec![Capability::Sense, Capability::Act]
            }
        };
        device = device.with_capabilities(caps);

        Self {
            pane_id: pane_id.into(),
            name: name.into(),
            model,
            device,
            metrics: AgentMetrics::default(),
            health: AgentHealth::Healthy,
            tier,
        }
    }

    /// Update metrics and re-evaluate health using deadbands.
    pub fn update_metrics(&mut self, metrics: AgentMetrics) -> AgentHealth {
        self.metrics = metrics;

        // Deadband: overload detection (pending tasks above threshold)
        let overload_db = Deadband::new(5.0, 0.5, DeadbandDirection::Above);
        match overload_db.check(self.metrics.pending_tasks as f64) {
            DeadbandState::Exceeded => {
                self.health = AgentHealth::Overloaded;
                return AgentHealth::Overloaded;
            }
            DeadbandState::Approaching => {
                self.health = AgentHealth::Strained;
                return AgentHealth::Strained;
            }
            DeadbandState::Normal => {}
        }

        // Deadband: idle detection (no activity for too long)
        let idle_db = Deadband::new(0.0, 60.0, DeadbandDirection::Above);
        match idle_db.check(self.metrics.idle_seconds) {
            DeadbandState::Exceeded => {
                self.health = AgentHealth::Idle;
                return AgentHealth::Idle;
            }
            _ => {}
        }

        // Deadband: error rate detection (too many errors)
        let error_db = Deadband::new(0.0, 3.0, DeadbandDirection::Above);
        match error_db.check(self.metrics.errors_last_interval as f64) {
            DeadbandState::Exceeded => {
                self.health = AgentHealth::Overloaded;
                return AgentHealth::Overloaded;
            }
            _ => {}
        }

        if self.metrics.is_generating {
            self.health = AgentHealth::Healthy;
        } else {
            self.health = AgentHealth::Healthy;
        }

        self.health
    }
}

impl PartialEq for FleetAgent {
    fn eq(&self, other: &Self) -> bool {
        self.pane_id == other.pane_id
    }
}

impl Eq for FleetAgent {}

/// Fleet-level escalation decision.
#[derive(Debug, Clone)]
pub enum EscalationAction {
    /// No escalation needed
    None,
    /// Escalate to a cloud agent (upgrade tier)
    EscalateToCloud {
        /// The cloud agent pane ID to hand off to
        target_pane: String,
        /// Reason for escalation
        reason: String,
    },
    /// Start a new cloud agent pane
    ProvisionCloudAgent {
        /// Suggested agent to launch
        agent_type: String,
        /// Reason
        reason: String,
    },
    /// De-escalate back to local (cost savings)
    DeEscalateToLocal {
        /// The local pane to hand off to
        target_pane: String,
    },
    /// Report that no capable agent is available
    NoCapableAgent(String),
}

/// The herdr fleet manager — orchestrates agents using cocapn-core concepts.
#[derive(Debug, Clone)]
pub struct Fleet {
    /// The compute stripe of agent tiers
    pub stripe: Stripe,
    /// Active agents indexed by pane ID
    pub agents: HashMap<String, FleetAgent>,
    /// Pending escalation requests
    pub pending_escalations: Vec<EscalationAction>,
    /// Ongoing handoffs
    pub handoffs: Vec<Handoff>,
    /// Task backlog — work items awaiting dispatch
    pub backlog: Vec<String>,
}

impl Fleet {
    pub fn new() -> Self {
        Self {
            stripe: Stripe::new(),
            agents: HashMap::new(),
            pending_escalations: Vec::new(),
            handoffs: Vec::new(),
            backlog: Vec::new(),
        }
    }

    /// Register a new agent pane with the fleet.
    pub fn register_agent(&mut self, agent: FleetAgent) -> StripeEvent {
        let tier = agent.tier;
        let pane_id = agent.pane_id.clone();
        self.agents.insert(pane_id.clone(), agent);

        let layer = StripeLayer {
            tier,
            device_id: pane_id,
            healthy: true,
            latency_ms: None,
        };
        self.stripe.add_layer(layer)
    }

    /// Remove an agent from the fleet (pane closed).
    pub fn deregister_agent(&mut self, pane_id: &str) -> Option<StripeEvent> {
        self.agents.remove(pane_id);
        self.stripe.remove_layer(pane_id)
    }

    /// Evaluate all agents and decide if escalation is needed.
    ///
    /// This uses deadbands to check each agent's health, then applies
    /// the push-down principle: prefer the cheapest tier that can handle
    /// the work, escalate only when needed.
    pub fn evaluate_and_escalate(&mut self) -> Vec<EscalationAction> {
        let mut actions = Vec::new();

        let mut overloaded_agent = None;
        let mut overworked_tier_counts: HashMap<DeviceTier, usize> = HashMap::new();

        // Collect health status across all agents
        for agent in self.agents.values() {
            match agent.health {
                AgentHealth::Overloaded | AgentHealth::Strained => {
                    *overworked_tier_counts.entry(agent.tier).or_insert(0) += 1;
                    if overloaded_agent.is_none() {
                        overloaded_agent = Some(agent.pane_id.clone());
                    }
                }
                _ => {}
            }
        }

        // If local agents are overwhelmed, escalate to cloud
        let local_count = overworked_tier_counts
            .get(&DeviceTier::Reflex)
            .unwrap_or(&0);
        let cloud_agents = self
            .agents
            .values()
            .filter(|a| a.tier >= DeviceTier::Cortex)
            .count();

        if *local_count > 0 && cloud_agents == 0 {
            let reason = format!(
                "{} local agent(s) overwhelmed, no cloud agent available — provisioning",
                local_count
            );
            actions.push(EscalationAction::ProvisionCloudAgent {
                agent_type: "claude".into(),
                reason: reason.clone(),
            });
            self.pending_escalations.push(EscalationAction::ProvisionCloudAgent {
                agent_type: "claude".into(),
                reason,
            });
        } else if *local_count > 0 && cloud_agents > 0 {
            // Find a cloud agent to hand off to
            if let Some(_ag) = overloaded_agent {
                if let Some(cloud) = self.agents.values().find(|a| a.tier >= DeviceTier::Cortex) {
                    let reason = "local agent overloaded - escalating to cloud".to_string();
                    actions.push(EscalationAction::EscalateToCloud {
                        target_pane: cloud.pane_id.clone(),
                        reason: reason.clone(),
                    });
                    self.pending_escalations
                        .push(EscalationAction::EscalateToCloud {
                            target_pane: cloud.pane_id.clone(),
                            reason,
                        });
                }
            }
        }

        // Check for idle cloud agents (cost savings — de-escalate)
        for agent in self.agents.values() {
            if agent.tier >= DeviceTier::Cortex && agent.health == AgentHealth::Idle {
                // Find a healthy local agent to de-escalate to
                if let Some(local) = self
                    .agents
                    .values()
                    .find(|a| a.tier == DeviceTier::Reflex && a.health == AgentHealth::Healthy)
                {
                    actions.push(EscalationAction::DeEscalateToLocal {
                        target_pane: local.pane_id.clone(),
                    });
                }
            }
        }

        actions
    }

    /// Start a crossfade handoff between two agent panes.
    pub fn begin_handoff(
        &mut self,
        from_pane: &str,
        to_pane: &str,
    ) -> Result<(), String> {
        let from = self
            .agents
            .get(from_pane)
            .ok_or_else(|| format!("Unknown agent: {}", from_pane))?
            .pane_id
            .clone();
        let to = self
            .agents
            .get(to_pane)
            .ok_or_else(|| format!("Unknown agent: {}", to_pane))?
            .pane_id
            .clone();

        let mut handoff = Handoff::new(from, to, Duration::from_secs(5));
        handoff.begin()?;
        self.handoffs.push(handoff);
        Ok(())
    }

    /// Advance all active handoffs by the given delta.
    /// Returns completed handoffs that should be finalized.
    pub fn tick_handoffs(&mut self, delta: Duration) -> Vec<String> {
        let mut completed = Vec::new();

        self.handoffs.retain(|h| !h.is_complete() && !h.is_cancelled());
        for handoff in &mut self.handoffs {
            handoff.progress(delta);
            if handoff.is_complete() {
                completed.push(handoff.from_device.clone());
            }
        }

        completed
    }

    /// Evaluate which features each tier can handle using push-down.
    pub fn compute_pushdown(&self, features: &[FeatureSpec]) -> Vec<(DeviceTier, usize)> {
        let mut result = Vec::new();
        for tier in &[DeviceTier::Reflex, DeviceTier::Backbone, DeviceTier::Cortex, DeviceTier::Cloud] {
            let pushed = push_down(features, *tier);
            let available = pushed.iter().filter(|p| p.status == FeatureStatus::Available).count();
            if available > 0 {
                result.push((*tier, available));
            }
        }
        result
    }

    /// Check if any agent's pending task count exceeds a deadband threshold.
    /// Returns the first overloaded agent pane ID.
    pub fn find_overloaded(&self) -> Option<&FleetAgent> {
        let overload_db = Deadband::new(5.0, 0.5, DeadbandDirection::Above);
        self
            .agents
            .values()
            .find(|a| overload_db.check(a.metrics.pending_tasks as f64) == DeadbandState::Exceeded)
    }

    /// Check if any agent has been idle for too long.
    pub fn find_idle(&self, idle_threshold_secs: f64) -> Vec<&FleetAgent> {
        let idle_db = Deadband::new(0.0, idle_threshold_secs, DeadbandDirection::Above);
        self
            .agents
            .values()
            .filter(|a| idle_db.check(a.metrics.idle_seconds) == DeadbandState::Exceeded)
            .collect()
    }

    /// Get the current fleet summary.
    pub fn summary(&self) -> FleetSummary {
        let mut total_agents = 0;
        let mut healthy = 0;
        let mut overloaded = 0;
        let mut idle = 0;
        let mut by_tier = HashMap::new();

        for agent in self.agents.values() {
            total_agents += 1;
            match agent.health {
                AgentHealth::Healthy => healthy += 1,
                AgentHealth::Overloaded | AgentHealth::Strained => overloaded += 1,
                AgentHealth::Idle => idle += 1,
            }
            *by_tier.entry(agent.tier).or_insert(0usize) += 1;
        }

        FleetSummary {
            total_agents,
            healthy,
            overloaded,
            idle,
            by_tier,
            active_handoffs: self.handoffs.len(),
            pending_escalations: self.pending_escalations.len(),
            backlog_depth: self.backlog.len(),
            active_tier: self.stripe.get_active_tier(),
        }
    }
}

/// A snapshot of fleet state for display.
#[derive(Debug, Clone)]
pub struct FleetSummary {
    pub total_agents: usize,
    pub healthy: usize,
    pub overloaded: usize,
    pub idle: usize,
    pub by_tier: HashMap<DeviceTier, usize>,
    pub active_handoffs: usize,
    pub pending_escalations: usize,
    pub backlog_depth: usize,
    pub active_tier: Option<DeviceTier>,
}

impl std::fmt::Display for FleetSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let active_tier = self
            .active_tier
            .map(|t| format!("{:?}", t))
            .unwrap_or_else(|| "None".into());
        writeln!(f, "── herdr-cocapn Fleet ──")?;
        writeln!(f, "  Agents: {}/{} healthy", self.healthy, self.total_agents)?;
        writeln!(f, "  Overloaded: {}  Idle: {}", self.overloaded, self.idle)?;
        writeln!(f, "  Active tier: {}", active_tier)?;
        writeln!(f, "  Handoffs: {}  Escalations pending: {}", self.active_handoffs, self.pending_escalations)?;
        writeln!(f, "  Backlog: {} items", self.backlog_depth)?;
        if !self.by_tier.is_empty() {
            writeln!(f, "  By tier:")?;
            for (tier, count) in &self.by_tier {
                writeln!(f, "    {:?}: {} agent(s)", tier, count)?;
            }
        }
        Ok(())
    }
}

fn model_to_tier(model: AgentModel) -> DeviceTier {
    match model {
        AgentModel::Local => DeviceTier::Reflex,
        AgentModel::Hybrid => DeviceTier::Backbone,
        AgentModel::Cloud => DeviceTier::Cortex,
        AgentModel::Unknown => DeviceTier::Reflex,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_agent() {
        let mut fleet = Fleet::new();
        let agent = FleetAgent::new("pane-1", "local-agent", AgentModel::Local);
        fleet.register_agent(agent);
        assert_eq!(fleet.agents.len(), 1);
        assert_eq!(fleet.stripe.layers().len(), 1);
    }

    #[test]
    fn test_deregister_agent() {
        let mut fleet = Fleet::new();
        let agent = FleetAgent::new("pane-1", "local-agent", AgentModel::Local);
        fleet.register_agent(agent);
        fleet.deregister_agent("pane-1");
        assert_eq!(fleet.agents.len(), 0);
    }

    #[test]
    fn test_deadband_overload_detection() {
        let mut agent = FleetAgent::new("pane-1", "test", AgentModel::Local);
        let metrics = AgentMetrics {
            pending_tasks: 15,
            ..Default::default()
        };
        assert_eq!(agent.update_metrics(metrics), AgentHealth::Overloaded);
    }

    #[test]
    fn test_deadband_idle_detection() {
        let mut agent = FleetAgent::new("pane-1", "test", AgentModel::Cloud);
        let metrics = AgentMetrics {
            idle_seconds: 120.0,
            ..Default::default()
        };
        assert_eq!(agent.update_metrics(metrics), AgentHealth::Idle);
    }

    #[test]
    fn test_escalation_triggers() {
        let mut fleet = Fleet::new();

        // Register two overloaded local agents
        let mut agent1 = FleetAgent::new("pane-1", "local-1", AgentModel::Local);
        agent1.update_metrics(AgentMetrics {
            pending_tasks: 20,
            ..Default::default()
        });
        fleet.register_agent(agent1);

        let mut agent2 = FleetAgent::new("pane-2", "local-2", AgentModel::Local);
        agent2.update_metrics(AgentMetrics {
            pending_tasks: 15,
            ..Default::default()
        });
        fleet.register_agent(agent2);

        let actions = fleet.evaluate_and_escalate();
        assert!(!actions.is_empty());
        assert!(actions.iter().any(|a| matches!(a, EscalationAction::ProvisionCloudAgent { .. })));
    }

    #[test]
    fn test_handoff_flow() {
        let mut fleet = Fleet::new();
        let local = FleetAgent::new("pane-1", "local", AgentModel::Local);
        let cloud = FleetAgent::new("pane-2", "cloud", AgentModel::Cloud);
        fleet.register_agent(local);
        fleet.register_agent(cloud);

        // Begin handoff
        assert!(fleet.begin_handoff("pane-1", "pane-2").is_ok());

        // Tick through the transition
        let completed = fleet.tick_handoffs(Duration::from_secs(6));
        assert!(!completed.is_empty());
        assert_eq!(completed[0], "pane-1");
    }

    #[test]
    fn test_pushdown_evaluation() {
        let features = vec![
            FeatureSpec {
                name: "code_review".into(),
                min_tier: DeviceTier::Reflex,
                memory_bytes: 100_000,
                compute_estimate: ComputeClass::Trivial,
            },
            FeatureSpec {
                name: "complex_refactoring".into(),
                min_tier: DeviceTier::Backbone,
                memory_bytes: 500_000_000,
                compute_estimate: ComputeClass::Light,
            },
            FeatureSpec {
                name: "large_model_inference".into(),
                min_tier: DeviceTier::Cortex,
                memory_bytes: 4_000_000_000,
                compute_estimate: ComputeClass::Heavy,
            },
        ];

        // Local can only do code review
        let fleet = Fleet::new();
        let results = fleet.compute_pushdown(&features);
        assert!(results.iter().any(|(t, _)| *t == DeviceTier::Reflex));
        assert!(results.iter().any(|(t, _)| *t == DeviceTier::Backbone));
        assert!(results.iter().any(|(t, _)| *t == DeviceTier::Cortex));
    }

    #[test]
    fn test_summary() {
        let mut fleet = Fleet::new();
        let agent = FleetAgent::new("pane-1", "test", AgentModel::Local);
        fleet.register_agent(agent);
        let summary = fleet.summary();
        assert_eq!(summary.total_agents, 1);
        assert_eq!(summary.healthy, 1);
    }
}
