//! An in-memory store so authentication behavior is tested independently of OS permissions.

use std::cell::RefCell;
use std::collections::BTreeMap;

use bee::auth::{Audience, Credential, CredentialStore};

#[derive(Default)]
pub struct MemoryStore {
    credentials: RefCell<BTreeMap<(&'static str, String), Credential>>,
}

// Keep audiences distinct even when a provider and an MCP server share a name.
fn key(audience: Audience<'_>) -> (&'static str, String) {
    match audience {
        Audience::Provider(name) => ("provider", name.to_owned()),
        Audience::McpServer(name) => ("mcp", name.to_owned()),
    }
}

impl CredentialStore for MemoryStore {
    fn get(&self, audience: Audience<'_>) -> bee::Result<Option<Credential>> {
        Ok(self.credentials.borrow().get(&key(audience)).cloned())
    }

    fn put(&self, audience: Audience<'_>, credential: &Credential) -> bee::Result<()> {
        self.credentials
            .borrow_mut()
            .insert(key(audience), credential.clone());
        Ok(())
    }

    fn remove(&self, audience: Audience<'_>) -> bee::Result<bool> {
        Ok(self
            .credentials
            .borrow_mut()
            .remove(&key(audience))
            .is_some())
    }
}
