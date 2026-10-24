//! Presets and login flows as data (ADR-009), in-process.
//!
//! Every test describes its own environment, so none reads a developer's real
//! configuration, and HTTP is a fake.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

use bee::Error;
use bee::config::{Config, Environment, project_config_path};
use bee::http::{Http, HttpResponse, Unreachable};
use bee::login;
use bee::preset::Catalog;
use bee::provider::{CredentialSource, Protocol, Providers};
use pretty_assertions::assert_eq;
use rstest::rstest;
use tempfile::TempDir;

/// A secret that must never appear in any diagnostic or debug output.
const SECRET: &str = "sk-super-secret-token";

#[test]
fn preset_model_mappings_merge_with_profile_overrides_and_added_endpoints() {
    let sandbox = Sandbox::new();
    sandbox.write(&sandbox.global(), "[presets.gateway]\nprefer=['openai-chat']\nprotocols={openai-chat={}}\nmodel_protocols={'vendor/model.v2'='openai-chat',inherited='openai-chat'}");
    sandbox.write(
        &sandbox.selected(),
        "[presets.gateway.model_protocols]\nadded='openai-responses'",
    );
    sandbox.write(&project_config_path(&sandbox.project_dir()), "[providers.p]\npreset='gateway'\nmodel_protocols={'vendor/model.v2'='openai-responses'}\nprotocols={openai-responses={}}");
    let providers = sandbox.providers().unwrap();
    let profile = providers.get("p").unwrap();
    for model in ["vendor/model.v2", "added"] {
        assert_eq!(
            profile.endpoint_for_model(model).protocol(),
            Protocol::OpenAiResponses
        );
    }
    assert_eq!(
        profile.endpoint_for_model("inherited").protocol(),
        Protocol::OpenAiChat
    );
}

#[test]
fn invalid_inherited_model_mappings_name_the_file_that_supplied_the_entry() {
    let sandbox = Sandbox::new();
    sandbox.write(&sandbox.global(), &format!("[presets.gateway]\nprotocols={{openai-chat={{}}}}\nmodel_protocols={{'{SECRET}'='openai-responses'}}"));
    sandbox.write(
        &sandbox.selected(),
        "[presets.gateway.model_protocols]\nother='openai-chat'",
    );
    sandbox.write(
        &project_config_path(&sandbox.project_dir()),
        "[providers.p]\npreset='gateway'",
    );
    let error = sandbox.providers().unwrap_err();
    assert!(
        error
            .to_string()
            .contains(&sandbox.global().display().to_string())
    );
    assert!(!format!("{error} {error:?}").contains(SECRET));
}

/// A login flow that shares no parameter name with the shipped one.
const ACME_FLOW: &str = r#"
[login_flows.acme]
kind = "pkce-key"
authorize_url = "https://id.acme.example/authorize"
exchange_url = "https://id.acme.example/v2/token"
headless = true

[login_flows.acme.authorize]
callback = "redirect"
challenge = "chal"
challenge_method = "meth"
state = "st"
extra = { app = "bee tests" }

[login_flows.acme.exchange]
code = "authorization_code"
verifier = "proof"
challenge_method = "meth"
response_key = "api_key"
"#;

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

    fn selected(&self) -> PathBuf {
        self.root.path().join("selected.toml")
    }

    fn project_dir(&self) -> PathBuf {
        self.root.path().join("project")
    }

    fn write(&self, path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn providers(&self) -> bee::Result<Providers> {
        fs::create_dir_all(self.project_dir()).unwrap();
        let env = Environment::new(
            None,
            Some(self.root.path().join("home")),
            self.selected().exists().then(|| self.selected()),
        );
        Providers::from_config(&Config::load(&env, &self.project_dir()).unwrap())
    }
}

/// Providers from a global file holding `text`.
fn global(text: &str) -> bee::Result<Providers> {
    let sandbox = Sandbox::new();
    sandbox.write(&sandbox.global(), text);
    sandbox.providers()
}

/// Providers from a project file holding `text`.
fn project(text: &str) -> bee::Result<Providers> {
    let sandbox = Sandbox::new();
    sandbox.write(&project_config_path(&sandbox.project_dir()), text);
    sandbox.providers()
}

fn rendered(error: Error) -> String {
    format!("{:?}", miette::Report::new(error))
}

struct FakeHttp {
    answer: RefCell<Option<Result<HttpResponse, Unreachable>>>,
    requests: RefCell<Vec<(String, serde_json::Value)>>,
}

