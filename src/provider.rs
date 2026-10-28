//! Compiled-in provider implementations and their protocol/authentication capabilities.

pub use crate::account::cache;
pub(crate) mod cloudflare;
pub mod registry;
pub mod runtime;
pub mod signin;

pub(crate) use crate::account::cloud_identifier;
pub(crate) use crate::account::normalize_base_url;
pub use crate::account::{
    CredentialSource, Endpoint, GoogleCloud, MaxTokensField, Protocol, ProtocolOptions,
    check_model_id,
};
pub use registry::{AuthMethod, Definition as Provider};
