//! Provider identity, defaults and configuration migration contracts.

mod support;

use bee::config::{Config, Environment};
use bee::provider::{CredentialSource, Protocol, Providers, registry};

fn load(text: &str) -> bee::Result<Providers> {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("bee.toml");
    std::fs::write(&path, text).unwrap();
    let config = Config::load(&Environment::new(None, None, Some(path)), root.path())?;
    Providers::from_config(&config)
}

#[test]
fn every_registered_type_has_unique_identity_and_resolves_its_native_defaults() {
    let mut ids = std::collections::BTreeSet::new();
    for definition in registry::definitions() {
        assert!(ids.insert(definition.id));
        if definition.id == "custom" {
            continue;
        }
        let providers = load(&format!("[providers.account]\ntype='{}'", definition.id)).unwrap();
        let instance = providers.get("account").unwrap();
        assert_eq!(instance.provider_type(), definition.id);
        assert_eq!(instance.name(), "account");
        assert_eq!(instance.display_name(), Some(definition.label));
        assert!(!instance.endpoints().is_empty());
    }
}

#[test]
fn native_defaults_preserve_protocol_preferences_and_environment_sources() {
    for (id, protocol, env) in [
        ("openai", Protocol::OpenAiResponses, "OPENAI_API_KEY"),
        (
            "anthropic",
            Protocol::AnthropicMessages,
            "ANTHROPIC_API_KEY",
        ),
        ("deepseek", Protocol::OpenAiChat, "DEEPSEEK_API_KEY"),
        ("google-aistudio", Protocol::OpenAiChat, "GEMINI_API_KEY"),
        ("google", Protocol::OpenAiChat, "GEMINI_API_KEY"),
        ("groq", Protocol::OpenAiChat, "GROQ_API_KEY"),
        ("mistral", Protocol::OpenAiChat, "MISTRAL_API_KEY"),
        ("xai", Protocol::OpenAiResponses, "XAI_API_KEY"),
        ("opencode", Protocol::OpenAiChat, "OPENCODE_API_KEY"),
        ("opencode-go", Protocol::OpenAiChat, "OPENCODE_GO_API_KEY"),
    ] {
        let providers = load(&format!("[providers.account]\ntype='{id}'")).unwrap();
        let instance = providers.get("account").unwrap();
        assert_eq!(instance.protocol(), protocol, "{id}");
        assert_eq!(
            instance.credential(),
            Some(&CredentialSource::Env(env.into())),
            "{id}"
        );
    }
}

#[test]
fn instances_of_one_type_keep_independent_credentials_models_and_headers() {
    let providers = load("provider='personal'\n[providers.personal]\ntype='openai'\ncredential={env='PERSONAL'}\nmodel='private/model.v2'\nheaders={x-account='one'}\n[providers.work]\ntype='openai'\ncredential={store=true}\nmodel='arbitrary-work-model'\nheaders={x-account='two'}").unwrap();
    let personal = providers.get("personal").unwrap();
    let work = providers.get("work").unwrap();
    assert_eq!(personal.provider_type(), work.provider_type());
    assert_ne!(personal.credential(), work.credential());
    assert_ne!(personal.headers(), work.headers());
    assert_ne!(personal.model(), work.model());
}

