use std::sync::Arc;

use axum::response::Response;
use bytes::Bytes;
use colored::Colorize;
use snafu::ResultExt;
use tracing::info;

use super::LLMProvider;
use crate::{
    codex_state::{CodexState, CodexTokenStatus},
    config::CODEX_CHATGPT_API_BASE,
    error::{ClewdrError, WreqSnafu},
    services::codex_actor::CodexActorHandle,
};

pub struct CodexInvocation {
    pub body: Bytes,
}

pub struct CodexProviderResponse {
    pub response: Response,
}

struct CodexSharedState {
    codex_actor_handle: CodexActorHandle,
}

#[derive(Clone)]
pub struct CodexProvider {
    shared: Arc<CodexSharedState>,
}

impl CodexProvider {
    pub fn new(codex_actor_handle: CodexActorHandle) -> Self {
        Self {
            shared: Arc::new(CodexSharedState {
                codex_actor_handle,
            }),
        }
    }

    pub fn actor_handle(&self) -> CodexActorHandle {
        self.shared.codex_actor_handle.clone()
    }

    /// Proxy GET /models to OpenAI, using a credential's access token
    pub async fn fetch_models(&self, query: &str) -> Result<Response, ClewdrError> {
        let mut state = CodexState::new(self.shared.codex_actor_handle.clone());

        // Get a credential for the bearer token
        let _cred = state.request_credential().await?;

        // Check/refresh token staleness (same as try_chat)
        match state.check_token() {
            CodexTokenStatus::None => {
                return Err(ClewdrError::BadRequest {
                    msg: "No Codex token available - login required via dashboard",
                });
            }
            CodexTokenStatus::Stale => {
                let current = state
                    .credential
                    .as_ref()
                    .and_then(|c| c.token.as_ref())
                    .ok_or(ClewdrError::UnexpectedNone {
                        msg: "No token to refresh",
                    })?
                    .clone();
                let new_token = state.refresh_token(&current).await?;
                if let Some(ref mut cred) = state.credential {
                    cred.token = Some(new_token);
                }
                state.return_credential(None).await;
            }
            CodexTokenStatus::Valid => {}
        }

        let access_token = state
            .credential
            .as_ref()
            .and_then(|c| c.token.as_ref())
            .ok_or(ClewdrError::UnexpectedNone {
                msg: "No access token for models request",
            })?
            .access_token
            .clone();

        let base = url::Url::parse(CODEX_CHATGPT_API_BASE).expect("invalid CODEX_CHATGPT_API_BASE");
        let mut url = base.join("models").expect("URL join for /models");
        // Ensure client_version is always present (ChatGPT backend requires it)
        if !query.is_empty() {
            url.set_query(Some(query));
        }
        if url.query_pairs().all(|(k, _)| k != "client_version") {
            url.query_pairs_mut()
                .append_pair("client_version", "1.0.0");
        }

        info!("[Codex][MODELS] GET {}", url.as_str().green());

        let mut req = state
            .client
            .get(url)
            .bearer_auth(&access_token)
            .header("OpenAI-Beta", "responses=experimental");

        // Add chatgpt-account-id header for device code auth
        if let Some(ref cred) = state.credential {
            if let Some(ref account_id) = cred.account_id {
                req = req.header("chatgpt-account-id", account_id.as_str());
            }
        }

        let resp = req
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to fetch models from OpenAI",
            })?;

        let status = resp.status();

        // Return credential back to the pool
        state.return_credential(None).await;

        // Passthrough the response, filtering stale headers
        let headers = resp.headers().clone();
        let bytes = resp.bytes().await.context(WreqSnafu {
            msg: "Failed to read models response",
        })?;
        let mut builder = http::Response::builder().status(status);
        for (key, value) in headers.iter() {
            let k = key.as_str();
            if k == "transfer-encoding" || k == "content-encoding" || k == "content-length" {
                continue;
            }
            builder = builder.header(key, value);
        }
        builder
            .body(axum::body::Body::from(bytes))
            .map_err(|e| ClewdrError::HttpError {
                loc: snafu::GenerateImplicitData::generate(),
                source: e,
            })
    }
}

#[async_trait::async_trait]
impl LLMProvider for CodexProvider {
    type Request = CodexInvocation;
    type Output = CodexProviderResponse;

    async fn invoke(&self, request: Self::Request) -> Result<Self::Output, ClewdrError> {
        let mut state = CodexState::new(self.shared.codex_actor_handle.clone());

        info!(
            "[Codex][REQ] body_size: {}",
            request.body.len().to_string().green()
        );

        let stopwatch = std::time::Instant::now();
        let response = state.try_chat(request.body).await?;
        let elapsed = stopwatch.elapsed();
        info!(
            "[Codex][FIN] elapsed: {}s",
            format!("{}", elapsed.as_secs_f32()).green()
        );

        Ok(CodexProviderResponse { response })
    }
}
