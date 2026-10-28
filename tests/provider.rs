//! Account configuration and native/generic protocol contracts, in-process.
//!
//! Every test describes its own environment and project directory, so none
//! touches a developer's real configuration.

use std::fs;
use std::path::{Path, PathBuf};

use bee::Error;
use bee::account::{
    Accounts as Providers, CredentialSource, MaxTokensField, ModelRef, Protocol, ProtocolOptions,
};
use bee::config::{Config, Environment, project_config_path};
use pretty_assertions::assert_eq;
use rstest::rstest;
use tempfile::TempDir;

/// A secret that must never appear in any diagnostic or debug output.
const SECRET: &str = "sk-super-secret-token";

struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            root: TempDir::new().unwrap(),
        }
    }

    fn global(&self) -> PathBuf {
        self.root.path().join("home/.config/bee/config.toml")
    }

    fn project_dir(&self) -> PathBuf {
        self.root.path().join("project")
    }

    fn project(&self) -> PathBuf {
        project_config_path(&self.project_dir())
    }

    fn write(&self, path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn providers(&self) -> bee::Result<Providers> {
        fs::create_dir_all(self.project_dir()).unwrap();
        let env = Environment::new(None, Some(self.root.path().join("home")), None);
        Providers::from_config(&Config::load(&env, &self.project_dir())?)
    }
}

/// Providers from a project file holding `text`.
fn load(text: &str) -> bee::Result<Providers> {
    let sandbox = Sandbox::new();
    sandbox.write(&sandbox.project(), text);
    sandbox.providers()
}

#[test]
fn model_suggestions_deduplicate_exact_ids_without_creating_a_default_or_rewriting_filters() {
    let providers = load("[accounts.work]\nprovider='openai'\ndiscovery=false\nmodels=['vendor/Model','vendor/Model','vendor/model']").unwrap();
    let account = providers.get("work").unwrap();
    assert_eq!(account.suggested_models(), ["vendor/Model", "vendor/model"]);
    assert_eq!(
        account.models(),
        ["vendor/Model", "vendor/Model", "vendor/model"]
    );
    assert_eq!(providers.configured_model(), None);
    let omitted = load("[accounts.work]\nprovider='openai'").unwrap();
    assert!(omitted.get("work").unwrap().suggested_models().is_empty());
    assert!(omitted.get("work").unwrap().model_filter().is_none());
}

#[test]
fn invalid_model_metadata_keys_and_values_keep_leaf_provenance_without_leaking_ids() {
    for (model, metadata) in [
        ("private-model-sentinel ", "{display_name='valid'}"),
        ("private-model-sentinel", "{display_name=42}"),
        ("private-model-sentinel", "{input_limit=0}"),
        ("private-model-sentinel", "{output_modalities=['']}"),
        (
            "private-model-sentinel",
            "{unknown='private-value-sentinel'}",
        ),
    ] {
        let sandbox = Sandbox::new();
        sandbox.write(
            &sandbox.global(),
            "[accounts.work]\nprovider='openai'\nmodel_metadata.base={display_name='Base'}",
        );
        sandbox.write(
            &sandbox.project(),
            &format!("[accounts.work.model_metadata]\n'{model}'={metadata}"),
        );
        let error = sandbox.providers().unwrap_err();
        match &error {
            Error::ProviderInvalid { key, path, .. } => {
                assert_eq!(key, "accounts.work.model_metadata");
                assert_eq!(path, &sandbox.project().display().to_string());
            }
            other => panic!("expected metadata validation error, got {other:?}"),
        }
        let output = rendered(error);
        assert!(!output.contains("private-model-sentinel"), "{output}");
        assert!(!output.contains("private-value-sentinel"), "{output}");
    }
}

#[test]
fn google_cloud_constructs_global_and_regional_targets_independently_of_connection_names() {
    for (location, host) in [
        ("global", "aiplatform.googleapis.com"),
        ("europe-west4", "europe-west4-aiplatform.googleapis.com"),
    ] {
        let providers = load(&format!("model='work/google/gemini-2.5-flash'\n[accounts.work]\nprovider='google-cloud'\nmodels=['google/gemini-2.5-flash']\ngoogle_cloud={{project='resource-project',location='{location}'}}")).unwrap();
        let profile = providers.get("work").unwrap();
        assert_eq!(profile.display_name(), Some("Google Cloud"));
        assert_eq!(
            profile.credential(),
            Some(&CredentialSource::ApplicationDefault)
        );
        assert_eq!(
            profile.endpoints()[0].base_url(),
            format!(
                "https://{host}/v1/projects/resource-project/locations/{location}/endpoints/openapi"
            )
        );
        assert!(!profile.discovery());
        assert_eq!(
            providers.configured_model(),
            Some(&ModelRef::parse("work/google/gemini-2.5-flash").unwrap())
        );
        assert!(!format!("{profile:?} {:?}", profile.google_cloud()).contains("resource-project"));
    }
}

#[rstest]
#[case("google_cloud={project='sk-super-secret-token/redirect'}")]
#[case("google_cloud={location='sk-super-secret-token.example'}")]
#[case("google_cloud={quota_project='sk-super-secret-token/path'}")]
#[case("protocols.openai-chat.base_url='https://evil.example/sk-super-secret-token'")]
#[case("protocols.openai-responses={}")]
#[case("headers={Authorization='sk-super-secret-token'}")]
#[case("credential={env='SECRET'}")]
#[case("credential={adc=false}")]
#[case("credential={adc=true,store=true}")]
#[case("discovery=true")]
fn google_cloud_refuses_unsafe_overrides_without_echoing_configuration(#[case] extra: &str) {
    let error = load(&format!(
        "[accounts.work]\nprovider='google-cloud'\n{extra}"
    ))
    .unwrap_err();
    assert!(!format!("{error:?} {error}").contains(SECRET));
}

#[test]
fn copilot_authentication_survives_env_overrides_and_disabled_discovery() {
    let providers = load("[accounts.work]\nprovider='github'\ncredential={env='COPILOT_GITHUB_TOKEN'}\ndiscovery=false").unwrap();
    let profile = providers.get("work").unwrap();
    assert!(profile.is_copilot());
    assert_eq!(profile.catalog(), Some("github-copilot"));
    assert!(!profile.discovery());
    assert!(profile.login_spec().is_none());
    assert_eq!(
        profile.credential(),
        Some(&CredentialSource::Env("COPILOT_GITHUB_TOKEN".into()))
    );
    assert_eq!(profile.endpoints().len(), 3);
}