#[test]
fn native_protocol_destinations_preserve_vendor_and_offering_boundaries() {
    for (id, routes) in [
        (
            "openai",
            vec![
                ("openai-responses", "https://api.openai.com/v1"),
                ("openai-chat", "https://api.openai.com/v1"),
            ],
        ),
        (
            "anthropic",
            vec![("anthropic-messages", "https://api.anthropic.com")],
        ),
        (
            "openrouter",
            vec![
                ("openai-chat", "https://openrouter.ai/api/v1"),
                ("openai-responses", "https://openrouter.ai/api/v1"),
            ],
        ),
        (
            "chatgpt",
            vec![("openai-responses", "https://api.openai.com/v1")],
        ),
        (
            "github",
            vec![
                ("openai-responses", "https://api.githubcopilot.com"),
                ("anthropic-messages", "https://api.githubcopilot.com"),
                ("openai-chat", "https://api.githubcopilot.com"),
            ],
        ),
        ("ollama", vec![("openai-chat", "http://localhost:11434/v1")]),
        (
            "opencode",
            vec![
                ("openai-chat", "https://opencode.ai/inference/openai/v1"),
                (
                    "openai-responses",
                    "https://opencode.ai/inference/openai/v1",
                ),
                (
                    "anthropic-messages",
                    "https://opencode.ai/inference/anthropic",
                ),
            ],
        ),
        (
            "opencode-go",
            vec![
                ("openai-chat", "https://opencode.ai/inference/go/openai/v1"),
                (
                    "openai-responses",
                    "https://opencode.ai/inference/go/openai/v1",
                ),
                (
                    "anthropic-messages",
                    "https://opencode.ai/inference/go/anthropic",
                ),
            ],
        ),
        (
            "deepseek",
            vec![
                ("openai-chat", "https://api.deepseek.com"),
                ("openai-responses", "https://api.deepseek.com"),
                ("anthropic-messages", "https://api.deepseek.com/anthropic"),
            ],
        ),
        (
            "google-aistudio",
            vec![(
                "openai-chat",
                "https://generativelanguage.googleapis.com/v1beta/openai",
            )],
        ),
        (
            "google",
            vec![(
                "openai-chat",
                "https://generativelanguage.googleapis.com/v1beta/openai",
            )],
        ),
        (
            "groq",
            vec![
                ("openai-chat", "https://api.groq.com/openai/v1"),
                ("openai-responses", "https://api.groq.com/openai/v1"),
            ],
        ),
        (
            "mistral",
            vec![("openai-chat", "https://api.mistral.ai/v1")],
        ),
        (
            "xai",
            vec![
                ("openai-responses", "https://api.x.ai/v1"),
                ("openai-chat", "https://api.x.ai/v1"),
            ],
        ),
    ] {
        let providers = load(&format!("[providers.account]\ntype='{id}'")).unwrap();
        let actual: Vec<_> = providers
            .get("account")
            .unwrap()
            .endpoints()
            .iter()
            .map(|endpoint| (endpoint.protocol().name(), endpoint.base_url()))
            .collect();
        assert_eq!(actual, routes, "{id}");
    }
}

#[test]
fn native_shorthand_keeps_registered_destinations_and_options_without_redirecting_credentials() {
    for (id, url) in [
        ("openrouter", "https://openrouter.ai/api/v1"),
        ("ollama", "http://localhost:11434/v1"),
        ("opencode-go", "https://opencode.ai/inference/go/openai/v1"),
    ] {
        for options in ["", "options={max_tokens_field='max_completion_tokens'}"] {
            let providers = load(&format!(
                "[providers.account]\ntype='{id}'\nprotocol='openai-chat'\n{options}"
            ))
            .unwrap();
            let instance = providers.get("account").unwrap();
            assert_eq!(instance.base_url(), url);
            assert_eq!(instance.endpoints().len(), 1);
        }
    }
    assert!(load("[providers.account]\ntype='ollama'\nprotocol='openai-responses'").is_err());
    let providers = load("[providers.account]\ntype='ollama'\nprotocol='openai-responses'\nbase_url='http://localhost:11434/v1'").unwrap();
    assert_eq!(
        providers.get("account").unwrap().base_url(),
        "http://localhost:11434/v1"
    );
}

#[test]
fn invalid_display_names_are_rejected_without_echoing_configured_values() {
    for value in [
        toml::Value::Integer(42),
        "".into(),
        " ".into(),
        "private-name-sentinel\n".into(),
        format!("private-name-sentinel{}", "x".repeat(256)).into(),
    ] {
        let value = serde_json::to_string(&value).unwrap();
        let error = load(&format!(
            "[providers.account]\ntype='openai'\ndisplay_name={value}"
        ))
        .unwrap_err();
        let output = format!("{error:?} {error}");
        assert!(!output.contains("private-name-sentinel"));
        assert!(output.contains("display_name"));
    }
}

