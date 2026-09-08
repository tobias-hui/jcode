// Named Responses profile (`wire_api = "responses"`) behavior: the instance
// pins the profile's base URL and API key, relaxes model validation to the
// configured model list, and reports the public
// `openai-compatible:<name>` route identity.

fn zen_profile_config() -> jcode_base::config::NamedProviderConfig {
    jcode_base::config::NamedProviderConfig {
        provider_type: jcode_base::config::NamedProviderType::OpenAiCompatible,
        base_url: "https://opencode.ai/zen/v1".to_string(),
        wire_api: jcode_base::config::NamedProviderWireApi::Responses,
        auth: jcode_base::config::NamedProviderAuth::Bearer,
        api_key_env: Some("JCODE_PROVIDER_ZEN_API_KEY".to_string()),
        default_model: Some("muse-spark-1.3-contributor-free".to_string()),
        models: vec![
            jcode_base::config::NamedProviderModelConfig {
                id: "muse-spark-1.3-contributor-free".to_string(),
                reasoning: Some(true),
                context_window: Some(262_144),
                ..Default::default()
            },
            jcode_base::config::NamedProviderModelConfig {
                id: "muse-spark-1.2-contributor-free".to_string(),
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

#[test]
fn profile_construction_pins_base_key_and_default_model() {
    let _guard = jcode_base::storage::lock_test_env();
    let _key = EnvVarGuard::set("JCODE_PROVIDER_ZEN_API_KEY", "zen-test-key");

    let provider = OpenAIProvider::new_for_profile("zen", &zen_profile_config())
        .expect("profile construction should succeed");

    assert_eq!(provider.model(), "muse-spark-1.3-contributor-free");
    assert_eq!(
        provider.profile_api_base(),
        Some("https://opencode.ai/zen/v1")
    );
    assert_eq!(
        provider
            .credentials
            .try_read()
            .expect("credential lock")
            .access_token,
        "zen-test-key",
        "the profile key must come from the profile env, never the shared OpenAI lane"
    );
}

#[test]
fn profile_construction_requires_key_for_bearer_auth() {
    let _guard = jcode_base::storage::lock_test_env();
    let _key = EnvVarGuard::remove("JCODE_PROVIDER_ZEN_API_KEY");

    let err = OpenAIProvider::new_for_profile("zen", &zen_profile_config())
        .err()
        .expect("missing profile key must fail construction");
    assert!(err.to_string().contains("JCODE_PROVIDER_ZEN_API_KEY"));
}

#[test]
fn profile_responses_url_uses_profile_base_not_env() {
    let _guard = jcode_base::storage::lock_test_env();
    let _key = EnvVarGuard::set("JCODE_PROVIDER_ZEN_API_KEY", "zen-test-key");
    let _base = EnvVarGuard::set("OPENAI_BASE_URL", "https://api.wodex.ai/v1");

    let provider = OpenAIProvider::new_for_profile("zen", &zen_profile_config())
        .expect("profile construction should succeed");
    let creds = provider.credentials.try_read().expect("credential lock");
    let url = OpenAIProvider::responses_url(
        &creds,
        provider.profile_api_base(),
    );
    assert_eq!(url, "https://opencode.ai/zen/v1/responses");
    assert!(
        OpenAIProvider::url_is_opencode_host(&url),
        "zen URLs must trigger the x-opencode-session header"
    );
}

#[test]
fn profile_set_model_accepts_configured_and_rejects_foreign() {
    let _guard = jcode_base::storage::lock_test_env();
    let _key = EnvVarGuard::set("JCODE_PROVIDER_ZEN_API_KEY", "zen-test-key");

    let provider = OpenAIProvider::new_for_profile("zen", &zen_profile_config())
        .expect("profile construction should succeed");

    provider
        .set_model("muse-spark-1.2-contributor-free")
        .expect("configured model must be accepted");
    assert_eq!(provider.model(), "muse-spark-1.2-contributor-free");

    let err = provider
        .set_model("gpt-5.6-luna")
        .expect_err("models outside the profile config must be rejected");
    assert!(err.to_string().contains("zen"));
}

#[test]
fn profile_route_identity_is_openai_compatible_profile() {
    let _guard = jcode_base::storage::lock_test_env();
    let _key = EnvVarGuard::set("JCODE_PROVIDER_ZEN_API_KEY", "zen-test-key");

    let provider = OpenAIProvider::new_for_profile("zen", &zen_profile_config())
        .expect("profile construction should succeed");
    let (label, api_method, detail) = provider
        .direct_openai_compatible_route_parts()
        .expect("profile must report an openai-compatible route identity");
    assert_eq!(label, "zen");
    assert_eq!(api_method, "openai-compatible:zen");
    assert_eq!(detail, "https://opencode.ai/zen/v1");
    assert_eq!(provider.display_name(), "zen");
}

#[test]
fn profile_credential_mode_is_pinned() {
    let _guard = jcode_base::storage::lock_test_env();
    let _key = EnvVarGuard::set("JCODE_PROVIDER_ZEN_API_KEY", "zen-test-key");

    let provider = OpenAIProvider::new_for_profile("zen", &zen_profile_config())
        .expect("profile construction should succeed");
    assert!(
        provider
            .set_credential_mode(OpenAICredentialMode::OAuth)
            .is_err(),
        "profile instances must refuse credential-mode switches"
    );
    assert_eq!(
        provider.available_models_for_switching(),
        vec![
            "muse-spark-1.3-contributor-free".to_string(),
            "muse-spark-1.2-contributor-free".to_string(),
        ]
    );
    assert_eq!(provider.context_window(), 262_144);
    assert!(
        !provider.supports_image_input(),
        "muse-spark has no image input declaration"
    );
}
