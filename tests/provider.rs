//! Provider profiles read from the layered configuration, in-process.
//!
//! Every test describes its own environment and project directory, so none
//! touches a developer's real configuration.

use std::fs;
use std::path::{Path, PathBuf};

use bee::Error;
use bee::config::{Config, Environment, project_config_path};
use bee::provider::{
    CredentialSource, LoginFlow, MaxTokensField, Protocol, ProtocolOptions, Providers,
};
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
protocol = "openai-chat"
base_url = "https://gateway.corp.example/v1/"
model = "corp/ft-2026"

[providers.local]
protocol = "openai-chat"
base_url = "http://localhost:11434/v1"
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
        "[providers.anthropic]\nprotocol = \"openai-chat\"\nbase_url = \"http://proxy.local\"",
    )
    .unwrap();

    assert_eq!(
        providers.get("anthropic").unwrap().protocol(),
        Protocol::OpenAiChat
    );
}

#[rstest]
#[case::openai("openai-chat", "https://api.openai.com/v1")]
#[case::anthropic("anthropic-messages", "https://api.anthropic.com")]
fn the_base_url_defaults_to_the_protocols_and_is_overridable(
    #[case] protocol: &str,
    #[case] default_url: &str,
) {
    let defaulted = load(&format!("[providers.p]\nprotocol = \"{protocol}\"")).unwrap();
    let overridden = load(&format!(
        "[providers.p]\nprotocol = \"{protocol}\"\nbase_url = \"https://other.example\""
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
        "[providers.p]\nprotocol = \"openai-chat\"\nmodel = \"{model}\"\nmodels = [\"{model}\"]"
    ))
    .unwrap();

    assert_eq!(providers.get("p").unwrap().model(), Some(model));
    assert_eq!(providers.get("p").unwrap().models(), [model]);
}

#[test]
fn headers_and_options_belong_only_to_their_own_profile() {
    let providers = load(
        r#"
provider = "a"

[providers.a]
protocol = "openai-chat"
headers = { "X-Team" = "infra" }
[providers.a.options]
max_tokens_field = "max_completion_tokens"

[providers.b]
protocol = "anthropic-messages"
[providers.b.options]
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
    let providers =
        load("[providers.p]\nprotocol = \"openai-chat\"\ncredential = { env = \"GATEWAY_KEY\" }")
            .unwrap();

    assert_eq!(
        providers.get("p").unwrap().credential(),
        Some(&CredentialSource::Env("GATEWAY_KEY".to_owned()))
    );
}

#[rstest]
#[case::token_only("{ store = true }", None)]
#[case::browser_sign_in(
    "{ store = true, login = \"openrouter\" }",
    Some(LoginFlow::OpenRouter)
)]
fn a_credential_can_live_in_the_store(#[case] credential: &str, #[case] login: Option<LoginFlow>) {
    let providers = load(&format!(
        "[providers.p]\nprotocol = \"openai-chat\"\ncredential = {credential}"
    ))
    .unwrap();

    assert_eq!(
        providers.get("p").unwrap().credential(),
        Some(&CredentialSource::Store { login })
    );
}

#[test]
fn the_debug_output_names_the_kind_of_credential_and_nothing_else() {
    let providers =
        load("[providers.p]\nprotocol = \"openai-chat\"\ncredential = { env = \"GATEWAY_KEY\" }")
            .unwrap();

    let debug = format!("{providers:?}");

    assert!(debug.contains("credential: Some(\"env\")"), "{debug}");
    assert!(!debug.contains("GATEWAY_KEY"), "{debug}");
}

#[test]
fn a_single_profile_is_the_default_without_naming_it() {
    let providers = load("[providers.only]\nprotocol = \"openai-chat\"").unwrap();

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
protocol = "openai-chat"
base_url = "https://global.example/v1"
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
        "provider = \"a\"\n[providers.a]\nprotocol = \"openai-chat\"",
    );
    sandbox.write(
        &sandbox.project(),
        "provider = \"b\"\n[providers.b]\nprotocol = \"anthropic-messages\"",
    );

    let providers = sandbox.providers().unwrap();

    assert_eq!(providers.names().collect::<Vec<_>>(), ["a", "b"]);
    assert_eq!(providers.default_name(), Some("b"));
}

