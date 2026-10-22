//! The desktop's terminals as the remote host sees them, minus input to the
//! ones the user locked read-only (see `shell::terminal::terminal_locks`): a
//! paired phone, or `oximux term send` through this desktop, is refused the
//! same as a keystroke in any of the desktop's own views.

use std::sync::Arc;

use oximux_remote_host::{AttachmentId, TerminalAttach, TerminalError, TerminalFrame, TerminalSource};
use oximux_remote_proto::messages::TerminalSummary;

use crate::shell::terminal::terminal_locks::{LockedPtys, pty_is_locked};

pub struct LockedTerminals {
    inner: Arc<dyn TerminalSource>,
    locked: LockedPtys,
}

impl LockedTerminals {
    pub fn new(inner: Arc<dyn TerminalSource>, locked: LockedPtys) -> Self {
        Self { inner, locked }
    }
}

#[async_trait::async_trait]
impl TerminalSource for LockedTerminals {
    async fn list(&self) -> Result<Vec<TerminalSummary>, TerminalError> {
        self.inner.list().await
    }

    async fn attach(
        &self,
        pty_id: &str,
    ) -> Result<(TerminalAttach, tokio::sync::mpsc::Receiver<TerminalFrame>), TerminalError> {
        self.inner.attach(pty_id).await
    }

    async fn input(&self, pty_id: &str, bytes: &[u8]) -> Result<(), TerminalError> {
        if pty_is_locked(&self.locked, pty_id) {
            return Err(TerminalError::ReadOnly);
        }
        self.inner.input(pty_id, bytes).await
    }

    async fn resize(&self, pty_id: &str, attachment: AttachmentId, cols: u16, rows: u16) -> Result<(), TerminalError> {
        self.inner.resize(pty_id, attachment, cols, rows).await
    }

    async fn detach(&self, pty_id: &str, attachment: AttachmentId) {
        self.inner.detach(pty_id, attachment).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Recorder(std::sync::Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl TerminalSource for Recorder {
        async fn list(&self) -> Result<Vec<TerminalSummary>, TerminalError> {
            Ok(Vec::new())
        }
        async fn attach(
            &self,
            _: &str,
        ) -> Result<(TerminalAttach, tokio::sync::mpsc::Receiver<TerminalFrame>), TerminalError> {
            Err(TerminalError::NotFound)
        }
        async fn input(&self, pty_id: &str, _: &[u8]) -> Result<(), TerminalError> {
            self.0.lock().unwrap().push(pty_id.to_owned());
            Ok(())
        }
        async fn resize(&self, _: &str, _: AttachmentId, _: u16, _: u16) -> Result<(), TerminalError> {
            Ok(())
        }
        async fn detach(&self, _: &str, _: AttachmentId) {}
    }

    #[tokio::test]
    async fn input_to_a_locked_terminal_is_refused_and_never_forwarded() {
        let inner = Arc::new(Recorder(std::sync::Mutex::new(Vec::new())));
        let locked = LockedPtys::default();
        locked.write().unwrap().insert("locked".into());
        let source = LockedTerminals::new(inner.clone(), Arc::clone(&locked));
        assert!(matches!(source.input("locked", b"x").await, Err(TerminalError::ReadOnly)));
        assert!(source.input("open", b"x").await.is_ok());
        assert_eq!(*inner.0.lock().unwrap(), vec!["open".to_string()]);
    }
}
