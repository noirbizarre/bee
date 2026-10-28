//! Turn-pinned native authority, independent of protocol adapters and ACP ownership.

use futures::future::BoxFuture;
use reqwest::header::{HeaderMap, HeaderValue};

use crate::account::Account as Instance;
use crate::auth::Secret;
use crate::llm::Error;

/// Provider-owned request-boundary checks and renewal. Implementations must not switch accounts.
pub trait Prepared: Send + Sync {
    /// Validate pinned authority and return the credential for this request.
    fn credential<'a>(
        &'a self,
        instance: &'a Instance,
        previous: Option<&'a Secret>,
    ) -> BoxFuture<'a, Result<Option<Secret>, Error>>;
    /// Trusted account-specific routing, if the provider resolved one.
    fn base_url(&self) -> Option<&str> {
        None
    }
    /// Add only implementation-owned headers after ordinary header validation.
    fn headers(&self, _headers: &mut HeaderMap, _agent: bool) -> Result<(), Error> {
        Ok(())
    }
}

impl Prepared for crate::auth::chatgpt::Session {
    fn credential<'a>(
        &'a self,
        instance: &'a Instance,
        _previous: Option<&'a Secret>,
    ) -> BoxFuture<'a, Result<Option<Secret>, Error>> {
        Box::pin(async move {
            if !instance.is_chatgpt() {
                return Err(Error::ChatGpt(
                    "renewable sessions require a validated ChatGPT instance",
                ));
            }
            self.credential().await.map(Some).map_err(|_| {
                Error::ChatGpt(
                    "cannot renew authentication; retry later or run `bee auth login <instance>`",
                )
            })
        })
    }
}

impl Prepared for crate::auth::google_cloud::Session {
    fn credential<'a>(
        &'a self,
        instance: &'a Instance,
        _previous: Option<&'a Secret>,
    ) -> BoxFuture<'a, Result<Option<Secret>, Error>> {
        Box::pin(async move {
            self.check(instance).map_err(|_| {
                Error::GoogleCloud("prepared session does not match the Cloud target")
            })?;
            self.credential()
                .await
                .map(|credential| Some(credential.secret().clone()))
                .map_err(|_| {
                    Error::GoogleCloud(
                        "ADC changed or cannot be renewed; check credentials and start a new turn",
                    )
                })
        })
    }
    fn headers(&self, headers: &mut HeaderMap, _agent: bool) -> Result<(), Error> {
        if let Some(project) = self.quota_project() {
            let mut value = HeaderValue::from_str(project).map_err(|_| Error::Headers)?;
            value.set_sensitive(true);
            headers.insert("x-goog-user-project", value);
        }
        Ok(())
    }
}

impl Prepared for crate::auth::copilot::Session {
    fn credential<'a>(
        &'a self,
        instance: &'a Instance,
        previous: Option<&'a Secret>,
    ) -> BoxFuture<'a, Result<Option<Secret>, Error>> {
        Box::pin(async move {
            if !instance.is_copilot() {
                return Err(Error::Copilot(
                    "prepared session does not match the provider type",
                ));
            }
            self.check(instance).await.map_err(|_| {
                Error::Copilot(
                    "credentials changed or became unavailable; start a new turn or repeat login",
                )
            })?;
            if previous != Some(self.credential.secret()) {
                return Err(Error::Copilot(
                    "request credential does not match the prepared session",
                ));
            }
            Ok(Some(self.credential.secret().clone()))
        })
    }
    fn base_url(&self) -> Option<&str> {
        Some(&self.target)
    }
    fn headers(&self, headers: &mut HeaderMap, agent: bool) -> Result<(), Error> {
        headers.insert(
            "openai-intent",
            HeaderValue::from_static("conversation-edits"),
        );
        headers.insert(
            "x-interaction-type",
            HeaderValue::from_static("conversation-agent"),
        );
        headers.insert(
            "x-interaction-id",
            HeaderValue::from_str(&self.interaction).map_err(|_| Error::Headers)?,
        );
        headers.insert(
            "x-initiator",
            HeaderValue::from_static(if agent { "agent" } else { "user" }),
        );
        Ok(())
    }
}
