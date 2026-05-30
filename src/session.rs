//! `Session` trait — the abstraction over an in-pane backend. With the
//! supervisor split, the real implementor is `client::ProxySession`; this
//! file owns only the trait + a `FakeSession` for tests.

use anyhow::Result;
use crossterm::event::{KeyEvent, MouseEvent};
use std::io;
use std::sync::Mutex;

pub type SessionId = u64;

/// Abstraction over an in-pane backend: today, a [`crate::client::ProxySession`]
/// talking to the supervisor; could be an in-process agent harness tomorrow.
/// Object-safe.
pub trait Session: Send + Sync {
    fn id(&self) -> SessionId;
    fn write_key(&self, key: KeyEvent) -> io::Result<()>;
    fn write_mouse(&self, ev: MouseEvent) -> io::Result<()>;
    /// Forward a paste payload to the PTY. The transport chunks it and wraps it
    /// in bracketed-paste escapes only when the inner app enabled bracketed
    /// paste (DECSET 2004).
    fn write_paste(&self, text: &str) -> io::Result<()>;
    fn resize(&self, rows: u16, cols: u16) -> Result<()>;
    fn parser(&self) -> &Mutex<vt100::Parser>;
}

#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
pub struct FakeSession {
    id: SessionId,
    parser: Mutex<vt100::Parser>,
    pub writes: Mutex<Vec<crossterm::event::KeyEvent>>,
    pub mice: Mutex<Vec<crossterm::event::MouseEvent>>,
    pub pastes: Mutex<Vec<String>>,
    pub resizes: Mutex<Vec<(u16, u16)>>,
}

#[cfg(test)]
impl FakeSession {
    pub fn new(id: SessionId) -> Arc<Self> {
        Arc::new(Self {
            id,
            parser: Mutex::new(vt100::Parser::new(24, 80, 0)),
            writes: Mutex::new(Vec::new()),
            mice: Mutex::new(Vec::new()),
            pastes: Mutex::new(Vec::new()),
            resizes: Mutex::new(Vec::new()),
        })
    }
}

#[cfg(test)]
impl Session for FakeSession {
    fn id(&self) -> SessionId {
        self.id
    }
    fn write_key(&self, key: KeyEvent) -> io::Result<()> {
        self.writes.lock().unwrap().push(key);
        Ok(())
    }
    fn write_mouse(&self, ev: MouseEvent) -> io::Result<()> {
        self.mice.lock().unwrap().push(ev);
        Ok(())
    }
    fn write_paste(&self, text: &str) -> io::Result<()> {
        self.pastes.lock().unwrap().push(text.to_string());
        Ok(())
    }
    fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.resizes.lock().unwrap().push((rows, cols));
        Ok(())
    }
    fn parser(&self) -> &Mutex<vt100::Parser> {
        &self.parser
    }
}