/// The diagnostic code an invalid configuration must produce.
#[rstest]
#[case::unknown_protocol(
    "[providers.p]\nprotocol = \"grpc\"",
    "bee::provider::unknown_protocol"
)]
#[case::protocol_by_name_only(
    "[providers.openai]\nbase_url = \"https://x.example\"",
    "bee::provider::missing_key"
)]
#[case::protocol_wrong_type("[providers.p]\nprotocol = 3", "bee::provider::wrong_type")]
#[case::providers_not_a_table("providers = \"x\"", "bee::provider::wrong_type")]
#[case::profile_not_a_table("[providers]\np = \"x\"", "bee::provider::wrong_type")]
#[case::misspelled_key(
    "[providers.p]\nprotocol = \"openai-chat\"\nbase_ulr = \"https://x.example\"",
    "bee::provider::unknown_key"
)]
#[case::bad_scheme(
    "[providers.p]\nprotocol = \"openai-chat\"\nbase_url = \"ftp://x.example\"",
    "bee::provider::invalid"
)]
#[case::url_without_host(
    "[providers.p]\nprotocol = \"openai-chat\"\nbase_url = \"https://\"",
    "bee::provider::invalid"
)]
#[case::url_with_userinfo(
    "[providers.p]\nprotocol = \"openai-chat\"\nbase_url = \"https://user:pw@x.example\"",
    "bee::provider::invalid"
)]
#[case::url_with_query(
    "[providers.p]\nprotocol = \"openai-chat\"\nbase_url = \"https://x.example/?k=v\"",
    "bee::provider::invalid"
)]
#[case::empty_model(
    "[providers.p]\nprotocol = \"openai-chat\"\nmodel = \"\"",
    "bee::provider::invalid"
)]
#[case::model_wrong_type(
    "[providers.p]\nprotocol = \"openai-chat\"\nmodel = 4",
    "bee::provider::wrong_type"
)]
#[case::models_not_an_array(
    "[providers.p]\nprotocol = \"openai-chat\"\nmodels = \"a\"",
    "bee::provider::wrong_type"
)]
#[case::blank_model_in_list(
    "[providers.p]\nprotocol = \"openai-chat\"\nmodels = [\"ok\", \" \"]",
    "bee::provider::invalid"
)]
#[case::header_name_with_space(
    "[providers.p]\nprotocol = \"openai-chat\"\nheaders = { \"Bad Name\" = \"v\" }",
    "bee::provider::invalid"
)]
#[case::header_value_with_newline(
    "[providers.p]\nprotocol = \"openai-chat\"\nheaders = { \"X-A\" = \"v\\r\\nX-B: w\" }",
    "bee::provider::invalid"
)]
#[case::header_value_not_a_string(
    "[providers.p]\nprotocol = \"openai-chat\"\nheaders = { \"X-A\" = 1 }",
    "bee::provider::wrong_type"
)]
#[case::headers_differing_by_case(
    "[providers.p]\nprotocol = \"openai-chat\"\nheaders = { \"X-A\" = \"1\", \"x-a\" = \"2\" }",
    "bee::provider::invalid"
)]
#[case::credential_without_a_source(
    "[providers.p]\nprotocol = \"openai-chat\"\ncredential = {}",
    "bee::provider::invalid"
)]
#[case::credential_with_an_unknown_source(
    "[providers.p]\nprotocol = \"openai-chat\"\ncredential = { file = \"/x\" }",
    "bee::provider::unknown_key"
)]
#[case::credential_that_is_a_literal_key(
    "[providers.p]\nprotocol = \"openai-chat\"\ncredential = { env = \"sk-abc-123\" }",
    "bee::provider::invalid"
)]
#[case::credential_with_two_sources(
    "[providers.p]\nprotocol = \"openai-chat\"\ncredential = { env = \"K\", store = true }",
    "bee::provider::invalid"
)]
#[case::credential_store_disabled(
    "[providers.p]\nprotocol = \"openai-chat\"\ncredential = { store = false }",
    "bee::provider::invalid"
)]
#[case::credential_store_of_the_wrong_type(
    "[providers.p]\nprotocol = \"openai-chat\"\ncredential = { store = \"yes\" }",
    "bee::provider::wrong_type"
)]
#[case::login_without_the_store(
    "[providers.p]\nprotocol = \"openai-chat\"\ncredential = { env = \"K\", login = \"openrouter\" }",
    "bee::provider::invalid"
)]
#[case::login_alone(
    "[providers.p]\nprotocol = \"openai-chat\"\ncredential = { login = \"openrouter\" }",
    "bee::provider::invalid"
)]
#[case::login_that_is_unknown(
    "[providers.p]\nprotocol = \"openai-chat\"\ncredential = { store = true, login = \"cloudflare\" }",
    "bee::provider::invalid"
)]
#[case::option_of_the_other_protocol(
    "[providers.p]\nprotocol = \"openai-chat\"\n[providers.p.options]\nmax_tokens = 5",
    "bee::provider::unknown_key"
)]
#[case::bad_option_value(
    "[providers.p]\nprotocol = \"openai-chat\"\n[providers.p.options]\nmax_tokens_field = \"x\"",
    "bee::provider::invalid"
)]
#[case::option_wrong_type(
    "[providers.p]\nprotocol = \"anthropic-messages\"\n[providers.p.options]\nmax_tokens = \"5\"",
    "bee::provider::wrong_type"
)]
#[case::option_out_of_range(
    "[providers.p]\nprotocol = \"anthropic-messages\"\n[providers.p.options]\nmax_tokens = 0",
    "bee::provider::invalid"
)]
#[case::default_that_is_not_defined(
    "provider = \"ghost\"\n[providers.p]\nprotocol = \"openai-chat\"",
    "bee::provider::unknown_default"
)]
#[case::default_without_any_profile("provider = \"ghost\"", "bee::provider::unknown_default")]
#[case::default_wrong_type(
    "provider = 1\n[providers.p]\nprotocol = \"openai-chat\"",
    "bee::provider::wrong_type"
)]
#[case::several_profiles_and_no_default(
    "[providers.a]\nprotocol = \"openai-chat\"\n[providers.b]\nprotocol = \"openai-chat\"",
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
        "[providers.work]\nprotocol = \"openai-chat\"\nbase_url = \"nope\"",
    );

    let text = rendered(sandbox.providers().unwrap_err());

    assert!(text.contains("providers.work.base_url"), "{text}");
    assert!(text.contains("bee.toml"), "{text}");
}

