//! Optional model facts and discovery, independent of inference and the ACP SDK.
//!
//! Public catalog data describes published capabilities, not verification of a
//! user's endpoint. No catalog entry can create a profile or supply credentials.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Config, Environment};
use crate::provider::{Profile, Protocol};

mod native;
pub use native::Integration;

/// Published API, deliberately unrelated to any inference endpoint.
pub const CATALOG_URL: &str = "https://models.dev/api.json";
/// Bound catalog downloads and cache reads before parsing untrusted JSON.
pub const MAX_CATALOG_BYTES: usize = 32 * 1024 * 1024;
/// A smaller bound for account-specific endpoint listings.
pub const MAX_LIST_BYTES: usize = 4 * 1024 * 1024;

/// Content-free discovery failures; upstream text can contain credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Error {
    /// HTTP did not complete within its bounded deadline.
    #[error("model discovery could not reach its source; check connectivity and retry")]
    Transport,
    /// The source rejected discovery, not necessarily inference.
    #[error("model discovery was rejected; check credentials and endpoint listing support")]
    Rejected,
    /// Authorization failure invalidates account visibility, unlike a temporary outage.
    #[error(
        "model discovery authorization failed; sign in again or check the configured credential"
    )]
    Unauthorized,
    /// No valid replacement snapshot was obtained.
    #[error("model discovery returned an invalid document; retry refresh or use configured IDs")]
    Malformed,
    /// Untrusted data exceeded a documented resource bound.
    #[error(
        "model discovery exceeded its size limit; use configured IDs or a smaller endpoint listing"
    )]
    Limit,
    /// Cache persistence is best effort for inference.
    #[error("the public model cache could not be accessed; check cache directory permissions")]
    Cache,
    /// Discovery commands must distinguish absent data from an empty list.
    #[error("no model catalog is available; enable catalog use and run `bee models refresh`")]
    Unavailable,
    /// Only implemented protocol adapters can enumerate endpoint models.
    #[error("this profile has no OpenAI-compatible model-listing endpoint; configure IDs manually")]
    Unsupported,
    /// Listing never starts an interactive credential flow.
    #[error(
        "endpoint discovery needs available credentials; check the environment or run `bee auth login`"
    )]
    Credentials,
}

/// Optional explicit metadata. Existing profile fields own routing/context limits.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    /// Human-readable label, never the model ID sent to inference.
    pub display_name: Option<String>,
    /// Published or configured accepted input modalities.
    pub input_modalities: Option<Vec<String>>,
    /// Published or configured produced modalities.
    pub output_modalities: Option<Vec<String>>,
    /// None means unknown, not unsupported.
    pub tool_call: Option<bool>,
    /// None means unknown, not unsupported.
    pub structured_output: Option<bool>,
    /// Distinct from total context capacity.
    pub input_limit: Option<u32>,
    /// Informational: never silently replaces request output settings.
    pub output_limit: Option<u32>,
}

impl std::fmt::Debug for Metadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // These may be configuration values; logs need shape, not content.
        f.write_str("Metadata([redacted])")
    }
}

impl Metadata {
    pub(crate) fn valid(&self) -> bool {
        self.display_name
            .as_ref()
            .is_none_or(|name| !name.trim().is_empty() && !name.chars().any(char::is_control))
            && [&self.input_modalities, &self.output_modalities]
                .into_iter()
                .all(|items| {
                    items.as_ref().is_none_or(|items| {
                        items.len() <= 32
                            && items
                                .iter()
                                .all(|s| !s.is_empty() && !s.chars().any(char::is_control))
                    })
                })
            && self.input_limit != Some(0)
            && self.output_limit != Some(0)
    }
}

/// Catalog policy, separate from per-profile routing/budget opt-ins.
#[derive(Debug, Clone)]
pub struct Settings {
    /// False disables both network and cached catalog use.
    pub enabled: bool,
    /// False permits cache-only use, without automatic refresh.
    pub refresh: bool,
    /// Successful refresh cadence; failures use bounded backoff.
    pub refresh_interval: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            refresh: true,
            refresh_interval: Duration::from_secs(86400),
        }
    }
}

