use std::sync::Arc;

use crate::dto::preset_dto::SavePresetResponseDto;
use crate::errors::ApplicationError;
use crate::services::skill_service::SkillService;
use tt_domain::errors::DomainError;
use tt_domain::models::preset::{DefaultPreset, Preset, PresetType};
use tt_ports::repositories::preset_repository::PresetRepository;

/// Service for managing presets
pub struct PresetService {
    preset_repository: Arc<dyn PresetRepository>,
}

impl PresetService {
    /// Create a new PresetService
    pub fn new(preset_repository: Arc<dyn PresetRepository>) -> Self {
        Self { preset_repository }
    }

    /// Save a preset
    ///
    /// # Arguments
    ///
    /// * `preset` - The preset to save
    ///
    /// # Returns
    ///
    /// * `Result<(), DomainError>` - Success or error
    pub async fn save_preset(&self, preset: &Preset) -> Result<(), DomainError> {
        tracing::debug!(
            "Saving preset: {} (type: {})",
            preset.name,
            preset.preset_type
        );

        preset.validate().map_err(DomainError::InvalidData)?;

        // Save the preset
        self.preset_repository.save_preset(preset).await?;

        tracing::debug!("Preset saved successfully: {}", preset.name);
        Ok(())
    }

    /// Create and persist a preset supplied by a command.
    pub async fn save_preset_data(
        &self,
        name: String,
        api_id: &str,
        data: serde_json::Value,
    ) -> Result<SavePresetResponseDto, DomainError> {
        let preset = self.create_preset(name, api_id, data)?;
        self.save_preset(&preset).await?;
        Ok(SavePresetResponseDto::new(preset.name))
    }

    /// Delete a preset and its linked skills. Cleanup failure is returned after deletion commits.
    ///
    /// # Arguments
    ///
    /// * `name` - Name of the preset to delete
    /// * `preset_type` - Type of the preset
    ///
    /// # Returns
    ///
    /// * `Result<(), ApplicationError>` - Success or deletion/linked-skill error
    pub async fn delete_preset(
        &self,
        name: &str,
        preset_type: &PresetType,
        skill_service: &SkillService,
    ) -> Result<(), ApplicationError> {
        tracing::debug!("Deleting preset: {} (type: {})", name, preset_type);
        if name.trim().is_empty() {
            return Err(ApplicationError::ValidationError(
                "Preset name cannot be empty".to_string(),
            ));
        }

        if !self
            .preset_repository
            .preset_exists(name, preset_type)
            .await?
        {
            return Err(ApplicationError::NotFound(format!(
                "Preset not found: {}",
                name
            )));
        }

        // Delete the preset
        self.preset_repository
            .delete_preset(name, preset_type)
            .await?;

        let deleted_skills = skill_service
            .delete_skills_for_source(
                "preset",
                &format!("preset:{}:{}", preset_type.to_api_id().trim(), name.trim()),
            )
            .await
            .inspect_err(|error| {
                tracing::error!(
                    target: tt_contracts::observability::USER_VISIBLE_ERROR,
                    "Deleted preset '{}' but could not delete its linked Agent Skills: {}",
                    name,
                    error
                );
            })?;
        if !deleted_skills.is_empty() {
            tracing::debug!(
                "Deleted {} Agent Skill(s) linked to preset '{}': {}",
                deleted_skills.len(),
                name,
                deleted_skills.join(", ")
            );
        }
        tracing::debug!("Preset deleted successfully: {}", name);
        Ok(())
    }

    /// Get a preset by name and type
    ///
    /// # Arguments
    ///
    /// * `name` - Name of the preset
    /// * `preset_type` - Type of the preset
    ///
    /// # Returns
    ///
    /// * `Result<Option<Preset>, DomainError>` - The preset if found, None otherwise
    pub async fn get_preset(
        &self,
        name: &str,
        preset_type: &PresetType,
    ) -> Result<Option<Preset>, DomainError> {
        tracing::debug!("Getting preset: {} (type: {})", name, preset_type);

        let preset = self.preset_repository.get_preset(name, preset_type).await?;

        if preset.is_some() {
            tracing::debug!("Preset found: {}", name);
        } else {
            tracing::debug!("Preset not found: {}", name);
        }

        Ok(preset)
    }

    /// List all presets of a specific type
    ///
    /// # Arguments
    ///
    /// * `preset_type` - Type of presets to list
    ///
    /// # Returns
    ///
    /// * `Result<Vec<String>, DomainError>` - List of preset names
    pub async fn list_presets(&self, preset_type: &PresetType) -> Result<Vec<String>, DomainError> {
        tracing::debug!("Listing presets of type: {}", preset_type);

        let presets = self.preset_repository.list_presets(preset_type).await?;

        tracing::debug!("Found {} presets of type {}", presets.len(), preset_type);

        Ok(presets)
    }

    /// Restore a default preset
    ///
    /// # Arguments
    ///
    /// * `name` - Name of the preset to restore
    /// * `preset_type` - Type of the preset
    ///
    /// # Returns
    ///
    /// * `Result<Option<DefaultPreset>, DomainError>` - The default preset if found, None otherwise
    pub async fn restore_default_preset(
        &self,
        name: &str,
        preset_type: &PresetType,
    ) -> Result<Option<DefaultPreset>, DomainError> {
        tracing::debug!("Restoring default preset: {} (type: {})", name, preset_type);

        let default_preset = self
            .preset_repository
            .get_default_preset(name, preset_type)
            .await?;

        if default_preset.is_some() {
            tracing::debug!("Default preset found for restoration: {}", name);
        } else {
            tracing::debug!("Default preset not found: {}", name);
        }

        Ok(default_preset)
    }

    /// Check if a preset exists
    ///
    /// # Arguments
    ///
    /// * `name` - Name of the preset
    /// * `preset_type` - Type of the preset
    ///
    /// # Returns
    ///
    /// * `Result<bool, DomainError>` - True if preset exists, false otherwise
    pub async fn preset_exists(
        &self,
        name: &str,
        preset_type: &PresetType,
    ) -> Result<bool, DomainError> {
        tracing::debug!(
            "Checking if preset exists: {} (type: {})",
            name,
            preset_type
        );

        let exists = self
            .preset_repository
            .preset_exists(name, preset_type)
            .await?;

        tracing::debug!("Preset {} exists: {}", name, exists);

        Ok(exists)
    }

    /// Create a preset from raw data
    ///
    /// # Arguments
    ///
    /// * `name` - Name of the preset
    /// * `api_id` - API ID string
    /// * `data` - Preset data as JSON value
    ///
    /// # Returns
    ///
    /// * `Result<Preset, DomainError>` - The created preset
    pub fn create_preset(
        &self,
        name: String,
        api_id: &str,
        data: serde_json::Value,
    ) -> Result<Preset, DomainError> {
        tracing::debug!("Creating preset: {} (api_id: {})", name, api_id);

        let preset_type = PresetType::from_api_id(api_id)
            .ok_or_else(|| DomainError::InvalidData(format!("Unknown API ID: {}", api_id)))?;

        let preset = Preset::new(name, preset_type, data);

        preset.validate().map_err(DomainError::InvalidData)?;

        tracing::debug!("Preset created successfully: {}", preset.name);

        Ok(preset)
    }
}
