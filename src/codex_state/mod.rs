mod chat;
mod exchange;

use snafu::ResultExt;
use wreq_util::Emulation;

use crate::{
    config::{CLEWDR_CONFIG, CODEX_CHATGPT_API_BASE, CodexCredential},
    error::{ClewdrError, WreqSnafu},
    services::codex_actor::CodexActorHandle,
};

#[derive(Clone)]
pub struct CodexState {
    pub codex_actor_handle: CodexActorHandle,
    pub credential: Option<CodexCredential>,
    pub client: wreq::Client,
    pub endpoint: url::Url,
    pub stream: bool,
}

pub enum CodexTokenStatus {
    None,
    Stale,
    Valid,
}

impl CodexState {
    pub fn new(codex_actor_handle: CodexActorHandle) -> Self {
        let proxy = CLEWDR_CONFIG.load().wreq_proxy.to_owned();
        let mut builder = wreq::Client::builder().emulation(Emulation::Chrome136);
        if let Some(ref p) = proxy {
            builder = builder.proxy(p.to_owned());
        }
        let client = builder
            .build()
            .expect("Failed to build codex HTTP client");

        // Device code login tokens use ChatGPT backend, not api.openai.com
        let endpoint =
            url::Url::parse(CODEX_CHATGPT_API_BASE).expect("Failed to parse CODEX_CHATGPT_API_BASE");

        CodexState {
            codex_actor_handle,
            credential: None,
            client,
            endpoint,
            stream: true,
        }
    }

    pub async fn request_credential(&mut self) -> Result<CodexCredential, ClewdrError> {
        let cred = self.codex_actor_handle.request().await?;
        self.credential = Some(cred.clone());
        // Rebuild client with latest proxy settings
        let proxy = CLEWDR_CONFIG.load().wreq_proxy.to_owned();
        let mut builder = wreq::Client::builder().emulation(Emulation::Chrome136);
        if let Some(ref p) = proxy {
            builder = builder.proxy(p.to_owned());
        }
        self.client = builder.build().context(WreqSnafu {
            msg: "Failed to build codex client with new credential",
        })?;
        Ok(self.credential.clone().unwrap())
    }

    pub async fn return_credential(&self, reason: Option<String>) {
        if let Some(ref cred) = self.credential {
            self.codex_actor_handle
                .return_credential(cred.to_owned(), reason)
                .await
                .unwrap_or_else(|e| {
                    tracing::error!("[Codex] Failed to return credential: {}", e);
                });
        }
    }

    pub fn check_token(&self) -> CodexTokenStatus {
        let Some(CodexCredential {
            token: Some(ref token_info),
            ..
        }) = self.credential
        else {
            return CodexTokenStatus::None;
        };
        if token_info.is_stale() {
            CodexTokenStatus::Stale
        } else {
            CodexTokenStatus::Valid
        }
    }
}