impl Settings {
    /// Read settings without exposing unknown keys or offending values.
    pub fn from_config(config: &Config) -> crate::Result<Self> {
        let mut settings = Self::default();
        let invalid = |reason| crate::Error::CatalogSetting {
            path: config
                .origin("catalog")
                .map_or_else(|| "<configuration>".into(), |p| p.display().to_string()),
            reason,
        };
        let Some(value) = config.table().get("catalog") else {
            return Ok(settings);
        };
        let table = value.as_table().ok_or_else(|| invalid("must be a table"))?;
        for (key, value) in table {
            match key.as_str() {
                "enabled" => {
                    settings.enabled = value
                        .as_bool()
                        .ok_or_else(|| invalid("enabled must be a boolean"))?
                }
                "refresh" => {
                    settings.refresh = value
                        .as_bool()
                        .ok_or_else(|| invalid("refresh must be a boolean"))?
                }
                "refresh_interval_secs" => {
                    let seconds = value
                        .as_integer()
                        .and_then(|v| u32::try_from(v).ok())
                        .filter(|v| *v > 0)
                        .ok_or_else(|| {
                            invalid("refresh_interval_secs must be a positive 32-bit integer")
                        })?;
                    settings.refresh_interval = Duration::from_secs(u64::from(seconds));
                }
                _ => return Err(invalid("contains an unknown setting")),
            }
        }
        Ok(settings)
    }
}

/// Injectable bounded GET transport. Requests must never be logged or Debugged.
pub trait Http: Send + Sync {
    /// No redirects; complete body reads are subject to timeout and size bounds.
    fn get<'a>(
        &'a self,
        url: &'a str,
        headers: reqwest::header::HeaderMap,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<u8>, Error>>;
}

/// Clock injection makes cache freshness independent of a test machine's time.
pub trait Clock: Send + Sync {
    /// Unix seconds, saturating before the epoch.
    fn now(&self) -> u64;
}

/// Production wall clock.
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> u64 {
        crate::auth::unix_now()
    }
}

/// Only public catalog bytes are persisted through this seam.
pub trait Cache: Send + Sync {
    /// A missing cache is not a failure.
    fn load(&self) -> Result<Option<Vec<u8>>, Error>;
    /// Publish a complete validated envelope, leaving old data on failure.
    fn store(&self, bytes: &[u8]) -> Result<(), Error>;
}

/// XDG public catalog cache; no path is a valid memory-only setup.
pub struct FileCache(pub Option<PathBuf>);
impl Cache for FileCache {
    fn load(&self) -> Result<Option<Vec<u8>>, Error> {
        let Some(path) = &self.0 else {
            return Ok(None);
        };
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Error::Cache),
        };
        let mut bytes = Vec::new();
        file.take((MAX_CATALOG_BYTES + 4096 + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Cache)?;
        if bytes.len() > MAX_CATALOG_BYTES + 4096 {
            return Err(Error::Limit);
        }
        Ok(Some(bytes))
    }
    fn store(&self, bytes: &[u8]) -> Result<(), Error> {
        let Some(path) = &self.0 else {
            return Ok(());
        };
        let parent = path.parent().ok_or(Error::Cache)?;
        std::fs::create_dir_all(parent).map_err(|_| Error::Cache)?;
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|_| Error::Cache)?;
        file.write_all(bytes).map_err(|_| Error::Cache)?;
        file.as_file().sync_all().map_err(|_| Error::Cache)?;
        file.persist(path).map_err(|_| Error::Cache)?;
        Ok(())
    }
}

/// Provider metadata is dynamic; this type is not a provider allowlist.
#[derive(Clone, Serialize, Deserialize)]
pub struct Provider {
    /// Public display label.
    pub name: String,
    /// SDK package hint, never an endpoint or credential source.
    pub npm: Option<String>,
    /// Exact model IDs as published by this provider.
    pub models: BTreeMap<String, Entry>,
}

/// Relevant published model fields, stripped of endpoint/request overrides.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Entry {
    /// Optional descriptive facts.
    pub metadata: Metadata,
    /// Published total context capacity.
    pub context_window: Option<u32>,
    /// Model-specific SDK signal, taking precedence even when unrecognized.
    pub npm: Option<String>,
}

/// Immutable view; refresh failure retains the previous validated catalog.
#[derive(Clone, Default, Serialize)]
pub struct Snapshot {
    /// Known public providers, not configured connections.
    pub providers: Arc<BTreeMap<String, Provider>>,
    /// Last successful fetch, absent when unavailable.
    pub fetched_at: Option<u64>,
    /// True for expired data or fallback following failed refresh.
    pub stale: bool,
    /// Sanitized most recent refresh/cache failure.
    pub error: Option<Error>,
    /// Persistence failure does not discard valid downloaded data.
    pub cache_error: Option<Error>,
    /// Endpoint visibility is connection/account scoped and never persisted.
    #[serde(skip)]
    pub endpoints: BTreeMap<String, EndpointListing>,
}

