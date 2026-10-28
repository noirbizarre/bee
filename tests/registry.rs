//! Provider inventory, account isolation and strict configuration contracts.

mod support;

use bee::account::{Accounts as Providers, CredentialSource, Protocol};
use bee::config::{Config, Environment};
use bee::provider::registry;

fn load(text: &str) -> bee::Result<Providers> {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("bee.toml");
    std::fs::write(&path, text).unwrap();
    let config = Config::load(&Environment::new(None, None, Some(path)), root.path())?;
    Providers::from_config(&config)
}

#[test]
fn cloudflare_accounts_derive_private_protocol_targets_without_discovery_or_defaults() {
    let providers = load("[accounts.work]\nprovider='cloudflare'\nsettings={account_id='work-account',gateway_id='work-gateway'}\nmodels=['anthropic/model']\n[accounts.personal]\nprovider='cloudflare'\nsettings={account_id='personal-account',gateway_id='personal-gateway'}\ncredential={env='PERSONAL_CLOUDFLARE_TOKEN'}").unwrap();
    let work = providers.get("work").unwrap();
    let personal = providers.get("personal").unwrap();
    assert_eq!(work.protocol(), Protocol::OpenAiChat);
    assert!(!work.discovery());
    assert!(providers.configured_model().is_none());
    assert_eq!(
        work.credential(),
        Some(&CredentialSource::Env("CLOUDFLARE_API_TOKEN".into()))
    );
    assert_ne!(work.credential(), personal.credential());
    for protocol in [
        Protocol::OpenAiChat,
        Protocol::OpenAiResponses,
        Protocol::AnthropicMessages,
    ] {
        let suffix = if protocol == Protocol::AnthropicMessages {
            "ai"
        } else {
            "ai/v1"
        };
        assert_eq!(
            work.endpoint(protocol).unwrap().base_url(),
            format!("https://api.cloudflare.com/client/v4/accounts/work-account/{suffix}")
        );
        assert_eq!(
            personal.endpoint(protocol).unwrap().base_url(),
            format!("https://api.cloudflare.com/client/v4/accounts/personal-account/{suffix}")
        );
    }
    let debug = format!("{providers:?}");
    assert!(!debug.contains("work-account"));
    assert!(!debug.contains("work-gateway"));
}

#[test]
fn cloudflare_rejects_missing_malformed_and_rerouted_private_settings_without_echoing_values() {
    let valid = "[accounts.work]\nprovider='cloudflare'\nsettings={account_id='private-account',gateway_id='private-gateway'}\n";
    for invalid in [
        "[accounts.work]\nprovider='cloudflare'".into(),
        valid.replace("gateway_id='private-gateway'", "gateway_id=123"),
        valid.replace("private-account", "private-account/path"),
        valid.replace("private-account", ".."),
        valid.replace("private-gateway", "private-gateway?query"),
        valid.replace("private-gateway", &"x".repeat(65)),
        valid.replace("private-account", &"x".repeat(65)),
        valid.replace("private-gateway", ""),
        format!("{valid}discovery=true"),
        format!("{valid}protocol='openai-chat'\nbase_url='https://other.example/v1'"),
        format!("{valid}headers={{'CF-AIG-Gateway-ID'='private-override'}}"),
        format!("{valid}headers={{'X-API-Key'='private-override'}}"),
        format!("{valid}settings_extra='private-override'"),
        format!("{valid}model_protocols={{'@cf/meta/llama'='anthropic-messages'}}"),
        format!("{valid}model_protocols={{'@cf/meta/llama'='openai-responses'}}"),
    ] {
        let error = load(&invalid).unwrap_err();
        let diagnostic = format!("{error:?} {error}");
        for private in ["private-account", "private-gateway", "private-override"] {
            assert!(!diagnostic.contains(private), "{diagnostic}");
        }
    }
    for gateway in ["x".to_owned(), "x".repeat(64)] {
        load(&valid.replace("private-gateway", &gateway)).unwrap();
    }
}

