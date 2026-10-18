use super::*;
use tt_application::dto::character_dto::{DeleteCharacterDto, RenameCharacterDto};
use tt_application::services::preset_service::PresetService;
use tt_domain::models::skill::{
    SkillImportInput, SkillInlineFile, SkillInstallRequest, SkillScope, SkillScopeFilter,
};

async fn install_linked_skill(service: &SkillService, scope: SkillScope, kind: &str) {
    service
        .install_import(SkillInstallRequest {
            input: SkillImportInput::InlineFiles {
                source: json!({ "kind": kind, "id": scope.label(), "label": "linked" }),
                files: vec![SkillInlineFile {
                    path: "SKILL.md".to_string(),
                    content: "---\nname: linked\ndescription: Linked guidance\n---\nGuidance"
                        .to_string(),
                    encoding: "utf8".to_string(),
                    media_type: None,
                    size_bytes: None,
                    sha256: None,
                }],
            },
            target_scope: scope,
            conflict_strategy: None,
        })
        .await
        .expect("install linked skill");
}

#[tokio::test]
async fn character_rename_and_delete_follow_stored_skill_identity() {
    let root = temp_root("character-linked-skill-lifecycle");
    let service = character_service(&root).await;
    let skills = SkillService::new(Arc::new(FileSkillRepository::new(root.join("skills"))));
    for name in ["Source", "Taken"] {
        service
            .create_character(create_character(name, None))
            .await
            .expect("create character");
    }
    install_linked_skill(
        &skills,
        SkillScope::Character {
            character_id: "Source".into(),
        },
        "character",
    )
    .await;

    let renamed = service
        .rename_character(
            RenameCharacterDto {
                old_name: "Source".into(),
                new_name: "Taken".into(),
            },
            &skills,
        )
        .await
        .expect("rename despite occupied display name");
    let stored_id = renamed
        .avatar
        .strip_suffix(".png")
        .expect("stored avatar stem");
    assert_ne!(stored_id, "Taken");
    let installed = skills.list_skills(SkillScopeFilter::All).await.unwrap();
    assert_eq!(installed.len(), 1);
    assert_eq!(
        installed[0].scope,
        SkillScope::Character {
            character_id: stored_id.into(),
        }
    );
    assert_eq!(
        installed[0].source_refs[0].id,
        format!("character:{stored_id}")
    );

    service
        .delete_character(
            DeleteCharacterDto {
                name: stored_id.into(),
                delete_chats: false,
            },
            &skills,
        )
        .await
        .expect("delete renamed character and linked skill");
    assert!(
        skills
            .list_skills(SkillScopeFilter::All)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(service.get_character("Taken").await.is_ok());
    fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn preset_delete_cleans_canonical_api_link_and_retains_other_sources() {
    let root = temp_root("preset-linked-skill-lifecycle");
    let repository = Arc::new(TestPresetRepository::default());
    let service = PresetService::new(repository.clone());
    let skills = SkillService::new(Arc::new(FileSkillRepository::new(root.join("skills"))));
    for name in ["Linked", "Other"] {
        service
            .save_preset_data(name.into(), "koboldhorde", json!({}))
            .await
            .unwrap();
        install_linked_skill(
            &skills,
            SkillScope::Preset {
                api_id: "kobold".into(),
                name: name.into(),
            },
            "preset",
        )
        .await;
    }
    service
        .delete_preset("Linked", &PresetType::Kobold, &skills)
        .await
        .unwrap();
    assert!(
        !repository
            .preset_exists("Linked", &PresetType::Kobold)
            .await
            .unwrap()
    );
    let installed = skills.list_skills(SkillScopeFilter::All).await.unwrap();
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].source_refs[0].id, "preset:kobold:Other");
    fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn linked_skill_failure_preserves_character_success_but_fails_preset_delete() {
    let root = temp_root("linked-skill-post-commit-failure");
    let characters = character_service(&root).await;
    characters
        .create_character(create_character("Source", None))
        .await
        .unwrap();
    let repository = Arc::new(TestPresetRepository::default());
    let presets = PresetService::new(repository.clone());
    presets
        .save_preset_data("Linked".into(), "openai", json!({}))
        .await
        .unwrap();
    let skill_root = root.join("skills");
    let skills = SkillService::new(Arc::new(FileSkillRepository::new(skill_root.clone())));
    fs::create_dir_all(skill_root.join("index")).await.unwrap();
    fs::write(skill_root.join("index/skills.json"), b"not json")
        .await
        .unwrap();

    // A primary-operation failure must not be replaced by the broken skill-index error.
    let error = presets
        .delete_preset("Missing", &PresetType::OpenAI, &skills)
        .await
        .unwrap_err();
    assert!(matches!(error, ApplicationError::NotFound(_)));
    let renamed = characters
        .rename_character(
            RenameCharacterDto {
                old_name: "Source".into(),
                new_name: "Renamed".into(),
            },
            &skills,
        )
        .await
        .expect("skill retarget failure cannot hide committed rename");
    let stored_id = renamed.avatar.strip_suffix(".png").unwrap();
    characters
        .delete_character(
            DeleteCharacterDto {
                name: stored_id.into(),
                delete_chats: false,
            },
            &skills,
        )
        .await
        .expect("skill cleanup failure cannot hide committed character deletion");
    assert!(characters.get_character(stored_id).await.is_err());
    let error = presets
        .delete_preset("Linked", &PresetType::OpenAI, &skills)
        .await
        .unwrap_err();
    assert!(matches!(error, ApplicationError::ValidationError(_)));
    assert!(
        !repository
            .preset_exists("Linked", &PresetType::OpenAI)
            .await
            .unwrap()
    );
    fs::remove_dir_all(root).await.unwrap();
}
