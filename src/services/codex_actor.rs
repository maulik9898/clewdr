use std::collections::VecDeque;

use colored::Colorize;
use ractor::{Actor, ActorProcessingErr, ActorRef, RpcReplyPort};
use serde::Serialize;
use snafu::{GenerateImplicitData, Location};
use tracing::{error, info, warn};

use crate::{
    config::{CLEWDR_CONFIG, ClewdrConfig, CodexCredential},
    error::ClewdrError,
};

#[derive(Debug, Serialize, Clone)]
pub struct CodexCredentialInfo {
    pub valid: Vec<CodexCredential>,
    pub exhausted: Vec<CodexCredential>,
}

#[derive(Debug)]
enum CodexActorMessage {
    Return(CodexCredential, Option<String>),
    Submit(CodexCredential),
    Request(RpcReplyPort<Result<CodexCredential, ClewdrError>>),
    GetStatus(RpcReplyPort<CodexCredentialInfo>),
    Delete(CodexCredential, RpcReplyPort<Result<(), ClewdrError>>),
}

#[derive(Debug)]
struct CodexActorState {
    valid: VecDeque<CodexCredential>,
    exhausted: Vec<CodexCredential>,
}

struct CodexActor;

impl CodexActor {
    fn save(state: &CodexActorState) {
        CLEWDR_CONFIG.rcu(|config| {
            let mut config = ClewdrConfig::clone(config);
            config.codex_credentials = state
                .valid
                .iter()
                .chain(state.exhausted.iter())
                .cloned()
                .collect();
            config
        });

        tokio::spawn(async move {
            let result = CLEWDR_CONFIG.load().save().await;
            match result {
                Ok(_) => info!("[Codex] Configuration saved successfully"),
                Err(e) => error!("[Codex] Save task failed: {}", e),
            }
        });
    }

    fn log(state: &CodexActorState) {
        info!(
            "[Codex] Valid: {}, Exhausted: {}",
            state.valid.len().to_string().green(),
            state.exhausted.len().to_string().yellow(),
        );
    }

    fn dispatch(state: &mut CodexActorState) -> Result<CodexCredential, ClewdrError> {
        let cred = state
            .valid
            .pop_front()
            .ok_or(ClewdrError::NoCookieAvailable)?;
        state.valid.push_back(cred.clone());
        Ok(cred)
    }

    fn collect(state: &mut CodexActorState, cred: CodexCredential, reason: Option<String>) {
        if let Some(reason) = reason {
            warn!("[Codex] Credential returned with reason: {reason}");
        }
        // Keep the credential in rotation regardless of the reason. Real Codex
        // rate-limit windows (5h / 7d) recover on their own, so we no longer
        // bench credentials on an artificial timer.
        if let Some(existing) = state.valid.iter_mut().find(|c| **c == cred) {
            *existing = cred;
            Self::save(state);
        }
    }

    fn accept(state: &mut CodexActorState, cred: CodexCredential) {
        // Check if already exists
        if state.valid.iter().any(|c| *c == cred) || state.exhausted.iter().any(|c| *c == cred) {
            warn!("[Codex] Credential already exists");
            return;
        }
        state.valid.push_back(cred);
        Self::save(state);
        Self::log(state);
    }

    fn report(state: &CodexActorState) -> CodexCredentialInfo {
        CodexCredentialInfo {
            valid: state.valid.clone().into(),
            exhausted: state.exhausted.clone(),
        }
    }

    fn delete(state: &mut CodexActorState, cred: CodexCredential) -> Result<(), ClewdrError> {
        let mut found = false;
        state.valid.retain(|c| {
            if *c == cred {
                found = true;
                false
            } else {
                true
            }
        });
        let prev_len = state.exhausted.len();
        state.exhausted.retain(|c| c != &cred);
        found |= state.exhausted.len() < prev_len;

        if found {
            Self::save(state);
            Self::log(state);
            Ok(())
        } else {
            Err(ClewdrError::UnexpectedNone {
                msg: "Delete operation did not find the codex credential",
            })
        }
    }
}

impl Actor for CodexActor {
    type Msg = CodexActorMessage;
    type State = CodexActorState;
    type Arguments = ();

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        _arguments: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let all = CLEWDR_CONFIG.load().codex_credentials.clone();
        let state = CodexActorState {
            valid: VecDeque::from(all),
            exhausted: Vec::new(),
        };
        CodexActor::log(&state);
        Ok(state)
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            CodexActorMessage::Return(cred, reason) => {
                Self::collect(state, cred, reason);
            }
            CodexActorMessage::Submit(cred) => {
                Self::accept(state, cred);
            }
            CodexActorMessage::Request(reply_port) => {
                let result = Self::dispatch(state);
                reply_port.send(result)?;
            }
            CodexActorMessage::GetStatus(reply_port) => {
                let info = Self::report(state);
                reply_port.send(info)?;
            }
            CodexActorMessage::Delete(cred, reply_port) => {
                let result = Self::delete(state, cred);
                reply_port.send(result)?;
            }
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        CodexActor::save(state);
        Ok(())
    }
}

#[derive(Clone)]
pub struct CodexActorHandle {
    actor_ref: ActorRef<CodexActorMessage>,
}

impl CodexActorHandle {
    pub async fn start() -> Result<Self, ractor::SpawnErr> {
        let (actor_ref, _join_handle) = Actor::spawn(None, CodexActor, ()).await?;
        Ok(Self { actor_ref })
    }

    pub async fn request(&self) -> Result<CodexCredential, ClewdrError> {
        ractor::call!(self.actor_ref, CodexActorMessage::Request).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!(
                    "Failed to communicate with CodexActor for request operation: {e}"
                ),
            }
        })?
    }

    pub async fn return_credential(
        &self,
        cred: CodexCredential,
        reason: Option<String>,
    ) -> Result<(), ClewdrError> {
        ractor::cast!(
            self.actor_ref,
            CodexActorMessage::Return(cred, reason)
        )
        .map_err(|e| ClewdrError::RactorError {
            loc: Location::generate(),
            msg: format!(
                "Failed to communicate with CodexActor for return operation: {e}"
            ),
        })
    }

    pub async fn submit(&self, cred: CodexCredential) -> Result<(), ClewdrError> {
        ractor::cast!(self.actor_ref, CodexActorMessage::Submit(cred)).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!(
                    "Failed to communicate with CodexActor for submit operation: {e}"
                ),
            }
        })
    }

    pub async fn get_status(&self) -> Result<CodexCredentialInfo, ClewdrError> {
        ractor::call!(self.actor_ref, CodexActorMessage::GetStatus).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!(
                    "Failed to communicate with CodexActor for get status operation: {e}"
                ),
            }
        })
    }

    pub async fn delete_credential(&self, cred: CodexCredential) -> Result<(), ClewdrError> {
        ractor::call!(self.actor_ref, CodexActorMessage::Delete, cred).map_err(|e| {
            ClewdrError::RactorError {
                loc: Location::generate(),
                msg: format!(
                    "Failed to communicate with CodexActor for delete operation: {e}"
                ),
            }
        })?
    }
}
