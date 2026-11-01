//! Portable inline user content. URIs never authorize fetching or filesystem access.

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Error, Message};
use crate::provider::Protocol;

/// One ordered user part, independent of ACP and provider continuation formats.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum UserPart {
    /// Supplied user text or described embedded context.
    Text(String),
    /// Inline binary input; validated again at the model boundary after recovery.
    Media {
        /// MIME type, never inferred from a URI.
        mime: String,
        /// Standard padded base64.
        data: String,
    },
}

impl std::fmt::Debug for UserPart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UserPart([redacted])")
    }
}

/// Fixed errors keep attachment contents out of diagnostics.
pub(crate) fn invalid(reason: &'static str) -> Error {
    Error::Content(reason)
}

/// Count serialized envelopes without allocating another potentially oversized attachment copy.
pub(crate) fn check_envelope<T: Serialize + ?Sized>(value: &T) -> Result<(), Error> {
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("content limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(24 * 1024 * 1024), value)
        .map_err(|_| invalid("reduce prompt content to 24 MiB serialized"))
}

impl UserPart {
    /// Validate supplied bytes before retaining them or creating a native request.
    pub fn media(mime: String, data: String) -> Result<Self, Error> {
        let part = Self::Media { mime, data };
        part.bytes()?;
        Ok(part)
    }

    pub(crate) fn modality(&self) -> Option<&'static str> {
        match self {
            Self::Text(_) => None,
            Self::Media { mime, .. } => match mime.as_str() {
                "image/png" | "image/jpeg" | "image/gif" | "image/webp" => Some("image"),
                "audio/wav" | "audio/x-wav" | "audio/mpeg" | "audio/mp3" => Some("audio"),
                "application/pdf" => Some("pdf"),
                _ => Some("unsupported"),
            },
        }
    }

    fn bytes(&self) -> Result<usize, Error> {
        let Self::Media { data, .. } = self else {
            return Ok(0);
        };
        if self.modality() == Some("unsupported") {
            return Err(invalid(
                "unsupported media MIME type; use PNG, JPEG, GIF, WebP, WAV, MP3 or PDF",
            ));
        }
        // Check the encoded envelope before decoding to bound temporary allocations.
        const MAX: usize = 8 * 1024 * 1024;
        if data.is_empty() || data.len() > MAX.div_ceil(3) * 4 {
            return Err(invalid("media must be nonempty and at most 8 MiB decoded"));
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| invalid("invalid media base64; send standard padded base64"))?;
        if decoded.is_empty() || decoded.len() > MAX {
            return Err(invalid("media must be nonempty and at most 8 MiB decoded"));
        }
        Ok(decoded.len())
    }

    /// Enforce known adapter envelopes both before insertion and after recovery/route resolution.
    pub(crate) fn check_route(&self, protocol: Protocol) -> Result<(), Error> {
        if protocol == Protocol::AnthropicMessages
            && self.modality() == Some("image")
            && let Self::Media { data, .. } = self
            && data.len() > 5 * 1024 * 1024
        {
            return Err(invalid(
                "Anthropic image input exceeds 5 MiB encoded; reduce the image size",
            ));
        }
        Ok(())
    }

    pub(super) fn wire(&self, protocol: Protocol) -> Result<Value, Error> {
        if let Self::Text(text) = self {
            return Ok(
                json!({"type": if protocol == Protocol::OpenAiResponses { "input_text" } else { "text" }, "text":text}),
            );
        }
        let Self::Media { mime, data } = self else {
            unreachable!()
        };
        let url = || format!("data:{mime};base64,{data}");
        match (protocol, self.modality()) {
            (Protocol::OpenAiChat, Some("image")) => {
                Ok(json!({"type":"image_url","image_url":{"url":url()}}))
            }
            (Protocol::OpenAiResponses, Some("image")) => {
                Ok(json!({"type":"input_image","image_url":url()}))
            }
            (Protocol::AnthropicMessages, Some("image")) => {
                Ok(json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}}))
            }
            (Protocol::OpenAiChat, Some("audio")) => Ok(
                json!({"type":"input_audio","input_audio":{"data":data,"format":if mime == "audio/wav" || mime == "audio/x-wav" { "wav" } else { "mp3" }}}),
            ),
            (Protocol::OpenAiChat, Some("pdf")) => {
                Ok(json!({"type":"file","file":{"filename":"context.pdf","file_data":url()}}))
            }
            (Protocol::OpenAiResponses, Some("pdf")) => {
                Ok(json!({"type":"input_file","filename":"context.pdf","file_data":url()}))
            }
            (Protocol::AnthropicMessages, Some("pdf")) => Ok(
                json!({"type":"document","source":{"type":"base64","media_type":mime,"data":data}}),
            ),
            _ => Err(invalid(
                "selected protocol cannot consume this media; select a compatible model and route",
            )),
        }
    }
}

/// Bound one user turn independently from the context token estimate.
pub(crate) fn validate_parts(parts: &[UserPart]) -> Result<(), Error> {
    check_envelope(parts)?;
    if parts
        .iter()
        .filter(|part| part.modality().is_some())
        .count()
        > 32
    {
        return Err(invalid(
            "reduce attachments to 32 parts and 16 MiB decoded per prompt",
        ));
    }
    let mut bytes = 0usize;
    for part in parts {
        bytes = bytes.saturating_add(part.bytes()?);
    }
    if bytes > 16 * 1024 * 1024 {
        return Err(invalid(
            "reduce attachments to 32 parts and 16 MiB decoded per prompt",
        ));
    }
    Ok(())
}

pub(super) fn validate(model: &super::Model, messages: &[Message]) -> Result<(), Error> {
    for message in messages {
        let Message::UserParts(parts) = message else {
            continue;
        };
        validate_parts(parts)?;
        for part in parts {
            if let Some(modality) = part.modality() {
                if !model
                    .selection
                    .input_modalities
                    .iter()
                    .any(|value| value == modality)
                {
                    return Err(invalid(
                        "selected model does not declare this input modality; configure input_modalities or select a compatible model (retained attachments also require support)",
                    ));
                }
                if !route_supports(&model.profile, model.selection.protocol, modality) {
                    return Err(invalid(
                        "selected provider route cannot consume this media; select a compatible model and route",
                    ));
                }
                part.check_route(model.selection.protocol)?;
            }
        }
    }
    Ok(())
}

/// Implemented transport support, independent of model availability and declarations.
pub(crate) fn route_supports(
    account: &crate::account::Account,
    protocol: Protocol,
    modality: &str,
) -> bool {
    crate::provider::content::supports(account, protocol, modality)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_part_and_aggregate_limits_are_independent() {
        let part = UserPart::media("image/png".into(), "aGk=".into()).unwrap();
        assert!(validate_parts(&vec![part.clone(); 32]).is_ok());
        assert!(validate_parts(&vec![part; 33]).is_err());
        let data = base64::engine::general_purpose::STANDARD.encode(vec![0; 8 * 1024 * 1024]);
        let part = UserPart::media("application/pdf".into(), data).unwrap();
        assert!(validate_parts(&[part.clone(), part.clone()]).is_ok());
        // Exceed decoded aggregate bytes while still fitting the separate serialized envelope.
        let extra = UserPart::media("application/pdf".into(), "aGk=".into()).unwrap();
        let over_total = [part.clone(), part, extra];
        assert!(check_envelope(&over_total).is_ok());
        assert!(validate_parts(&over_total).is_err());
        let oversized =
            base64::engine::general_purpose::STANDARD.encode(vec![0; 8 * 1024 * 1024 + 1]);
        assert!(UserPart::media("application/pdf".into(), oversized).is_err());
    }
}
