//! Agent profiles, runs, run history and Agent Sessions.
//!
//! Each arm calls the same shared service as the native command of the same name
//! (`agent_commands.rs` / `agent_session_commands.rs`), with the same `dto` argument and
//! response shape. `subscribe_agent_run_live_projection` needs a server-pushed channel and is
//! deliberately not handled here.

use serde_json::Value;
use tt_application::dto::agent_dto::{
    AgentApplyCurrentModelConnectionSnapshotDto, AgentApplyCurrentModelConnectionSnapshotResultDto,
    AgentBuildCurrentModelConnectionSnapshotDto, AgentBuildCurrentModelConnectionSnapshotResultDto,
    AgentCopyChatPersistentStatesDto, AgentListProfilesResultDto, AgentListToolsDto,
    AgentLoadProfileResultDto, AgentPreparePromptAssemblyDto, AgentProfileIdDto,
    AgentRepairProfileFileDto, AgentResolveSystemPromptDto, AgentResolveSystemPromptResultDto,
    AgentRetargetPresetRefsDto, AgentRetargetPresetRefsResultDto, AgentSaveProfileDto,
};
use tt_application::services::agent_workspace_lifecycle_service::AgentChatWorkspaceTarget;
use tt_domain::models::agent::AgentInvocationExitPolicy;
use tt_runtime::CoreServices;

use super::args::Args;
use super::{json, unsupported};
use crate::error::CommandError;
use crate::host::Host;

const COMMANDS: &[&str] = &[
    // Profiles and tool surface.
    "list_agent_profiles",
    "load_agent_profile",
    "save_agent_profile",
    "delete_agent_profile",
    "diagnose_agent_profile",
    "repair_agent_profile_file",
    "retarget_agent_profile_preset_refs",
    "list_agent_tools",
    "resolve_agent_system_prompt",
    // Prompt assembly and model connection snapshots.
    "prepare_agent_prompt_assembly",
    "read_agent_prompt_assembly_request",
    "resolve_agent_prompt_assembly",
    "build_agent_current_model_connection_snapshot",
    "apply_agent_current_model_connection_snapshot",
    // Run lifecycle.
    "start_agent_run",
    "resume_agent_run",
    "cancel_agent_run",
    "submit_agent_run_guidance",
    "finish_agent_run_presentation",
    "read_agent_run_checkpoint",
    "resolve_agent_chat_commit",
    "resolve_agent_persistent_state_metadata_update",
    // Run inspection and history.
    "list_agent_runs",
    "read_agent_run_events",
    "read_agent_task_detail",
    "read_agent_model_turn",
    "read_agent_workspace_file",
    "plan_agent_run_prune",
    "apply_agent_run_prune",
    // Chat-scoped persistent states.
    "copy_agent_chat_persistent_states",
    "prune_agent_chat_persistent_states",
    // Agent Sessions.
    "list_agent_sessions",
    "create_agent_session",
    "read_agent_session",
    "rename_agent_session",
    "delete_agent_session",
    "load_agent_session_profile",
    "save_agent_session_profile",
    "prepare_agent_session_run",
    "start_agent_session_run",
];

pub(super) fn handles(command: &str) -> bool {
    COMMANDS.contains(&command)
}