#[rstest]
#[case("auth='unsupported-secret'")]
#[case("headers={Authorization='sk-super-secret-token'}")]
#[case("headers={X-Interaction-Id='sk-super-secret-token'}")]
#[case("credential={store=true,login='chatgpt'}")]
#[case("protocols.openai-chat.base_url='https://evil.example/sk-super-secret-token'")]
fn copilot_rejects_unsafe_connection_overrides_without_repeating_values(#[case] extra: &str) {
    let error = load(&format!("[accounts.work]\nprovider='github'\n{extra}")).unwrap_err();
    let rendered = format!("{error:?} {error}");
    assert!(!rendered.contains(SECRET));
    assert!(!rendered.contains("unsupported-secret"));
}

#[test]
fn context_windows_merge_per_model_across_instance_layers() {
    let sandbox = Sandbox::new();
    sandbox.write(&sandbox.global(), "[accounts.work]\nprovider='openai'\ncontext_window=10000\nmodel_context_windows={small=2000,large=20000}\n");
    sandbox.write(
        &sandbox.project(),
        "[accounts.work]\ncontext_window=12000\nmodel_context_windows={large=30000}\n",
    );
    let providers = sandbox.providers().unwrap();
    let profile = providers.get("work").unwrap();
    assert_eq!(profile.context_window("small"), Some(2000));
    assert_eq!(profile.context_window("large"), Some(30000));
    assert_eq!(profile.context_window("unknown"), Some(12000));
    let unknown = load("[accounts.work]\nprovider='custom'\n[accounts.work.protocols.openai-chat]\nbase_url='https://gateway.example'").unwrap();
    assert_eq!(
        unknown.get("work").unwrap().context_window("anything"),
        None
    );
}

#[rstest]
#[case("context_window=0")]
#[case("context_window=-1")]
#[case("context_window=4294967296")]
#[case("context_window='sk-super-secret-token'")]
#[case("model_context_windows={ 'sk-super-secret-token'=0 }")]
#[case("model_context_windows={ 'sk-super-secret-token'='sk-super-secret-token' }")]
#[case("model_context_windows={ 'sk-super-secret-token'=4294967296 }")]
#[case("model_context_windows='sk-super-secret-token'")]
fn invalid_context_limits_never_echo_values_or_model_map_keys(#[case] setting: &str) {
    let error = load(&format!(
        "[accounts.work]\nprovider='custom'\n{setting}\n[accounts.work.protocols.openai-chat]\nbase_url='https://gateway.example'"
    ))
    .unwrap_err();
    assert!(!format!("{error:?} {error}").contains(SECRET));
}

#[test]
fn context_model_map_diagnostics_preserve_leaf_provenance_and_redact_debug() {
    let sandbox = Sandbox::new();
    sandbox.write(&sandbox.global(), &format!("[accounts.work]\nprovider='custom'\nmodel_context_windows={{'{SECRET}'=0}}\n[accounts.work.protocols.openai-chat]\nbase_url='https://gateway.example'"));
    sandbox.write(&sandbox.project(), "[accounts.work]\ncontext_window=2000");
    let error = sandbox.providers().unwrap_err();
    let Error::ProviderInvalid { path, .. } = error else {
        panic!("invalid context limit must retain its typed provenance");
    };
    // Diagnostic formatting escapes Windows separators; compare filesystem paths.
    assert_eq!(Path::new(&path), sandbox.global());
    sandbox.write(&sandbox.global(), &format!("[accounts.work]\nprovider='custom'\nmodel_context_windows={{'{SECRET}'=1000}}\n[accounts.work.protocols.openai-chat]\nbase_url='https://gateway.example'"));
    assert!(!format!("{:?}", sandbox.providers().unwrap()).contains(SECRET));
}

