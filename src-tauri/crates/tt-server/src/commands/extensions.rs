//! Page-extensible capabilities: third-party frontend extensions, the extension key/value store,
//! Agent Skills and MCP server registrations.
//!
//! Each arm calls the same shared service as the native command of the same name. The browser
//! has no filesystem of its own, so Skill imports that name a server path are confined to the
//! host staging area (archives) or refused (directories).

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::Serialize;
use serde_json::Value;
use tt_application::dto::mcp_dto::{
    CallLegacyMcpToolDto, CreateMcpServerDto, McpExecutionCallIdDto, McpRegistrationIdDto,
    McpTestCallIdDto, SetMcpServerStateDto, SetMcpToolDescriptionOverrideDto,
    SetMcpToolPermissionDto, TestMcpToolCallDto, UpdateMcpServerDto,
};
use tt_contracts::extension_store::EntryKind;
use tt_domain::models::skill::{
    DEFAULT_SKILL_READ_FALLBACK_MAX_CHARS, SkillImportInput, SkillInstallRequest, SkillMoveRequest,
    SkillReadRequest, SkillScope, SkillScopeRetargetRequest, SkillWriteRequest,
};
use tt_runtime::CoreServices;

use super::args::Args;
use super::{json, unsupported};
use crate::error::CommandError;
use crate::host::Host;

const COMMANDS: &[&str] = &[
    "get_extension_version",
    "get_extension_branches",
    "install_extension",
    "update_extension",
    "delete_extension",
    "move_extension",
    "switch_extension_branch",
    "list_extension_store_keys",
    "list_extension_store_tables",
    "rename_extension_store_key",
    "delete_extension_store_entry",
    "delete_extension_store_table",
    "list_skills",
    "list_skill_files",
    "read_skill_file",
    "write_skill_file",
    "delete_skill",
    "move_skill",
    "retarget_skill_scope",
    "export_skill",
    "discover_skill_imports",
    "preview_skill_import",
    "install_skill_import",
    "discard_skill_import_archive",
    "download_skill_import_url",
    "list_mcp_servers",
    "create_mcp_server",
    "update_mcp_server",
    "remove_mcp_server",
    "set_mcp_server_state",
    "discover_mcp_tools",
    "refresh_mcp_tools",
    "set_mcp_tool_permission",
    "set_mcp_tool_description_override",
    "list_legacy_mcp_tools",
    "call_legacy_mcp_tool",
    "start_legacy_mcp_tool_call",
    "cancel_legacy_mcp_tool_call",
    "start_mcp_test_call",
    "cancel_mcp_test_call",
    "test_mcp_tool_call",
];

pub(super) fn handles(command: &str) -> bool {
    COMMANDS.contains(&command)
}

/// Wire shape of the native `export_skill` response.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SkillExportPayload {
    file_name: String,
    content_base64: String,
    sha256: String,
}

/// Skill imports that reference server paths must stay inside the host staging area.
///
/// A directory import names an arbitrary server directory; the browser cannot pick one, so it is
/// refused. An archive import must be a file the page staged. The page's path string is kept
/// unchanged because the shared repository keys discovered archives by it.
async fn confined_import(
    host: &Host,
    input: SkillImportInput,
) -> Result<SkillImportInput, CommandError> {
    match &input {
        SkillImportInput::Directory { .. } => Err(CommandError::BadRequest(
            "Skill directory import is not available in the browser host".into(),
        )),
        SkillImportInput::ArchiveFile { path, .. } => {
            host.files.validate_staged_path(path).await?;
            Ok(input)
        }
        SkillImportInput::InlineFiles { .. } | SkillImportInput::ArchiveBase64 { .. } => Ok(input),
    }
}

