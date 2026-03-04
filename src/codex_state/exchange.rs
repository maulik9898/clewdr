use chrono::Utc;
use serde::{Deserialize, Deserializer, Serialize};
use snafu::ResultExt;
use tracing::{debug, info};

use crate::{
    config::{
        CLEWDR_CONFIG, CODEX_DEVICE_CODE_URL, CODEX_DEVICE_TOKEN_URL, CODEX_DEVICE_VERIFY_URL,
        CODEX_TOKEN_URL, CodexTokenInfo,
    },
    error::{ClewdrError, WreqSnafu},
};

use super::CodexState;

/// Raw API response from OpenAI's deviceauth/usercode endpoint.
/// Note: OpenAI does NOT return `verification_url` — we construct it ourselves.
#[derive(Debug, Deserialize)]
struct RawDeviceCodeResponse {
    #[serde(alias = "user_code", alias = "usercode")]
    user_code: String,
    device_auth_id: String,
    #[serde(default = "default_interval", deserialize_with = "deserialize_interval")]
    interval: u64,
}

/// Public device code response with the verification URL constructed by us.
#[derive(Debug, Serialize, Clone)]
pub struct DeviceCodeResponse {
    pub verification_url: String,
    pub user_code: String,
    pub device_auth_id: String,
    pub interval: u64,
}

fn default_interval() -> u64 {
    5
}

/// OpenAI returns `interval` as a string (e.g. `"5"`), not a number.
fn deserialize_interval<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrU64 {
        Num(u64),
        Str(String),
    }

    match StringOrU64::deserialize(deserializer)? {
        StringOrU64::Num(n) => Ok(n),
        StringOrU64::Str(s) => s
            .trim()
            .parse::<u64>()
            .map_err(|e| de::Error::custom(format!("invalid interval string: {e}"))),
    }
}

#[derive(Debug, Deserialize)]
struct DeviceTokenResponse {
    authorization_code: String,
    #[serde(default)]
    code_verifier: Option<String>,
    #[serde(default)]
    code_challenge: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenExchangeResponse {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RefreshTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenError {
    #[serde(default)]
    error: Option<String>,
}

impl CodexState {
    /// Step 1: Request a device code from OpenAI
    pub async fn request_device_code(&self) -> Result<DeviceCodeResponse, ClewdrError> {
        let client_id = CLEWDR_CONFIG.load().codex_client_id();
        let resp = self
            .client
            .post(CODEX_DEVICE_CODE_URL)
            .json(&serde_json::json!({
                "client_id": client_id,
            }))
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to request device code",
            })?;

        if !resp.status().is_success() {
            return Err(ClewdrError::BadRequest {
                msg: "Failed to get device code from OpenAI",
            });
        }

        let raw: RawDeviceCodeResponse =
            resp.json().await.context(WreqSnafu {
                msg: "Failed to parse device code response",
            })?;

        let device_code = DeviceCodeResponse {
            verification_url: CODEX_DEVICE_VERIFY_URL.to_string(),
            user_code: raw.user_code,
            device_auth_id: raw.device_auth_id,
            interval: raw.interval,
        };

        info!(
            "[Codex] Device code: {} (interval: {}s)",
            device_code.user_code, device_code.interval
        );
        Ok(device_code)
    }

