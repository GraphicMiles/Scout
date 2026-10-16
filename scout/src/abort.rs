//! Cancellation that carries a reason.
//!
//! A bare cancellation flag cannot distinguish "the caller cancelled" from "the
//! memory watchdog killed this" or "the server is shutting down". Surfacing the
//! wrong code would mislead a caller into retrying something that will never
//! succeed, or into treating a resource kill as a routine timeout.

use crate::error::{ErrorCode, ScoutError};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Abort {
    token: CancellationToken,
    reason: Arc<Mutex<Option<ScoutError>>>,
}

impl Default for Abort {
    fn default() -> Self {
        Self::new()
    }
}

impl Abort {
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            reason: Arc::new(Mutex::new(None)),
        }
    }

    pub fn token(&self) -> &CancellationToken {
        &self.token
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Cancel, recording why. The first reason recorded wins, so the originating
    /// cause is not overwritten by a later cascade.
    pub fn cancel_with(&self, err: ScoutError) {
        let mut slot = self.reason.lock().unwrap();
        if slot.is_none() {
            *slot = Some(err);
        }
        drop(slot);
        self.token.cancel();
    }

    /// The recorded reason, or a generic cancellation if none was set.
    pub fn reason(&self) -> ScoutError {
        self.reason
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| ScoutError::new(ErrorCode::Cancelled, "Operation cancelled"))
    }

    /// Resolve when cancelled, yielding the recorded reason.
    pub async fn cancelled(&self) -> ScoutError {
        self.token.cancelled().await;
        self.reason()
    }

    /// Derive a child scope that is cancelled when this one is.
    pub fn child(&self) -> Abort {
        let child = Abort::new();
        let parent = self.clone();
        let c = child.clone();
        tokio::spawn(async move {
            parent.token.cancelled().await;
            c.cancel_with(parent.reason());
        });
        child
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn preserves_the_originating_reason() {
        let a = Abort::new();
        a.cancel_with(ScoutError::new(ErrorCode::ResourceLimit, "memory"));
        a.cancel_with(ScoutError::new(ErrorCode::Cancelled, "later"));
        assert_eq!(a.reason().code, ErrorCode::ResourceLimit);
        assert!(a.is_cancelled());
    }

    #[tokio::test]
    async fn default_reason_is_cancelled() {
        let a = Abort::new();
        a.token().cancel();
        assert_eq!(a.reason().code, ErrorCode::Cancelled);
    }
}