pub(super) async fn dispatch(
    host: &Host,
    s: &CoreServices,
    command: &str,
    a: &Args,
) -> Result<Value, CommandError> {
    match command {
        // Extensions. Install/update/switch fetch a git repository and write its JavaScript into
        // the extensions directory, which this server then serves to every page.
        "get_extension_version" => json(
            s.extension_service
                .get_extension_version(
                    &a.required::<String>("extension_name")?,
                    a.required("global")?,
                )
                .await?,
        ),
        "get_extension_branches" => json(
            s.extension_service
                .get_extension_branches(
                    &a.required::<String>("extension_name")?,
                    a.required("global")?,
                )
                .await?,
        ),
        "install_extension" => json(
            s.extension_service
                .install_extension(
                    &a.required::<String>("url")?,
                    a.required("global")?,
                    a.optional("branch")?,
                )
                .await?,
        ),
        "update_extension" => json(
            s.extension_service
                .update_extension(
                    &a.required::<String>("extension_name")?,
                    a.required("global")?,
                )
                .await?,
        ),
        "delete_extension" => json(
            s.extension_service
                .delete_extension(
                    &a.required::<String>("extension_name")?,
                    a.required("global")?,
                )
                .await?,
        ),
        "move_extension" => json(
            s.extension_service
                .move_extension(
                    &a.required::<String>("extension_name")?,
                    &a.required::<String>("source")?,
                    &a.required::<String>("destination")?,
                )
                .await?,
        ),
        "switch_extension_branch" => json(
            s.extension_service
                .switch_extension_branch(
                    &a.required::<String>("extension_name")?,
                    &a.required::<String>("branch")?,
                    a.required("global")?,
                )
                .await?,
        ),

        // Extension store (byte open/commit transport lives in `resources`).
        "list_extension_store_keys" => json(
            s.extension_store_service
                .list_keys(
                    &a.required::<String>("namespace")?,
                    a.optional::<String>("table")?.as_deref(),
                    a.required::<EntryKind>("kind")?,
                )
                .await?,
        ),
        "list_extension_store_tables" => json(
            s.extension_store_service
                .list_tables(&a.required::<String>("namespace")?)
                .await?,
        ),
        "rename_extension_store_key" => json(
            s.extension_store_service
                .rename_json_key(
                    &a.required::<String>("namespace")?,
                    a.optional::<String>("table")?.as_deref(),
                    &a.required::<String>("key")?,
                    &a.required::<String>("new_key")?,
                )
                .await?,
        ),
        "delete_extension_store_entry" => json(
            s.extension_store_service
                .delete_entry(
                    &a.required::<String>("namespace")?,
                    a.optional::<String>("table")?.as_deref(),
                    &a.required::<String>("key")?,
                    a.required::<EntryKind>("kind")?,
                )
                .await?,
        ),
        "delete_extension_store_table" => json(
            s.extension_store_service
                .delete_table(
                    &a.required::<String>("namespace")?,
                    &a.required::<String>("table")?,
                )
                .await?,
        ),

        // Agent Skills. Skill packages may contain scripts; they are stored as workspace files and
        // only run inside the agent workspace shell, never as server processes.
        "list_skills" => json(s.skill_service.list_skills(a.default("scope")?).await?),
        "list_skill_files" => json(
            s.skill_service
                .list_skill_files(
                    a.default::<SkillScope>("scope")?,
                    &a.required::<String>("name")?,
                )
                .await?,
        ),
        "read_skill_file" => json(
            s.skill_service
                .read_skill_file(SkillReadRequest {
                    scope: a.default("scope")?,
                    name: a.required("name")?,
                    path: a.required("path")?,
                    start_line: a.optional("start_line")?,
                    line_count: a.optional("line_count")?,
                    max_output_chars: DEFAULT_SKILL_READ_FALLBACK_MAX_CHARS,
                })
                .await?,
        ),
        "write_skill_file" => json(
            s.skill_service
                .write_skill_file(SkillWriteRequest {
                    scope: a.default("scope")?,
                    name: a.required("name")?,
                    path: a.required("path")?,
                    content: a.required("content")?,
                    expected_sha256: a.optional("expected_sha256")?,
                })
                .await?,
        ),
        "delete_skill" => json(
            s.skill_service
                .delete_skill(
                    a.default::<SkillScope>("scope")?,
                    &a.required::<String>("name")?,
                )
                .await?,
        ),
        "move_skill" => json(
            s.skill_service
                .move_skill(a.required::<SkillMoveRequest>("request")?)
                .await?,
        ),
        "retarget_skill_scope" => json(
            s.skill_service
                .retarget_scope(a.required::<SkillScopeRetargetRequest>("request")?)
                .await?,
        ),
        "export_skill" => {
            let exported = s
                .skill_service
                .export_skill(
                    a.default::<SkillScope>("scope")?,
                    &a.required::<String>("name")?,
                )
                .await?;
            json(SkillExportPayload {
                file_name: exported.file_name,
                content_base64: BASE64_STANDARD.encode(exported.bytes),
                sha256: exported.sha256,
            })
        }
        "discover_skill_imports" => {
            let input = confined_import(host, a.required("input")?).await?;
            json(s.skill_service.discover_imports(input).await?)
        }
        "preview_skill_import" => {
            let input = confined_import(host, a.required("input")?).await?;
            json(
                s.skill_service
                    .preview_import(input, a.default("target_scope")?)
                    .await?,
            )
        }
        "install_skill_import" => {
            let mut request: SkillInstallRequest = a.required("request")?;
            request.input = confined_import(host, request.input).await?;
            json(s.skill_service.install_import(request).await?)
        }
        // Only releases the repository's in-memory extraction keyed by this string; it touches
        // no page-named path, and must still work after the staged archive itself is gone.
        "discard_skill_import_archive" => json(
            s.skill_service
                .discard_import_archive(&a.required::<String>("path")?)
                .await?,
        ),
        "download_skill_import_url" => json(
            s.skill_service
                .download_import_url(&a.required::<String>("url")?)
                .await?,
        ),

        // MCP. Registrations are Streamable HTTP endpoints only (`McpEndpoint` accepts http/https);
        // no transport spawns a local process.
        "list_mcp_servers" => json(s.mcp_service.list_servers().await?),
        "create_mcp_server" => {
            let dto: CreateMcpServerDto = a.required("dto")?;
            json(
                s.mcp_service
                    .create_server(
                        dto.display_name,
                        dto.endpoint,
                        dto.headers,
                        dto.protocol_version,
                    )
                    .await?,
            )
        }
        "update_mcp_server" => {
            let dto: UpdateMcpServerDto = a.required("dto")?;
            json(
                s.mcp_service
                    .update_server(
                        &dto.registration_id,
                        dto.display_name,
                        dto.endpoint,
                        dto.headers,
                        dto.protocol_version,
                    )
                    .await?,
            )
        }
        "set_mcp_server_state" => {
            let dto: SetMcpServerStateDto = a.required("dto")?;
            json(
                s.mcp_service
                    .set_server_state(&dto.registration_id, dto.state)
                    .await?,
            )
        }
        "remove_mcp_server" => {
            let dto: McpRegistrationIdDto = a.required("dto")?;
            json(s.mcp_service.remove_server(&dto.registration_id).await?)
        }
        "discover_mcp_tools" => {
            let dto: McpRegistrationIdDto = a.required("dto")?;
            json(s.mcp_service.discover_tools(&dto.registration_id).await?)
        }
        "refresh_mcp_tools" => {
            let dto: McpRegistrationIdDto = a.required("dto")?;
            json(s.mcp_service.refresh_tools(&dto.registration_id).await?)
        }
        "set_mcp_tool_permission" => {
            let dto: SetMcpToolPermissionDto = a.required("dto")?;
            json(
                s.mcp_service
                    .set_tool_permission(&dto.registration_id, dto.native_name, dto.permission)
                    .await?,
            )
        }
        "set_mcp_tool_description_override" => {
            let dto: SetMcpToolDescriptionOverrideDto = a.required("dto")?;
            json(
                s.mcp_service
                    .set_tool_description_override(
                        &dto.registration_id,
                        dto.native_name,
                        dto.override_,
                    )
                    .await?,
            )
        }
        "start_mcp_test_call" => {
            let dto: McpTestCallIdDto = a.required("dto")?;
            json(s.mcp_service.start_call(&dto.call_id).await?)
        }
        "test_mcp_tool_call" => {
            let dto: TestMcpToolCallDto = a.required("dto")?;
            json(
                s.mcp_service
                    .test_call(
                        &dto.call_id,
                        &dto.registration_id,
                        dto.native_name,
                        dto.arguments_json,
                    )
                    .await?,
            )
        }
        "cancel_mcp_test_call" => {
            let dto: McpTestCallIdDto = a.required("dto")?;
            json(s.mcp_service.cancel_call(&dto.call_id).await?)
        }
        "list_legacy_mcp_tools" => json(s.mcp_service.list_legacy_tools_cached().await?),
        "start_legacy_mcp_tool_call" => {
            let dto: McpExecutionCallIdDto = a.required("dto")?;
            json(s.mcp_service.start_call(&dto.execution_call_id).await?)
        }
        "call_legacy_mcp_tool" => {
            let dto: CallLegacyMcpToolDto = a.required("dto")?;
            json(
                s.mcp_service
                    .call_legacy_tool(&dto.execution_call_id, &dto.tool_id, dto.arguments_json)
                    .await?,
            )
        }
        "cancel_legacy_mcp_tool_call" => {
            let dto: McpExecutionCallIdDto = a.required("dto")?;
            json(s.mcp_service.cancel_call(&dto.execution_call_id).await?)
        }
        _ => Err(unsupported(command)),
    }
}