    /// Step 2: Poll once for device code authorization.
    /// Returns Ok(Some(...)) on success, Ok(None) if still pending, Err on terminal failure.
    /// The frontend should call this repeatedly with an interval.
    pub async fn poll_device_code_once(
        &self,
        device_auth_id: &str,
        user_code: &str,
    ) -> Result<Option<(String, Option<String>, Option<String>)>, ClewdrError> {
        let resp = self
            .client
            .post(CODEX_DEVICE_TOKEN_URL)
            .json(&serde_json::json!({
                "device_auth_id": device_auth_id,
                "user_code": user_code,
            }))
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to poll device token",
            })?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if status.is_success() {
            let token_resp: DeviceTokenResponse =
                serde_json::from_str(&body).map_err(|_| ClewdrError::BadRequest {
                    msg: "Failed to parse device token response",
                })?;
            return Ok(Some((
                token_resp.authorization_code,
                token_resp.code_verifier,
                token_resp.code_challenge,
            )));
        }

        // Check if still pending or terminal error
        if let Ok(err) = serde_json::from_str::<DeviceTokenError>(&body) {
            if let Some(ref error) = err.error {
                if error == "authorization_pending" || error == "slow_down" {
                    debug!("[Codex] Device code authorization pending...");
                    return Ok(None);
                }
                if error == "expired_token" {
                    return Err(ClewdrError::BadRequest {
                        msg: "Device code expired",
                    });
                }
                if error == "access_denied" {
                    return Err(ClewdrError::BadRequest {
                        msg: "Device code access denied by user",
                    });
                }
            }
        }

        // Unknown non-success — treat as pending
        debug!("[Codex] Poll response {}: {}", status, body);
        Ok(None)
    }

    /// Step 3: Exchange authorization code for tokens
    /// OpenAI expects application/x-www-form-urlencoded, NOT JSON.
    pub async fn exchange_code_for_tokens(
        &self,
        authorization_code: &str,
        code_verifier: Option<&str>,
    ) -> Result<CodexTokenInfo, ClewdrError> {
        let client_id = CLEWDR_CONFIG.load().codex_client_id();
        let redirect_uri = "https://auth.openai.com/deviceauth/callback";

        info!("[Codex] Exchanging authorization code for tokens");

        let mut form = vec![
            ("client_id", client_id.as_str()),
            ("grant_type", "authorization_code"),
            ("code", authorization_code),
            ("redirect_uri", redirect_uri),
        ];

        let verifier_owned;
        if let Some(verifier) = code_verifier {
            verifier_owned = verifier.to_string();
            form.push(("code_verifier", &verifier_owned));
        }

        let resp = self
            .client
            .post(CODEX_TOKEN_URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(&form)
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to exchange code for tokens",
            })?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::error!("[Codex] Token exchange failed: {}", body);
            return Err(ClewdrError::BadRequest {
                msg: "Failed to exchange authorization code for tokens",
            });
        }

        let token_resp: TokenExchangeResponse =
            resp.json().await.context(WreqSnafu {
                msg: "Failed to parse token exchange response",
            })?;

        Ok(CodexTokenInfo {
            access_token: token_resp.access_token,
            refresh_token: token_resp.refresh_token,
            id_token: token_resp.id_token,
            last_refresh: Utc::now(),
        })
    }

    /// Refresh an existing token
    pub async fn refresh_token(
        &self,
        current_token: &CodexTokenInfo,
    ) -> Result<CodexTokenInfo, ClewdrError> {
        let client_id = CLEWDR_CONFIG.load().codex_client_id();

        info!("[Codex] Refreshing token");

        let resp = self
            .client
            .post(CODEX_TOKEN_URL)
            .json(&serde_json::json!({
                "client_id": client_id,
                "grant_type": "refresh_token",
                "refresh_token": current_token.refresh_token,
                "scope": "openid profile email",
            }))
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to refresh codex token",
            })?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::error!("[Codex] Token refresh failed: {}", body);
            return Err(ClewdrError::BadRequest {
                msg: "Failed to refresh codex token - re-login required",
            });
        }

        let refresh_resp: RefreshTokenResponse =
            resp.json().await.context(WreqSnafu {
                msg: "Failed to parse refresh token response",
            })?;

        Ok(CodexTokenInfo {
            access_token: refresh_resp.access_token,
            refresh_token: refresh_resp
                .refresh_token
                .unwrap_or_else(|| current_token.refresh_token.clone()),
            id_token: refresh_resp.id_token.or_else(|| current_token.id_token.clone()),
            last_refresh: Utc::now(),
        })
    }
}