pub(super) async fn dispatch(
    _host: &Host,
    s: &CoreServices,
    command: &str,
    a: &Args,
) -> Result<Value, CommandError> {
    let runtime = &s.agent_runtime_service;
    let profiles = &s.agent_profile_service;
    let history = &s.agent_run_history_service;
    match command {
        "list_agent_profiles" => {
            let list = profiles.list_profiles().await?;
            json(AgentListProfilesResultDto {
                profiles: list.profiles,
                issues: list.issues,
            })
        }
        "load_agent_profile" => {
            let dto: AgentProfileIdDto = a.required("dto")?;
            json(AgentLoadProfileResultDto {
                profile: profiles.load_profile(&dto.profile_id).await?,
            })
        }
        "save_agent_profile" => json(
            runtime
                .save_profile(a.required::<AgentSaveProfileDto>("dto")?.profile)
                .await?,
        ),
        "delete_agent_profile" => {
            let dto: AgentProfileIdDto = a.required("dto")?;
            json(profiles.delete_profile(&dto.profile_id).await?)
        }
        "diagnose_agent_profile" => {
            let dto: AgentProfileIdDto = a.required("dto")?;
            json(
                s.agent_profile_diagnostic_service
                    .diagnose_profile(&dto.profile_id, runtime.tool_catalog())
                    .await?,
            )
        }
        "repair_agent_profile_file" => {
            let dto: AgentRepairProfileFileDto = a.required("dto")?;
            json(
                profiles
                    .repair_profile_file(&dto.profile_id, dto.action)
                    .await?,
            )
        }
        "retarget_agent_profile_preset_refs" => {
            let dto: AgentRetargetPresetRefsDto = a.required("dto")?;
            let result = profiles.retarget_preset_refs(dto.from, dto.to).await?;
            json(AgentRetargetPresetRefsResultDto {
                updated: result.profile_ids.len() + usize::from(result.session_profile_updated),
                profile_ids: result
                    .profile_ids
                    .iter()
                    .map(|id| id.as_str().to_string())
                    .collect(),
                session_profile_updated: result.session_profile_updated,
            })
        }
        "list_agent_tools" => {
            let dto = a.optional::<AgentListToolsDto>("dto")?.unwrap_or_default();
            json(runtime.tool_catalog_items(dto.context).await?)
        }
        "resolve_agent_system_prompt" => {
            let dto: AgentResolveSystemPromptDto = a.required("dto")?;
            let agent_system_prompt = runtime
                .resolve_agent_system_prompt(dto.profile_id.as_deref())
                .await?;
            json(AgentResolveSystemPromptResultDto {
                agent_system_prompt,
            })
        }

        "prepare_agent_prompt_assembly" => {
            let dto: AgentPreparePromptAssemblyDto = a.required("dto")?;
            let assembly = &s.prompt_assembly_service;
            let profile = assembly
                .resolve_profile(dto.profile_id.as_deref(), runtime.tool_catalog())
                .await?;
            let visible_tools = runtime.visible_model_tools(&profile).await?;
            json(
                assembly
                    .prepare_frontend_prompt_assembly(
                        dto,
                        profile,
                        &visible_tools,
                        AgentInvocationExitPolicy::RunFinishAllowed,
                    )
                    .await?,
            )
        }
        "read_agent_prompt_assembly_request" => json(
            runtime
                .read_prompt_assembly_request(a.required("dto")?)
                .await?,
        ),
        "resolve_agent_prompt_assembly" => {
            json(runtime.resolve_prompt_assembly(a.required("dto")?).await?)
        }
        "build_agent_current_model_connection_snapshot" => {
            let dto: AgentBuildCurrentModelConnectionSnapshotDto = a.required("dto")?;
            let current_model_connection = s
                .prompt_assembly_service
                .build_current_model_connection_snapshot(
                    &dto.settings,
                    &dto.model,
                    dto.secret_id.as_deref(),
                )?;
            json(AgentBuildCurrentModelConnectionSnapshotResultDto {
                current_model_connection,
            })
        }
        "apply_agent_current_model_connection_snapshot" => {
            let dto: AgentApplyCurrentModelConnectionSnapshotDto = a.required("dto")?;
            let settings = s
                .prompt_assembly_service
                .apply_current_model_connection_snapshot(
                    dto.settings,
                    &dto.current_model_connection,
                )?;
            json(AgentApplyCurrentModelConnectionSnapshotResultDto { settings })
        }

        "start_agent_run" => json(runtime.start_run(a.required("dto")?).await?),
        "resume_agent_run" => json(runtime.resume_run(a.required("dto")?).await?),
        "cancel_agent_run" => json(runtime.cancel_run(a.required("dto")?).await?),
        "submit_agent_run_guidance" => json(runtime.submit_guidance(a.required("dto")?).await?),
        "finish_agent_run_presentation" => {
            json(runtime.finish_run_presentation(a.required("dto")?).await?)
        }
        "read_agent_run_checkpoint" => json(runtime.read_run_checkpoint(a.required("dto")?).await?),
        "resolve_agent_chat_commit" => json(runtime.resolve_chat_commit(a.required("dto")?).await?),
        "resolve_agent_persistent_state_metadata_update" => json(
            runtime
                .resolve_persistent_state_metadata_update(a.required("dto")?)
                .await?,
        ),

        "list_agent_runs" => json(history.list_runs(a.required("dto")?).await?),
        "read_agent_run_events" => json(runtime.read_events(a.required("dto")?).await?),
        "read_agent_task_detail" => json(runtime.read_task_detail(a.required("dto")?).await?),
        "read_agent_model_turn" => json(runtime.read_model_turn(a.required("dto")?).await?),
        "read_agent_workspace_file" => json(runtime.read_workspace_file(a.required("dto")?).await?),
        "plan_agent_run_prune" => json(history.plan_run_prune(a.required("dto")?).await?),
        "apply_agent_run_prune" => json(history.apply_run_prune(a.required("dto")?).await?),

        "copy_agent_chat_persistent_states" => {
            let dto: AgentCopyChatPersistentStatesDto = a.required("dto")?;
            json(
                s.chat_service
                    .copy_agent_persistent_states(
                        &AgentChatWorkspaceTarget {
                            chat_ref: dto.source_chat_ref,
                            stable_chat_id: dto.source_stable_chat_id,
                        },
                        &AgentChatWorkspaceTarget {
                            chat_ref: dto.target_chat_ref,
                            stable_chat_id: dto.target_stable_chat_id,
                        },
                    )
                    .await?,
            )
        }
        "prune_agent_chat_persistent_states" => json(
            s.chat_service
                .prune_agent_persistent_states(a.required("dto")?)
                .await?,
        ),

        "list_agent_sessions" => json(runtime.list_sessions().await?),
        "create_agent_session" => json(runtime.create_session().await?),
        "read_agent_session" => json(runtime.read_session(a.required("dto")?).await?),
        "rename_agent_session" => json(runtime.rename_session(a.required("dto")?).await?),
        "delete_agent_session" => json(runtime.delete_session(a.required("dto")?).await?),
        "load_agent_session_profile" => json(runtime.load_session_profile().await?),
        "save_agent_session_profile" => {
            json(runtime.save_session_profile(a.required("dto")?).await?)
        }
        "prepare_agent_session_run" => json(runtime.prepare_session_run(a.required("dto")?).await?),
        "start_agent_session_run" => json(runtime.start_session_run(a.required("dto")?).await?),
        _ => Err(unsupported(command)),
    }
}
