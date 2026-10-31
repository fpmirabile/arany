use crate::session::events::*;

#[test]
fn subscription_compaction_pins_account_and_weaker_bound() {
    let mut record = CompactionRecord {
        rate_limits: None,
        covered_run_id: RunId::new(),
        covered_sequence: 1,
        source_digest: [1; 32],
        recent_files: Vec::new(),
        provider: "openai".into(),
        model: "gpt-5.4".into(),
        prompt: crate::provider::EffectivePrompts::default()
            .pin(crate::provider::PromptFile::Compaction),
        compiler_version: crate::session::COMPACTION_COMPILER_VERSION,
        source_bytes: 4,
        source_tokens_estimate: 1,
        input_tokens: None,
        output_tokens: None,
        response_id: None,
        wire_provenance: None,
        chatgpt_provenance: None,
        output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
        status: CompactionStatus::Failed {
            reason: CompactionFailure::ProviderUnavailable,
        },
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
    };
    assert!(record.validate().is_ok());
    let mut misfiled = record.clone();
    misfiled.prompt.file = crate::provider::PromptFile::Primary;
    assert!(misfiled.validate().is_err());
    let encoded = serde_json::to_value(&record).unwrap();
    assert!(encoded.get("recent_files").is_none());
    let mut legacy = encoded.clone();
    legacy.as_object_mut().unwrap().remove("recent_files");
    assert_eq!(
        serde_json::from_value::<CompactionRecord>(legacy).unwrap(),
        record
    );
    assert!(encoded["chatgpt_provenance"].is_null());
    assert_eq!(encoded["output_token_bound"], "provider_enforced");
    for field in [
        "wire_provenance",
        "chatgpt_provenance",
        "output_token_bound",
    ] {
        let mut missing = encoded.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<CompactionRecord>(missing).is_err());
    }
    for (field, current) in [(
        "compiler_version",
        crate::session::COMPACTION_COMPILER_VERSION,
    )] {
        assert_eq!(encoded[field], current);
        for version in (0..=crate::session::COMPACTION_COMPILER_VERSION + 1)
            .filter(|version| *version != current)
        {
            let mut invalid = encoded.clone();
            invalid[field] = serde_json::json!(version);
            let invalid: CompactionRecord = serde_json::from_value(invalid).unwrap();
            let historical_compiler = field == "compiler_version"
                && (crate::session::MIN_RECORDED_COMPACTION_COMPILER_VERSION
                    ..=crate::session::COMPACTION_COMPILER_VERSION)
                    .contains(&version);
            assert_eq!(
                invalid.validate().is_ok(),
                historical_compiler,
                "{field}={version}"
            );
        }
    }
    record.provider = "chatgpt".into();
    record.chatgpt_provenance = Some(crate::provider::ChatGptProvenance {
        account_id: Uuid::now_v7(),
        admission_fingerprint: [7; 32],
        admission: crate::provider::ChatGptAdmission::AccountConsent,
    });
    assert!(record.validate().is_err());
    record.output_token_bound = crate::provider::OutputTokenBound::LocalAcceptanceOnly;
    assert!(record.validate().is_ok());
    let encoded = serde_json::to_value(&record).unwrap();
    assert_eq!(
        encoded["chatgpt_provenance"]["admission"],
        "account_consent"
    );
    assert_eq!(
        serde_json::from_value::<CompactionRecord>(encoded.clone()).unwrap(),
        record
    );
    record.recent_files = vec![crate::session::RecentFile {
        path: "src/main.rs".into(),
        digest: "a".repeat(64),
    }];
    assert!(record.validate().is_ok());
    let count_boundary: Vec<crate::session::RecentFile> = (0
        ..crate::session::events::MAX_COMPACTION_RECENT_FILES)
        .map(|index| crate::session::RecentFile {
            path: format!("src/file-{index}.rs"),
            digest: "a".repeat(64),
        })
        .collect();
    record.recent_files = count_boundary;
    assert!(record.validate().is_ok());
    let byte_boundary: Vec<crate::session::RecentFile> = (0
        ..crate::session::events::MAX_COMPACTION_RECENT_FILES)
        .map(|index| crate::session::RecentFile {
            path: format!(
                "d{index}/{}",
                "x".repeat(
                    424 - (if index + 1 == crate::session::events::MAX_COMPACTION_RECENT_FILES {
                        1
                    } else {
                        0
                    }) - 3
                )
            ),
            digest: "a".repeat(64),
        })
        .collect();
    let byte_boundary_size = serde_json::to_vec(&byte_boundary).unwrap().len();
    assert_eq!(
        byte_boundary_size,
        crate::session::events::MAX_COMPACTION_RECENT_FILES_BYTES
    );
    assert!(
        byte_boundary
            .iter()
            .all(|file| crate::validate::valid_relative(&file.path, false))
    );
    record.recent_files = byte_boundary.clone();
    assert!(record.validate().is_ok());
    let count_overflow = (0..=crate::session::events::MAX_COMPACTION_RECENT_FILES)
        .map(|index| crate::session::RecentFile {
            path: format!("src/file-{index}.rs"),
            digest: "a".repeat(64),
        })
        .collect::<Vec<_>>();
    assert!(
        count_overflow
            .iter()
            .all(|file| crate::validate::valid_relative(&file.path, false))
    );
    let mut byte_overflow = byte_boundary.clone();
    byte_overflow
        .last_mut()
        .expect("recent-file byte boundary")
        .path
        .push('x');
    assert!(
        serde_json::to_vec(&byte_overflow).unwrap().len()
            == crate::session::events::MAX_COMPACTION_RECENT_FILES_BYTES + 1
    );
    assert!(
        byte_overflow
            .iter()
            .all(|file| crate::validate::valid_relative(&file.path, false))
    );
    for invalid in [
        vec![crate::session::RecentFile {
            path: "../main.rs".into(),
            digest: "a".repeat(64),
        }],
        vec![crate::session::RecentFile {
            path: "src/main.rs".into(),
            digest: "x".repeat(64),
        }],
        count_overflow,
        vec![
            crate::session::RecentFile {
                path: "src/main.rs".into(),
                digest: "a".repeat(64),
            },
            crate::session::RecentFile {
                path: "src/main.rs".into(),
                digest: "b".repeat(64),
            },
        ],
        byte_overflow,
    ] {
        record.recent_files = invalid;
        assert!(record.validate().is_err());
    }
    for (field, value) in [
        ("admission", serde_json::json!("unchecked")),
        ("admission", serde_json::Value::Null),
        ("unknown", serde_json::json!(true)),
    ] {
        let mut invalid = encoded.clone();
        invalid["chatgpt_provenance"][field] = value;
        assert!(serde_json::from_value::<CompactionRecord>(invalid).is_err());
    }
    record
        .chatgpt_provenance
        .as_mut()
        .unwrap()
        .admission_fingerprint = [0; 32];
    assert!(record.validate().is_err());
}