/// Account-specific, in-memory endpoint enumeration.
#[derive(Clone, Default, Serialize)]
pub struct EndpointListing {
    /// Deduplicated IDs; ChatGPT preserves the account's server ordering.
    pub models: Vec<String>,
    /// Account display names also provide indexed membership for ordered listings.
    pub display_names: BTreeMap<String, String>,
    /// Last success, absent before a complete result; visibility is not a permanent guarantee.
    pub fetched_at: Option<u64>,
    /// Failure leaves no account-specific fallback after credential changes.
    pub error: Option<Error>,
    /// Last attempt is separate from the last successful observation.
    pub attempted_at: u64,
    /// Retained successful records can be stale after an outage.
    pub stale: bool,
    /// A native API, official integration, or generic endpoint; never verification.
    pub provenance: &'static str,
    /// Published catalogue and account-filtered visibility have different semantics.
    pub scope: &'static str,
    /// Documented facts only; absent values remain unknown.
    pub entries: BTreeMap<String, Entry>,
    /// Only native results may supply an initial model.
    pub native: bool,
    /// Actual successful inference observations, not lasting entitlement guarantees.
    pub inference_succeeded_at: BTreeMap<String, u64>,
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    version: u32,
    fetched_at: u64,
    catalog: Value,
}

fn positive(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v > 0)
}

/// Parse published API JSON, not the upstream repository's authoring format.
pub fn parse(bytes: &[u8]) -> Result<BTreeMap<String, Provider>, Error> {
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(Error::Limit);
    }
    parse_value(&serde_json::from_slice(bytes).map_err(|_| Error::Malformed)?)
}

fn parse_value(value: &Value) -> Result<BTreeMap<String, Provider>, Error> {
    let providers = value.as_object().ok_or(Error::Malformed)?;
    if providers.len() > 4096 {
        return Err(Error::Limit);
    }
    let mut result = BTreeMap::new();
    let mut count = 0;
    for (id, value) in providers {
        crate::provider::check_model_id(id).map_err(|_| Error::Malformed)?;
        let models = value
            .get("models")
            .and_then(Value::as_object)
            .ok_or(Error::Malformed)?;
        count += models.len();
        if count > 100_000 {
            return Err(Error::Limit);
        }
        let mut entries = BTreeMap::new();
        for (id, model) in models {
            crate::provider::check_model_id(id).map_err(|_| Error::Malformed)?;
            if !model.is_object() {
                return Err(Error::Malformed);
            }
            let modalities = |key| {
                model["modalities"][key].as_array().and_then(|items| {
                    items
                        .iter()
                        .map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
            };
            let metadata = Metadata {
                display_name: model["name"].as_str().map(str::to_owned),
                input_modalities: modalities("input"),
                output_modalities: modalities("output"),
                tool_call: model["tool_call"].as_bool(),
                structured_output: model["structured_output"].as_bool(),
                input_limit: positive(&model["limit"]["input"]),
                output_limit: positive(&model["limit"]["output"]),
            };
            entries.insert(
                id.clone(),
                Entry {
                    metadata: if metadata.valid() {
                        metadata
                    } else {
                        Metadata::default()
                    },
                    context_window: positive(&model["limit"]["context"]),
                    npm: model["provider"]["npm"].as_str().map(str::to_owned),
                },
            );
        }
        let name = value["name"]
            .as_str()
            .filter(|name| !name.chars().any(char::is_control))
            .unwrap_or(id)
            .to_owned();
        result.insert(
            id.clone(),
            Provider {
                name,
                npm: value["npm"].as_str().map(str::to_owned),
                models: entries,
            },
        );
    }
    Ok(result)
}

/// One resolved field, including why it is known. Declarations are not verification.
#[derive(Clone, Serialize, PartialEq, Eq)]
pub struct Fact {
    /// Typed JSON value for ACP/CLI presentation.
    pub value: Value,
    /// Configuration, models.dev, or protocol preference.
    pub source: &'static str,
    /// Declared, inferred, or unverified.
    pub confidence: &'static str,
    /// Catalog facts carry freshness; configuration does not expire.
    pub stale: bool,
    /// Observation time for catalog facts.
    pub fetched_at: Option<u64>,
}

impl std::fmt::Debug for Fact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fact")
            .field("source", &self.source)
            .field("confidence", &self.confidence)
            .field("stale", &self.stale)
            .finish_non_exhaustive()
    }
}

