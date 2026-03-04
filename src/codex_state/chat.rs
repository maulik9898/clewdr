use axum::response::{IntoResponse, Sse, sse::Event as SseEvent};
use bytes::Bytes;
use colored::Colorize;
use eventsource_stream::Eventsource;
use futures::TryStreamExt;
use snafu::{GenerateImplicitData, ResultExt};
use tracing::{error, info};

use crate::{
    codex_state::{CodexState, CodexTokenStatus},
    config::CLEWDR_CONFIG,
    error::{ClewdrError, WreqSnafu},
};

impl CodexState {
    /// Main entry point: proxy a request to OpenAI Responses API with retry logic
    pub async fn try_chat(
        &mut self,
        body: Bytes,
    ) -> Result<axum::response::Response, ClewdrError> {
        for i in 0..CLEWDR_CONFIG.load().max_retries + 1 {
            if i > 0 {
                info!("[Codex][RETRY] attempt: {}", i.to_string().green());
            }
            let mut state = self.clone();
            let body = body.clone();

            let cred = state.request_credential().await?;
            let label = cred.ellipse();

            let result = async {
                // Check/refresh token
                match state.check_token() {
                    CodexTokenStatus::None => {
                        return Err(ClewdrError::BadRequest {
                            msg: "No Codex token available - login required via dashboard",
                        });
                    }
                    CodexTokenStatus::Stale => {
                        info!("Token stale, refreshing");
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
                    CodexTokenStatus::Valid => {
                        info!("Token is valid, proceeding with request");
                    }
                }

                let access_token = state
                    .credential
                    .as_ref()
                    .and_then(|c| c.token.as_ref())
                    .ok_or(ClewdrError::UnexpectedNone {
                        msg: "No access token found in codex credential",
                    })?
                    .access_token
                    .clone();

                state.forward_request(&access_token, body).await
            }
            .await;

            match result {
                Ok(res) => return Ok(res),
                Err(e) => {
                    error!("[Codex][{}] {}", label.green(), e);
                    // On 401, try refreshing and retry
                    if matches!(&e, ClewdrError::ClaudeHttpError { code, .. } if code.as_u16() == 401)
                    {
                        if let Some(ref cred) = state.credential {
                            if let Some(ref token) = cred.token {
                                match state.refresh_token(token).await {
                                    Ok(new_token) => {
                                        if let Some(ref mut c) = state.credential {
                                            c.token = Some(new_token);
                                        }
                                        state.return_credential(None).await;
                                        continue;
                                    }
                                    Err(_) => {
                                        state
                                            .return_credential(Some(
                                                "Token refresh failed".to_string(),
                                            ))
                                            .await;
                                        continue;
                                    }
                                }
                            }
                        }
                        state
                            .return_credential(Some("Unauthorized".to_string()))
                            .await;
                        continue;
                    }
                    // On 429, mark exhausted
                    if matches!(&e, ClewdrError::ClaudeHttpError { code, .. } if code.as_u16() == 429)
                    {
                        state
                            .return_credential(Some("Rate limited".to_string()))
                            .await;
                        continue;
                    }
                    // On 5xx, retry with exponential backoff
                    if matches!(&e, ClewdrError::ClaudeHttpError { code, .. } if code.is_server_error())
                    {
                        let delay = std::time::Duration::from_millis(1000 * 2u64.pow(i as u32));
                        info!("[Codex] Server error, retrying in {:?}", delay);
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(ClewdrError::TooManyRetries)
    }

    /// Forward the raw request body to OpenAI's Responses API and stream back SSE
    async fn forward_request(
        &self,
        access_token: &str,
        body: Bytes,
    ) -> Result<axum::response::Response, ClewdrError> {
        // Check if the request body has "stream": true
        let request_wants_stream = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("stream")?.as_bool())
            .unwrap_or(false);

        let url = self
            .endpoint
            .join("responses")
            .expect("URL join for /responses");

        let mut req = self
            .client
            .post(url.as_str())
            .bearer_auth(access_token)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("OpenAI-Beta", "responses=experimental");

        // Add chatgpt-account-id header for device code auth
        if let Some(ref cred) = self.credential {
            if let Some(ref account_id) = cred.account_id {
                req = req.header("chatgpt-account-id", account_id.as_str());
            }
        }

        let resp = req
            .body(wreq::Body::from(body))
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to send request to OpenAI",
            })?;

        let status = resp.status();
        if !status.is_success() {
            let error_body = resp.text().await.unwrap_or_default();
            return Err(ClewdrError::ClaudeHttpError {
                code: status,
                inner: crate::error::ClaudeErrorBody {
                    message: serde_json::json!(error_body),
                    r#type: "codex_error".to_string(),
                    code: Some(status.as_u16()),
                },
            });
        }

        // ChatGPT backend may not return Content-Type header, so also check request's stream flag
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let is_streaming = content_type.contains("text/event-stream") || request_wants_stream;

        if is_streaming {
            // SSE passthrough
            let stream = resp.bytes_stream().eventsource().map_ok(|event| {
                let e = SseEvent::default().event(event.event).id(event.id);
                let e = if let Some(retry) = event.retry {
                    e.retry(retry)
                } else {
                    e
                };
                e.data(event.data)
            });
            Ok(Sse::new(stream)
                .keep_alive(Default::default())
                .into_response())
        } else {
            // Non-streaming: pass through the response, filtering stale hop-by-hop headers
            let response_headers = resp.headers().clone();
            let bytes = resp.bytes().await.context(WreqSnafu {
                msg: "Failed to read OpenAI response body",
            })?;
            let mut builder = http::Response::builder().status(status);
            for (key, value) in response_headers.iter() {
                let k = key.as_str();
                if k == "transfer-encoding" || k == "content-encoding" || k == "content-length" {
                    continue;
                }
                builder = builder.header(key, value);
            }
            let response = builder
                .body(axum::body::Body::from(bytes))
                .map_err(|e| ClewdrError::HttpError {
                    loc: snafu::Location::generate(),
                    source: e,
                })?;
            Ok(response)
        }
    }
}
