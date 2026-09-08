// `wire_api = "responses"` named profiles must activate through the
// OpenAI Responses profile factory (pinned to the profile's own base URL and
// key), never through the OpenRouter runtime.

#[test]
fn test_responses_wire_profile_activates_via_responses_factory() {
    with_clean_provider_test_env(|| {
        let jcode_home = std::env::var_os("JCODE_HOME").expect("test JCODE_HOME should be set");
        std::fs::write(
            std::path::PathBuf::from(jcode_home).join("config.toml"),
            r#"
[providers.zen]
type = "openai-compatible"
base_url = "https://opencode.ai/zen/v1"
wire_api = "responses"
default_model = "muse-spark-1.3-contributor-free"

[[providers.zen.models]]
id = "muse-spark-1.3-contributor-free"
input = ["text"]
"#,
        )
        .expect("write test config.toml");
        crate::config::invalidate_config_cache();

        let factory_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let factory_calls_ref = Arc::clone(&factory_calls);
        external::register_openai_responses_profile_factory(move |spec| {
            factory_calls_ref.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(spec.name, "zen");
            assert_eq!(spec.config.base_url, "https://opencode.ai/zen/v1");
            Ok(Arc::new(StubExternalRuntime::new(
                "zen",
                "zen",
                "openai-compatible:zen",
                &["muse-spark-1.3-contributor-free"],
            )))
        });

        let provider = MultiProvider {
            claude: RwLock::new(None),
            anthropic: RwLock::new(None),
            openai: RwLock::new(None),
            copilot_api: RwLock::new(None),
            antigravity: RwLock::new(None),
            gemini: RwLock::new(None),
            cursor: RwLock::new(None),
            bedrock: RwLock::new(None),
            openrouter: RwLock::new(None),
            openai_compatible_profiles: RwLock::new(std::collections::HashMap::new()),
            active_openai_compatible_profile: RwLock::new(None),
            active: RwLock::new(ActiveProvider::Claude),
            use_claude_cli: false,
            startup_notices: RwLock::new(Vec::new()),
            initial_provider: None,
            routes_memo: std::sync::Mutex::new(None),
            post_auth_refreshes_pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };

        provider
            .set_model("zen:muse-spark-1.3-contributor-free")
            .expect("responses-wire named profile model should be selectable");
        assert_eq!(
            factory_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the Responses profile factory must be used exactly once"
        );
        assert_eq!(provider.model(), "muse-spark-1.3-contributor-free");
        assert_eq!(
            ProviderRegistry::new(&provider).active_compatible_profile_id(),
            Some("zen".to_string())
        );
    });
}

#[test]
fn test_responses_wire_api_config_value_parses() {
    let responses_cfg: crate::config::NamedProviderConfig = toml::from_str(
        r#"
type = "openai-compatible"
base_url = "https://example.com/v1"
wire_api = "responses"
"#,
    )
    .expect("wire_api = responses must parse");
    assert_eq!(
        responses_cfg.wire_api,
        crate::config::NamedProviderWireApi::Responses
    );

    let default_cfg: crate::config::NamedProviderConfig = toml::from_str(
        r#"
type = "openai-compatible"
base_url = "https://example.com/v1"
"#,
    )
    .expect("named provider config without wire_api must parse");
    assert_eq!(
        default_cfg.wire_api,
        crate::config::NamedProviderWireApi::ChatCompletions,
        "omitting wire_api must keep the chat-completions default"
    );
}
