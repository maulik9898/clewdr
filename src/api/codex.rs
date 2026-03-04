use std::sync::Arc;

use axum::{Json, extract::State, response::Response};
use axum_auth::AuthBearer;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tracing::{error, info};
use wreq::StatusCode;

use super::error::ApiError;
use crate::{
    codex_state::CodexState,
    config::{CLEWDR_CONFIG, CodexCredential},
    error::ClewdrError,
    providers::{
        LLMProvider,
        codex::{CodexInvocation, CodexProvider},
    },
    services::codex_actor::CodexActorHandle,
};

/// Proxy endpoint for Codex requests - pure passthrough to OpenAI Responses API
pub async fn api_codex_responses(
    State(provider): State<Arc<CodexProvider>>,
    body: Bytes,
) -> Result<Response, ClewdrError> {
    let result = provider.invoke(CodexInvocation { body }).await?;
    Ok(result.response)
}

/// Proxy GET /codex/v1/models to OpenAI's /v1/models endpoint
pub async fn api_codex_models(
    State(provider): State<Arc<CodexProvider>>,
    req: axum::extract::Request,
) -> Result<Response, ClewdrError> {
    let query = req.uri().query().unwrap_or("");
    let result = provider.fetch_models(query).await?;
    Ok(result)
}

// ---- Device code login endpoints ----

#[derive(Serialize)]
pub struct DeviceCodeLoginResponse {
    verification_url: String,
    user_code: String,
    device_auth_id: String,
    interval: u64,
}

#[derive(Deserialize)]
pub struct DeviceCodePollRequest {
    device_auth_id: String,
    user_code: String,
}

#[derive(Serialize)]
pub struct PollResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

/// Start device code login flow
pub async fn api_codex_login_start(
    State(handle): State<CodexActorHandle>,
    AuthBearer(t): AuthBearer,
) -> Result<Json<DeviceCodeLoginResponse>, ApiError> {
    if !CLEWDR_CONFIG.load().admin_auth(&t) {
        return Err(ApiError::unauthorized());
    }

    let state = CodexState::new(handle);
    match state.request_device_code().await {
        Ok(dc) => Ok(Json(DeviceCodeLoginResponse {
            verification_url: dc.verification_url,
            user_code: dc.user_code,
            device_auth_id: dc.device_auth_id,
            interval: dc.interval,
        })),
        Err(e) => {
            error!("[Codex] Failed to start login: {}", e);
            Err(ApiError::internal(format!(
                "Failed to start device code login: {}",
                e
            )))
        }
    }
}

/// Poll for device code login completion
pub async fn api_codex_login_poll(
    State(handle): State<CodexActorHandle>,
    AuthBearer(t): AuthBearer,
    Json(req): Json<DeviceCodePollRequest>,
) -> Result<Json<PollResponse>, ApiError> {
    if !CLEWDR_CONFIG.load().admin_auth(&t) {
        return Err(ApiError::unauthorized());
    }

    let state = CodexState::new(handle.clone());

    // Poll once — frontend calls this repeatedly with an interval
    let poll_result = state
        .poll_device_code_once(&req.device_auth_id, &req.user_code)
        .await;

    match poll_result {
        Ok(Some((auth_code, code_verifier, _code_challenge))) => {
            // Authorization received — exchange for tokens
            match state
                .exchange_code_for_tokens(&auth_code, code_verifier.as_deref())
                .await
            {
                Ok(token_info) => {
                    // Extract label from id_token, account_id from access_token (nested claim)
                    let label = token_info
                        .id_token
                        .as_deref()
                        .and_then(|t| extract_jwt_claim(t, "email"));
                    let account_id = extract_nested_jwt_claim(
                        &token_info.access_token,
                        "https://api.openai.com/auth",
                        "chatgpt_account_id",
                    );

                    let cred = CodexCredential {
                        token: Some(token_info),
                        account_id,
                        label: label.clone(),
                        reset_time: None,
                    };

                    if let Err(e) = handle.submit(cred).await {
                        error!("[Codex] Failed to submit credential: {}", e);
                        return Err(ApiError::internal("Failed to save credential"));
                    }

                    info!("[Codex] Login successful for: {}", label.as_deref().unwrap_or("unknown"));
                    Ok(Json(PollResponse {
                        status: "complete".to_string(),
                        label,
                    }))
                }
                Err(e) => {
                    error!("[Codex] Token exchange failed: {}", e);
                    Err(ApiError::internal(format!(
                        "Token exchange failed: {}",
                        e
                    )))
                }
            }
        }
        Ok(None) => {
            // Still pending — frontend should call again after interval
            Ok(Json(PollResponse {
                status: "pending".to_string(),
                label: None,
            }))
        }
        Err(e) => {
            // Terminal error (expired, denied, etc.)
            Err(ApiError::bad_request(e.to_string()))
        }
    }
}

/// Get all codex credentials
pub async fn api_get_codex_credentials(
    State(handle): State<CodexActorHandle>,
    AuthBearer(t): AuthBearer,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !CLEWDR_CONFIG.load().admin_auth(&t) {
        return Err(ApiError::unauthorized());
    }

    match handle.get_status().await {
        Ok(info) => Ok(Json(serde_json::json!({
            "valid": info.valid,
            "exhausted": info.exhausted,
        }))),
        Err(e) => Err(ApiError::internal(format!(
            "Failed to get codex credentials: {}",
            e
        ))),
    }
}

/// Delete a codex credential
pub async fn api_delete_codex_credential(
    State(handle): State<CodexActorHandle>,
    AuthBearer(t): AuthBearer,
    Json(cred): Json<CodexCredential>,
) -> Result<StatusCode, ApiError> {
    if !CLEWDR_CONFIG.load().admin_auth(&t) {
        return Err(ApiError::unauthorized());
    }

    match handle.delete_credential(cred).await {
        Ok(_) => {
            info!("[Codex] Credential deleted successfully");
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => {
            error!("[Codex] Failed to delete credential: {}", e);
            Err(ApiError::internal(format!(
                "Failed to delete credential: {}",
                e
            )))
        }
    }
}

/// Extract a top-level string claim from a JWT (no signature verification)
fn extract_jwt_claim(token: &str, claim: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        parts[1],
    )
    .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    claims
        .get(claim)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Extract a nested claim from a JWT (e.g. `"https://api.openai.com/auth"` → `"chatgpt_account_id"`)
fn extract_nested_jwt_claim(token: &str, path: &str, claim: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        parts[1],
    )
    .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    claims
        .get(path)?
        .get(claim)?
        .as_str()
        .map(|s| s.to_string())
}
