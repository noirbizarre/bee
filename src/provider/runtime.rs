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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Credential, copilot};
    use crate::http::{HttpResponse, LoginHttp, Unreachable};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct AccountHttp(AtomicUsize);

    struct CloudHttp(AtomicUsize);

    impl crate::auth::google_cloud::Http for CloudHttp {
        fn request(
            &self,
            request: crate::auth::google_cloud::Request,
        ) -> BoxFuture<'_, crate::Result<serde_json::Value>> {
            assert!(matches!(
                request,
                crate::auth::google_cloud::Request::Token(_)
            ));
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(
                    serde_json::json!({"access_token":"private-cloud-access", "token_type":"Bearer", "expires_in":3600}),
                )
            })
        }
    }

    #[tokio::test]
    async fn prepared_cloud_runtime_rejects_target_and_adc_changes_before_another_token_request() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("adc.json");
        let mut document = serde_json::json!({"type":"authorized_user", "client_id":"fixture-client", "client_secret":"private-client-secret", "refresh_token":"private-refresh"});
        std::fs::write(&path, document.to_string()).unwrap();
        let env = crate::config::Environment::new(None, Some(root.path().into()), None)
            .with_variable("GOOGLE_APPLICATION_CREDENTIALS", path.to_str().unwrap());
        let accounts = crate::test_support::configured_providers(
            "[accounts.work]\nprovider='google-cloud'\ngoogle_cloud={project='resource-project',location='global'}\n[accounts.other]\nprovider='google-cloud'\ngoogle_cloud={project='other-project',location='global'}",
        );
        let original = accounts.get("work").unwrap();
        let http = std::sync::Arc::new(CloudHttp(AtomicUsize::new(0)));
        let session = crate::auth::google_cloud::Session::with_http(original, &env, http.clone())
            .await
            .unwrap();
        let first = Prepared::credential(&session, original, None)
            .await
            .unwrap()
            .unwrap();
        let error = Prepared::credential(&session, accounts.get("other").unwrap(), Some(&first))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Error::GoogleCloud("prepared session does not match the Cloud target")
        ));
        document["refresh_token"] = "private-replacement-refresh".into();
        std::fs::write(&path, document.to_string()).unwrap();
        let error = Prepared::credential(&session, original, Some(&first))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Error::GoogleCloud(
                "ADC changed or cannot be renewed; check credentials and start a new turn"
            )
        ));
        assert!(!format!("{error} {error:?}").contains("private-"));
        assert_eq!(http.0.load(Ordering::SeqCst), 1);
    }

    impl LoginHttp for AccountHttp {
        fn post_json<'a>(
            &'a self,
            _: &'a str,
            _: String,
        ) -> BoxFuture<'a, Result<HttpResponse, Unreachable>> {
            panic!("prepared authority checks must not start a login")
        }

        fn get_authenticated<'a>(
            &'a self,
            _: &'a str,
            _: HeaderMap,
        ) -> BoxFuture<'a, Result<HttpResponse, Unreachable>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(HttpResponse { status: 200, body: r#"{"chat_enabled":true,"endpoints":{"api":"https://api.githubcopilot.com"}}"#.into() })
            })
        }
    }

    #[tokio::test]
    async fn prepared_copilot_runtime_refuses_wrong_providers_accounts_and_request_credentials() {
        let accounts = crate::test_support::configured_providers(
            "[accounts.work]\nprovider='github'\ncredential={env='GITHUB_TOKEN'}\n[accounts.other]\nprovider='github'\ncredential={env='OTHER_TOKEN'}\n[accounts.api]\nprovider='openai'",
        );
        let profile = accounts.get("work").unwrap();
        let credential =
            Credential::new(Secret::new("private-github-token")).for_provider("github");
        let http = AccountHttp(AtomicUsize::new(0));
        let session = copilot::Session::prepare(
            profile,
            credential.clone(),
            None,
            "interaction".into(),
            &http,
        )
        .await
        .unwrap();
        assert_eq!(
            Prepared::credential(&session, profile, Some(credential.secret()))
                .await
                .unwrap()
                .unwrap(),
            *credential.secret()
        );
        for (account, previous, reason) in [
            (
                "api",
                Some(credential.secret()),
                "prepared session does not match the provider type",
            ),
            (
                "other",
                Some(credential.secret()),
                "credentials changed or became unavailable; start a new turn or repeat login",
            ),
            (
                "work",
                None,
                "request credential does not match the prepared session",
            ),
            (
                "work",
                Some(&Secret::new("private-replacement-token")),
                "request credential does not match the prepared session",
            ),
        ] {
            let error = Prepared::credential(&session, accounts.get(account).unwrap(), previous)
                .await
                .unwrap_err();
            assert!(matches!(&error, Error::Copilot(actual) if *actual == reason));
            assert!(!format!("{error} {error:?}").contains("private-"));
        }
        assert_eq!(http.0.load(Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prepared_copilot_runtime_cannot_resume_after_logout_and_identical_token_reimport() {
        use crate::auth::{Audience, CredentialStore, FileStore};

        let root = tempfile::tempdir().unwrap();
        let store = FileStore::new(root.path().join("credentials.json"));
        let accounts = crate::test_support::configured_providers(
            "[accounts.work]\nprovider='github'\ncredential={store=true}",
        );
        let profile = accounts.get("work").unwrap();
        let credential =
            Credential::new(Secret::new("private-github-token")).for_provider("github");
        store.put(Audience::Provider("work"), &credential).unwrap();
        let http = AccountHttp(AtomicUsize::new(0));
        let session = copilot::Session::prepare(
            profile,
            credential.clone(),
            Some(store.clone()),
            "interaction".into(),
            &http,
        )
        .await
        .unwrap();
        for reimport in [false, true] {
            store.remove(Audience::Provider("work")).unwrap();
            if reimport {
                store.put(Audience::Provider("work"), &credential).unwrap();
            }
            let error = Prepared::credential(&session, profile, Some(credential.secret()))
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                Error::Copilot(
                    "credentials changed or became unavailable; start a new turn or repeat login"
                )
            ));
            assert!(!format!("{error} {error:?}").contains("private-github-token"));
        }
        assert_eq!(http.0.load(Ordering::SeqCst), 1);
    }
}
