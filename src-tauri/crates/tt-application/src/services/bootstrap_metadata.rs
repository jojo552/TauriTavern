//! Complete startup metadata shared by native and browser hosts.

use tt_domain::ios_policy::IosPolicyActivationReport;

use crate::dto::bootstrap_dto::BootstrapMetadataDto;
use crate::dto::group_dto::GroupDto;
use crate::errors::ApplicationError;

use super::avatar_service::AvatarService;
use super::character_service::CharacterService;
use super::group_service::GroupService;
use super::secret_service::SecretService;

/// Read all four domains concurrently; any failed read rejects the complete snapshot.
/// The host supplies its policy report and owns failure presentation.
pub async fn read_bootstrap_metadata(
    character_service: &CharacterService,
    group_service: &GroupService,
    avatar_service: &AvatarService,
    secret_service: &SecretService,
    ios_policy: &IosPolicyActivationReport,
) -> Result<BootstrapMetadataDto, ApplicationError> {
    let (characters, groups, avatars, secret_state) = tokio::join!(
        character_service.get_all_characters(true),
        group_service.get_all_groups(),
        avatar_service.get_avatars(),
        secret_service.read_secret_state(),
    );

    // A failed read is not an empty domain. Keep domain context and a stable error priority.
    let characters = characters.map_err(|error| {
        ApplicationError::InternalError(format!(
            "Failed to load bootstrap characters snapshot: {error}"
        ))
    })?;
    let groups = groups
        .map(|groups| groups.into_iter().map(GroupDto::from).collect())
        .map_err(|error| {
            ApplicationError::InternalError(format!(
                "Failed to load bootstrap groups snapshot: {error}"
            ))
        })?;
    let avatars = avatars.map_err(|error| {
        ApplicationError::InternalError(format!(
            "Failed to load bootstrap avatars snapshot: {error}"
        ))
    })?;
    let secret_state = secret_state.map_err(|error| {
        ApplicationError::InternalError(format!(
            "Failed to load bootstrap secret state snapshot: {error}"
        ))
    })?;

    Ok(BootstrapMetadataDto {
        characters,
        groups,
        avatars,
        secret_state,
        ios_policy: ios_policy.clone(),
    })
}