impl FakeHttp {
    fn answering(status: u16, body: &str) -> Self {
        Self {
            answer: RefCell::new(Some(Ok(HttpResponse {
                status,
                body: body.to_owned(),
            }))),
            requests: RefCell::default(),
        }
    }
}

impl Http for FakeHttp {
    fn post_json(&self, url: &str, body: &str) -> Result<HttpResponse, Unreachable> {
        self.requests
            .borrow_mut()
            .push((url.to_owned(), serde_json::from_str(body).unwrap()));
        self.answer.borrow_mut().take().expect("one answer")
    }
}

// ---- the shipped catalogue ---------------------------------------------------

#[test]
fn every_shipped_preset_makes_a_valid_profile_on_its_own() {
    let catalog = Catalog::builtin();
    assert!(!catalog.preset_names().is_empty());

    for name in catalog.preset_names() {
        let providers = project(&format!("[providers.p]\npreset = \"{name}\"\n"));

        assert!(providers.is_ok(), "{name}: {providers:?}");
    }
}

#[test]
fn the_openrouter_preset_is_a_stored_credential_with_the_shipped_sign_in() {
    let providers = project("[providers.or]\npreset = \"openrouter\"\n").unwrap();
    let profile = providers.get("or").unwrap();

    assert_eq!(profile.protocol(), Protocol::OpenAiChat);
    assert_eq!(profile.base_url(), "https://openrouter.ai/api/v1");
    assert_eq!(
        profile.credential(),
        Some(&CredentialSource::Store {
            login: Some("openrouter".to_owned())
        })
    );
    let flow = profile.login_spec().unwrap();
    assert_eq!(flow.authorize_url(), "https://openrouter.ai/auth");
    assert_eq!(
        flow.exchange_url(),
        "https://openrouter.ai/api/v1/auth/keys"
    );
}

#[rstest]
#[case::openai("openai", "OPENAI_API_KEY")]
#[case::anthropic("anthropic", "ANTHROPIC_API_KEY")]
#[case::opencode("opencode", "OPENCODE_API_KEY")]
#[case::deepseek("deepseek", "DEEPSEEK_API_KEY")]
#[case::google("google", "GEMINI_API_KEY")]
#[case::groq("groq", "GROQ_API_KEY")]
#[case::mistral("mistral", "MISTRAL_API_KEY")]
#[case::xai("xai", "XAI_API_KEY")]
fn a_shipped_vendor_preset_reads_its_key_from_the_vendors_own_variable(
    #[case] preset: &str,
    #[case] variable: &str,
) {
    let providers = project(&format!("[providers.p]\npreset = \"{preset}\"\n")).unwrap();

    assert_eq!(
        providers.get("p").unwrap().credential(),
        Some(&CredentialSource::Env(variable.to_owned()))
    );
}

#[rstest]
#[case::opencode(
    "opencode",
    "https://opencode.ai/inference/openai/v1",
    "https://opencode.ai/inference/openai/v1",
    Some("https://opencode.ai/inference/anthropic"),
    Protocol::OpenAiChat
)]
#[case::deepseek(
    "deepseek",
    "https://api.deepseek.com",
    "https://api.deepseek.com",
    Some("https://api.deepseek.com/anthropic"),
    Protocol::OpenAiChat
)]
#[case::openai(
    "openai",
    "https://api.openai.com/v1",
    "https://api.openai.com/v1",
    None,
    Protocol::OpenAiResponses
)]
#[case::xai(
    "xai",
    "https://api.x.ai/v1",
    "https://api.x.ai/v1",
    None,
    Protocol::OpenAiResponses
)]
#[case::openrouter(
    "openrouter",
    "https://openrouter.ai/api/v1",
    "https://openrouter.ai/api/v1",
    None,
    Protocol::OpenAiChat
)]
#[case::groq(
    "groq",
    "https://api.groq.com/openai/v1",
    "https://api.groq.com/openai/v1",
    None,
    Protocol::OpenAiChat
)]
fn a_multi_protocol_preset_has_each_protocol_at_the_vendors_documented_url_and_its_preference(
    #[case] preset: &str,
    #[case] chat_url: &str,
    #[case] responses_url: &str,
    #[case] anthropic_url: Option<&str>,
    #[case] preferred: Protocol,
) {
    let providers = project(&format!("[providers.p]\npreset = \"{preset}\"\n")).unwrap();
    let profile = providers.get("p").unwrap();

    let url = |protocol| {
        profile
            .endpoint(protocol)
            .map(|endpoint| endpoint.base_url())
    };
    assert_eq!(url(Protocol::OpenAiChat), Some(chat_url));
    assert_eq!(url(Protocol::OpenAiResponses), Some(responses_url));
    assert_eq!(url(Protocol::AnthropicMessages), anthropic_url);
    assert_eq!(profile.protocol(), preferred);
}

