//! Scaleway serverless routing; project scope belongs to the account, not the model.

use toml::{Table, Value};

pub(crate) fn validate_field(name: &str, text: &str) -> Result<(), &'static str> {
    match name {
        "project_id" => {
            // Only a UUID can occupy the project path segment; URL fragments must never become routing context.
            if text.len() != 36
                || !text.bytes().enumerate().all(|(index, byte)| {
                    if matches!(index, 8 | 13 | 18 | 23) {
                        byte == b'-'
                    } else {
                        byte.is_ascii_hexdigit()
                    }
                })
            {
                return Err(
                    "Scaleway project_id must be a hyphenated UUID; copy it from Project settings",
                );
            }
        }
        // The global host currently serves Paris; accepting other regions would promise routing we cannot provide.
        "region" if text != "fr-par" => {
            return Err(
                "Scaleway Generative APIs supports only region fr-par; omit region or use fr-par",
            );
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn base_url(settings: &Table) -> String {
    match settings.get("project_id").and_then(Value::as_str) {
        Some(project) => format!("https://api.scaleway.ai/{project}/v1"),
        None => "https://api.scaleway.ai/v1".into(),
    }
}

pub(crate) fn defaults(settings: &Table, mut defaults: Table) -> Table {
    defaults["protocols"]["openai-chat"]
        .as_table_mut()
        .expect("registered Chat endpoint")
        .insert("base_url".into(), Value::String(base_url(settings)));
    defaults
}

pub(crate) fn reserved_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "x-api-key" | "x-auth-token"
    )
}

#[cfg(test)]
mod tests;
