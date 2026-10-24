//! Provider profiles read from the layered configuration, in-process.
//!
//! Every test describes its own environment and project directory, so none
//! touches a developer's real configuration.

use std::fs;
use std::path::{Path, PathBuf};

use bee::Error;
use bee::config::{Config, Environment, project_config_path};
use bee::provider::{CredentialSource, MaxTokensField, Protocol, ProtocolOptions, Providers};
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
        Providers::from_config(&Config::load(&env, &self.project_dir()).unwrap())
    }
}

/// Providers from a project file holding `text`.
fn load(text: &str) -> bee::Result<Providers> {
    let sandbox = Sandbox::new();
    sandbox.write(&sandbox.project(), text);
    sandbox.providers()
}

#[test]
fn exact_model_mappings_override_preference_without_restricting_unknown_models() {
    let providers = load(r#"
[providers.gateway]
prefer = ["openai-chat"]
model = "vendor/model.v2"
models = ["suggested"]
model_protocols = { "vendor/model.v2" = "openai-responses", "模型 / custom:1" = "anthropic-messages" }
protocols = { openai-chat = {}, openai-responses = {}, anthropic-messages = {} }
"#).unwrap();
    let profile = providers.get("gateway").unwrap();
    assert_eq!(
        profile
            .endpoint_for_model(profile.default_model().unwrap())
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
    assert_eq!(profile.suggested_models(), ["vendor/model.v2", "suggested"]);
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
            "[providers.p]\nprotocols={{openai-chat={{}}}}\nmodel_protocols={mapping}"
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
    sandbox.write(&sandbox.global(), "[providers.p]\nprefer=['openai-chat']\nprotocols={openai-chat={},openai-responses={},anthropic-messages={}}\nmodel_protocols={'vendor/a.v2'='openai-chat',other='anthropic-messages'}");
    let selected = sandbox.root.path().join("selected.toml");
    sandbox.write(
        &selected,
        "[providers.p.model_protocols]\n'vendor/a.v2'='openai-responses'",
    );
    sandbox.write(&sandbox.project(), "[providers.p]\nmodel_protocols={}");
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

    sandbox.write(&sandbox.global(), &format!("[providers.p]\nprotocols={{openai-chat={{}}}}\nmodel_protocols={{'{SECRET}'='openai-responses'}}"));
    // An unrelated contribution must not move the invalid entry's origin.
    sandbox.write(
        &sandbox.project(),
        "[providers.p.model_protocols]\nother='openai-chat'",
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
        &format!("[providers.p.model_protocols]\n'{SECRET}'=1"),
    );
    let error = Config::load(&env, &sandbox.project_dir()).unwrap_err();
    assert!(matches!(error, Error::ConfigConflict { .. }));
    assert!(!format!("{error} {error:?}").contains(SECRET));
}

#[test]
fn inference_limits_are_positive_and_work_in_presets() {
    let providers = load("[presets.local]\ntimeout_ms=456\nmax_output_tokens=123\n[ presets.local.protocols.openai-chat ]\n[providers.work]\npreset='local'\n").unwrap();
    let profile = providers.get("work").unwrap();
    assert_eq!(profile.timeout(), std::time::Duration::from_millis(456));
    assert_eq!(profile.max_output_tokens(), 123);
    for field in ["timeout_ms", "max_output_tokens"] {
        for value in ["0", "-1", "4294967296", "'sk-secret'"] {
            let error = load(&format!(
                "[providers.work]\n{field}={value}\n[providers.work.protocols.openai-chat]"
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
    assert_eq!(providers.default_name(), None);
}

#[test]
fn several_profiles_can_share_one_protocol_with_their_own_endpoints() {
    let providers = load(
        r#"
provider = "work"

[providers.work]
protocols = { openai-chat = { base_url = "https://gateway.corp.example/v1/" } }
model = "corp/ft-2026"

[providers.local]
protocols = { openai-chat = { base_url = "http://localhost:11434/v1" } }
model = "llama3:8b"
"#,
    )
    .unwrap();

    let work = providers.get("work").unwrap();
    let local = providers.get("local").unwrap();
    assert_eq!(work.protocol(), Protocol::OpenAiChat);
    assert_eq!(local.protocol(), Protocol::OpenAiChat);
    assert_eq!(work.base_url(), "https://gateway.corp.example/v1");
    assert_eq!(local.base_url(), "http://localhost:11434/v1");
    assert_eq!(providers.default_name(), Some("work"));
}

#[test]
fn a_profile_name_says_nothing_about_its_protocol() {
    // Named like a commercial provider, and not speaking its protocol.
    let providers = load(
        "[providers.anthropic]\nprotocols = { openai-chat = { base_url = \"http://proxy.local\" } }",
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
fn the_base_url_defaults_to_the_protocols_and_is_overridable(
    #[case] protocol: &str,
    #[case] default_url: &str,
) {
    let defaulted = load(&format!("[providers.p.protocols.{protocol}]")).unwrap();
    let overridden = load(&format!(
        "[providers.p.protocols.{protocol}]\nbase_url = \"https://other.example\""
    ))
    .unwrap();

    assert_eq!(defaulted.get("p").unwrap().base_url(), default_url);
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
fn any_model_id_is_accepted_without_a_catalog_or_an_allowlist(#[case] model: &str) {
    let providers = load(&format!(
        "[providers.p]\nprotocols = {{ openai-chat = {{}} }}\nmodel = \"{model}\"\nmodels = [\"{model}\"]"
    ))
    .unwrap();

    assert_eq!(providers.get("p").unwrap().model(), Some(model));
    assert_eq!(providers.get("p").unwrap().models(), [model]);
}

/// A profile that speaks both protocols, each at its own URL.
const BOTH: &str = r#"
[providers.p]
prefer = ["anthropic-messages", "openai-chat"]

[providers.p.protocols.openai-chat]
base_url = "https://gw.example/openai/v1"
options = { max_tokens_field = "max_completion_tokens" }

[providers.p.protocols.anthropic-messages]
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
            max_tokens_field: MaxTokensField::MaxCompletionTokens
        }
    );
    assert_eq!(
        *anthropic.options(),
        ProtocolOptions::AnthropicMessages {
            version: None,
            max_tokens: Some(2048)
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
        "[providers.p]\nprefer = [\"anthropic-messages\"]\n\
         [providers.p.protocols.openai-chat]\n[providers.p.protocols.anthropic-messages]",
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
    assert!(load("[providers.p.protocols.openai-chat]").is_ok());
    assert!(
        load("[providers.p]\nprefer = [\"openai-chat\"]\n[providers.p.protocols.openai-chat]")
            .is_ok()
    );
}

#[test]
fn the_responses_protocol_takes_no_options_and_says_so() {
    let text = rendered(
        load("[providers.p.protocols.openai-responses.options]\nmax_tokens = 5").unwrap_err(),
    );

    assert!(text.contains("bee::provider::unknown_key"), "{text}");
    assert!(text.contains("no keys"), "{text}");
}

#[test]
fn the_responses_protocol_has_no_settings_and_an_empty_options_table_is_fine() {
    let providers = load("[providers.p.protocols.openai-responses]\noptions = {}").unwrap();

    assert_eq!(
        *providers.get("p").unwrap().options(),
        ProtocolOptions::OpenAiResponses
    );
}

#[test]
fn protocols_that_prefer_leaves_out_follow_in_the_order_of_protocol_all() {
    let providers = load(
        "[providers.p]\nprefer = [\"anthropic-messages\"]\n\
         [providers.p.protocols.openai-responses]\n[providers.p.protocols.openai-chat]\n\
         [providers.p.protocols.anthropic-messages]",
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
    let providers = load("[providers.p.protocols.openai-chat]").unwrap();

    assert!(
        providers
            .get("p")
            .unwrap()
            .endpoint(Protocol::AnthropicMessages)
            .is_none()
    );
}

#[test]
fn headers_and_options_belong_only_to_their_own_profile() {
    let providers = load(
        r#"
provider = "a"

[providers.a]
headers = { "X-Team" = "infra" }
[providers.a.protocols.openai-chat.options]
max_tokens_field = "max_completion_tokens"

[providers.b.protocols.anthropic-messages.options]
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
            max_tokens_field: MaxTokensField::MaxCompletionTokens
        }
    );
    assert_eq!(
        *b.options(),
        ProtocolOptions::AnthropicMessages {
            version: Some("2023-06-01".to_owned()),
            max_tokens: Some(4096)
        }
    );
}

#[test]
fn a_credential_is_a_named_environment_variable() {
    let providers = load(
        "[providers.p]\nprotocols = { openai-chat = {} }\ncredential = { env = \"GATEWAY_KEY\" }",
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
        "[providers.p]\nprotocols = {{ openai-chat = {{}} }}\ncredential = {credential}"
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
        "[providers.p]\nprotocols = { openai-chat = {} }\ncredential = { env = \"GATEWAY_KEY\" }",
    )
    .unwrap();

    let debug = format!("{providers:?}");

    assert!(debug.contains("credential: Some(\"env\")"), "{debug}");
    assert!(!debug.contains("GATEWAY_KEY"), "{debug}");
}

#[test]
fn a_single_profile_is_the_default_without_naming_it() {
    let providers = load("[providers.only]\nprotocols = { openai-chat = {} }").unwrap();

    assert_eq!(providers.default_name(), Some("only"));
}

#[test]
fn the_project_file_overrides_the_global_one_key_by_key() {
    let sandbox = Sandbox::new();
    sandbox.write(
        &sandbox.global(),
        r#"
provider = "gw"
[providers.gw]
protocols = { openai-chat = { base_url = "https://global.example/v1" } }
model = "global-model"
models = ["g1", "g2"]
headers = { "X-Org" = "global", "X-Keep" = "yes" }
"#,
    );
    sandbox.write(
        &sandbox.project(),
        r#"
[providers.gw]
model = "project-model"
models = ["p1"]
headers = { "X-Org" = "project" }
"#,
    );

    let providers = sandbox.providers().unwrap();

    let gw = providers.get("gw").unwrap();
    // Scalars: the higher layer wins, the rest is inherited.
    assert_eq!(gw.model(), Some("project-model"));
    assert_eq!(gw.base_url(), "https://global.example/v1");
    // Arrays are replaced, never concatenated (ADR-006).
    assert_eq!(gw.models(), ["p1"]);
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
fn a_project_can_add_a_profile_and_choose_it_as_the_default() {
    let sandbox = Sandbox::new();
    sandbox.write(
        &sandbox.global(),
        "provider = \"a\"\n[providers.a]\nprotocols = { openai-chat = {} }",
    );
    sandbox.write(
        &sandbox.project(),
        "provider = \"b\"\n[providers.b]\nprotocols = { anthropic-messages = {} }",
    );

    let providers = sandbox.providers().unwrap();

    assert_eq!(providers.names().collect::<Vec<_>>(), ["a", "b"]);
    assert_eq!(providers.default_name(), Some("b"));
}

/// The diagnostic code an invalid configuration must produce.
#[rstest]
#[case::unknown_protocol(
    "[providers.p]\nprotocols = { grpc = {} }",
    "bee::provider::unknown_protocol"
)]
#[case::protocols_by_name_only(
    "[providers.openai]\nmodel = \"gpt-4o\"",
    "bee::provider::missing_key"
)]
#[case::protocols_wrong_type("[providers.p]\nprotocols = 3", "bee::provider::wrong_type")]
#[case::protocols_as_a_list_of_names(
    "[providers.p]\nprotocols = [\"openai-chat\"]",
    "bee::provider::wrong_type"
)]
#[case::no_protocol_listed("[providers.p]\nprotocols = {}", "bee::provider::invalid")]
#[case::protocol_entry_not_a_table(
    "[providers.p]\nprotocols = { openai-chat = \"https://x.example\" }",
    "bee::provider::wrong_type"
)]
#[case::the_singular_protocol_key(
    "[providers.p]\nprotocol = \"openai-chat\"",
    "bee::provider::unknown_key"
)]
#[case::base_url_beside_protocols(
    "[providers.p]\nprotocols = { openai-chat = {} }\nbase_url = \"https://x.example\"",
    "bee::provider::unknown_key"
)]
#[case::options_beside_protocols(
    "[providers.p]\nprotocols = { openai-chat = {} }\noptions = {}",
    "bee::provider::unknown_key"
)]
#[case::several_protocols_without_a_preference(
    "[providers.p]\nprotocols = { openai-chat = {}, anthropic-messages = {} }",
    "bee::provider::no_preference"
)]
#[case::prefer_names_an_unknown_protocol(
    "[providers.p]\nprotocols = { openai-chat = {} }\nprefer = [\"grpc\"]",
    "bee::provider::unknown_protocol"
)]
#[case::prefer_names_a_protocol_the_profile_lacks(
    "[providers.p]\nprotocols = { openai-chat = {} }\nprefer = [\"anthropic-messages\"]",
    "bee::provider::invalid"
)]
#[case::prefer_repeats_a_protocol(
    "[providers.p]\nprotocols = { openai-chat = {} }\nprefer = [\"openai-chat\", \"openai-chat\"]",
    "bee::provider::invalid"
)]
#[case::prefer_not_an_array(
    "[providers.p]\nprotocols = { openai-chat = {} }\nprefer = \"openai-chat\"",
    "bee::provider::wrong_type"
)]
#[case::an_option_of_a_protocol_with_none(
    "[providers.p]\nprotocols = { openai-responses = { options = { max_tokens_field = \"max_tokens\" } } }",
    "bee::provider::unknown_key"
)]
#[case::all_three_protocols_without_a_preference(
    "[providers.p]\nprotocols = { openai-chat = {}, openai-responses = {}, anthropic-messages = {} }",
    "bee::provider::no_preference"
)]
#[case::an_endpoint_with_an_unknown_key(
    "[providers.p]\nprotocols = { openai-chat = { version = \"1\" } }",
    "bee::provider::unknown_key"
)]
#[case::an_option_of_another_protocols_endpoint(
    "[providers.p]\nprotocols = { openai-chat = { options = { max_tokens = 5 } } }",
    "bee::provider::unknown_key"
)]
#[case::providers_not_a_table("providers = \"x\"", "bee::provider::wrong_type")]
#[case::profile_not_a_table("[providers]\np = \"x\"", "bee::provider::wrong_type")]
#[case::misspelled_key(
    "[providers.p]\nprotocols = { openai-chat = {} }\nbase_ulr = \"https://x.example\"",
    "bee::provider::unknown_key"
)]
#[case::bad_scheme(
    "[providers.p]\nprotocols = { openai-chat = { base_url = \"ftp://x.example\" } }",
    "bee::provider::invalid"
)]
#[case::url_without_host(
    "[providers.p]\nprotocols = { openai-chat = { base_url = \"https://\" } }",
    "bee::provider::invalid"
)]
#[case::url_with_userinfo(
    "[providers.p]\nprotocols = { openai-chat = { base_url = \"https://user:pw@x.example\" } }",
    "bee::provider::invalid"
)]
#[case::url_with_query(
    "[providers.p]\nprotocols = { openai-chat = { base_url = \"https://x.example/?k=v\" } }",
    "bee::provider::invalid"
)]
#[case::empty_model(
    "[providers.p]\nprotocols = { openai-chat = {} }\nmodel = \"\"",
    "bee::provider::invalid"
)]
#[case::model_wrong_type(
    "[providers.p]\nprotocols = { openai-chat = {} }\nmodel = 4",
    "bee::provider::wrong_type"
)]
#[case::models_not_an_array(
    "[providers.p]\nprotocols = { openai-chat = {} }\nmodels = \"a\"",
    "bee::provider::wrong_type"
)]
#[case::blank_model_in_list(
    "[providers.p]\nprotocols = { openai-chat = {} }\nmodels = [\"ok\", \" \"]",
    "bee::provider::invalid"
)]
#[case::header_name_with_space(
    "[providers.p]\nprotocols = { openai-chat = {} }\nheaders = { \"Bad Name\" = \"v\" }",
    "bee::provider::invalid"
)]
#[case::header_value_with_newline(
    "[providers.p]\nprotocols = { openai-chat = {} }\nheaders = { \"X-A\" = \"v\\r\\nX-B: w\" }",
    "bee::provider::invalid"
)]
#[case::header_value_not_a_string(
    "[providers.p]\nprotocols = { openai-chat = {} }\nheaders = { \"X-A\" = 1 }",
    "bee::provider::wrong_type"
)]
#[case::headers_differing_by_case(
    "[providers.p]\nprotocols = { openai-chat = {} }\nheaders = { \"X-A\" = \"1\", \"x-a\" = \"2\" }",
    "bee::provider::invalid"
)]
#[case::credential_without_a_source(
    "[providers.p]\nprotocols = { openai-chat = {} }\ncredential = {}",
    "bee::provider::invalid"
)]
#[case::credential_with_an_unknown_source(
    "[providers.p]\nprotocols = { openai-chat = {} }\ncredential = { file = \"/x\" }",
    "bee::provider::unknown_key"
)]
#[case::credential_that_is_a_literal_key(
    "[providers.p]\nprotocols = { openai-chat = {} }\ncredential = { env = \"sk-abc-123\" }",
    "bee::provider::invalid"
)]
#[case::credential_with_two_sources(
    "[providers.p]\nprotocols = { openai-chat = {} }\ncredential = { env = \"K\", store = true }",
    "bee::provider::invalid"
)]
#[case::credential_store_disabled(
    "[providers.p]\nprotocols = { openai-chat = {} }\ncredential = { store = false }",
    "bee::provider::invalid"
)]
#[case::credential_store_of_the_wrong_type(
    "[providers.p]\nprotocols = { openai-chat = {} }\ncredential = { store = \"yes\" }",
    "bee::provider::wrong_type"
)]
#[case::login_without_the_store(
    "[providers.p]\nprotocols = { openai-chat = {} }\ncredential = { env = \"K\", login = \"openrouter\" }",
    "bee::provider::invalid"
)]
#[case::login_alone(
    "[providers.p]\nprotocols = { openai-chat = {} }\ncredential = { login = \"openrouter\" }",
    "bee::provider::invalid"
)]
#[case::login_that_is_unknown(
    "[providers.p]\nprotocols = { openai-chat = {} }\ncredential = { store = true, login = \"cloudflare\" }",
    "bee::provider::unknown_login"
)]
#[case::option_of_the_other_protocol(
    "[providers.p]\nprotocols = { openai-chat = {} }\n[providers.p.options]\nmax_tokens = 5",
    "bee::provider::unknown_key"
)]
#[case::bad_option_value(
    "[providers.p.protocols.openai-chat.options]\nmax_tokens_field = \"x\"",
    "bee::provider::invalid"
)]
#[case::option_wrong_type(
    "[providers.p.protocols.anthropic-messages.options]\nmax_tokens = \"5\"",
    "bee::provider::wrong_type"
)]
#[case::option_out_of_range(
    "[providers.p.protocols.anthropic-messages.options]\nmax_tokens = 0",
    "bee::provider::invalid"
)]
#[case::default_that_is_not_defined(
    "provider = \"ghost\"\n[providers.p]\nprotocols = { openai-chat = {} }",
    "bee::provider::unknown_default"
)]
#[case::default_without_any_profile("provider = \"ghost\"", "bee::provider::unknown_default")]
#[case::default_wrong_type(
    "provider = 1\n[providers.p]\nprotocols = { openai-chat = {} }",
    "bee::provider::wrong_type"
)]
#[case::several_profiles_and_no_default(
    "[providers.a]\nprotocols = { openai-chat = {} }\n[providers.b]\nprotocols = { openai-chat = {} }",
    "bee::provider::no_default"
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
        "[providers.work]\nprotocols = { openai-chat = { base_url = \"nope\" } }",
    );

    let text = rendered(sandbox.providers().unwrap_err());

    assert!(
        text.contains("providers.work.protocols.openai-chat.base_url"),
        "{text}"
    );
    assert!(text.contains("bee.toml"), "{text}");
}