/// Metadata projection used by both CLI and ACP, without SDK dependencies.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelInfo {
    /// Opaque model ID, independent of any registry.
    pub id: String,
    /// Independent visibility/suggestion sources.
    pub sources: Vec<&'static str>,
    /// Per-field source and confidence, with explicit overrides already applied.
    pub facts: BTreeMap<String, Fact>,
}

impl Snapshot {
    /// Native ordering may initialize a selection, but never replaces an existing one.
    pub fn native_default(&self, profile: &Profile) -> Option<&str> {
        self.endpoints
            .get(profile.name())
            .filter(|listing| listing.native)?
            .models
            .first()
            .map(String::as_str)
    }
    /// Exact profile association only; catalog absence is ordinary.
    pub fn entry(&self, profile: &Profile, model: &str) -> Option<(&Provider, &Entry)> {
        let provider = self.providers.get(profile.catalog_provider()?)?;
        Some((provider, provider.models.get(model)?))
    }

    fn fact(&self, value: Value, source: &'static str, confidence: &'static str) -> Fact {
        let catalog = source == "models.dev";
        Fact {
            value,
            source,
            confidence,
            stale: catalog && self.stale,
            fetched_at: if catalog { self.fetched_at } else { None },
        }
    }

    /// Routing hint from documented SDK package signals, never a model family.
    pub fn hint(&self, profile: &Profile, model: &str) -> Option<Protocol> {
        if !profile.catalog_protocol_hints() {
            return None;
        }
        let (provider, entry) = self.entry(profile, model)?;
        match entry.npm.as_deref().or(provider.npm.as_deref())? {
            "@ai-sdk/openai" => Some(Protocol::OpenAiResponses),
            "@ai-sdk/openai-compatible" => Some(Protocol::OpenAiChat),
            "@ai-sdk/anthropic" => Some(Protocol::AnthropicMessages),
            _ => None,
        }
    }

    /// A frozen turn decision; this operation performs no I/O.
    pub fn resolve(&self, profile: &Profile, model: &str) -> Selection {
        let hint = self.hint(profile, model);
        let endpoint = profile.endpoint_for_model_with_hint(model, hint);
        let route_source = if profile.has_model_protocol(model) {
            "configuration"
        } else if hint.is_some_and(|hint| hint == endpoint.protocol()) {
            "models.dev"
        } else {
            "prefer"
        };
        let explicit = profile.context_window(model);
        let catalog = self
            .entry(profile, model)
            .and_then(|(_, entry)| entry.context_window)
            .filter(|_| profile.catalog_context_window());
        let window = explicit.or(catalog);
        Selection {
            protocol: endpoint.protocol(),
            window,
            protocol_fact: self.fact(
                endpoint.protocol().name().into(),
                route_source,
                if route_source == "models.dev" {
                    "inferred"
                } else {
                    "declared"
                },
            ),
            window_fact: window.map(|value| {
                self.fact(
                    value.into(),
                    if explicit.is_some() {
                        "configuration"
                    } else {
                        "models.dev"
                    },
                    "declared",
                )
            }),
        }
    }