#[test]
fn an_unknown_protocol_lists_the_supported_ones() {
    let text = rendered(load("[providers.p]\nprotocol = \"grpc\"").unwrap_err());

    assert!(text.contains("openai-chat"), "{text}");
    assert!(text.contains("anthropic-messages"), "{text}");
}

#[rstest]
#[case::protocol(format!("[providers.p]\nprotocol = \"{SECRET}\""))]
#[case::base_url(format!("[providers.p]\nprotocol = \"openai-chat\"\nbase_url = \"https://u:{SECRET}@x.example\""))]
#[case::credential(format!("[providers.p]\nprotocol = \"openai-chat\"\ncredential = {{ env = \"{SECRET}\" }}"))]
#[case::login(format!("[providers.p]\nprotocol = \"openai-chat\"\ncredential = {{ store = true, login = \"{SECRET}\" }}"))]
#[case::store(format!("[providers.p]\nprotocol = \"openai-chat\"\ncredential = {{ store = \"{SECRET}\" }}"))]
#[case::header(format!("[providers.p]\nprotocol = \"openai-chat\"\nheaders = {{ \"Bad Name\" = \"{SECRET}\" }}"))]
#[case::header_value(format!("[providers.p]\nprotocol = \"openai-chat\"\nheaders = {{ \"X-A\" = \"{SECRET}\\n\" }}"))]
#[case::wrong_type_key(format!("[providers.p]\nprotocol = \"openai-chat\"\nmodel = [\"{SECRET}\"]"))]
#[case::unknown_key_value(format!("[providers.p]\nprotocol = \"openai-chat\"\napi_key = \"{SECRET}\""))]
#[case::default(format!("provider = \"{SECRET}\"\n[providers.p]\nprotocol = \"openai-chat\""))]
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
        "[providers.p]\nprotocol = \"openai-chat\"\nheaders = {{ \"Authorization\" = \"Bearer {SECRET}\" }}"
    ))
    .unwrap();

    let debug = format!("{providers:?}");
    assert!(
        debug.contains("Authorization"),
        "names stay visible: {debug}"
    );
    assert!(!debug.contains(SECRET), "{debug}");
}
