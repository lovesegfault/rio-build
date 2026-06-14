//! Stage 5: build-log rendering.
//!
//! One spawned renderer task drains a [`RenderEvent`] channel fed by
//! every per-root `watch_stream` task and the coordinator main loop.
//! All renderers write to **stderr**; stdout is reserved for the final
//! result-path lines (the machine-readable surface).

use rio_proto::types::BuildEvent;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub mod plain;
pub mod term;

/// What the coordinator hands the renderer.
#[derive(Debug)]
pub enum RenderEvent {
    /// A `BuildEvent` from one of the per-root watch streams.
    Build(BuildEvent),
}

/// Cloneable sender into the renderer task. `send` never blocks; a
/// dropped receiver makes it a no-op (the [`null`](Self::null) handle).
#[derive(Clone)]
pub struct RenderHandle(Option<mpsc::UnboundedSender<RenderEvent>>);

impl RenderHandle {
    /// A handle whose sends are dropped (tests).
    pub fn null() -> Self {
        Self(None)
    }

    pub fn send(&self, ev: RenderEvent) {
        if let Some(tx) = &self.0 {
            let _ = tx.send(ev);
        }
    }
}

/// Spawn the renderer task. Returns the send handle plus a `stop`
/// future: drop every clone of the handle, then await the future to
/// drain the channel and clear the screen.
pub fn spawn() -> (RenderHandle, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let mut out = std::io::stderr();
        while let Some(ev) = rx.recv().await {
            plain::on_event(&mut out, &ev);
        }
    });
    (RenderHandle(Some(tx)), task)
}

/// `/nix/store/<hash>-foo.drv` → `foo.drv`.
pub(crate) fn short_drv(path: &str) -> &str {
    let base = path.rsplit('/').next().unwrap_or(path);
    match base.split_once('-') {
        Some((hash, rest)) if hash.len() == 32 => rest,
        _ => base,
    }
}
