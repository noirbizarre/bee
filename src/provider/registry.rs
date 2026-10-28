//! Compiled-in integrations. Instance names never select an implementation.

use std::sync::LazyLock;

use toml::{Table, Value};

/// Authentication method, independent of token acquisition and storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMethod {
    /// A static secret, including manually supplied bearer tokens.
    ApiKey,
    /// Access tokens obtained through an authorization grant.
    OAuth,
}

/// Where a declared field belongs; sensitivity alone does not determine ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldOwner {
    /// Private ordinary instance settings, never model metadata.
    Instance,
    /// Secret-bearing credential storage or an environment binding.
    Credential,
}

/// Field validation is structural and never includes the rejected value in an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    /// Nonblank, control-free UTF-8 text.
    Text,
    /// A resource/routing identifier, not a URL or arbitrary request fragment.
    Identifier,
}

/// A provider-declared input. Values are deliberately absent from descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    /// Stable configuration/credential key.
    pub name: &'static str,
    /// Whether acquisition/validation requires a value.
    pub required: bool,
    /// Whether the value must be secret-bearing.
    pub secret: bool,
    /// Persistence boundary for this input.
    pub owner: FieldOwner,
    /// Accepted value shape.
    pub value_type: FieldType,
}

impl Field {
    /// Compiled-in prompt text cannot disclose a configured value.
    pub fn prompt_label(&self) -> &'static str {
        match self.name {
            "account_id" => "Cloudflare Account ID",
            "gateway_id" => "Cloudflare Gateway ID",
            _ => self.name,
        }
    }
}

const KEY_FIELDS: &[Field] = &[Field {
    name: "key",
    required: true,
    secret: true,
    owner: FieldOwner::Credential,
    value_type: FieldType::Text,
}];
const CLOUDFLARE_FIELDS: &[Field] = &[
    Field {
        name: "account_id",
        required: true,
        secret: false,
        owner: FieldOwner::Instance,
        value_type: FieldType::Identifier,
    },
    Field {
        name: "gateway_id",
        required: true,
        secret: false,
        owner: FieldOwner::Instance,
        value_type: FieldType::Identifier,
    },
    KEY_FIELDS[0],
];
const CLOUD_FIELDS: &[Field] = &[
    Field {
        name: "project",
        required: false,
        secret: false,
        owner: FieldOwner::Instance,
        value_type: FieldType::Identifier,
    },
    Field {
        name: "location",
        required: false,
        secret: false,
        owner: FieldOwner::Instance,
        value_type: FieldType::Identifier,
    },
    Field {
        name: "quota_project",
        required: false,
        secret: false,
        owner: FieldOwner::Instance,
        value_type: FieldType::Identifier,
    },
];

/// An integration definition, registered in code rather than user-maintained presets.
pub struct Definition {
    /// Stable integration identity, distinct from an instance name.
    pub id: &'static str,
    /// Safe compiled-in presentation label.
    pub label: &'static str,
    /// Supported authentication methods; empty for external/no-auth integrations.
    pub methods: &'static [AuthMethod],
    /// Provider-owned input schema.
    pub fields: &'static [Field],
    defaults: fn() -> Table,
}