#[test]
fn exact_model_mappings_override_preference_without_restricting_unknown_models() {
    let providers = load(r#"
model = "gateway/vendor/model.v2"
[accounts.gateway]
provider = "custom"
discovery = false
prefer = ["openai-chat"]
models = ["vendor/model.v2", "suggested"]
model_protocols = { "vendor/model.v2" = "openai-responses", "模型 / custom:1" = "anthropic-messages" }
protocols = { openai-chat = {base_url='https://gateway.example/chat'}, openai-responses = {base_url='https://gateway.example/responses'}, anthropic-messages = {base_url='https://gateway.example/messages'} }
"#).unwrap();
    let profile = providers.get("gateway").unwrap();
    assert_eq!(
        profile
            .endpoint_for_model(&providers.configured_model().unwrap().model)
            .protocol(),
        Protocol::OpenAiResponses
    );
    assert_eq!(
        profile.endpoint_for_model("模型 / custom:1").protocol(),
        Protocol::AnthropicMessages
    );
    for model in [
        "suggested",
        "unknown",
        "Vendor/model.v2",
        "vendor/model.v2-extra",
    ] {
        assert_eq!(
            profile.endpoint_for_model(model).protocol(),
            Protocol::OpenAiChat
        );
    }
    assert_eq!(profile.models(), ["vendor/model.v2", "suggested"]);
}

#[test]
fn invalid_model_mappings_are_rejected_without_printing_keys_or_values() {
    for mapping in [
        format!("'{SECRET}'"),
        format!("{{ '{SECRET}' = 1 }}"),
        format!("{{ '{SECRET}' = '{SECRET}' }}"),
        format!("{{ '{SECRET}' = 'openai-responses' }}"),
        "{ '' = 'openai-chat' }".into(),
        "{ ' trailing ' = 'openai-chat' }".into(),
        "{ \"control\\nkey\" = 'openai-chat' }".into(),
    ] {
        let error = load(&format!(
            "[accounts.p]\nprovider='custom'\nprotocols={{openai-chat={{base_url='https://gateway.example'}}}}\nmodel_protocols={mapping}"
        ))
        .unwrap_err();
        assert!(!format!("{error} {error:?}").contains(SECRET));
        let diagnostic = rendered(error);
        assert!(diagnostic.contains("model_protocols"));
        assert!(!diagnostic.contains(SECRET));
    }
}

#[test]
fn layered_model_mappings_merge_literal_ids_and_preserve_entry_origins() {
    let sandbox = Sandbox::new();
    sandbox.write(&sandbox.global(), "[accounts.p]\nprovider='custom'\nprefer=['openai-chat']\nprotocols={openai-chat={base_url='https://gateway.example/chat'},openai-responses={base_url='https://gateway.example/responses'},anthropic-messages={base_url='https://gateway.example/messages'}}\nmodel_protocols={'vendor/a.v2'='openai-chat',other='anthropic-messages'}");
    let selected = sandbox.root.path().join("selected.toml");
    sandbox.write(
        &selected,
        "[accounts.p.model_protocols]\n'vendor/a.v2'='openai-responses'",
    );
    sandbox.write(&sandbox.project(), "[accounts.p]\nmodel_protocols={}");
    let env = Environment::new(None, Some(sandbox.root.path().join("home")), Some(selected));
    let config = Config::load(&env, &sandbox.project_dir()).unwrap();
    let providers = Providers::from_config(&config).unwrap();
    let profile = providers.get("p").unwrap();
    assert_eq!(
        profile.endpoint_for_model("vendor/a.v2").protocol(),
        Protocol::OpenAiResponses
    );
    assert_eq!(
        profile.endpoint_for_model("other").protocol(),
        Protocol::AnthropicMessages
    );

    sandbox.write(&sandbox.global(), &format!("[accounts.p]\nprovider='custom'\nprotocols={{openai-chat={{base_url='https://gateway.example'}}}}\nmodel_protocols={{'{SECRET}'='openai-responses'}}"));
    // An unrelated contribution must not move the invalid entry's origin.
    sandbox.write(
        &sandbox.project(),
        "[accounts.p.model_protocols]\nother='openai-chat'",
    );
    let env = Environment::new(None, Some(sandbox.root.path().join("home")), None);
    let config = Config::load(&env, &sandbox.project_dir()).unwrap();
    assert!(!format!("{config:?}").contains(SECRET));
    let error = Providers::from_config(&config).unwrap_err();
    let Error::ProviderInvalid { path, .. } = &error else {
        panic!("expected an invalid mapping, got {error:?}");
    };
    // Windows accepts both separators; compare paths rather than rendered text.
    assert_eq!(Path::new(path), sandbox.global());
    assert!(!format!("{error:?}").contains(SECRET));
    sandbox.write(
        &sandbox.project(),
        &format!("[accounts.p.model_protocols]\n'{SECRET}'=1"),
    );
    let error = Config::load(&env, &sandbox.project_dir()).unwrap_err();
    assert!(matches!(error, Error::ConfigConflict { .. }));
    assert!(!format!("{error} {error:?}").contains(SECRET));
}

#[test]
fn inference_limits_are_positive_and_override_native_defaults() {
    let providers =
        load("[accounts.work]\nprovider='openai'\ntimeout_ms=456\nmax_output_tokens=123\n")
            .unwrap();
    let profile = providers.get("work").unwrap();
    assert_eq!(profile.timeout(), std::time::Duration::from_millis(456));
    assert_eq!(profile.max_output_tokens(), 123);
    for field in ["timeout_ms", "max_output_tokens"] {
        for value in ["0", "-1", "4294967296", "'sk-secret'"] {
            let error = load(&format!(
                "[accounts.work]\nprovider='custom'\n{field}={value}\n[accounts.work.protocols.openai-chat]\nbase_url='https://gateway.example'"
            ))
            .unwrap_err();
            assert!(!format!("{error:?} {error}").contains("sk-secret"));
        }
    }
}

fn rendered(error: Error) -> String {
    format!("{:?}", miette::Report::new(error))
}

#[test]
fn no_provider_configuration_is_not_an_error() {
    let providers = load("").unwrap();

    assert!(providers.is_empty());
    assert!(providers.configured_model().is_none());
}

#[test]
fn several_accounts_can_share_one_protocol_with_their_own_endpoints() {
    let providers = load(
        r#"
model = "work/corp/ft-2026"

[accounts.work]
provider = "custom"
protocols = { openai-chat = { base_url = "https://gateway.corp.example/v1/" } }
models = ["corp/ft-2026"]

[accounts.local]
provider = "custom"
protocols = { openai-chat = { base_url = "http://localhost:11434/v1" } }
models = ["llama3:8b"]
"#,
    )
    .unwrap();

    let work = providers.get("work").unwrap();
    let local = providers.get("local").unwrap();
    assert_eq!(work.protocol(), Protocol::OpenAiChat);
    assert_eq!(local.protocol(), Protocol::OpenAiChat);
    assert_eq!(work.base_url(), "https://gateway.corp.example/v1");
    assert_eq!(local.base_url(), "http://localhost:11434/v1");
    assert_eq!(
        providers
            .configured_model()
            .map(|model| model.account.as_str()),
        Some("work")
    );
}

#[test]
fn an_account_name_says_nothing_about_its_protocol() {
    // Named like a commercial provider, and not speaking its protocol.
    let providers = load(
        "[accounts.anthropic]\nprovider='custom'\nprotocols = { openai-chat = { base_url = \"http://proxy.local\" } }",
    )
    .unwrap();

    assert_eq!(
        providers.get("anthropic").unwrap().protocol(),
        Protocol::OpenAiChat
    );
}

#[rstest]
#[case::openai("openai-chat", "https://api.openai.com/v1")]
#[case::responses("openai-responses", "https://api.openai.com/v1")]
#[case::anthropic("anthropic-messages", "https://api.anthropic.com")]
fn custom_protocols_require_explicit_base_urls_and_preserve_overrides(
    #[case] protocol: &str,
    #[case] default_url: &str,
) {
    assert!(
        load(&format!(
            "[accounts.p]\nprovider='custom'\n[accounts.p.protocols.{protocol}]"
        ))
        .is_err()
    );
    let explicit = load(&format!("[accounts.p]\nprovider='custom'\n[accounts.p.protocols.{protocol}]\nbase_url='{default_url}'")).unwrap();
    let overridden = load(&format!(
        "[accounts.p]\nprovider='custom'\n[accounts.p.protocols.{protocol}]\nbase_url = \"https://other.example\""
    ))
    .unwrap();

    assert_eq!(explicit.get("p").unwrap().base_url(), default_url);
    assert_eq!(
        overridden.get("p").unwrap().base_url(),
        "https://other.example"
    );
}

#[rstest]
#[case("gpt-4o")]
#[case("acme/ft-llama3:2026-01")]
#[case("accounts/me/models/tuned@v2")]
#[case("model that is not in any catalog")]
fn declared_raw_model_ids_are_preserved_without_a_catalog(#[case] model: &str) {
    let providers = load(&format!(
        "model='p/{model}'\n[accounts.p]\nprovider='custom'\ndiscovery=false\nprotocols = {{ openai-chat = {{base_url='https://gateway.example'}} }}\nmodels = [\"{model}\"]"
    ))
    .unwrap();

    assert_eq!(providers.configured_model().unwrap().model, model);
    assert_eq!(providers.get("p").unwrap().models(), [model]);
}

/// A custom account that speaks both protocols, each at its own URL.
const BOTH: &str = r#"
[accounts.p]
provider = "custom"
prefer = ["anthropic-messages", "openai-chat"]

[accounts.p.protocols.openai-chat]
base_url = "https://gw.example/openai/v1"
options = { max_tokens_field = "max_completion_tokens" }

[accounts.p.protocols.anthropic-messages]
base_url = "https://gw.example/anthropic"
options = { max_tokens = 2048 }
"#;

#[test]
fn a_provider_can_speak_several_protocols_each_with_its_own_url_and_options() {
    let providers = load(BOTH).unwrap();
    let profile = providers.get("p").unwrap();

    let openai = profile.endpoint(Protocol::OpenAiChat).unwrap();
    let anthropic = profile.endpoint(Protocol::AnthropicMessages).unwrap();
    assert_eq!(openai.base_url(), "https://gw.example/openai/v1");
    assert_eq!(anthropic.base_url(), "https://gw.example/anthropic");
    assert_eq!(
        *openai.options(),
        ProtocolOptions::OpenAiChat {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            cache: Default::default(),
        }
    );
    assert_eq!(
        *anthropic.options(),
        ProtocolOptions::AnthropicMessages {
            version: None,
            max_tokens: Some(2048),
            cache: Default::default(),
        }
    );
}

#[test]
fn the_endpoints_come_in_the_order_of_prefer_and_the_first_is_the_one_requests_use() {
    let providers = load(BOTH).unwrap();
    let profile = providers.get("p").unwrap();

    let order: Vec<_> = profile.endpoints().iter().map(|e| e.protocol()).collect();
    assert_eq!(order, [Protocol::AnthropicMessages, Protocol::OpenAiChat]);
    assert_eq!(profile.protocol(), Protocol::AnthropicMessages);
    assert_eq!(profile.base_url(), "https://gw.example/anthropic");
}

#[test]
fn swapping_prefer_swaps_the_preferred_endpoint_whatever_the_names_sort_like() {
    let swapped = BOTH.replace(
        "prefer = [\"anthropic-messages\", \"openai-chat\"]",
        "prefer = [\"openai-chat\", \"anthropic-messages\"]",
    );

    let providers = load(&swapped).unwrap();

    assert_eq!(providers.get("p").unwrap().protocol(), Protocol::OpenAiChat);
}

#[test]
fn protocols_that_prefer_leaves_out_follow_in_a_fixed_order() {
    let providers = load(
        "[accounts.p]\nprovider='custom'\nprefer = [\"anthropic-messages\"]\n\
         [accounts.p.protocols.openai-chat]\nbase_url='https://gateway.example/chat'\n[accounts.p.protocols.anthropic-messages]\nbase_url='https://gateway.example/messages'",
    )
    .unwrap();

    let order: Vec<_> = providers
        .get("p")
        .unwrap()
        .endpoints()
        .iter()
        .map(|e| e.protocol())
        .collect();
    assert_eq!(order, [Protocol::AnthropicMessages, Protocol::OpenAiChat]);
}

#[test]
fn one_protocol_needs_no_preference_and_prefer_may_still_name_it() {
    assert!(load("[accounts.p]\nprovider='custom'\n[accounts.p.protocols.openai-chat]\nbase_url='https://gateway.example'").is_ok());
    assert!(
        load("[accounts.p]\nprovider='custom'\nprefer = [\"openai-chat\"]\n[accounts.p.protocols.openai-chat]\nbase_url='https://gateway.example'")
            .is_ok()
    );
}

#[test]
fn the_responses_protocol_rejects_unrelated_generation_options() {
    let text = rendered(
        load("[accounts.p]\nprovider='custom'\n[accounts.p.protocols.openai-responses]\nbase_url='https://gateway.example'\n[accounts.p.protocols.openai-responses.options]\nmax_tokens = 5").unwrap_err(),
    );

    assert!(text.contains("bee::provider::unknown_key"), "{text}");
    assert!(text.contains("prompt_cache_key"), "{text}");
}

#[test]
fn an_empty_responses_options_table_keeps_native_cache_defaults() {
    let providers = load("[accounts.p]\nprovider='custom'\n[accounts.p.protocols.openai-responses]\nbase_url='https://gateway.example'\noptions = {}").unwrap();

    assert_eq!(
        *providers.get("p").unwrap().options(),
        ProtocolOptions::OpenAiResponses {
            cache: Default::default()
        }
    );
}

#[test]
fn native_cache_configuration_is_typed_and_invalid_values_never_reach_diagnostics() {
    for (protocol, option) in [
        ("openai-chat", "prompt_cache_key = 42"),
        ("openai-responses", "prompt_cache_key = {mode='explicit'}"),
        (
            "openai-responses",
            "prompt_cache_key = {mode='sk-super-secret-token'}",
        ),
        (
            "openai-responses",
            "prompt_cache_retention = 'sk-super-secret-token'",
        ),
        (
            "openai-responses",
            "prompt_cache_options = {enabled='sk-super-secret-token'}",
        ),
        (
            "openai-responses",
            "prompt_cache_options = {ttl='sk-super-secret-token'}",
        ),
        (
            "openai-responses",
            "prompt_cache_options = {mode='sk-super-secret-token'}",
        ),
        (
            "openai-responses",
            "prompt_cache_options = {breakpoints='sk-super-secret-token'}",
        ),
        (
            "openai-responses",
            "prompt_cache_options = {enabled=true}\nprompt_cache_retention='24h'",
        ),
        (
            "anthropic-messages",
            "cache_control = {policy='sk-super-secret-token'}",
        ),
        (
            "anthropic-messages",
            "cache_control = {ttl='sk-super-secret-token'}",
        ),
        ("anthropic-messages", "prompt_cache_retention = '24h'"),
    ] {
        let error = load(&format!(
            "[accounts.p]\nprovider='custom'\n[accounts.p.protocols.{protocol}]\nbase_url='https://gateway.example'\n[accounts.p.protocols.{protocol}.options]\n{option}"
        ))
        .unwrap_err();
        assert!(!format!("{error:?}").contains(SECRET));
        let diagnostic = rendered(error);
        assert!(!diagnostic.contains(SECRET));
        assert!(diagnostic.contains("bee::provider::"));
    }
    for value in ["", &"a".repeat(65)] {
        assert!(load(&format!("[accounts.p]\nprovider='custom'\n[accounts.p.protocols.openai-responses]\nbase_url='https://gateway.example'\n[accounts.p.protocols.openai-responses.options]\nprompt_cache_key={{mode='explicit',value='{value}'}}")).is_err());
    }
}

#[test]
fn higher_layers_can_disable_inherited_write_policies_without_changing_value_types() {
    use bee::account::cache::{AnthropicPolicy, CacheKey, Retention};
    let sandbox = Sandbox::new();
    sandbox.write(
        &sandbox.global(),
        r#"
[accounts.p]
provider='custom'
prefer=['openai-responses']
[accounts.p.protocols.openai-responses]
base_url='https://gateway.example/responses'
[accounts.p.protocols.openai-responses.options]
prompt_cache_options = { enabled=true, mode='explicit', ttl='12h', breakpoints='instructions' }
prompt_cache_key = {mode='explicit', value='private-application-key'}
[accounts.p.protocols.anthropic-messages]
base_url='https://gateway.example/messages'
[accounts.p.protocols.anthropic-messages.options]
cache_control = {policy='stable-prefix-and-automatic',ttl='1h'}
"#,
    );
    sandbox.write(
        &sandbox.project(),
        r#"
[accounts.p.protocols.openai-responses.options]
prompt_cache_options = {enabled=false}
prompt_cache_key = {mode='disabled'}
prompt_cache_retention = 'default'
[accounts.p.protocols.anthropic-messages.options]
cache_control = {policy='disabled'}
"#,
    );
    let providers = sandbox.providers().unwrap();
    let profile = providers.get("p").unwrap();
    let ProtocolOptions::OpenAiResponses { cache } = profile
        .endpoint(Protocol::OpenAiResponses)
        .unwrap()
        .options()
    else {
        panic!("Responses options expected")
    };
    assert!(!cache.options.enabled);
    assert_eq!(cache.key, CacheKey::Disabled);
    assert_eq!(cache.retention, Retention::Default);
    let ProtocolOptions::AnthropicMessages { cache, .. } = profile
        .endpoint(Protocol::AnthropicMessages)
        .unwrap()
        .options()
    else {
        panic!("Anthropic options expected")
    };
    assert_eq!(cache.policy, AnthropicPolicy::Disabled);
    assert!(!format!("{profile:?}").contains("private-application-key"));
}

#[test]
fn protocols_that_prefer_leaves_out_follow_in_the_order_of_protocol_all() {
    let providers = load(
        "[accounts.p]\nprovider='custom'\nprefer = [\"anthropic-messages\"]\n\
         [accounts.p.protocols.openai-responses]\nbase_url='https://gateway.example/responses'\n[accounts.p.protocols.openai-chat]\nbase_url='https://gateway.example/chat'\n\
         [accounts.p.protocols.anthropic-messages]\nbase_url='https://gateway.example/messages'",
    )
    .unwrap();

    let order: Vec<_> = providers
        .get("p")
        .unwrap()
        .endpoints()
        .iter()
        .map(|e| e.protocol())
        .collect();
    assert_eq!(
        order,
        [
            Protocol::AnthropicMessages,
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses
        ]
    );
}

#[test]
fn an_endpoint_the_profile_does_not_list_is_none() {
    let providers = load("[accounts.p]\nprovider='custom'\n[accounts.p.protocols.openai-chat]\nbase_url='https://gateway.example'").unwrap();

    assert!(
        providers
            .get("p")
            .unwrap()
            .endpoint(Protocol::AnthropicMessages)
            .is_none()
    );
}

#[test]
fn headers_and_options_belong_only_to_their_own_account() {
    let providers = load(
        r#"
[accounts.a]
provider='custom'
headers = { "X-Team" = "infra" }
[accounts.a.protocols.openai-chat]
base_url='https://gateway.example/chat'
[accounts.a.protocols.openai-chat.options]
max_tokens_field = "max_completion_tokens"

[accounts.b]
provider='custom'
[accounts.b.protocols.anthropic-messages]
base_url='https://gateway.example/messages'
[accounts.b.protocols.anthropic-messages.options]
version = "2023-06-01"
max_tokens = 4096
"#,
    )
    .unwrap();

    let a = providers.get("a").unwrap();
    let b = providers.get("b").unwrap();
    assert_eq!(a.headers(), [("X-Team".to_owned(), "infra".to_owned())]);
    assert!(b.headers().is_empty());
    assert_eq!(
        *a.options(),
        ProtocolOptions::OpenAiChat {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            cache: Default::default(),
        }
    );
    assert_eq!(
        *b.options(),
        ProtocolOptions::AnthropicMessages {
            version: Some("2023-06-01".to_owned()),
            max_tokens: Some(4096),
            cache: Default::default(),
        }
    );
}

#[test]
fn a_credential_is_a_named_environment_variable() {
    let providers = load(
        "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\ncredential = { env = \"GATEWAY_KEY\" }",
    )
    .unwrap();

    assert_eq!(
        providers.get("p").unwrap().credential(),
        Some(&CredentialSource::Env("GATEWAY_KEY".to_owned()))
    );
}

#[rstest]
#[case::token_only("{ store = true }", None)]
#[case::browser_sign_in("{ store = true, login = \"openrouter\" }", Some("openrouter"))]
fn a_credential_can_live_in_the_store(#[case] credential: &str, #[case] login: Option<&str>) {
    let providers = load(&format!(
        "[accounts.p]\nprovider='custom'\nprotocols = {{ openai-chat = {{base_url='https://gateway.example'}} }}\ncredential = {credential}"
    ))
    .unwrap();

    assert_eq!(
        providers.get("p").unwrap().credential(),
        Some(&CredentialSource::Store {
            login: login.map(str::to_owned)
        })
    );
}

#[test]
fn the_debug_output_names_the_kind_of_credential_and_nothing_else() {
    let providers = load(
        "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\ncredential = { env = \"GATEWAY_KEY\" }",
    )
    .unwrap();

    let debug = format!("{providers:?}");

    assert!(debug.contains("credential: Some(\"env\")"), "{debug}");
    assert!(!debug.contains("GATEWAY_KEY"), "{debug}");
}

#[test]
fn a_single_account_and_model_do_not_create_a_default() {
    let providers =
        load("[accounts.only]\nprovider='openai'\ndiscovery=false\nmodels=['sole-model']").unwrap();

    assert_eq!(providers.configured_model(), None);
    assert!(providers.configured_model().is_none());
}

#[test]
fn the_project_file_overrides_the_global_one_key_by_key() {
    let sandbox = Sandbox::new();
    sandbox.write(
        &sandbox.global(),
        r#"
model = "gw/global-model"
[accounts.gw]
provider='custom'
protocols = { openai-chat = { base_url = "https://global.example/v1" } }
models = ["global-model", "g1", "g2"]
headers = { "X-Org" = "global", "X-Keep" = "yes" }
"#,
    );
    sandbox.write(
        &sandbox.project(),
        r#"
model = "gw/project-model"
[accounts.gw]
models = ["project-model", "p1"]
headers = { "X-Org" = "project" }
"#,
    );

    let providers = sandbox.providers().unwrap();

    let gw = providers.get("gw").unwrap();
    // Scalars: the higher layer wins, the rest is inherited.
    assert_eq!(providers.configured_model().unwrap().model, "project-model");
    assert_eq!(gw.base_url(), "https://global.example/v1");
    // Arrays are replaced, never concatenated (ADR-006).
    assert_eq!(gw.models(), ["project-model", "p1"]);
    // Tables merge key by key.
    assert_eq!(
        gw.headers(),
        [
            ("X-Keep".to_owned(), "yes".to_owned()),
            ("X-Org".to_owned(), "project".to_owned())
        ]
    );
}

#[test]
fn a_project_can_add_an_account_and_choose_a_qualified_default() {
    let sandbox = Sandbox::new();
    sandbox.write(
        &sandbox.global(),
        "model='a/first'\n[accounts.a]\nprovider='custom'\nmodels=['first']\nprotocols = { openai-chat = {base_url='https://gateway.example/chat'} }",
    );
    sandbox.write(
        &sandbox.project(),
        "model='b/second'\n[accounts.b]\nprovider='custom'\nmodels=['second']\nprotocols = { anthropic-messages = {base_url='https://gateway.example/messages'} }",
    );

    let providers = sandbox.providers().unwrap();

    assert_eq!(providers.names().collect::<Vec<_>>(), ["a", "b"]);
    assert_eq!(
        providers
            .configured_model()
            .map(|model| model.account.as_str()),
        Some("b")
    );
}

#[test]
fn model_declarations_preserve_omitted_and_empty_filters_in_both_discovery_modes() {
    for discovery in [false, true] {
        for (declaration, expected) in [
            ("", None),
            ("models=[]", Some(Vec::<String>::new())),
            (
                "models=['vendor/model','local']",
                Some(vec!["vendor/model".into(), "local".into()]),
            ),
        ] {
            let accounts = load(&format!(
                "[accounts.p]\nprovider='openai'\ndiscovery={discovery}\n{declaration}"
            ))
            .unwrap();
            let account = accounts.get("p").unwrap();
            assert_eq!(account.discovery(), discovery);
            assert_eq!(account.model_filter(), expected.as_deref());
            assert_eq!(accounts.configured_model(), None);
        }
    }
}

#[test]
fn configured_defaults_require_declared_or_potentially_discovered_availability() {
    for (discovery, declaration, valid) in [
        (false, "", false),
        (false, "models=[]", false),
        (false, "models=['selected']", true),
        (false, "models=['other']", false),
        (true, "", true),
        (true, "models=[]", false),
        (true, "models=['selected']", true),
        (true, "models=['other']", false),
    ] {
        let result = load(&format!(
            "model='p/selected'\n[accounts.p]\nprovider='openai'\ndiscovery={discovery}\n{declaration}"
        ));
        assert_eq!(
            result.is_ok(),
            valid,
            "discovery={discovery}, {declaration}"
        );
    }
}

#[test]
fn obsolete_selectors_and_account_fields_are_rejected_without_value_echoes() {
    for text in [
        format!("provider='{SECRET}'"),
        format!("[providers.p]\ntype='{SECRET}'"),
        format!("[accounts.p]\nprovider='openai'\nmodel='{SECRET}'"),
        format!("[accounts.p]\nprovider='openai'\ntype='{SECRET}'"),
        format!("[accounts.p]\nprovider='openai'\ndiscover_models='{SECRET}'"),
        format!("[accounts.p]\nprovider='openai'\ncatalog_provider='{SECRET}'"),
        format!("[accounts.p]\nprovider='openai'\nenabled='{SECRET}'"),
        format!("[accounts.p]\nprovider='openai'\ndiscovery='{SECRET}'"),
        format!("[accounts.p]\nprovider='openai'\ncatalog=['{SECRET}']"),
        format!("[profiles.bad]\nconnections=['{SECRET}']"),
        format!("[profiles.bad]\nprovider='{SECRET}'"),
        format!("[profiles.bad.models.p]\nmodel='{SECRET}'"),
    ] {
        let error = load(&text).unwrap_err();
        assert!(!format!("{error} {error:?}").contains(SECRET));
    }
    let accounts =
        load("[accounts.p]\nprovider='openai'\ncatalog='private-catalog'\ndiscovery=false")
            .unwrap();
    assert_eq!(
        accounts.get("p").unwrap().catalog(),
        Some("private-catalog")
    );
    assert!(!format!("{accounts:?}").contains("private-catalog"));
}

/// The diagnostic code an invalid configuration must produce.
#[rstest]
#[case::unknown_protocol(
    "[accounts.p]\nprovider='custom'\nprotocols = { grpc = {base_url='https://gateway.example'} }",
    "bee::provider::unknown_protocol"
)]
#[case::protocols_by_name_only("[accounts.openai]\nmodels = ['gpt-4o']", "bee::provider::invalid")]
#[case::protocols_wrong_type(
    "[accounts.p]\nprovider='openai'\nprotocols = 3",
    "bee::provider::wrong_type"
)]
#[case::protocols_as_a_list_of_names(
    "[accounts.p]\nprovider='openai'\nprotocols = [\"openai-chat\"]",
    "bee::provider::wrong_type"
)]
#[case::no_protocol_listed(
    "[accounts.p]\nprovider='custom'\nprotocols = {}",
    "bee::provider::invalid"
)]
#[case::protocol_entry_not_a_table(
    "[accounts.p]\nprovider='openai'\nprotocols = { openai-chat = \"https://x.example\" }",
    "bee::provider::wrong_type"
)]
#[case::the_singular_protocol_key(
    "[accounts.p]\nprovider='custom'\nprotocol = \"openai-chat\"",
    "bee::provider::invalid"
)]
#[case::base_url_beside_protocols(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\nbase_url = \"https://x.example\"",
    "bee::provider::invalid"
)]
#[case::options_beside_protocols(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\noptions = {}",
    "bee::provider::invalid"
)]
#[case::several_protocols_without_a_preference(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example/chat'}, anthropic-messages = {base_url='https://gateway.example/messages'} }",
    "bee::provider::no_preference"
)]
#[case::prefer_names_an_unknown_protocol(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\nprefer = [\"grpc\"]",
    "bee::provider::unknown_protocol"
)]
#[case::prefer_names_a_protocol_the_profile_lacks(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\nprefer = [\"anthropic-messages\"]",
    "bee::provider::invalid"
)]
#[case::prefer_repeats_a_protocol(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\nprefer = [\"openai-chat\", \"openai-chat\"]",
    "bee::provider::invalid"
)]
#[case::prefer_not_an_array(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\nprefer = \"openai-chat\"",
    "bee::provider::wrong_type"
)]
#[case::an_option_of_a_protocol_with_none(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-responses = {base_url='https://gateway.example', options = { max_tokens_field = \"max_tokens\" } } }",
    "bee::provider::unknown_key"
)]
#[case::all_three_protocols_without_a_preference(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example/chat'}, openai-responses = {base_url='https://gateway.example/responses'}, anthropic-messages = {base_url='https://gateway.example/messages'} }",
    "bee::provider::no_preference"
)]
#[case::an_endpoint_with_an_unknown_key(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example', version = \"1\" } }",
    "bee::provider::unknown_key"
)]
#[case::an_option_of_another_protocols_endpoint(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example', options = { max_tokens = 5 } } }",
    "bee::provider::unknown_key"
)]
#[case::accounts_not_a_table("accounts = \"x\"", "bee::provider::wrong_type")]
#[case::account_not_a_table("[accounts]\np = \"x\"", "bee::provider::wrong_type")]
#[case::misspelled_key(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\nbase_ulr = \"https://x.example\"",
    "bee::provider::unknown_key"
)]
#[case::bad_scheme(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = { base_url = \"ftp://x.example\" } }",
    "bee::provider::invalid"
)]
#[case::url_without_host(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = { base_url = \"https://\" } }",
    "bee::provider::invalid"
)]
#[case::url_with_userinfo(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = { base_url = \"https://user:pw@x.example\" } }",
    "bee::provider::invalid"
)]
#[case::url_with_query(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = { base_url = \"https://x.example/?k=v\" } }",
    "bee::provider::invalid"
)]
#[case::empty_model("model=''\n[accounts.p]\nprovider='openai'", "bee::profile::setting")]
#[case::model_wrong_type("model=4\n[accounts.p]\nprovider='openai'", "bee::profile::setting")]
#[case::models_not_an_array(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\nmodels = \"a\"",
    "bee::provider::wrong_type"
)]
#[case::blank_model_in_list(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\nmodels = [\"ok\", \" \"]",
    "bee::provider::invalid"
)]
#[case::header_name_with_space(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\nheaders = { \"Bad Name\" = \"v\" }",
    "bee::provider::invalid"
)]
#[case::header_value_with_newline(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\nheaders = { \"X-A\" = \"v\\r\\nX-B: w\" }",
    "bee::provider::invalid"
)]
#[case::header_value_not_a_string(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\nheaders = { \"X-A\" = 1 }",
    "bee::provider::wrong_type"
)]
#[case::headers_differing_by_case(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\nheaders = { \"X-A\" = \"1\", \"x-a\" = \"2\" }",
    "bee::provider::invalid"
)]
#[case::credential_without_a_source(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\ncredential = {}",
    "bee::provider::invalid"
)]
#[case::credential_with_an_unknown_source(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\ncredential = { file = \"/x\" }",
    "bee::provider::unknown_key"
)]
#[case::credential_that_is_a_literal_key(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\ncredential = { env = \"sk-abc-123\" }",
    "bee::provider::invalid"
)]
#[case::credential_with_two_sources(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\ncredential = { env = \"K\", store = true }",
    "bee::provider::invalid"
)]
#[case::credential_store_disabled(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\ncredential = { store = false }",
    "bee::provider::invalid"
)]
#[case::credential_store_of_the_wrong_type(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\ncredential = { store = \"yes\" }",
    "bee::provider::wrong_type"
)]
#[case::login_without_the_store(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\ncredential = { env = \"K\", login = \"openrouter\" }",
    "bee::provider::invalid"
)]
#[case::login_alone(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\ncredential = { login = \"openrouter\" }",
    "bee::provider::invalid"
)]
#[case::login_that_is_unknown(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\ncredential = { store = true, login = \"cloudflare\" }",
    "bee::provider::unknown_login"
)]
#[case::option_of_the_other_protocol(
    "[accounts.p]\nprovider='custom'\nprotocols = { openai-chat = {base_url='https://gateway.example'} }\n[accounts.p.options]\nmax_tokens = 5",
    "bee::provider::invalid"
)]
#[case::bad_option_value(
    "[accounts.p]\nprovider='custom'\n[accounts.p.protocols.openai-chat]\nbase_url='https://gateway.example'\n[accounts.p.protocols.openai-chat.options]\nmax_tokens_field = \"x\"",
    "bee::provider::invalid"
)]
#[case::option_wrong_type(
    "[accounts.p]\nprovider='custom'\n[accounts.p.protocols.anthropic-messages]\nbase_url='https://gateway.example'\n[accounts.p.protocols.anthropic-messages.options]\nmax_tokens = \"5\"",
    "bee::provider::wrong_type"
)]
#[case::option_out_of_range(
    "[accounts.p]\nprovider='custom'\n[accounts.p.protocols.anthropic-messages]\nbase_url='https://gateway.example'\n[accounts.p.protocols.anthropic-messages.options]\nmax_tokens = 0",
    "bee::provider::invalid"
)]
#[case::default_that_is_not_defined(
    "model='ghost/model'\n[accounts.p]\nprovider='openai'",
    "bee::profile::setting"
)]
#[case::default_without_any_account("model='ghost/model'", "bee::profile::setting")]
#[case::default_wrong_type("model=1\n[accounts.p]\nprovider='openai'", "bee::profile::setting")]
#[case::undeclared_default(
    "model='p/missing'\n[accounts.p]\nprovider='openai'\ndiscovery=false\nmodels=['declared']",
    "bee::profile::setting"
)]
fn an_invalid_configuration_is_refused_with_its_own_code(#[case] text: &str, #[case] code: &str) {
    let error = load(text).unwrap_err();

    let text = rendered(error);
    assert!(text.contains(code), "expected {code}, got:\n{text}");
}

