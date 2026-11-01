//! Implemented prompt transports, separate from model declarations and availability.

use super::Protocol;
use crate::account::Account;

pub(crate) fn supports(account: &Account, protocol: Protocol, modality: &str) -> bool {
    if account.is_copilot() || account.is_chatgpt() {
        // These subscription contracts implement inline vision, not general API file/audio inputs.
        return modality == "image";
    }
    if matches!(account.provider(), "cloudflare" | "google-cloud") {
        // Their REST/Vertex compatibility contracts differ from generic OpenAI media parts.
        return false;
    }
    if modality == "pdf"
        && (account.provider() == "mistral"
            || account
                .endpoint(protocol)
                .is_some_and(|endpoint| endpoint.base_url() == "https://api.mistral.ai/v1"))
    {
        // Mistral requires document_url parts; OpenAI's file shape does not implement that contract.
        return false;
    }
    match modality {
        "image" | "pdf" => true,
        "audio" => protocol == Protocol::OpenAiChat,
        _ => false,
    }
}