impl Definition {
    /// Validate the private settings envelope without serializing it into public model facts.
    pub fn validate_fields(&self, owner: FieldOwner, fields: &Table) -> Result<(), &'static str> {
        self.inspect_fields(owner, fields, true)
    }

    /// Login may collect absent fields, but never repairs malformed configured values.
    pub(crate) fn inspect_fields(
        &self,
        owner: FieldOwner,
        fields: &Table,
        complete: bool,
    ) -> Result<(), &'static str> {
        if fields.keys().any(|name| {
            !self
                .fields
                .iter()
                .any(|field| field.name == name && field.owner == owner)
        }) {
            return Err("unknown provider-specific field; inspect `bee providers show <type>`");
        }
        for field in self.fields.iter().filter(|field| field.owner == owner) {
            let Some(value) = fields.get(field.name) else {
                if field.required && complete {
                    return Err(
                        "required provider-specific field is missing; inspect `bee providers show <type>`",
                    );
                }
                continue;
            };
            if field.secret && owner != FieldOwner::Credential {
                return Err("secret fields must live in credential storage");
            }
            let text = value
                .as_str()
                .ok_or("provider-specific field must be text")?;
            if text.trim().is_empty() || text.len() > 8192 || text.chars().any(char::is_control) {
                return Err(
                    "provider-specific field must be nonblank control-free text of at most 8192 bytes",
                );
            }
            if field.value_type == FieldType::Identifier
                && !text
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                return Err(
                    "provider-specific identifier must contain only letters, digits, dots, hyphens or underscores",
                );
            }
            if self.id == "cloudflare" && owner == FieldOwner::Instance {
                super::cloudflare::validate_field(field.name, text)?;
            }
        }
        Ok(())
    }
    pub(crate) fn defaults(&self) -> Table {
        (self.defaults)()
    }

    /// Optional native policies are resolved by registration, never by instance names.
    pub(crate) fn integration(&self) -> Option<crate::models::Integration> {
        let mut integration = crate::models::Integration {
            preference_filtered: false,
            account_target: false,
            official: false,
            offering_root: None,
        };
        match self.id {
            "openrouter" => integration.preference_filtered = true,
            "chatgpt" => integration.official = true,
            "github" => integration.account_target = true,
            "opencode" => integration.offering_root = Some("https://opencode.ai/inference"),
            "opencode-go" => integration.offering_root = Some("https://opencode.ai/inference/go"),
            _ => return None,
        }
        Some(integration)
    }
}

impl std::fmt::Debug for Definition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Definition").field("id", &self.id).finish()
    }
}

fn defaults(
    label: &str,
    catalog: Option<&str>,
    env: Option<&str>,
    routes: &[(&str, &str)],
) -> Table {
    let mut table = Table::new();
    table.insert("display_name".into(), Value::String(label.into()));
    if let Some(catalog) = catalog {
        table.insert("catalog".into(), Value::String(catalog.into()));
    }
    if let Some(env) = env {
        table.insert(
            "credential".into(),
            Value::Table(Table::from_iter([(
                "env".into(),
                Value::String(env.into()),
            )])),
        );
    }
    table.insert(
        "prefer".into(),
        Value::Array(
            routes
                .iter()
                .map(|(p, _)| Value::String((*p).into()))
                .collect(),
        ),
    );
    table.insert(
        "protocols".into(),
        Value::Table(
            routes
                .iter()
                .map(|(protocol, url)| {
                    let endpoint = if url.is_empty() {
                        Table::new()
                    } else {
                        Table::from_iter([("base_url".into(), Value::String((*url).into()))])
                    };
                    ((*protocol).into(), Value::Table(endpoint))
                })
                .collect(),
        ),
    );
    table
}

fn native(mut table: Table, discover: bool, login: Option<&str>) -> Table {
    table.insert("discovery".into(), Value::Boolean(discover));
    if let Some(login) = login {
        table.insert(
            "credential".into(),
            Value::Table(Table::from_iter([
                ("store".into(), Value::Boolean(true)),
                ("login".into(), Value::String(login.into())),
            ])),
        );
    }
    table
}