#[test]
fn an_invalid_value_names_its_key_and_file_so_it_can_be_found() {
    let sandbox = Sandbox::new();
    sandbox.write(
        &sandbox.project(),
        "[accounts.work]\nprovider='custom'\nprotocols = { openai-chat = { base_url = \"nope\" } }",
    );

    let text = rendered(sandbox.providers().unwrap_err());

    assert!(
        text.contains("accounts.work.protocols.openai-chat.base_url"),
        "{text}"
    );
    assert!(text.contains("bee.toml"), "{text}");
}

#[test]
fn an_unknown_protocol_lists_the_supported_ones() {
    let error = load("[accounts.p]\nprovider='custom'\nprotocols = { grpc = {base_url='https://gateway.example'} }").unwrap_err();
    // The help is the one place the list appears, and the renderer wraps it at
    // the terminal width, even inside a hyphenated name.
    let text = rendered(error).split_whitespace().collect::<String>();

    for name in ["openai-chat", "openai-responses", "anthropic-messages"] {
        assert!(text.contains(name), "{name} in {text}");
    }
}

#[rstest]
#[case::protocol_name(format!("[accounts.p]\nprovider='custom'\nprotocols = {{ \"{SECRET}\" = {{base_url='https://gateway.example'}} }}"))]
#[case::prefer(format!("[accounts.p]\nprovider='custom'\nprotocols = {{ openai-chat = {{base_url='https://gateway.example'}} }}\nprefer = [\"{SECRET}\"]"))]
#[case::base_url(format!("[accounts.p]\nprovider='custom'\nprotocols = {{ openai-chat = {{ base_url = \"https://u:{SECRET}@x.example\" }} }}"))]
#[case::credential(format!("[accounts.p]\nprovider='custom'\nprotocols = {{ openai-chat = {{base_url='https://gateway.example'}} }}\ncredential = {{ env = \"{SECRET}\" }}"))]
#[case::login(format!("[accounts.p]\nprovider='custom'\nprotocols = {{ openai-chat = {{base_url='https://gateway.example'}} }}\ncredential = {{ store = true, login = \"{SECRET}\" }}"))]
#[case::store(format!("[accounts.p]\nprovider='custom'\nprotocols = {{ openai-chat = {{base_url='https://gateway.example'}} }}\ncredential = {{ store = \"{SECRET}\" }}"))]
#[case::header(format!("[accounts.p]\nprovider='custom'\nprotocols = {{ openai-chat = {{base_url='https://gateway.example'}} }}\nheaders = {{ \"Bad Name\" = \"{SECRET}\" }}"))]
#[case::header_value(format!("[accounts.p]\nprovider='custom'\nprotocols = {{ openai-chat = {{base_url='https://gateway.example'}} }}\nheaders = {{ \"X-A\" = \"{SECRET}\\n\" }}"))]
#[case::wrong_type_key(format!("model=[\"{SECRET}\"]\n[accounts.p]\nprovider='openai'"))]
#[case::unknown_key_value(format!("[accounts.p]\nprovider='custom'\nprotocols = {{ openai-chat = {{base_url='https://gateway.example'}} }}\napi_key = \"{SECRET}\""))]
#[case::default(format!("model = \"{SECRET}/model\"\n[accounts.p]\nprovider='openai'"))]
fn no_diagnostic_repeats_a_value_from_the_file(#[case] text: String) {
    let error = load(&text).unwrap_err();

    let display = error.to_string();
    let rendered = rendered(error);
    assert!(!display.contains(SECRET), "{display}");
    assert!(!rendered.contains(SECRET), "{rendered}");
}

#[test]
fn direct_and_enclosing_provider_debug_output_hides_configured_strings() {
    let providers = load(&format!(
        "model='{SECRET}/{SECRET}'\n[accounts.{SECRET}]\nprovider='custom'\nmodels=['{SECRET}']\ncredential={{env='SECRET_ENV_SENTINEL'}}\nheaders = {{ \"{SECRET}\" = \"Bearer {SECRET}\" }}\n[accounts.{SECRET}.protocols.anthropic-messages]\nbase_url='https://gateway.example/{SECRET}'\noptions.version='{SECRET}'"
    ))
    .unwrap();
    let profile = providers.get(SECRET).unwrap();
    let endpoint = profile.preferred();
    for debug in [
        format!("{providers:?}"),
        format!("{profile:?}"),
        format!("{endpoint:?}"),
        format!("{:?}", endpoint.options()),
        format!("{:?}", profile.credential()),
        format!(
            "{:?}",
            CredentialSource::Store {
                login: Some(SECRET.into())
            }
        ),
    ] {
        assert!(!debug.contains(SECRET), "{debug}");
        assert!(!debug.contains("SECRET_ENV_SENTINEL"), "{debug}");
    }
    assert!(format!("{endpoint:?}").contains("AnthropicMessages"));
}