    /// Merge field-by-field so explicit false and unknown remain different.
    pub fn info(&self, profile: &Profile, model: &str) -> ModelInfo {
        let mut facts = BTreeMap::new();
        let mut sources = Vec::new();
        if profile.suggested_models().contains(&model) {
            sources.push("configuration");
        }
        if self.endpoints.get(profile.name()).is_some_and(|listing| {
            // Endpoint enumeration sorts once; scanning again for each projected
            // model would make a large account listing quadratic under ACP's lock.
            listing.display_names.contains_key(model)
                || listing.entries.contains_key(model)
                || listing
                    .models
                    .binary_search_by(|id| id.as_str().cmp(model))
                    .is_ok()
        }) {
            sources.push("endpoint");
        }
        if let Some((_, entry)) = self.entry(profile, model) {
            sources.push("models.dev");
            for (key, value) in serde_json::to_value(&entry.metadata)
                .expect("metadata serializes")
                .as_object()
                .expect("object")
            {
                if !value.is_null() {
                    facts.insert(
                        key.clone(),
                        self.fact(value.clone(), "models.dev", "declared"),
                    );
                }
            }
            if let Some(window) = entry.context_window {
                facts.insert(
                    "context_window".into(),
                    self.fact(window.into(), "models.dev", "declared"),
                );
            }
        }
        if let Some((listing, name)) = self
            .endpoints
            .get(profile.name())
            .and_then(|listing| listing.display_names.get(model).map(|name| (listing, name)))
        {
            // Account labels override public metadata, but never explicit configuration.
            let mut fact = self.fact(name.clone().into(), "endpoint", "declared");
            fact.fetched_at = listing.fetched_at;
            fact.stale = listing.stale;
            facts.insert("display_name".into(), fact);
        }
        if let Some(listing) = self.endpoints.get(profile.name())
            && let Some(entry) = listing.entries.get(model)
        {
            // Native declarations enrich public metadata below explicit configuration.
            for (key, value) in serde_json::to_value(&entry.metadata)
                .expect("metadata serializes")
                .as_object()
                .expect("object")
            {
                if !value.is_null() {
                    facts.insert(
                        key.clone(),
                        Fact {
                            value: value.clone(),
                            source: listing.provenance,
                            confidence: "declared",
                            stale: listing.stale,
                            fetched_at: listing.fetched_at,
                        },
                    );
                }
            }
            if let Some(window) = entry.context_window {
                facts.insert(
                    "context_window".into(),
                    Fact {
                        value: window.into(),
                        source: listing.provenance,
                        confidence: "declared",
                        stale: listing.stale,
                        fetched_at: listing.fetched_at,
                    },
                );
            }
        }
        for metadata in [Some(profile.metadata()), profile.model_metadata(model)]
            .into_iter()
            .flatten()
        {
            for (key, value) in serde_json::to_value(metadata)
                .expect("metadata serializes")
                .as_object()
                .expect("object")
            {
                if !value.is_null() {
                    facts.insert(
                        key.clone(),
                        self.fact(value.clone(), "configuration", "declared"),
                    );
                }
            }
        }
        if let Some(window) = profile.context_window(model) {
            facts.insert(
                "context_window".into(),
                self.fact(window.into(), "configuration", "declared"),
            );
        }
        let selection = self.resolve(profile, model);
        facts.insert("protocol".into(), selection.protocol_fact);
        if let Some(window) = selection.window_fact {
            facts.insert("effective_context_window".into(), window);
        }
        ModelInfo {
            id: model.to_owned(),
            sources,
            facts,
        }
    }

    /// Deterministic suggestions never become configured defaults.
    pub fn suggestions(&self, profile: &Profile, selected: Option<&str>) -> Vec<ModelInfo> {
        let mut ids: Vec<String> = profile
            .suggested_models()
            .into_iter()
            .map(str::to_owned)
            .collect();
        // Large public catalogs must not make selector deduplication quadratic.
        let mut seen: BTreeSet<String> = ids.iter().cloned().collect();
        let endpoints = self.endpoints.get(profile.name());
        let catalog = profile
            .catalog_provider()
            .and_then(|id| self.providers.get(id));
        for id in selected
            .into_iter()
            .chain(
                endpoints
                    .into_iter()
                    .flat_map(|listing| listing.models.iter().map(String::as_str)),
            )
            .chain(
                catalog
                    .into_iter()
                    .flat_map(|provider| provider.models.keys().map(String::as_str)),
            )
        {
            if seen.insert(id.to_owned()) {
                ids.push(id.to_owned());
            }
        }
        ids.iter().map(|id| self.info(profile, id)).collect()
    }
}

/// Selected protocol and budget are frozen for the entire model/tool loop.
#[derive(Clone)]
pub struct Selection {
    /// Always one of the profile's configured protocols.
    pub protocol: Protocol,
    /// Explicit or opted-in catalog capacity, unknown otherwise.
    pub window: Option<u32>,
    /// Routing decision provenance.
    pub protocol_fact: Fact,
    /// Enforced capacity provenance, not merely an informational catalog limit.
    pub window_fact: Option<Fact>,
}

/// Shared immutable snapshots; no locks are held during network or disk work.
pub struct Service {
    settings: Settings,
    http: Arc<dyn Http>,
    cache: Arc<dyn Cache>,
    clock: Arc<dyn Clock>,
    state: Mutex<Snapshot>,
    refresh_lock: tokio::sync::Mutex<()>,
    refresh_generation: std::sync::atomic::AtomicU64,
    endpoint_epochs: Mutex<BTreeMap<String, u64>>,
    endpoint_identities: Mutex<BTreeMap<String, (Profile, Option<crate::auth::Credential>)>>,
    endpoint_locks: Mutex<BTreeMap<String, Arc<EndpointFlight>>>,
    login_http: Arc<dyn crate::http::LoginHttp>,
    changed: tokio::sync::watch::Sender<u64>,
}