#[test]
fn every_registered_type_has_unique_identity_and_resolves_its_native_defaults() {
    let mut ids = std::collections::BTreeSet::new();
    for definition in registry::definitions() {
        assert!(ids.insert(definition.id));
        if definition.id == "custom" {
            continue;
        }
        let settings = if definition.id == "cloudflare" {
            "\nsettings={account_id='fixture-account',gateway_id='fixture-gateway'}"
        } else {
            ""
        };
        let providers = load(&format!(
            "[accounts.account]\nprovider='{}'{settings}",
            definition.id
        ))
        .unwrap();
        let instance = providers.get("account").unwrap();
        assert_eq!(instance.provider(), definition.id);
        assert_eq!(instance.name(), "account");
        assert_eq!(instance.display_name(), Some(definition.label));
        assert!(!instance.endpoints().is_empty());
    }
    assert_eq!(
        ids.into_iter().collect::<Vec<_>>(),
        [
            "anthropic",
            "chatgpt",
            "cloudflare",
            "custom",
            "deepseek",
            "github",
            "google-aistudio",
            "google-cloud",
            "groq",
            "mistral",
            "ollama",
            "openai",
            "opencode",
            "opencode-go",
            "openrouter",
            "xai"
        ]
    );
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
        ("groq", Protocol::OpenAiChat, "GROQ_API_KEY"),
        ("mistral", Protocol::OpenAiChat, "MISTRAL_API_KEY"),
        ("xai", Protocol::OpenAiResponses, "XAI_API_KEY"),
        ("opencode", Protocol::OpenAiChat, "OPENCODE_API_KEY"),
        ("opencode-go", Protocol::OpenAiChat, "OPENCODE_GO_API_KEY"),
    ] {
        let providers = load(&format!("[accounts.account]\nprovider='{id}'")).unwrap();
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
    let providers = load("model='personal/private/model.v2'\n[accounts.personal]\nprovider='openai'\ncredential={env='PERSONAL'}\nmodels=['private/model.v2']\nheaders={x-account='one'}\n[accounts.work]\nprovider='openai'\ncredential={store=true}\nmodels=['arbitrary-work-model']\nheaders={x-account='two'}").unwrap();
    let personal = providers.get("personal").unwrap();
    let work = providers.get("work").unwrap();
    assert_eq!(personal.provider(), work.provider());
    assert_ne!(personal.credential(), work.credential());
    assert_ne!(personal.headers(), work.headers());
    assert_ne!(personal.models(), work.models());
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
        let providers = load(&format!("[accounts.account]\nprovider='{id}'")).unwrap();
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
                "[accounts.account]\nprovider='{id}'\nprotocol='openai-chat'\n{options}"
            ))
            .unwrap();
            let instance = providers.get("account").unwrap();
            assert_eq!(instance.base_url(), url);
            assert_eq!(instance.endpoints().len(), 1);
        }
    }
    assert!(load("[accounts.account]\nprovider='ollama'\nprotocol='openai-responses'").is_err());
    let providers = load("[accounts.account]\nprovider='ollama'\nprotocol='openai-responses'\nbase_url='http://localhost:11434/v1'").unwrap();
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
            "[accounts.account]\nprovider='openai'\ndisplay_name={value}"
        ))
        .unwrap_err();
        let output = format!("{error:?} {error}");
        assert!(!output.contains("private-name-sentinel"));
        assert!(output.contains("display_name"));
    }
}