#[test]
fn an_unknown_protocol_lists_the_supported_ones() {
    let error = load("[providers.p]\nprotocols = { grpc = {} }").unwrap_err();
    // The help is the one place the list appears, and the renderer wraps it at
    // the terminal width, even inside a hyphenated name.
    let text = rendered(error).split_whitespace().collect::<String>();

    for name in ["openai-chat", "openai-responses", "anthropic-messages"] {
        assert!(text.contains(name), "{name} in {text}");
    }
}

#[rstest]
#[case::protocol_name(format!("[providers.p]\nprotocols = {{ \"{SECRET}\" = {{}} }}"))]
#[case::prefer(format!("[providers.p]\nprotocols = {{ openai-chat = {{}} }}\nprefer = [\"{SECRET}\"]"))]
#[case::base_url(format!("[providers.p]\nprotocols = {{ openai-chat = {{ base_url = \"https://u:{SECRET}@x.example\" }} }}"))]
#[case::credential(format!("[providers.p]\nprotocols = {{ openai-chat = {{}} }}\ncredential = {{ env = \"{SECRET}\" }}"))]
#[case::login(format!("[providers.p]\nprotocols = {{ openai-chat = {{}} }}\ncredential = {{ store = true, login = \"{SECRET}\" }}"))]
#[case::store(format!("[providers.p]\nprotocols = {{ openai-chat = {{}} }}\ncredential = {{ store = \"{SECRET}\" }}"))]
#[case::header(format!("[providers.p]\nprotocols = {{ openai-chat = {{}} }}\nheaders = {{ \"Bad Name\" = \"{SECRET}\" }}"))]
#[case::header_value(format!("[providers.p]\nprotocols = {{ openai-chat = {{}} }}\nheaders = {{ \"X-A\" = \"{SECRET}\\n\" }}"))]
#[case::wrong_type_key(format!("[providers.p]\nprotocols = {{ openai-chat = {{}} }}\nmodel = [\"{SECRET}\"]"))]
#[case::unknown_key_value(format!("[providers.p]\nprotocols = {{ openai-chat = {{}} }}\napi_key = \"{SECRET}\""))]
#[case::default(format!("provider = \"{SECRET}\"\n[providers.p]\nprotocols = {{ openai-chat = {{}} }}"))]
fn no_diagnostic_repeats_a_value_from_the_file(#[case] text: String) {
    let error = load(&text).unwrap_err();

    let display = error.to_string();
    let rendered = rendered(error);
    assert!(!display.contains(SECRET), "{display}");
    assert!(!rendered.contains(SECRET), "{rendered}");
}

#[test]
fn the_debug_output_of_providers_hides_header_values() {
    let providers = load(&format!(
        "[providers.p]\nprotocols = {{ openai-chat = {{}} }}\nheaders = {{ \"Authorization\" = \"Bearer {SECRET}\" }}"
    ))
    .unwrap();

    let debug = format!("{providers:?}");
    assert!(
        debug.contains("Authorization"),
        "names stay visible: {debug}"
    );
    assert!(!debug.contains(SECRET), "{debug}");
}
