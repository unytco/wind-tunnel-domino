use crate::context::HolochainAgentContext;
use crate::runner_context::HolochainRunnerContext;
use anyhow::Context;
use holochain_client_instrumented::prelude::{
    handle_api_err, AdminWebsocket, AppWebsocket, AuthorizeSigningCredentialsPayload,
    ClientAgentSigner,
};
use holochain_conductor_api::{AppInfo, AppInfoStatus, CellInfo};
use holochain_types::prelude::*;
use holochain_types::prelude::{
    AppBundleSource, CellId, ExternIO, InstallAppPayload, InstalledAppId, RoleName,
};
use holochain_types::websocket::AllowedOrigins;
use kitsune2_api::AgentInfoSigned;
use kitsune2_core::Ed25519Verifier;
use rand::seq::SliceRandom;
use rand::thread_rng;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wind_tunnel_runner::prelude::{
    AgentContext, HookResult, Reporter, RunnerContext, UserValuesConstraint, WindTunnelResult,
};

/// Sets the `app_ws_url` value in [HolochainRunnerContext] using a valid app port on the target conductor.
///
/// After calling this function you will be able to use the `app_ws_url` in your agent hooks:
/// ```rust
/// use holochain_wind_tunnel_runner::prelude::{HolochainAgentContext, HolochainRunnerContext};
/// use wind_tunnel_runner::prelude::{AgentContext, HookResult};
///
/// fn agent_setup(ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext>) -> HookResult {
///     let app_ws_url = ctx.runner_context().get().app_ws_url();
///     Ok(())
/// }
/// ```
///
/// Method:
/// - Connects to an admin port using the connection string from the context.
/// - Lists app interfaces and if there are any, uses the first one.
/// - If there are no app interfaces, attaches a new one.
/// - Reads the current admin URL from the [RunnerContext] and swaps the admin port for the app port.
/// - Sets the `app_ws_url` value in [HolochainRunnerContext].
pub fn configure_app_ws_url(
    ctx: &mut RunnerContext<HolochainRunnerContext>,
) -> WindTunnelResult<()> {
    let admin_ws_url = ctx.get_connection_string().to_string();
    let reporter = ctx.reporter();
    let app_port = ctx
        .executor()
        .execute_in_place(async move {
            log::debug!("Connecting a Holochain admin client: {}", admin_ws_url);
            let admin_client = AdminWebsocket::connect(admin_ws_url, reporter)
                .await
                .context("Unable to connect admin client")?;

            let existing_app_interfaces = admin_client
                .list_app_interfaces()
                .await
                .map_err(handle_api_err)?;

            let existing_app_ports = existing_app_interfaces
                .into_iter()
                .filter_map(|interface| {
                    if interface.allowed_origins == AllowedOrigins::Any
                        && interface.installed_app_id.is_none()
                    {
                        Some(interface.port)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            if !existing_app_ports.is_empty() {
                Ok(*existing_app_ports.first().context("No app ports found")?)
            } else {
                let attached_app_port = admin_client
                    // Don't specify the port, let the conductor pick one
                    .attach_app_interface(0, AllowedOrigins::Any, None)
                    .await
                    .map_err(handle_api_err)?;
                Ok(attached_app_port)
            }
        })
        .context("Failed to set up app port")?;

    // Use the admin URL with the app port we just got to derive a URL for the app websocket
    let admin_ws_url = ctx.get_connection_string();
    let mut admin_ws_url = url::Url::parse(admin_ws_url)
        .map_err(|e| anyhow::anyhow!("Failed to parse admin URL: {}", e))?;
    admin_ws_url
        .set_port(Some(app_port))
        .map_err(|_| anyhow::anyhow!("Failed to set app port on admin URL"))?;

    ctx.get_mut().app_ws_url = Some(admin_ws_url.to_string());

    Ok(())
}

/// Opinionated app installation which will give you what you need in most cases.
///
/// The [RoleName] you provide is used to find the cell id within the installed app that you want
/// to call during your scenario.
///
/// Requires:
/// - The [HolochainRunnerContext] to have a valid `app_ws_url`. Consider calling [configure_app_ws_url] in your setup before using this function.
///
/// Call this function as follows:
/// ```rust
/// use std::path::Path;
/// use holochain_wind_tunnel_runner::prelude::{HolochainAgentContext, HolochainRunnerContext, install_app, AgentContext, HookResult};
///
/// fn agent_setup(ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext>) -> HookResult {
///     install_app(ctx, Path::new("path/to/your/happ").to_path_buf(), &"your_role_name".to_string())?;
///     Ok(())
/// }
/// ```
///
/// After calling this function you will be able to use the `installed_app_id`, `cell_id` and `app_agent_client` in your agent hooks:
/// ```rust
/// use holochain_wind_tunnel_runner::prelude::{HolochainAgentContext, HolochainRunnerContext, AgentContext, HookResult};
///
/// fn agent_behaviour(ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext>) -> HookResult {
///     let installed_app_id = ctx.get().installed_app_id()?;
///     let cell_id = ctx.get().cell_id();
///     let app_agent_client = ctx.get().app_client();
///
///     Ok(())
/// }
/// ```
///
/// Method:
/// - Connects to an admin port using the connection string from the runner context.
/// - Generates an agent public key.
/// - Installs the app using the provided `app_path` and the agent public key.
/// - Enables the app.
/// - Authorizes signing credentials.
/// - Connects to the app websocket.
/// - Sets the `installed_app_id`, `cell_id` and `app_agent_client` values in [HolochainAgentContext].
pub fn install_app<SV>(
    ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext<SV>>,
    app_path: PathBuf,
    role_name: &RoleName,
) -> WindTunnelResult<()>
where
    SV: UserValuesConstraint,
{
    let admin_ws_url = ctx.runner_context().get_connection_string().to_string();
    let app_ws_url = ctx.runner_context().get().app_ws_url();
    let installed_app_id = installed_app_id_for_agent(ctx);
    let reporter = ctx.runner_context().reporter();
    let run_id = ctx.runner_context().get_run_id().to_string();

    let (installed_app_id, cell_id, app_client) = ctx
        .runner_context()
        .executor()
        .execute_in_place(async move {
            log::debug!("Connecting a Holochain admin client: {}", admin_ws_url);
            let client = AdminWebsocket::connect(admin_ws_url, reporter.clone()).await?;

            let key = client
                .generate_agent_pub_key()
                .await
                .map_err(handle_api_err)?;
            log::debug!("Generated agent pub key: {:}", key);

            let content = std::fs::read(app_path)?;

            let app_info = client
                .install_app(InstallAppPayload {
                    source: AppBundleSource::Bytes(content),
                    agent_key: Some(key),
                    installed_app_id: Some(installed_app_id.clone()),
                    roles_settings: None,
                    network_seed: Some(run_id),
                    ignore_genesis_failure: false,
                    allow_throwaway_random_agent_key: false,
                })
                .await
                .map_err(handle_api_err)?;
            log::debug!("Installed app: {:}", installed_app_id);

            client
                .enable_app(installed_app_id.clone())
                .await
                .map_err(handle_api_err)?;
            log::debug!("Enabled app: {:}", installed_app_id);

            let cell_id = get_cell_id_for_role_name(&app_info, role_name)?;
            log::debug!("Got cell id: {:}", cell_id);

            let credentials = client
                .authorize_signing_credentials(AuthorizeSigningCredentialsPayload {
                    cell_id: cell_id.clone(),
                    functions: None,
                })
                .await?;
            log::debug!("Authorized signing credentials");

            let signer = ClientAgentSigner::default();
            signer.add_credentials(cell_id.clone(), credentials);

            let issued = client
                .issue_app_auth_token(installed_app_id.clone().into())
                .await
                .map_err(|e| {
                    anyhow::anyhow!("Could not issue auth token for app client: {:?}", e)
                })?;

            let app_client =
                AppWebsocket::connect(app_ws_url, issued.token, signer.into(), reporter).await?;

            Ok((installed_app_id, cell_id, app_client))
        })
        .context("Failed to install app")?;

    ctx.get_mut().installed_app_id = Some(installed_app_id);
    ctx.get_mut().cell_id = Some(cell_id);
    ctx.get_mut().app_client = Some(app_client);

    Ok(())
}
/// Used an installed app as though it had been installed by [install_app].
///
/// It doesn't matter whether the app was installed by [install_app], but if it wasn't then it is
/// your responsibility to make sure the naming expectations are met. Namely, the app is installed
/// under `<agent_name>-app`.
///
/// Once this function has run, you should be able to use any functions that would normally use the
/// outputs of [install_app]. This makes it a useful drop-in if you want to run further code against
/// an installed app after a scenario has finished.
///
/// Call this function as follows:
/// ```rust
/// use std::path::Path;
/// use holochain_wind_tunnel_runner::prelude::{HolochainAgentContext, HolochainRunnerContext, use_installed_app, AgentContext, HookResult};
///
/// fn agent_setup(ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext>) -> HookResult {
///     use_installed_app(ctx, &"your_role_name".to_string())?;
///     Ok(())
/// }
/// ```
///
/// Method:
/// - Connects to an admin port using the connection string from the runner context.
/// - Generates the expected installed_app_id for this agent.
/// - Gets a list of installed apps and tries to find the matching one by app id.
/// - If the app is not found, or is not in the Running state, then error.
/// - Authorizes signing credentials.
/// - Connects to the app websocket.
/// - Sets the `installed_app_id`, `cell_id` and `app_agent_client` values in [HolochainAgentContext].
pub fn use_installed_app<SV>(
    ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext<SV>>,
    role_name: &RoleName,
) -> HookResult
where
    SV: UserValuesConstraint,
{
    let admin_ws_url = ctx.runner_context().get_connection_string().to_string();
    let app_ws_url = ctx.runner_context().get().app_ws_url();
    let reporter = ctx.runner_context().reporter();
    let installed_app_id = installed_app_id_for_agent(ctx);

    let (installed_app_id, cell_id, app_client) = ctx
        .runner_context()
        .executor()
        .execute_in_place(async move {
            let client = AdminWebsocket::connect(admin_ws_url, reporter.clone()).await?;

            let app_infos = client.list_apps(None).await.map_err(handle_api_err)?;
            let app_info = app_infos
                .into_iter()
                .find(|app_info| app_info.installed_app_id == installed_app_id)
                .ok_or(anyhow::anyhow!("App not found: {installed_app_id:?}"))?;

            if app_info.status != AppInfoStatus::Running {
                anyhow::bail!("App is not running: {installed_app_id:?}");
            }

            let cell_id = get_cell_id_for_role_name(&app_info, role_name)?;

            let credentials = client
                .authorize_signing_credentials(AuthorizeSigningCredentialsPayload {
                    cell_id: cell_id.clone(),
                    functions: None,
                })
                .await?;

            let signer = ClientAgentSigner::default();
            signer.add_credentials(cell_id.clone(), credentials);

            let issued = client
                .issue_app_auth_token(installed_app_id.clone().into())
                .await
                .map_err(|e| {
                    anyhow::anyhow!("Could not issue auth token for app client: {:?}", e)
                })?;

            let app_client =
                AppWebsocket::connect(app_ws_url, issued.token, signer.into(), reporter).await?;

            Ok((installed_app_id, cell_id, app_client))
        })?;

    ctx.get_mut().installed_app_id = Some(installed_app_id);
    ctx.get_mut().cell_id = Some(cell_id);
    ctx.get_mut().app_client = Some(app_client);

    Ok(())
}

/// Tries to wait for a minimum number of agents to be discovered.
///
/// If you call this function in your agent setup, then the scenario will become configurable using
/// the `MIN_AGENTS` environment variable. The default value is 2.
///
/// Note that the number of agents seen by each node includes itself. So having two conductors with
/// one agent on each, means that each node will immediately see one agent after app installation.
///
/// Example:
/// ```rust
/// use std::path::Path;
/// use std::time::Duration;
/// use holochain_wind_tunnel_runner::prelude::{HolochainAgentContext, HolochainRunnerContext, AgentContext, HookResult, install_app, try_wait_for_min_agents};
///
/// fn agent_setup(ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext>) -> HookResult {
///     install_app(ctx, Path::new("path/to/your/happ").to_path_buf(), &"your_role_name".to_string())?;
///     try_wait_for_min_agents(ctx, Duration::from_secs(60))?;
///     Ok(())
/// }
/// ```
///
/// Note that if no apps have been installed, you are waiting for too many agents, or anything else
/// prevents enough agents being discovered then the function will wait up to the `wait_for` duration
/// before continuing. It will not fail if too few agents were discovered.
///
/// Note that the smallest resolution is 1s. This is because the function will sleep between
/// querying agents from the conductor. You could probably not use this function for performance
/// testing peer discovery!
pub fn try_wait_for_min_agents<SV>(
    ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext<SV>>,
    wait_for: Duration,
) -> HookResult
where
    SV: UserValuesConstraint,
{
    let admin_ws_url = ctx.runner_context().get_connection_string().to_string();
    let reporter = ctx.runner_context().reporter();
    let agent_name = ctx.agent_name().to_string();

    let min_agents = std::env::var("MIN_AGENTS")
        .ok()
        .map(|s| s.parse().expect("MIN_AGENTS must be a number"))
        .unwrap_or(2);

    ctx.runner_context()
        .executor()
        .execute_in_place(async move {
            let client = AdminWebsocket::connect(admin_ws_url, reporter.clone()).await?;

            let start_discovery = Instant::now();
            for _ in 0..wait_for.as_secs() {
                let agent_list = client.agent_info(None).await?;

                if agent_list.len() >= min_agents {
                    break;
                }

                tokio::time::sleep(Duration::from_secs(1)).await;
            }

            println!(
                "Discovery for agent {} took: {}s",
                agent_name,
                start_discovery.elapsed().as_secs()
            );

            Ok(())
        })?;

    Ok(())
}

/// Uninstall an application. Intended to be used by scenarios that clean up after themselves or
/// need to uninstall and re-install the same application.
///
/// Requires:
/// - Either you provide the `installed_app_id` or the [HolochainAgentContext] must have an `installed_app_id`.
///   Note that this means that when passing `None`, only the last app that was installed using [install_app] will be uninstalled.
///
/// Call this function as follows:
/// ```rust
/// use std::path::Path;
/// use holochain_wind_tunnel_runner::prelude::{HolochainAgentContext, HolochainRunnerContext, uninstall_app, AgentContext, HookResult};
///
/// fn agent_teardown(ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext>) -> HookResult {
///     uninstall_app(ctx, None)?;
///     Ok(())
/// }
/// ```
///
/// Or if you are uninstalling in the agent behaviour and in the teardown:
/// ```rust
/// use std::path::Path;
/// use holochain_wind_tunnel_runner::prelude::{HolochainAgentContext, HolochainRunnerContext, uninstall_app, AgentContext, HookResult, install_app};
///
/// fn agent_behaviour(ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext>) -> HookResult {
///    install_app(ctx, Path::new("path/to/your/happ").to_path_buf(), &"your_role_name".to_string())?;
///    uninstall_app(ctx, None)?;
///    Ok(())
/// }
///
/// fn agent_teardown(ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext>) -> HookResult {
///     // The app may have already been uninstalled if the scenario stopped after uninstalling the app but the agent behaviour is
///     // not guaranteed to complete so we don't error when uninstalling here.
///     uninstall_app(ctx, None).ok();
///     Ok(())
/// }
/// ```
///
/// Method:
/// - Either uses the provided `installed_app_id` or gets the `installed_app_id` from the agent context.
/// - Connects to an admin port using the connection string from the runner context.
/// - Uninstalls the specified app and returns the result.
pub fn uninstall_app<SV>(
    ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext<SV>>,
    installed_app_id: Option<InstalledAppId>,
) -> HookResult
where
    SV: UserValuesConstraint,
{
    let admin_ws_url = ctx.runner_context().get_connection_string().to_string();

    let installed_app_id = installed_app_id.or_else(|| ctx.get().installed_app_id().ok());
    if installed_app_id.is_none() {
        // If there is no installed app id, we can't uninstall anything
        log::info!("No installed app id found, skipping uninstall");
        return Ok(());
    }

    let reporter = ctx.runner_context().reporter();

    ctx.runner_context()
        .executor()
        .execute_in_place(async move {
            let admin_client = AdminWebsocket::connect(admin_ws_url, reporter).await?;

            admin_client
                .uninstall_app(installed_app_id.unwrap())
                .await
                .map_err(handle_api_err)?;
            Ok(())
        })?;

    Ok(())
}

/// Calls a zome function on the cell specified in `ctx.get().cell_id()`.
///
/// Requires:
/// - The [HolochainAgentContext] to have a valid `cell_id`. Consider calling [install_app] in your setup before using this function.
/// - The [HolochainAgentContext] to have a valid `app_agent_client`. Consider calling [install_app] in your setup before using this function.
///
/// Call this function as follows:
/// ```rust
/// use holochain_types::prelude::ActionHash;
/// use holochain_wind_tunnel_runner::prelude::{call_zome, HolochainAgentContext, HolochainRunnerContext, AgentContext, HookResult};
///
/// fn agent_behaviour(ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext>) -> HookResult {
///     // Return type determined by why you assign the result to
///     let action_hash: ActionHash = call_zome(
///         ctx,
///         "crud", // zome name
///         "create_sample_entry", // function name
///         "this is a test entry value" // payload
///     )?;
///
///     Ok(())
/// }
/// ```
///
/// Method:
/// - Gets the `cell_id` and `app_agent_client` from the context.
/// - Tries to serialize the input payload.
/// - Calls the zome function using the `app_agent_client`.
/// - Tries to deserialize and return the response.
pub fn call_zome<I, O, SV>(
    ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext<SV>>,
    zome_name: &str,
    fn_name: &str,
    payload: I,
) -> anyhow::Result<O>
where
    O: std::fmt::Debug + serde::de::DeserializeOwned,
    I: serde::Serialize + std::fmt::Debug,
    SV: UserValuesConstraint,
{
    let cell_id = ctx.get().cell_id();
    let app_agent_client = ctx.get().app_client();
    ctx.runner_context().executor().execute_in_place(async {
        let result = app_agent_client
            .call_zome(
                cell_id.into(),
                zome_name,
                fn_name,
                ExternIO::encode(payload).context("Encoding failure")?,
            )
            .await
            .map_err(handle_api_err)?;

        result
            .decode()
            .map_err(|e| anyhow::anyhow!("Decoding failure: {:?}", e))
    })
}

/// Get a randomized list of peers connected to the conductor in the `ctx` for a given cell.
///
/// Requires:
/// - The [HolochainAgentContext] to have a valid `cell_id`. Consider calling [install_app] in your setup before using this function.
///
/// Call this function as follows:
/// ```rust
/// use holochain_types::prelude::ActionHash;
/// use holochain_wind_tunnel_runner::prelude::{call_zome, HolochainAgentContext, HolochainRunnerContext, AgentContext, HookResult, get_peer_list_randomized};
///
/// fn agent_behaviour(ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext>) -> HookResult {
///     let peer_list = get_peer_list_randomized(ctx)?;
///     println!("Connected peers: {:?}", peer_list);
///     Ok(())
/// }
/// ```
///
/// Method:
/// - calls `app_agent_info` on the websocket.
/// - filters out self
/// - shuffles the list
pub fn get_peer_list_randomized<SV>(
    ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext<SV>>,
) -> WindTunnelResult<Vec<AgentPubKey>>
where
    SV: UserValuesConstraint,
{
    let cell_id = ctx.get().cell_id();
    let reporter: Arc<Reporter> = ctx.runner_context().reporter();
    let admin_ws_url = ctx.runner_context().get_connection_string().to_string();

    ctx.runner_context()
        .executor()
        .execute_in_place(async move {
            let admin_client = AdminWebsocket::connect(admin_ws_url, reporter).await?;
            // No more agents available to signal, get a new list.
            // This is also the initial condition.
            let agent_infos_encoded = admin_client
                .agent_info(Some(cell_id.clone()))
                .await
                .context("Failed to get agent info")?;
            let mut agent_infos = Vec::new();
            for info in agent_infos_encoded {
                let a = AgentInfoSigned::decode(&Ed25519Verifier, info.as_bytes())?;
                agent_infos.push(AgentPubKey::from_k2_agent(&a.agent))
            }
            let mut peer_list = agent_infos
                .into_iter()
                .filter(|k| k != cell_id.agent_pubkey()) // Don't include ourselves!
                .collect::<Vec<_>>();
            peer_list.shuffle(&mut thread_rng());
            Ok(peer_list)
        })
}

pub(crate) fn installed_app_id_for_agent<SV>(
    ctx: &mut AgentContext<HolochainRunnerContext, HolochainAgentContext<SV>>,
) -> String
where
    SV: UserValuesConstraint,
{
    let agent_name = ctx.agent_name().to_string();
    format!("{}-app", agent_name).to_string()
}

pub(crate) fn get_cell_id_for_role_name(
    app_info: &AppInfo,
    role_name: &RoleName,
) -> anyhow::Result<CellId> {
    match app_info
        .cell_info
        .get(role_name)
        .ok_or(anyhow::anyhow!("Role not found"))?
        .first()
        .ok_or(anyhow::anyhow!("Cell not found"))?
    {
        CellInfo::Provisioned(c) => Ok(c.cell_id.clone()),
        _ => anyhow::bail!("Cell not provisioned"),
    }
}