#[derive(Default)]
struct EndpointFlight {
    lock: tokio::sync::Mutex<()>,
    generation: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Service([discovery])")
    }
}

impl Service {
    /// Inject every external dependency; construction itself performs no I/O.
    pub fn new(
        settings: Settings,
        http: Arc<dyn Http>,
        cache: Arc<dyn Cache>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            settings,
            http,
            cache,
            clock,
            state: Mutex::new(Snapshot::default()),
            refresh_lock: tokio::sync::Mutex::new(()),
            refresh_generation: std::sync::atomic::AtomicU64::new(0),
            endpoint_epochs: Mutex::new(BTreeMap::new()),
            endpoint_identities: Mutex::new(BTreeMap::new()),
            endpoint_locks: Mutex::new(BTreeMap::new()),
            login_http: Arc::new(crate::http::AsyncLoginHttp),
            changed: tokio::sync::watch::channel(0).0,
        }
    }

    /// Renewal uses the caller's transport, including hermetic authentication tests.
    pub fn with_login_http(mut self, http: Arc<dyn crate::http::LoginHttp>) -> Self {
        self.login_http = http;
        self
    }

    /// Inference already resolved credentials; do not wait for polling to notice replacement.
    pub(crate) fn observe_account(
        &self,
        profile: &Profile,
        credential: Option<&crate::auth::Credential>,
    ) -> bool {
        let mut identities = self
            .endpoint_identities
            .lock()
            .expect("endpoint identities");
        let changed = identities
            .get(profile.name())
            .is_some_and(|(old, account)| {
                old != profile || !native::same_account(account.as_ref(), credential)
            });
        if changed {
            self.invalidate_endpoint(profile.name());
        }
        identities.insert(
            profile.name().into(),
            (profile.clone(), credential.cloned()),
        );
        changed
    }

    /// Only matching account evidence may constrain an automatically remembered model.
    pub(crate) fn account_snapshot(
        &self,
        profile: &Profile,
        credential: Option<&crate::auth::Credential>,
    ) -> Snapshot {
        // Match identity and copy visibility under the same lock order as account invalidation.
        let identities = self
            .endpoint_identities
            .lock()
            .expect("endpoint identities");
        let mut snapshot = self.snapshot();
        if identities.get(profile.name()).is_none_or(|(old, account)| {
            old != profile || !native::same_account(account.as_ref(), credential)
        }) {
            snapshot.endpoints.remove(profile.name());
        }
        snapshot
    }

    /// Record an actual completed inference only while the same account owns visibility.
    pub fn record_inference_success(
        &self,
        profile: &Profile,
        credential: Option<&crate::auth::Credential>,
        model: &str,
    ) {
        let identities = self
            .endpoint_identities
            .lock()
            .expect("endpoint identities");
        if identities.get(profile.name()).is_none_or(|(old, account)| {
            old != profile || !native::same_account(account.as_ref(), credential)
        }) {
            return;
        }
        let mut state = self.state.lock().expect("model state lock");
        let Some(listing) = state.endpoints.get_mut(profile.name()) else {
            // Manual inference without any discovered catalogue has no options to refresh.
            return;
        };
        listing
            .inference_succeeded_at
            .insert(model.into(), self.clock.now());
        drop(state);
        drop(identities);
        self.notify();
    }

    /// Independent connection workers keep a slow account from delaying another.
    pub async fn run_endpoint(&self, profile: &Profile, env: &Environment) {
        let mut backoff = 60;
        loop {
            let delay = if self.list_endpoint(profile, env).await.is_ok() {
                backoff = 60;
                300
            } else {
                let delay = backoff;
                backoff = (backoff * 2).min(3600);
                delay
            };
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    }

    /// Production dependencies, with optional memory-only cache fallback.
    pub fn production(settings: Settings, env: &Environment) -> Self {
        Self::new(
            settings,
            Arc::new(crate::http::DiscoveryHttp),
            Arc::new(FileCache(env.model_cache_file())),
            Arc::new(SystemClock),
        )
    }

    /// Latest data only; never waits for a request or cache read.
    pub fn snapshot(&self) -> Snapshot {
        let mut snapshot = self.state.lock().expect("model state lock").clone();
        if snapshot.fetched_at.is_some_and(|time| {
            self.clock.now().saturating_sub(time) >= self.settings.refresh_interval.as_secs()
        }) {
            snapshot.stale = true;
        }
        for listing in snapshot.endpoints.values_mut() {
            listing.stale |= listing
                .fetched_at
                .is_none_or(|time| self.clock.now().saturating_sub(time) >= 300);
        }
        snapshot
    }

    /// Subscribe before starting the worker so no publication is lost.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub(crate) fn notify(&self) {
        self.changed
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    /// Cache loading runs on a blocking worker, never the protocol dispatcher.
    pub async fn load_cache(&self) {
        if !self.settings.enabled {
            return;
        }
        let cache = Arc::clone(&self.cache);
        let now = self.clock.now();
        // JSON parsing can be substantial for a whole public registry. Keep it,
        // as well as disk reads, off the single-threaded protocol reactor.
        let loaded = tokio::task::spawn_blocking(move || {
            let bytes = cache.load()?.ok_or(Error::Unavailable)?;
            if bytes.len() > MAX_CATALOG_BYTES + 4096 {
                return Err(Error::Limit);
            }
            let envelope: Envelope =
                serde_json::from_slice(&bytes).map_err(|_| Error::Malformed)?;
            if envelope.version != 1 || envelope.fetched_at > now {
                return Err(Error::Malformed);
            }
            Ok((parse_value(&envelope.catalog)?, envelope.fetched_at))
        })
        .await;
        if let Ok(Ok((providers, fetched_at))) = loaded {
            let mut state = self.state.lock().expect("model state lock");
            // A late cache load must never overwrite a completed refresh.
            if state.fetched_at.is_none() {
                state.providers = Arc::new(providers);
                state.fetched_at = Some(fetched_at);
            }
            drop(state);
            self.notify();
        }
    }

    /// Explicit refresh overrides automatic-refresh policy, not disabled catalog use.
    pub async fn refresh(&self) -> Result<(), Error> {
        if !self.settings.enabled {
            return Err(Error::Unavailable);
        }
        // Concurrent callers share the request already in flight, including failure.
        let generation = self
            .refresh_generation
            .load(std::sync::atomic::Ordering::SeqCst);
        let _guard = self.refresh_lock.lock().await;
        if generation
            != self
                .refresh_generation
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            return self.snapshot().error.map_or(Ok(()), Err);
        }
        let result = async {
            let bytes = self
                .http
                .get(
                    CATALOG_URL,
                    reqwest::header::HeaderMap::new(),
                    MAX_CATALOG_BYTES,
                )
                .await?;
            let fetched_at = self.clock.now();
            let cache = Arc::clone(&self.cache);
            // Parsing/serialization and cache publication must not monopolize ACP.
            let (providers, cache_error) = tokio::task::spawn_blocking(move || {
                let providers = parse(&bytes)?;
                let catalog = serde_json::from_slice(&bytes).map_err(|_| Error::Malformed)?;
                let bytes = serde_json::to_vec(&Envelope {
                    version: 1,
                    fetched_at,
                    catalog,
                })
                .map_err(|_| Error::Malformed)?;
                Ok::<_, Error>((providers, cache.store(&bytes).err()))
            })
            .await
            .map_err(|_| Error::Cache)??;
            let mut state = self.state.lock().expect("model state lock");
            state.providers = Arc::new(providers);
            state.fetched_at = Some(fetched_at);
            state.stale = false;
            state.error = None;
            state.cache_error = cache_error;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let mut state = self.state.lock().expect("model state lock");
            state.error = Some(error);
            state.stale = state.fetched_at.is_some();
        }
        self.refresh_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.notify();
        result
    }

    /// CLI queries can wait; inference only ever calls snapshot().
    pub async fn prepare(&self) {
        self.load_cache().await;
        let snapshot = self.snapshot();
        if self.settings.refresh && (snapshot.fetched_at.is_none() || snapshot.stale) {
            let _ = self.refresh().await;
        }
    }

    /// Automatic catalog work is owned by its caller's connection task.
    pub async fn run(&self) {
        self.load_cache().await;
        if !self.settings.enabled || !self.settings.refresh {
            return;
        }
        let mut backoff = 60;
        loop {
            let snapshot = self.snapshot();
            let delay = if snapshot.fetched_at.is_none() || snapshot.stale {
                if self.refresh().await.is_err() {
                    let delay = backoff;
                    backoff = (backoff * 2).min(3600);
                    delay
                } else {
                    backoff = 60;
                    self.settings.refresh_interval.as_secs()
                }
            } else {
                self.settings
                    .refresh_interval
                    .as_secs()
                    .saturating_sub(
                        self.clock
                            .now()
                            .saturating_sub(snapshot.fetched_at.unwrap_or(0)),
                    )
                    .max(1)
            };
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    }

    /// Forget account-specific visibility as soon as credentials change.
    pub fn invalidate_endpoint(&self, profile: &str) -> u64 {
        let mut epochs = self.endpoint_epochs.lock().expect("endpoint epochs lock");
        let epoch = epochs.entry(profile.to_owned()).or_default();
        *epoch = epoch.wrapping_add(1);
        let generation = *epoch;
        self.state
            .lock()
            .expect("model state lock")
            .endpoints
            .remove(profile);
        drop(epochs);
        self.notify();
        generation
    }

    /// Refresh an account-scoped listing, never persisting its IDs or headers.
    pub async fn list_endpoint(
        &self,
        profile: &Profile,
        env: &Environment,
    ) -> Result<EndpointListing, Error> {
        let flight = self
            .endpoint_locks
            .lock()
            .expect("endpoint locks")
            .entry(profile.name().into())
            .or_default()
            .clone();
        let generation = flight.generation.load(std::sync::atomic::Ordering::SeqCst);
        let _guard = flight.lock.lock().await;
        let credential = native::credential(profile, env, self.login_http.as_ref(), |credential| {
            self.observe_account(profile, credential);
        })
        .await;
        if let Ok(credential) = &credential {
            let changed = self.observe_account(profile, credential.as_ref());
            if !changed
                && generation != flight.generation.load(std::sync::atomic::Ordering::SeqCst)
                && let Some(listing) = self.snapshot().endpoints.get(profile.name()).cloned()
            {
                return listing.error.map_or(Ok(listing), Err);
            }
        }
        let epoch = *self
            .endpoint_epochs
            .lock()
            .expect("endpoint epochs lock")
            .entry(profile.name().into())
            .or_default();
        let result = match credential {
            Ok(ref credential) => {
                // Bound a whole paginated refresh, not only each individual page.
                tokio::time::timeout(
                    Duration::from_secs(30),
                    native::discover(self.http.as_ref(), profile, credential.as_ref()),
                )
                .await
                .unwrap_or(Err(Error::Transport))
            }
            Err(error) => Err(error),
        };
        // Re-read without renewing: token rotation is harmless, replacement/logout is not.
        let result = if credential.is_ok() {
            match native::current_credential(profile, env).await {
                Ok(current)
                    if native::same_account(
                        current.as_ref(),
                        credential.as_ref().ok().and_then(Option::as_ref),
                    ) =>
                {
                    result
                }
                Ok(_) | Err(Error::Unauthorized) => Err(Error::Unauthorized),
                Err(error) if result.is_ok() => Err(error),
                Err(_) => result,
            }
        } else {
            result
        };
        let mut listing = match result {
            Ok(mut listing) => {
                listing.fetched_at = Some(self.clock.now());
                if let Some(previous) = self.snapshot().endpoints.get(profile.name()) {
                    listing.inference_succeeded_at = previous.inference_succeeded_at.clone();
                }
                listing
            }
            Err(error) => {
                let mut listing = if matches!(error, Error::Unauthorized) {
                    native::empty_listing(profile)
                } else {
                    self.snapshot()
                        .endpoints
                        .get(profile.name())
                        .cloned()
                        .unwrap_or_else(|| native::empty_listing(profile))
                };
                listing.error = Some(error);
                listing.stale = true;
                listing
            }
        };
        listing.attempted_at = self.clock.now();
        let epochs = self.endpoint_epochs.lock().expect("endpoint epochs lock");
        // Login/invalidation can race an old account's in-flight response. Never
        // publish that response after visibility was explicitly invalidated.
        if epochs.get(profile.name()) != Some(&epoch) {
            return Err(Error::Credentials);
        }
        self.state
            .lock()
            .expect("model state lock")
            .endpoints
            .insert(profile.name().to_owned(), listing.clone());
        drop(epochs);
        flight
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.notify();
        listing.error.map_or(Ok(listing), Err)
    }
}

/// Enumerate only configured OpenAI-compatible endpoints, with existing credentials.
pub async fn endpoint_models(
    http: &dyn Http,
    profile: &Profile,
    env: &Environment,
) -> Result<Vec<String>, Error> {
    let credential = native::credential(profile, env, &crate::http::AsyncLoginHttp, |_| {}).await?;
    native::discover(http, profile, credential.as_ref())
        .await
        .map(|listing| listing.models)
}