#[test]
fn instance_methods_describe_supported_material_not_the_configured_acquisition_flow() {
    let providers = load("[accounts.account]\nprovider='github'\ncredential={store=true}").unwrap();
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
fn custom_shorthand_preserves_routing_without_expanding_declared_models() {
    let providers = load("model='gateway/private/anything.v3'\n[accounts.gateway]\nprovider='custom'\nprotocol='openai-chat'\nbase_url='https://gateway.example/v1/'\ndiscovery=false\nmodels=['private/anything.v3']\ncredential={store=true}\nheaders={x-private='private-header-sentinel'}\noptions={max_tokens_field='max_completion_tokens'}").unwrap();
    let instance = providers.get("gateway").unwrap();
    assert_eq!(instance.provider(), "custom");
    assert_eq!(instance.base_url(), "https://gateway.example/v1");
    assert_eq!(
        instance.endpoint_for_model("unlisted/model").protocol(),
        Protocol::OpenAiChat
    );
    assert!(!format!("{instance:?}").contains("private-header-sentinel"));
    assert_eq!(instance.models(), ["private/anything.v3"]);
}

#[test]
fn explicit_custom_configuration_cannot_accidentally_target_a_public_default() {
    for text in [
        "provider='custom'\nprotocol='openai-chat'",
        "provider='custom'\nprotocols={openai-chat={}}",
        "provider='custom'\nprotocol='openai-chat'\nbase_url='https://gateway.example'\nprotocols={openai-chat={}}",
        "provider='custom'\nbase_url='https://gateway.example'",
        "provider='custom'\nprefer=['openai-chat']\nprotocols={openai-chat={base_url='https://gateway.example'},anthropic-messages={}}",
    ] {
        assert!(load(&format!("[accounts.test]\n{text}")).is_err());
    }
}

#[test]
fn removed_configuration_has_value_free_diagnostics() {
    for text in [
        "[providers.account]\npreset='secret-sentinel'",
        "[presets.secret-sentinel]\nprotocols={openai-chat={}}",
        "[login_flows.secret-sentinel]\nkind='secret-sentinel'",
        "[accounts.account]\nprovider='secret-sentinel'",
        "[accounts.account]\nprovider='google'",
        "[accounts.account]\nprovider='grok'",
    ] {
        let error = load(text).unwrap_err();
        let output = format!("{error:?}\n{error}");
        assert!(!output.contains("secret-sentinel"), "{output}");
        assert!(output.contains("invalid provider setting"), "{output}");
    }
}

#[test]
fn a_higher_layer_cannot_hide_obsolete_configuration() {
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("bee/config.toml");
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(&global, "[providers.work]\npreset='openai'").unwrap();
    let selected = root.path().join("selected.toml");
    std::fs::write(&selected, "[accounts.work]\nprovider='openai'").unwrap();
    let env = Environment::new(Some(root.path().into()), None, Some(selected));
    assert!(Config::load(&env, root.path()).is_err());
}

#[test]
fn native_oauth_and_adc_keep_their_target_restrictions() {
    for text in [
        "provider='chatgpt'\nheaders={x-extra='unsafe'}",
        "provider='chatgpt'\ncredential={store=true,login='openrouter'}",
        "provider='custom'\nprotocol='openai-responses'\nbase_url='https://api.openai.com/v1'\ncredential={store=true,login='chatgpt'}",
        "provider='chatgpt'\nprotocols.openai-responses.base_url='https://untrusted.example'",
        "provider='github'\nprotocols.openai-chat.base_url='https://untrusted.example'",
        "provider='google-cloud'\nheaders={x-extra='unsafe'}",
    ] {
        assert!(load(&format!("[accounts.account]\n{text}")).is_err());
    }
}

#[test]
fn private_settings_are_provider_declared_and_never_echo_rejected_values() {
    for (provider, settings) in [
        ("openai", "{key='private-settings-sentinel'}"),
        ("google-cloud", "'private-settings-sentinel'"),
        ("google-cloud", "{project=42}"),
        ("google-cloud", "{project='private-settings-sentinel/path'}"),
        ("google-cloud", "{project=\"private-settings-sentinel\\n\"}"),
        ("google-cloud", "{project=' '}"),
    ] {
        let error = load(&format!(
            "[accounts.work]\nprovider='{provider}'\nsettings={settings}"
        ))
        .unwrap_err();
        let output = format!("{error} {error:?}");
        assert!(output.contains("accounts.work.settings"), "{output}");
        assert!(!output.contains("private-settings-sentinel"), "{output}");
    }
}

#[test]
fn declared_cloud_settings_construct_targets_and_reject_competing_legacy_settings() {
    let accounts = load("[accounts.work]\nprovider='google-cloud'\nsettings={project='private-project',location='europe-west4',quota_project='private-quota'}").unwrap();
    let account = accounts.get("work").unwrap();
    assert_eq!(account.settings()["project"], "private-project");
    assert_eq!(
        account.google_cloud().unwrap().quota_project.as_deref(),
        Some("private-quota")
    );
    assert_eq!(
        account.base_url(),
        "https://europe-west4-aiplatform.googleapis.com/v1/projects/private-project/locations/europe-west4/endpoints/openapi"
    );
    assert!(!format!("{accounts:?}").contains("private-"));

    let error = load("[accounts.work]\nprovider='google-cloud'\nsettings={project='private-project'}\ngoogle_cloud={project='other-private-project'}").unwrap_err();
    assert!(
        error
            .to_string()
            .contains("use settings or google_cloud, not both")
    );
    assert!(!format!("{error} {error:?}").contains("private-project"));
}

#[test]
fn credential_field_bindings_require_environment_ownership_and_declared_extra_fields() {
    for (credential, reason) in [
        ("{store=true,fields={}}", "require an environment source"),
        (
            "{env='PRIVATE_ENV',fields={key='PRIVATE_BINDING'}}",
            "unknown credential field",
        ),
        (
            "{env='PRIVATE_ENV',fields={undeclared='PRIVATE_BINDING'}}",
            "unknown credential field",
        ),
        (
            "{env='PRIVATE_ENV',fields='PRIVATE_BINDING'}",
            "must be a table",
        ),
    ] {
        let error = load(&format!(
            "[accounts.work]\nprovider='openai'\ncredential={credential}"
        ))
        .unwrap_err();
        let output = format!("{error} {error:?}");
        assert!(output.contains(reason), "{output}");
        assert!(!output.contains("PRIVATE_"), "{output}");
    }
    // A key-only provider needs no extra bindings; the key source stays in `env`.
    let accounts =
        load("[accounts.work]\nprovider='openai'\ncredential={env='PRIVATE_ENV',fields={}}")
            .unwrap();
    let account = accounts.get("work").unwrap();
    assert!(account.credential_env_fields().is_empty());
    assert_eq!(
        account.credential(),
        Some(&CredentialSource::Env("PRIVATE_ENV".into()))
    );
}

#[test]
fn native_acquisition_cannot_be_borrowed_by_compatible_accounts() {
    for (setting, reason) in [
        (
            "provider='openai'\ncredential={store=true,login='github'}",
            "GitHub device login requires provider = 'github'",
        ),
        (
            "provider='openai'\ncredential={adc=true}",
            "Google Cloud settings and ADC require the google-cloud provider",
        ),
        (
            "provider='openai'\ngoogle_cloud={project='private-target'}",
            "Google Cloud settings and ADC require the google-cloud provider",
        ),
    ] {
        let error = load(&format!("[accounts.work]\n{setting}")).unwrap_err();
        let output = format!("{error} {error:?}");
        assert!(output.contains(reason), "{output}");
        assert!(!output.contains("private-target"), "{output}");
        assert!(matches!(error, bee::Error::ProviderInvalid { .. }));
    }
}