/// Registry order is stable; new integrations need only a definition here and their implementation.
pub fn definitions() -> &'static [Definition] {
    static DEFINITIONS: LazyLock<Vec<Definition>> = LazyLock::new(|| {
        let definitions = vec![
            Definition {
                id: "custom",
                label: "Custom",
                methods: &[AuthMethod::ApiKey, AuthMethod::OAuth],
                fields: KEY_FIELDS,
                defaults: Table::new,
            },
            Definition {
                id: "cloudflare",
                label: "Cloudflare AI Gateway",
                methods: &[AuthMethod::ApiKey],
                fields: CLOUDFLARE_FIELDS,
                defaults: || {
                    native(
                        defaults(
                            "Cloudflare AI Gateway",
                            None,
                            Some("CLOUDFLARE_API_TOKEN"),
                            &[
                                ("openai-chat", ""),
                                ("openai-responses", ""),
                                ("anthropic-messages", ""),
                            ],
                        ),
                        false,
                        None,
                    )
                },
            },
            Definition {
                id: "openai",
                label: "OpenAI",
                methods: &[AuthMethod::ApiKey],
                fields: KEY_FIELDS,
                defaults: || {
                    defaults(
                        "OpenAI",
                        Some("openai"),
                        Some("OPENAI_API_KEY"),
                        &[("openai-responses", ""), ("openai-chat", "")],
                    )
                },
            },
            Definition {
                id: "anthropic",
                label: "Anthropic",
                methods: &[AuthMethod::ApiKey],
                fields: KEY_FIELDS,
                defaults: || {
                    defaults(
                        "Anthropic",
                        Some("anthropic"),
                        Some("ANTHROPIC_API_KEY"),
                        &[("anthropic-messages", "")],
                    )
                },
            },
            Definition {
                id: "openrouter",
                label: "OpenRouter",
                methods: &[AuthMethod::ApiKey],
                fields: KEY_FIELDS,
                defaults: || {
                    native(
                        defaults(
                            "OpenRouter",
                            Some("openrouter"),
                            None,
                            &[
                                ("openai-chat", "https://openrouter.ai/api/v1"),
                                ("openai-responses", "https://openrouter.ai/api/v1"),
                            ],
                        ),
                        true,
                        Some("openrouter"),
                    )
                },
            },
            Definition {
                id: "chatgpt",
                label: "ChatGPT",
                methods: &[AuthMethod::OAuth],
                fields: &[],
                defaults: || {
                    native(
                        defaults(
                            "ChatGPT",
                            None,
                            None,
                            &[("openai-responses", "https://api.openai.com/v1")],
                        ),
                        true,
                        Some("chatgpt"),
                    )
                },
            },
            Definition {
                id: "github",
                label: "GitHub Copilot",
                methods: &[AuthMethod::ApiKey, AuthMethod::OAuth],
                fields: KEY_FIELDS,
                defaults: || {
                    let mut table = native(
                        defaults(
                            "GitHub Copilot",
                            Some("github-copilot"),
                            None,
                            &[
                                ("openai-responses", "https://api.githubcopilot.com"),
                                ("anthropic-messages", "https://api.githubcopilot.com"),
                                ("openai-chat", "https://api.githubcopilot.com"),
                            ],
                        ),
                        true,
                        Some("github"),
                    );
                    table.insert("auth".into(), Value::String("github".into()));
                    table
                },
            },
            Definition {
                id: "ollama",
                label: "Ollama",
                methods: &[],
                fields: &[],
                defaults: || {
                    defaults(
                        "Ollama",
                        None,
                        None,
                        &[("openai-chat", "http://localhost:11434/v1")],
                    )
                },
            },
            Definition {
                id: "opencode",
                label: "OpenCode Zen",
                methods: &[AuthMethod::ApiKey],
                fields: KEY_FIELDS,
                defaults: || {
                    native(
                        defaults(
                            "OpenCode Zen",
                            Some("opencode"),
                            Some("OPENCODE_API_KEY"),
                            &[
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
                        false,
                        None,
                    )
                },
            },
            Definition {
                id: "opencode-go",
                label: "OpenCode Go",
                methods: &[AuthMethod::ApiKey],
                fields: KEY_FIELDS,
                defaults: || {
                    native(
                        defaults(
                            "OpenCode Go",
                            Some("opencode-go"),
                            Some("OPENCODE_GO_API_KEY"),
                            &[
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
                        true,
                        None,
                    )
                },
            },
            Definition {
                id: "deepseek",
                label: "DeepSeek",
                methods: &[AuthMethod::ApiKey],
                fields: KEY_FIELDS,
                defaults: || {
                    defaults(
                        "DeepSeek",
                        Some("deepseek"),
                        Some("DEEPSEEK_API_KEY"),
                        &[
                            ("openai-chat", "https://api.deepseek.com"),
                            ("openai-responses", "https://api.deepseek.com"),
                            ("anthropic-messages", "https://api.deepseek.com/anthropic"),
                        ],
                    )
                },
            },
            Definition {
                id: "google-aistudio",
                label: "Google AI Studio",
                methods: &[AuthMethod::ApiKey],
                fields: KEY_FIELDS,
                defaults: || {
                    let mut table = defaults(
                        "Google AI Studio",
                        Some("google"),
                        Some("GEMINI_API_KEY"),
                        &[(
                            "openai-chat",
                            "https://generativelanguage.googleapis.com/v1beta/openai",
                        )],
                    );
                    table.insert("discovery".into(), Value::Boolean(true));
                    table
                },
            },
            Definition {
                id: "groq",
                label: "Groq",
                methods: &[AuthMethod::ApiKey],
                fields: KEY_FIELDS,
                defaults: || {
                    defaults(
                        "Groq",
                        Some("groq"),
                        Some("GROQ_API_KEY"),
                        &[
                            ("openai-chat", "https://api.groq.com/openai/v1"),
                            ("openai-responses", "https://api.groq.com/openai/v1"),
                        ],
                    )
                },
            },
            Definition {
                id: "mistral",
                label: "Mistral",
                methods: &[AuthMethod::ApiKey],
                fields: KEY_FIELDS,
                defaults: || {
                    defaults(
                        "Mistral",
                        Some("mistral"),
                        Some("MISTRAL_API_KEY"),
                        &[("openai-chat", "https://api.mistral.ai/v1")],
                    )
                },
            },
            Definition {
                id: "xai",
                label: "xAI",
                methods: &[AuthMethod::ApiKey],
                fields: KEY_FIELDS,
                defaults: || {
                    defaults(
                        "xAI",
                        Some("xai"),
                        Some("XAI_API_KEY"),
                        &[
                            ("openai-responses", "https://api.x.ai/v1"),
                            ("openai-chat", "https://api.x.ai/v1"),
                        ],
                    )
                },
            },
            Definition {
                id: "google-cloud",
                label: "Google Cloud",
                methods: &[],
                fields: CLOUD_FIELDS,
                defaults: || {
                    let mut table = defaults("Google Cloud", None, None, &[("openai-chat", "")]);
                    table.insert("auth".into(), Value::String("google-cloud".into()));
                    table.insert(
                        "credential".into(),
                        Value::Table(Table::from_iter([("adc".into(), Value::Boolean(true))])),
                    );
                    table
                },
            },
        ];
        #[cfg(test)]
        {
            definitions
                .into_iter()
                .chain([tests::fixture_definition()])
                .collect()
        }
        #[cfg(not(test))]
        {
            definitions
        }
    });
    &DEFINITIONS
}

/// Exact lookup; never infer a type from an account/instance name.
pub fn definition(id: &str) -> Option<&'static Definition> {
    definitions().iter().find(|definition| definition.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Audience, CredentialStore, Material};

    pub(super) fn fixture_definition() -> Definition {
        // Registration alone adds an integration, including its private routing/auth schema.
        Definition {
            id: "fixture-gateway",
            label: "Test gateway",
            methods: &[AuthMethod::ApiKey],
            fields: &[
                Field {
                    name: "account",
                    required: true,
                    secret: false,
                    owner: FieldOwner::Instance,
                    value_type: FieldType::Identifier,
                },
                Field {
                    name: "gateway",
                    required: true,
                    secret: false,
                    owner: FieldOwner::Instance,
                    value_type: FieldType::Identifier,
                },
                Field {
                    name: "key",
                    required: true,
                    secret: true,
                    owner: FieldOwner::Credential,
                    value_type: FieldType::Text,
                },
                Field {
                    name: "secondary",
                    required: true,
                    secret: true,
                    owner: FieldOwner::Credential,
                    value_type: FieldType::Text,
                },
                Field {
                    name: "optional",
                    required: false,
                    secret: true,
                    owner: FieldOwner::Credential,
                    value_type: FieldType::Text,
                },
            ],
            defaults: || {
                defaults(
                    "Test gateway",
                    None,
                    None,
                    &[("openai-chat", "https://gateway.example/v1")],
                )
            },
        }
    }

    #[test]
    #[cfg(unix)]
    fn registration_alone_supports_independent_account_fields_and_complete_secret_import() {
        let root = tempfile::tempdir().unwrap();
        let env = crate::config::Environment::new(Some(root.path().join("config")), None, None)
            .with_variable("FIRST", "first-key")
            .with_variable("SECOND", "second-key")
            .with_variable("EXTRA", "secondary-secret");
        let path = env.global_config_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[accounts.one]\nprovider='fixture-gateway'\nsettings={account='private-account-one',gateway='private-gateway-one'}\nmetadata={display_name='private-metadata-one'}\nmodel_metadata={'private-model-one'={display_name='private-model-label-one'}}\ncredential={env='FIRST',fields={secondary='EXTRA'}}\n[accounts.two]\nprovider='fixture-gateway'\nsettings={account='private-account-two',gateway='private-gateway-two'}\nmetadata={display_name='private-metadata-two'}\ncredential={env='SECOND',fields={secondary='EXTRA'}}").unwrap();
        let config = crate::config::Config::load(&env, root.path()).unwrap();
        let instances = crate::account::Accounts::from_config(&config).unwrap();
        let store = crate::auth::FileStore::new(root.path().join("credentials.json"));
        for name in ["one", "two"] {
            let plan = crate::config::persist::prepare(&env, root.path(), name).unwrap();
            let credential = plan.import_credential(&env).unwrap().unwrap();
            let Material::ApiKey { fields } = credential.material() else {
                panic!("static secret material expected")
            };
            assert_eq!(
                credential.secret().expose(),
                if name == "one" {
                    "first-key"
                } else {
                    "second-key"
                }
            );
            assert_eq!(fields["secondary"].expose(), "secondary-secret");
            assert!(!format!("{credential:?}").contains("secondary-secret"));
            store.put(Audience::Provider(name), &credential).unwrap();
            plan.commit(&env, root.path()).unwrap();
        }
        assert_ne!(
            store.get(Audience::Provider("one")).unwrap(),
            store.get(Audience::Provider("two")).unwrap()
        );
        assert_ne!(
            instances.get("one").unwrap().settings(),
            instances.get("two").unwrap().settings()
        );
        assert!(!format!("{instances:?}").contains("private-account"));
        for debug in [
            format!("{config:?}"),
            format!("{instances:?}"),
            format!("{env:?}"),
            format!("{:?}", store.get(Audience::Provider("one")).unwrap()),
        ] {
            for private in [
                "private-account",
                "private-gateway",
                "private-metadata",
                "private-model",
                "first-key",
                "second-key",
                "secondary-secret",
            ] {
                assert!(!debug.contains(private), "{debug}");
            }
        }
        let one = crate::auth::resolve(instances.get("one").unwrap(), &env, &store, 0)
            .unwrap()
            .unwrap();
        let two = crate::auth::resolve(instances.get("two").unwrap(), &env, &store, 0)
            .unwrap()
            .unwrap();
        assert!(
            !crate::selection::Reference::new(
                instances.get("one").unwrap(),
                "model".into(),
                Some(&one)
            )
            .matches(instances.get("two").unwrap(), Some(&two))
        );
    }

    #[test]
    fn field_schemas_reject_missing_unknown_invalid_and_misplaced_secrets() {
        let definition = fixture_definition();
        for settings in [
            "account='only-one'",
            "account='a'\ngateway='b'\nextra='secret'",
            "account='https://unsafe.example'\ngateway='b'",
            "account=3\ngateway='b'",
        ] {
            assert!(
                definition
                    .validate_fields(FieldOwner::Instance, &settings.parse().unwrap())
                    .is_err()
            );
        }
        assert!(
            definition
                .validate_fields(FieldOwner::Credential, &"key='one'".parse().unwrap())
                .is_err()
        );
        assert!(
            definition
                .validate_fields(
                    FieldOwner::Credential,
                    &"key='one'\nsecondary='two'".parse().unwrap()
                )
                .is_ok()
        );
    }
}
