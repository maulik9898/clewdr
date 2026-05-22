use colored::Colorize;
use http::Method;
use serde_json::Value;
use snafu::ResultExt;

use super::ClaudeCodeState;
use crate::{
    config::Reason,
    error::{CheckClaudeErr, ClewdrError, WreqSnafu},
    utils::print_out_json,
};

/// Returns true if the capability list indicates a usable Claude subscription
/// (personal Pro/Max or a team/enterprise plan).
fn has_subscription(capabilities: &[String]) -> bool {
    capabilities.iter().any(|c| {
        c.contains("pro") || c.contains("enterprise") || c.contains("raven") || c.contains("max")
    })
}

/// Preference rank for OAuth authorization. Lower is tried first.
///
/// Personal subscription orgs (Max/Pro) are preferred because team/enterprise
/// orgs frequently disallow the personal OAuth/Claude Code flow (and may be
/// canceled/disabled), which makes authorization fail.
fn org_priority(capabilities: &[String]) -> u8 {
    if capabilities.iter().any(|c| c.contains("max")) {
        0
    } else if capabilities.iter().any(|c| c.contains("pro")) {
        1
    } else if has_subscription(capabilities) {
        2
    } else {
        3
    }
}

impl ClaudeCodeState {
    /// Fetches the account bootstrap and returns the UUIDs of all chat-capable
    /// organizations, ordered by authorization preference (see [`org_priority`]).
    ///
    /// The caller should try each org in turn, since the first candidate may be
    /// a canceled or OAuth-restricted organization.
    pub async fn get_organizations(&self) -> Result<Vec<String>, ClewdrError> {
        let end_point = self
            .endpoint
            .join("api/bootstrap")
            .expect("Url parse error");
        let res = self
            .build_request(Method::GET, end_point)
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to bootstrap",
            })?
            .check_claude()
            .await?;
        let bootstrap = res.json::<Value>().await.context(WreqSnafu {
            msg: "Failed to parse bootstrap response",
        })?;
        print_out_json(&bootstrap, "bootstrap_res.json");
        if bootstrap["account"].is_null() {
            return Err(Reason::Null.into());
        }
        let memberships = bootstrap["account"]["memberships"]
            .as_array()
            .ok_or(Reason::Null)?;

        // Collect every chat-capable org along with its capabilities.
        let mut candidates: Vec<(String, Vec<String>)> = memberships
            .iter()
            .filter_map(|m| {
                let org = m["organization"].as_object()?;
                let capabilities = org
                    .get("capabilities")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|c| c.as_str().map(str::to_string))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if !capabilities.iter().any(|c| c == "chat") {
                    return None;
                }
                let uuid = org.get("uuid")?.as_str()?.to_string();
                Some((uuid, capabilities))
            })
            .collect();

        if candidates.is_empty() {
            return Err(Reason::Null.into());
        }

        // Require at least one org with a usable subscription, otherwise this is
        // a free account that cannot use the Claude Code flow.
        if !candidates.iter().any(|(_, caps)| has_subscription(caps)) {
            return Err(Reason::Free.into());
        }

        // Prefer personal Max/Pro orgs over team/enterprise ones.
        candidates.sort_by_key(|(_, caps)| org_priority(caps));

        let email = bootstrap["account"]["email_address"]
            .as_str()
            .unwrap_or_default();
        println!(
            "[{}]\nemail: {}\norgs: {}",
            self.cookie.as_ref().unwrap().cookie.ellipse().green(),
            email.blue(),
            candidates
                .iter()
                .map(|(uuid, caps)| format!("{} [{}]", uuid, caps.join(",")))
                .collect::<Vec<_>>()
                .join(", ")
                .blue()
        );

        Ok(candidates.into_iter().map(|(uuid, _)| uuid).collect())
    }
}
