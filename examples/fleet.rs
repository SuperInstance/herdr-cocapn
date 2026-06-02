//! herdr-cocapn fleet demo
//!
//! Run with: cargo run --example fleet
//!
//! This shows how herdr agents become a managed fleet using cocapn-core:
//! - Multiple local agents running in parallel
//! - Deadbands detect overload and idle conditions
//! - Escalation provisions cloud agents when local is overwhelmed
//! - Crossfade handoff transitions between agents
//! - Push-down ensures cheap hardware runs what it can

use std::time::Duration;
use std::thread;

use herdr::fleet::{
    AgentHealth, AgentMetrics, AgentModel, EscalationAction, Fleet, FleetAgent,
};

fn main() {
    println!("╔══════════════════════════════════════════╗");
    println!("║   herdr-cocapn Fleet Management Demo    ║");
    println!("╚══════════════════════════════════════════╝");
    println!();

    // 1. Create the fleet
    let mut fleet = Fleet::new();
    println!("🟢 Fleet created");

    // 2. Register local agents (Reflex tier)
    let agent_a = FleetAgent::new("pane-1", "codex-a", AgentModel::Local);
    let agent_b = FleetAgent::new("pane-2", "codex-b", AgentModel::Local);
    let agent_c = FleetAgent::new("pane-3", "codex-c", AgentModel::Local);

    let events = vec![
        fleet.register_agent(agent_a),
        fleet.register_agent(agent_b),
        fleet.register_agent(agent_c),
    ];
    println!("🟢 Registered 3 local agents (Reflex tier)");

    // 3. Start a handoff demo between two agents
    println!();
    println!("── Crossfade Handoff Demo ──");

    // Register a cloud agent
    let cloud_agent = FleetAgent::new("pane-4", "claude-cloud", AgentModel::Cloud);
    fleet.register_agent(cloud_agent);
    println!("🟢 Registered cloud agent (Cortex tier)");

    // Begin handoff from local to cloud
    if let Err(e) = fleet.begin_handoff("pane-1", "pane-4") {
        eprintln!("Handoff error: {}", e);
    }
    println!("▶️  Handoff started: pane-1 → pane-4");

    // Simulate ticking the handoff
    for i in 1..=10 {
        let completed = fleet.tick_handoffs(Duration::from_millis(600));
        let progress = (i as f64) * 10.0;
        let arrow = match fleet.handoffs.first() {
            Some(h) => match h.state {
                cocapn_core::handoff::HandoffState::FadingOut => "⏳ Fading Out",
                cocapn_core::handoff::HandoffState::Crossfading => "🔄 Crossfading (50/50)",
                cocapn_core::handoff::HandoffState::FadingIn => "⏳ Fading In",
                _ => "✅ Complete",
            },
            None => "✅ Complete",
        };
        println!("  {}% — {}", progress as u8, arrow);

        if !completed.is_empty() {
            println!("✅ Handoff complete: {} transferred", completed[0]);
            break;
        }
    }

    // 4. Deadband overload detection demo
    println!();
    println!("── Deadband Overload Detection ──");

    // Simulate pending tasks building up on a local agent
    let overloaded_pane = "pane-2";
    if let Some(agent) = fleet.agents.get_mut(overloaded_pane) {
        let metrics = AgentMetrics {
            pending_tasks: 12,
            avg_completion_secs: 45.0,
            tokens_last_interval: 0,
            errors_last_interval: 0,
            is_generating: true,
            idle_seconds: 0.0,
        };
        let health = agent.update_metrics(metrics);
        println!(
            "Agent {}: pending_tasks=12 → {:?}",
            overloaded_pane, health
        );
    }

    // Another agent just idling
    if let Some(agent) = fleet.agents.get_mut("pane-3") {
        let metrics = AgentMetrics {
            pending_tasks: 0,
            avg_completion_secs: 0.0,
            tokens_last_interval: 0,
            errors_last_interval: 0,
            is_generating: false,
            idle_seconds: 120.0,
        };
        let health = agent.update_metrics(metrics);
        println!(
            "Agent pane-3: idle_seconds=120 → {:?}",
            health
        );
    }

    // 5. Escalation demo
    println!();
    println!("── Escalation Decision ──");

    let actions = fleet.evaluate_and_escalate();
    for action in &actions {
        match action {
            EscalationAction::EscalateToCloud { target_pane, reason } => {
                println!(
                    "⬆️  ESCALATE: Hand off to {} — {}",
                    target_pane, reason
                );
            }
            EscalationAction::ProvisionCloudAgent { agent_type, reason } => {
                println!("🆕 PROVISION: Launch {} — {}", agent_type, reason);
            }
            EscalationAction::DeEscalateToLocal { target_pane } => {
                println!("⬇️  DE-ESCALATE: Return work to {} (save cost)", target_pane);
            }
            EscalationAction::NoCapableAgent(reason) => {
                println!("⚠️  NO AGENT: {}", reason);
            }
            EscalationAction::None => {}
        }
    }

    // 6. Find idle agents
    let idle = fleet.find_idle(30.0);
    if !idle.is_empty() {
        println!();
        println!("── Idle Agents (cost saving candidates) ──");
        for agent in &idle {
            println!(
                "  💤 {} ({:?}) — idle for {:.0}s",
                agent.name, agent.model, agent.metrics.idle_seconds
            );
        }
    }

    // 7. Final fleet summary
    println!();
    println!("── Fleet Summary ──");
    println!("{}", fleet.summary());
}