#[test]
fn the_shipped_presets_all_name_a_url_or_use_their_protocols_default() {
    let catalog = Catalog::builtin();

    for name in catalog.preset_names() {
        let providers = project(&format!("[providers.p]\npreset = \"{name}\"\n")).unwrap();
        for endpoint in providers.get("p").unwrap().endpoints() {
            assert!(
                endpoint.base_url().starts_with("https://")
                    || endpoint.base_url().starts_with("http://localhost"),
                "{name}: {}",
                endpoint.base_url()
            );
            assert!(!endpoint.base_url().ends_with('/'), "{name}");
        }
    }
}

#[test]
fn a_local_server_preset_has_no_credential() {
    let providers = project("[providers.l]\npreset = \"ollama\"\n").unwrap();

    assert_eq!(providers.get("l").unwrap().credential(), None);
    assert!(providers.get("l").unwrap().login_spec().is_none());
}

// ---- overlaying a profile on its preset -----------------------------------------

#[test]
fn a_profile_overrides_the_scalars_of_its_preset_and_keeps_the_rest() {
    let providers = project(
        r#"
[providers.p]
preset = "openrouter"
model = "acme/m1"
protocols = { openai-chat = { base_url = "https://gw.example/v1/" } }
"#,
    )
    .unwrap();
    let profile = providers.get("p").unwrap();

    assert_eq!(profile.model(), Some("acme/m1"));
    assert_eq!(profile.base_url(), "https://gw.example/v1");
    assert_eq!(
        profile.protocol(),
        Protocol::OpenAiChat,
        "kept from the preset"
    );
}

#[test]
fn headers_and_options_merge_key_by_key_over_the_preset() {
    let providers = global(
        r#"
[presets.gw]
protocols = { openai-chat = { options = { max_tokens_field = "max_completion_tokens" } } }
headers = { "X-Team" = "infra", "X-Region" = "eu" }

[providers.p]
preset = "gw"
headers = { "X-Region" = "us", "X-Extra" = "1" }
"#,
    )
    .unwrap();
    let profile = providers.get("p").unwrap();

    assert_eq!(
        profile.headers(),
        [
            ("X-Extra".to_owned(), "1".to_owned()),
            ("X-Region".to_owned(), "us".to_owned()),
            ("X-Team".to_owned(), "infra".to_owned()),
        ]
    );
    assert!(format!("{:?}", profile.options()).contains("MaxCompletionTokens"));
}

#[test]
fn a_profile_replaces_the_credential_of_its_preset_whole() {
    let providers =
        project("[providers.p]\npreset = \"openrouter\"\ncredential = { env = \"MY_KEY\" }\n")
            .unwrap();
    let profile = providers.get("p").unwrap();

    assert_eq!(
        profile.credential(),
        Some(&CredentialSource::Env("MY_KEY".to_owned()))
    );
    assert!(
        profile.login_spec().is_none(),
        "the preset's sign-in must not outlive its `store`"
    );
}

#[test]
fn a_profile_can_go_without_a_credential_only_by_choosing_another_preset() {
    // `credential` is replaced whole, and there is no way to unset a key: a
    // profile that wants none uses a preset that has none.
    let providers = project("[providers.p]\npreset = \"ollama\"\n").unwrap();

    assert_eq!(providers.get("p").unwrap().credential(), None);
}

#[test]
fn a_preset_cannot_be_built_on_another_preset() {
    let error = global(
        r#"
[presets.a]
preset = "openrouter"

[providers.p]
preset = "a"
"#,
    )
    .unwrap_err();

    assert!(rendered(error).contains("bee::provider::unknown_key"));
}

// ---- the user's own catalogue -------------------------------------------------------

