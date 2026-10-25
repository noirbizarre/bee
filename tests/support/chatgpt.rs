//! Official-flow fixtures shared by CLI and ACP success-path tests.
#![allow(dead_code)]

use std::sync::{
    Mutex,
    atomic::{AtomicU16, Ordering},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bee::http::{HttpResponse, LoginHttp, Unreachable};
use futures::future::BoxFuture;
use rsa::{pkcs1::DecodeRsaPrivateKey, traits::PublicKeyParts};
use serde_json::{Value, json};

const KEY: &str = include_str!("../fixtures/oidc-test-key.pem");
pub const ACCESS: &str = "fixture-access-secret";
pub const RENEWED: &str = "fixture-renewed-secret";

pub struct OAuth {
    nonce: Mutex<String>,
    pub scopes: Mutex<String>,
    pub grants: Mutex<Vec<String>>,
    pub revoke_status: AtomicU16,
    jwks: Value,
}

impl OAuth {
    pub fn new() -> Self {
        let key = rsa::RsaPrivateKey::from_pkcs1_pem(KEY).unwrap();
        Self {
            nonce: Mutex::new(String::new()),
            scopes: Mutex::new(
                "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"
                    .into(),
            ),
            grants: Mutex::new(Vec::new()),
            revoke_status: AtomicU16::new(200),
            jwks: json!({"keys":[{"kty":"RSA","kid":"fixture","use":"sig","alg":"RS256","n":URL_SAFE_NO_PAD.encode(key.n().to_bytes_be()),"e":URL_SAFE_NO_PAD.encode(key.e().to_bytes_be())}]}),
        }
    }

    pub fn callback(&self, authorize: &str) -> reqwest::Url {
        let url = reqwest::Url::parse(authorize).unwrap();
        let query: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert!(!query.contains_key("id_token_hint"));
        *self.nonce.lock().unwrap() = query["nonce"].clone();
        let mut callback = reqwest::Url::parse(&query["redirect_uri"]).unwrap();
        callback
            .query_pairs_mut()
            .append_pair("state", &query["state"])
            .append_pair("code", "fixture-authorization-secret")
            .append_pair("client_id", "issued-client");
        callback
    }

    pub fn browser(&self, authorize: &str) {
        use std::io::{Read, Write};
        let callback = self.callback(authorize);
        std::thread::spawn(move || {
            let mut socket =
                std::net::TcpStream::connect(("127.0.0.1", callback.port().unwrap())).unwrap();
            write!(
                socket,
                "GET {}?{} HTTP/1.1\r\nHost: localhost\r\n\r\n",
                callback.path(),
                callback.query().unwrap()
            )
            .unwrap();
            let mut response = String::new();
            socket.read_to_string(&mut response).unwrap();
            assert!(response.starts_with("HTTP/1.1 200"));
        });
    }

    fn token(&self, renewed: bool) -> Value {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some("fixture".into());
        let claims = json!({"iss":"https://auth.openai.com","aud":"issued-client","sub":"account","nonce":self.nonce.lock().unwrap().clone(),"exp":bee::auth::unix_now()+3600});
        let id = jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_rsa_pem(KEY.as_bytes()).unwrap(),
        )
        .unwrap();
        json!({"access_token":if renewed {RENEWED} else {ACCESS},"refresh_token":"fixture-refresh-secret","id_token":id,"token_type":"Bearer","scope":self.scopes.lock().unwrap().clone(),"expires_in":3600})
    }
}

impl LoginHttp for OAuth {
    fn post_json<'a>(
        &'a self,
        _: &'a str,
        _: String,
    ) -> BoxFuture<'a, Result<HttpResponse, Unreachable>> {
        Box::pin(async { panic!("OAuth must use form encoding") })
    }
    fn post_form<'a>(
        &'a self,
        url: &'a str,
        body: String,
    ) -> BoxFuture<'a, Result<HttpResponse, Unreachable>> {
        Box::pin(async move {
            if url.ends_with("/revoke") {
                return Ok(HttpResponse {
                    status: self.revoke_status.load(Ordering::SeqCst),
                    body: String::new(),
                });
            }
            assert_eq!(url, "https://auth.openai.com/api/accounts/oauth/token");
            let query = reqwest::Url::parse(&format!("https://localhost?{body}")).unwrap();
            let fields: std::collections::BTreeMap<_, _> =
                query.query_pairs().into_owned().collect();
            assert_eq!(fields["client_id"], "issued-client");
            assert_eq!(fields["resource"], "https://api.openai.com/v1");
            let grant = fields["grant_type"].clone();
            self.grants.lock().unwrap().push(grant.clone());
            Ok(HttpResponse {
                status: 200,
                body: self.token(grant == "refresh_token").to_string(),
            })
        })
    }
    fn get<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<HttpResponse, Unreachable>> {
        Box::pin(async move {
            Ok(HttpResponse {status:200,body:if url.ends_with("openid-configuration") {json!({"issuer":"https://auth.openai.com","jwks_uri":"https://auth.openai.com/keys","revocation_endpoint":"https://auth.openai.com/revoke"})} else {self.jwks.clone()}.to_string()})
        })
    }
}
