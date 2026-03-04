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

const INTERVAL: u64 = 300;

#[derive(Debug, Serialize, Clone)]
pub struct CodexCredentialInfo {
    pub valid: Vec<CodexCredential>,
    pub exhausted: Vec<CodexCredential>,
}

#[derive(Debug)]
enum CodexActorMessage {
    Return(CodexCredential, Option<String>),
    Submit(CodexCredential),
    CheckReset,
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

    fn reset(state: &mut CodexActorState) {
        let mut reset_creds = Vec::new();
        state.exhausted.retain(|cred| {
            let reset_cred = cred.clone().reset();
            if reset_cred.reset_time.is_none() {
                reset_creds.push(reset_cred);
                false
            } else {
                true
            }
        });
        if reset_creds.is_empty() {
            return;
        }
        for c in reset_creds {
            state.valid.push_back(c);
        }
        Self::log(state);
    }

    fn dispatch(state: &mut CodexActorState) -> Result<CodexCredential, ClewdrError> {
        Self::reset(state);
        let cred = state
            .valid
            .pop_front()
            .ok_or(ClewdrError::NoCookieAvailable)?;
        state.valid.push_back(cred.clone());
        Ok(cred)
    }

    fn collect(state: &mut CodexActorState, cred: CodexCredential, reason: Option<String>) {
        let Some(reason) = reason else {
            // Update in place
            if let Some(existing) = state.valid.iter_mut().find(|c| **c == cred) {
                *existing = cred;
                Self::save(state);
            }
            return;
        };
        warn!("[Codex] Credential returned with reason: {}", reason);
        state.valid.retain(|c| c != &cred);
        let mut exhausted_cred = cred;
        exhausted_cred.reset_time = Some(chrono::Utc::now().timestamp() + 3600);
        state.exhausted.push(exhausted_cred);
        Self::save(state);
        Self::log(state);
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
        let valid = VecDeque::from_iter(all.iter().filter(|c| c.reset_time.is_none()).cloned());
        let exhausted: Vec<_> = all.into_iter().filter(|c| c.reset_time.is_some()).collect();

        let state = CodexActorState { valid, exhausted };
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
            CodexActorMessage::CheckReset => {
                Self::reset(state);
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
        let handle = Self {
            actor_ref: actor_ref.clone(),
        };
        handle.spawn_timeout_checker().await;
        Ok(handle)
    }

    async fn spawn_timeout_checker(&self) {
        let actor_ref = self.actor_ref.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(INTERVAL));
            loop {
                interval.tick().await;
                if ractor::cast!(actor_ref, CodexActorMessage::CheckReset).is_err() {
                    break;
                }
            }
        });
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