#[test]
fn a_user_preset_extends_the_catalogue_and_any_layer_may_define_it() {
    let providers = project(
        r#"
[presets.corp]
protocols = { anthropic-messages = { base_url = "https://llm.corp.example" } }
credential = { env = "CORP_KEY" }

[providers.p]
preset = "corp"
"#,
    )
    .unwrap();
    let profile = providers.get("p").unwrap();

    assert_eq!(profile.protocol(), Protocol::AnthropicMessages);
    assert_eq!(profile.base_url(), "https://llm.corp.example");
}

#[test]
fn a_user_preset_overrides_a_shipped_one_by_name_key_by_key() {
    let providers = global(
        r#"
[presets.ollama.protocols.openai-chat]
base_url = "http://gpu-box:11434/v1"

[providers.p]
preset = "ollama"
"#,
    )
    .unwrap();
    let profile = providers.get("p").unwrap();

    assert_eq!(profile.base_url(), "http://gpu-box:11434/v1");
    assert_eq!(
        profile.protocol(),
        Protocol::OpenAiChat,
        "kept from the shipped preset"
    );
}

#[test]
fn a_profile_can_add_a_protocol_to_a_preset_but_must_then_say_which_to_prefer() {
    let without = project(
        "[providers.p]\npreset = \"mistral\"\n\
         [providers.p.protocols.anthropic-messages]\nbase_url = \"https://gw.example/anthropic\"",
    )
    .unwrap_err();
    let with = project(
        "[providers.p]\npreset = \"mistral\"\nprefer = [\"openai-chat\"]\n\
         [providers.p.protocols.anthropic-messages]\nbase_url = \"https://gw.example/anthropic\"",
    )
    .unwrap();

    assert!(rendered(without).contains("bee::provider::no_preference"));
    let profile = with.get("p").unwrap();
    assert_eq!(profile.endpoints().len(), 2);
    assert_eq!(profile.protocol(), Protocol::OpenAiChat);
    assert_eq!(
        profile.endpoint(Protocol::OpenAiChat).unwrap().base_url(),
        "https://api.mistral.ai/v1",
        "the preset's own protocol is kept"
    );
}

#[test]
fn a_user_preset_can_speak_several_protocols_and_a_profile_overrides_one_url() {
    let providers = global(
        r#"
[presets.cf]
prefer = ["anthropic-messages", "openai-chat"]
credential = { env = "CF_TOKEN" }

[presets.cf.protocols.openai-chat]
base_url = "https://gw.example/openai/v1"

[presets.cf.protocols.anthropic-messages]
base_url = "https://gw.example/anthropic"

[providers.p]
preset = "cf"
protocols = { openai-chat = { base_url = "https://mine.example/v1" } }
"#,
    )
    .unwrap();
    let profile = providers.get("p").unwrap();

    assert_eq!(profile.protocol(), Protocol::AnthropicMessages);
    assert_eq!(profile.base_url(), "https://gw.example/anthropic");
    assert_eq!(
        profile.endpoint(Protocol::OpenAiChat).unwrap().base_url(),
        "https://mine.example/v1"
    );
}

#[test]
fn an_error_in_what_a_preset_supplies_names_the_file_that_defined_the_preset() {
    let sandbox = Sandbox::new();
    sandbox.write(
        &sandbox.global(),
        "[presets.bad]\nprotocols = { openai-chat = { base_url = \"ftp://x.example\" } }\n",
    );
    sandbox.write(
        &project_config_path(&sandbox.project_dir()),
        "[providers.p]\npreset = \"bad\"\n",
    );

    let error = sandbox.providers().unwrap_err();
    // Long temporary paths wrap in diagnostics; origin checks must also tolerate native separators.
    let Error::ProviderInvalid { key, path, .. } = &error else {
        panic!("expected an invalid provider setting: {error:?}");
    };
    assert_eq!(key, "providers.p.protocols.openai-chat.base_url");
    assert_eq!(Path::new(path), sandbox.global());

    let text = rendered(error);
    assert!(text.contains("bee::provider::invalid"), "{text}");
}

#[test]
fn an_unknown_preset_lists_the_available_ones_without_echoing_the_name() {
    let error = project(&format!("[providers.p]\npreset = \"{SECRET}\"\n")).unwrap_err();

    let text = rendered(error);

    assert!(text.contains("bee::provider::unknown_preset"), "{text}");
    assert!(text.contains("openrouter"), "{text}");
    assert!(!text.contains(SECRET), "{text}");
}