#[test]
fn instance_methods_describe_supported_material_not_the_configured_acquisition_flow() {
    let providers = load("[providers.account]\ntype='github'\ncredential={store=true}").unwrap();
    assert_eq!(
        providers.get("account").unwrap().auth_methods(),
        &[registry::AuthMethod::ApiKey, registry::AuthMethod::OAuth]
    );
    assert_eq!(
        bee::auth::Credential::new(bee::auth::Secret::new("imported-token")).method(),
        registry::AuthMethod::ApiKey
    );
}

#[test]
fn custom_shorthand_normalizes_to_supported_protocols_without_a_model_allowlist() {
    let providers = load("[providers.gateway]\ntype='custom'\nprotocol='openai-chat'\nbase_url='https://gateway.example/v1/'\nmodel='private/anything.v3'\ncredential={store=true}\nheaders={x-private='private-header-sentinel'}\noptions={max_tokens_field='max_completion_tokens'}").unwrap();
    let instance = providers.get("gateway").unwrap();
    assert_eq!(instance.provider_type(), "custom");
    assert_eq!(instance.base_url(), "https://gateway.example/v1");
    assert_eq!(
        instance.endpoint_for_model("unlisted/model").protocol(),
        Protocol::OpenAiChat
    );
    assert!(!format!("{instance:?}").contains("private-header-sentinel"));
}

#[test]
fn explicit_custom_configuration_cannot_accidentally_target_a_public_default() {
    for text in [
        "type='custom'\nprotocol='openai-chat'",
        "type='custom'\nprotocols={openai-chat={}}",
        "type='custom'\nprotocol='openai-chat'\nbase_url='https://gateway.example'\nprotocols={openai-chat={}}",
        "type='custom'\nbase_url='https://gateway.example'",
    ] {
        assert!(load(&format!("[providers.test]\n{text}")).is_err());
    }
}

#[test]
fn removed_configuration_has_actionable_value_free_migration_diagnostics() {
    for text in [
        "[providers.account]\npreset='secret-sentinel'",
        "[presets.secret-sentinel]\nprotocols={openai-chat={}}",
        "[login_flows.secret-sentinel]\nkind='secret-sentinel'",
        "[providers.account]\ntype='secret-sentinel'",
    ] {
        let error = load(text).unwrap_err();
        let output = format!("{error:?}\n{error}");
        assert!(!output.contains("secret-sentinel"), "{output}");
        assert!(output.contains("type"), "{output}");
    }
}

#[test]
fn a_higher_layer_cannot_hide_obsolete_configuration() {
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("bee/config.toml");
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(&global, "[providers.work]\npreset='openai'").unwrap();
    let selected = root.path().join("selected.toml");
    std::fs::write(&selected, "[providers.work]\ntype='openai'").unwrap();
    let env = Environment::new(Some(root.path().into()), None, Some(selected));
    assert!(Config::load(&env, root.path()).is_err());
}

#[test]
fn native_oauth_and_adc_keep_their_target_restrictions() {
    for text in [
        "type='chatgpt'\nheaders={x-extra='unsafe'}",
        "type='chatgpt'\ncredential={store=true,login='openrouter'}",
        "type='custom'\nprotocol='openai-responses'\nbase_url='https://api.openai.com/v1'\ncredential={store=true,login='chatgpt'}",
        "type='chatgpt'\nprotocols.openai-responses.base_url='https://untrusted.example'",
        "type='github'\nprotocols.openai-chat.base_url='https://untrusted.example'",
        "type='google-cloud'\nheaders={x-extra='unsafe'}",
    ] {
        assert!(load(&format!("[providers.account]\n{text}")).is_err());
    }
}
