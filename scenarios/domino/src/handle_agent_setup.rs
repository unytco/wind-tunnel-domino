use super::handle_scenario_setup::ScenarioValues;
use crate::domino_agent::DominoAgentExt;
use holochain_types::prelude::YamlProperties;
use holochain_wind_tunnel_runner::prelude::*;
use holochain_wind_tunnel_runner::scenario_happ_bytes;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;

pub fn agent_setup(
    ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext<ScenarioValues>>,
) -> HookResult {
    let progenitor_agent_pubkey = ctx.runner_context().get().progenitor_agent_pubkey();
    let prop = serde_yaml::to_value(serde_json::json!({
        "progenitor_agent_pubkey": progenitor_agent_pubkey.to_string(),
    }))?;
    log::info!("DNA properties: {:?}", prop);

    let dna_properties = HashMap::from([("alliance".to_string(), YamlProperties::new(prop))]);

    // let assigned_behaviour = ctx.assigned_behaviour().to_string();
    if ctx.agent_name().contains("agent-0") {
        log::info!("Installing app for initiator agent pubkey (Progenitor)");
        custom_install_app_from_bytes(
            ctx,
            scenario_happ_bytes!("domino"),
            &"alliance".to_string(),
            Some(progenitor_agent_pubkey),
            Some(dna_properties),
        )?;
    } else {
        log::info!("Installing app for participant agent pubkey");
        custom_install_app_from_bytes(
            ctx,
            scenario_happ_bytes!("domino"),
            &"alliance".to_string(),
            None,
            Some(dna_properties),
        )?;
    }
    try_wait_for_min_agents(ctx, Duration::from_secs(120))?;

    ctx.domino_init()?;

    log::info!(
        "Agent setup complete for {}, with agent pub key {:?}, dna hash {:?}",
        ctx.agent_name(),
        ctx.get().cell_id().agent_pubkey(),
        ctx.get().cell_id().dna_hash()
    );

    // Every agent creates a code template to flag that they have joined the network
    let _ = ctx.domino_create_flag_template()?;

    // Note: get all code template and check the author of them as possible agents to transact with
    // I think we should generate the list based on the behaviour of the agent
    // so in some cases we should only have one agent to transact with
    // and in other cases we should have all agents to transact with
    ctx.get_mut().scenario_values.session_start_time = Some(Instant::now());
    Ok(())
}