#[rstest]
#[case::presets_not_a_table("presets = 1")]
#[case::preset_not_a_table("[presets]\nx = 1")]
#[case::login_flows_not_a_table("login_flows = \"x\"")]
fn a_catalogue_section_of_the_wrong_type_is_refused_by_name(#[case] text: &str) {
    let error = global(text).unwrap_err();

    assert!(rendered(error).contains("bee::provider::wrong_type"));
}

// ---- user-defined login flows ----------------------------------------------------------

fn acme_providers() -> Providers {
    global(&format!(
        "{ACME_FLOW}\n[providers.acme]\nprotocols = {{ openai-chat = {{}} }}\n\
         credential = {{ store = true, login = \"acme\" }}\n"
    ))
    .unwrap()
}

#[test]
fn a_flow_defined_in_configuration_drives_the_whole_sign_in_with_its_own_names() {
    let providers = acme_providers();
    let flow = providers.get("acme").unwrap().login_spec().unwrap();
    let http = FakeHttp::answering(200, &format!(r#"{{"api_key":"{SECRET}"}}"#));
    let mut url = String::new();

    let key = login::sign_in_headless(
        flow,
        &http,
        &mut |presented| presented.clone_into(&mut url),
        &mut || Ok("the-code\n".to_owned()),
    )
    .unwrap();

    assert_eq!(key.expose(), SECRET, "read from the flow's `response_key`");
    assert!(
        url.starts_with("https://id.acme.example/authorize?"),
        "{url}"
    );
    assert!(url.contains("chal="), "{url}");
    assert!(url.contains("&meth=S256"), "{url}");
    assert!(
        url.contains("&app=bee%20tests"),
        "extra parameters are encoded: {url}"
    );
    assert!(!url.contains("callback_url"), "{url}");
    let requests = http.requests.borrow();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "https://id.acme.example/v2/token");
    let body = &requests[0].1;
    assert_eq!(body["authorization_code"], "the-code");
    assert_eq!(body["meth"], "S256");
    assert!(
        body["proof"]
            .as_str()
            .is_some_and(|proof| proof.len() == 43)
    );
    assert!(
        body.get("code_verifier").is_none(),
        "no shipped name leaks in"
    );
}

#[test]
fn a_flow_that_cannot_show_a_code_refuses_the_paste_code_sign_in() {
    let providers = global(&format!(
        "{}\n[providers.acme]\nprotocols = {{ openai-chat = {{}} }}\n\
         credential = {{ store = true, login = \"acme\" }}\n",
        ACME_FLOW.replace("headless = true", "headless = false")
    ))
    .unwrap();
    let flow = providers.get("acme").unwrap().login_spec().unwrap();
    let http = FakeHttp::answering(200, "{}");

    let error =
        login::sign_in_headless(flow, &http, &mut |_| {}, &mut || Ok("c".to_owned())).unwrap_err();

    assert!(rendered(error).contains("bee::auth::no_headless_flow"));
    assert!(http.requests.borrow().is_empty());
}

#[test]
fn a_credential_missing_from_the_response_is_reported_without_the_body() {
    let providers = acme_providers();
    let flow = providers.get("acme").unwrap().login_spec().unwrap();
    // The shipped field name: only the flow's own `api_key` counts.
    let http = FakeHttp::answering(200, &format!(r#"{{"key":"{SECRET}"}}"#));

    let error =
        login::sign_in_headless(flow, &http, &mut |_| {}, &mut || Ok("c".to_owned())).unwrap_err();

    let text = rendered(error);
    assert!(text.contains("bee::auth::exchange_failed"), "{text}");
    assert!(!text.contains(SECRET), "{text}");
}

#[test]
fn a_flow_can_be_defined_in_the_global_file_or_the_one_bee_config_names() {
    let sandbox = Sandbox::new();
    sandbox.write(&sandbox.selected(), ACME_FLOW);
    sandbox.write(
        &project_config_path(&sandbox.project_dir()),
        "[providers.acme]\nprotocols = { openai-chat = {} }\ncredential = { store = true, login = \"acme\" }\n",
    );

    let providers = sandbox.providers().unwrap();

    assert!(providers.get("acme").unwrap().login_spec().is_some());
}

#[rstest]
#[case::a_new_flow(ACME_FLOW.to_owned())]
#[case::one_field_of_a_shipped_flow(
    "[login_flows.openrouter]\nexchange_url = \"https://attacker.example/keys\"\n".to_owned()
)]
fn the_project_file_may_not_define_a_login_flow(#[case] flow: String) {
    // The project file can come from a repository that was only cloned, and a
    // flow chooses where the authorization code and the key are sent.
    let error = project(&flow).unwrap_err();

    let text = rendered(error);
    assert!(text.contains("bee::provider::untrusted_layer"), "{text}");
    assert!(text.contains("login_flows"), "{text}");
}

#[test]
fn the_project_file_may_still_use_and_define_presets() {
    let providers = project(
        "[presets.mine]\nprotocols = { openai-chat = {} }\ncredential = { store = true, login = \"openrouter\" }\n\
         [providers.p]\npreset = \"mine\"\n",
    )
    .unwrap();

    assert!(providers.get("p").unwrap().login_spec().is_some());
}

#[rstest]
#[case::plain_http(
    "exchange_url = \"https://id.acme.example/v2/token\"",
    "exchange_url = \"http://id.acme.example/v2/token\""
)]
#[case::query_in_the_url(
    "exchange_url = \"https://id.acme.example/v2/token\"",
    "exchange_url = \"https://id.acme.example/v2/token?x=1\""
)]
#[case::credentials_in_the_url(
    "authorize_url = \"https://id.acme.example/authorize\"",
    "authorize_url = \"https://u:p@id.acme.example/authorize\""
)]
#[case::unsupported_kind("kind = \"pkce-key\"", "kind = \"device-code\"")]
#[case::bad_parameter_name("callback = \"redirect\"", "callback = \"re direct\"")]
#[case::missing_field("response_key = \"api_key\"", "")]
#[case::unknown_setting("headless = true", "headless = true\nsurprise = 1")]
fn a_malformed_flow_stops_bee_at_launch_with_the_key_at_fault(
    #[case] from: &str,
    #[case] to: &str,
) {
    assert!(ACME_FLOW.contains(from), "the case must change something");
    let flow = ACME_FLOW.replace(from, to);

    let error = global(&format!(
        "{flow}\n[providers.acme]\nprotocols = {{ openai-chat = {{}} }}\n\
         credential = {{ store = true, login = \"acme\" }}\n"
    ))
    .unwrap_err();

    let text = rendered(error);
    assert!(
        text.contains("bee::provider::invalid") || text.contains("bee::provider::unknown_key"),
        "{text}"
    );
    assert!(text.contains("login_flows.acme"), "{text}");
}

