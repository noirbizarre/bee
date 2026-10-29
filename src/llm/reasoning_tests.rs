use super::*;
use crate::provider::Protocol;
use serde_json::json;

fn model(protocol: Protocol, url: &str, options: &str) -> Model {
    let root = tempfile::tempdir().unwrap();
    let path = crate::config::project_config_path(root.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!(
        "model='work/model'\n[accounts.work]\nprovider='custom'\nmodels=['model']\n[accounts.work.protocols.{}]\nbase_url='{url}'\noptions={{{options}}}", protocol.name()
    )).unwrap();
    let config =
        crate::config::Config::load(&crate::config::Environment::default(), root.path()).unwrap();
    let accounts = crate::account::Accounts::from_config(&config).unwrap();
    Model::new(accounts.get("work").unwrap().clone(), "model".into(), None).unwrap()
}

fn request(model: &Model) -> Result<serde_json::Value, Error> {
    match model.endpoint().protocol() {
        Protocol::OpenAiResponses => responses::request(model, &[], &[]),
        Protocol::AnthropicMessages => anthropic::request(model, &[], &[]),
        _ => unreachable!(),
    }
}

#[test]
fn reasoning_request_controls_are_explicit_and_bound_to_verified_native_routes() {
    for (protocol, url, key, option, reset, expected) in [
        (
            Protocol::OpenAiResponses,
            "https://api.openai.com/v1",
            "reasoning",
            "reasoning_summary='auto'",
            "reasoning_summary='default'",
            json!({"summary":"auto"}),
        ),
        (
            Protocol::AnthropicMessages,
            "https://api.anthropic.com",
            "thinking",
            "thinking={type='adaptive',display='summarized'}",
            "thinking={type='default'}",
            json!({"type":"adaptive","display":"summarized"}),
        ),
    ] {
        assert!(
            request(&model(protocol, url, ""))
                .unwrap()
                .get(key)
                .is_none()
        );
        assert!(
            request(&model(protocol, url, reset))
                .unwrap()
                .get(key)
                .is_none()
        );
        assert_eq!(
            request(&model(protocol, url, option)).unwrap()[key],
            expected
        );
        for unverified in [
            "http://127.0.0.1:1",
            "https://gateway.example",
            "https://api.openai.com.evil/v1",
        ] {
            assert!(matches!(
                request(&model(protocol, unverified, option)),
                Err(Error::Unsupported(_))
            ));
        }
    }
}

#[test]
fn streamed_thoughts_have_finite_entity_and_utf8_byte_budgets() {
    let mut thoughts = Thoughts::default();
    let mut visible = String::new();
    let mut output = |event: Stream<'_>| {
        if let Stream::Thought { text, .. } = event {
            visible.push_str(text)
        }
    };
    thoughts.delta(0, 0, "hé", &mut output).unwrap();
    thoughts.complete(0, 0, "héllo", &mut output).unwrap();
    assert_eq!(
        thoughts.complete(0, 0, "changed", &mut output),
        Err(Error::Malformed)
    );
    assert_eq!(thoughts.delta(128, 0, "", &mut output), Err(Error::Limit));
    assert_eq!(thoughts.delta(0, 128, "", &mut output), Err(Error::Limit));
    assert_eq!(
        thoughts.delta(1, 0, &"é".repeat(2 * 1024 * 1024), &mut output),
        Err(Error::Limit)
    );
    for item in 1..128 {
        thoughts.delta(item, 0, "", &mut output).unwrap();
    }
    assert_eq!(thoughts.delta(0, 1, "", &mut output), Err(Error::Limit));
    assert_eq!(visible, "héllo");
}