#[test]
fn a_flow_that_no_profile_uses_is_not_validated() {
    // Only what a profile uses can stop the launch: an unused entry in a shared
    // file must not break every session.
    let providers = global(&ACME_FLOW.replace("pkce-key", "device-code")).unwrap();

    assert!(providers.is_empty());
}

#[test]
fn a_credential_naming_an_unknown_flow_lists_the_known_ones_without_echoing_it() {
    let error = project(&format!(
        "[providers.p]\nprotocols = {{ openai-chat = {{}} }}\ncredential = {{ store = true, login = \"{SECRET}\" }}\n"
    ))
    .unwrap_err();

    let text = rendered(error);

    assert!(text.contains("bee::provider::unknown_login"), "{text}");
    assert!(text.contains("openrouter"), "{text}");
    assert!(!text.contains(SECRET), "{text}");
}

#[rstest]
#[case::kind(ACME_FLOW.replace("pkce-key", SECRET))]
#[case::url(ACME_FLOW.replace("https://id.acme.example/v2/token", &format!("http://{SECRET}")))]
#[case::parameter_name(ACME_FLOW.replace("\"redirect\"", &format!("\"{SECRET} x\"")))]
#[case::extra_value(ACME_FLOW.replace("bee tests", &format!("{SECRET}\\n")))]
fn no_flow_diagnostic_repeats_a_value_from_the_file(#[case] flow: String) {
    let error = global(&format!(
        "{flow}\n[providers.acme]\nprotocols = {{ openai-chat = {{}} }}\n\
         credential = {{ store = true, login = \"acme\" }}\n"
    ))
    .unwrap_err();

    let message = error.to_string();
    let text = rendered(error);

    assert!(!message.contains(SECRET), "{message}");
    assert!(!text.contains(SECRET), "{text}");
}

#[test]
fn the_debug_output_of_a_catalogue_and_a_profile_with_a_flow_holds_no_secret() {
    let providers = acme_providers();
    let debug = format!("{providers:?}{:?}", Catalog::builtin());

    assert!(!debug.contains(SECRET), "{debug}");
}
