//! Eval-parent stand-in: speaks the `rio.evaljob` protocol over a
//! socketpair and feeds canned `ResultFrame`s — this is how the
//! coordinator is integration-tested WITHOUT libexpr.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rio_build_cli::framing;
use rio_proto::evaljob::{
    AttrsetExpansion, CoordinatorFrame, IfdCompletion, ResultFrame, WorkerFrame, coordinator_frame,
    worker_frame,
};

/// Records of what the coordinator sent downstream.
#[derive(Default)]
pub struct ParentSeen {
    pub ack_digests: Vec<Vec<u8>>,
    pub ifd_completions: Vec<IfdCompletion>,
    pub shutdown: bool,
}

pub struct StubParent {
    pub seen: Arc<Mutex<ParentSeen>>,
}

/// Spawn the stub on one end of a `UnixStream::pair`. On each
/// `WorkItem` it streams the scripted frames for that attr; it records
/// `AckFeedback` / `IfdCompletion`, and exits (closing the channel) on
/// `Shutdown`.
pub fn spawn(
    stream: std::os::unix::net::UnixStream,
    script: HashMap<String, Vec<ResultFrame>>,
) -> StubParent {
    spawn_expanding(stream, script, HashMap::new())
}

/// Like [`spawn`], but attrs in `expansions` answer with an
/// `AttrsetExpansion` frame instead of result frames (the way a real
/// worker reports a `.#checks`-style attrset installable).
pub fn spawn_expanding(
    stream: std::os::unix::net::UnixStream,
    script: HashMap<String, Vec<ResultFrame>>,
    expansions: HashMap<String, AttrsetExpansion>,
) -> StubParent {
    let seen = Arc::new(Mutex::new(ParentSeen::default()));
    let seen_task = Arc::clone(&seen);
    // The task ends on Shutdown or channel EOF; the handle is not
    // needed (dropping it detaches, which is exactly the semantics of
    // a separate eval-parent process).
    tokio::spawn(async move {
        stream.set_nonblocking(true).expect("nonblocking");
        let mut stream = tokio::net::UnixStream::from_std(stream).expect("tokio wrap");
        let (mut r, mut w) = stream.split();
        loop {
            let frame: Option<CoordinatorFrame> =
                framing::read_frame(&mut r).await.expect("read frame");
            match frame.and_then(|f| f.msg) {
                None => break, // coordinator closed
                Some(coordinator_frame::Msg::Work(item)) => {
                    if let Some(exp) = expansions.get(&item.attr) {
                        let frame = WorkerFrame {
                            msg: Some(worker_frame::Msg::Expansion(exp.clone())),
                        };
                        framing::write_frame(&mut w, &frame).await.expect("write");
                        continue;
                    }
                    let frames = script.get(&item.attr).cloned().unwrap_or_default();
                    if frames.is_empty() {
                        let err = WorkerFrame {
                            msg: Some(worker_frame::Msg::Error(rio_proto::evaljob::WorkerError {
                                attr: item.attr,
                                message: "stub has no script for this attr".into(),
                                fatal: false,
                            })),
                        };
                        framing::write_frame(&mut w, &err).await.expect("write");
                        continue;
                    }
                    for f in frames {
                        let frame = WorkerFrame {
                            msg: Some(worker_frame::Msg::Result(f)),
                        };
                        framing::write_frame(&mut w, &frame).await.expect("write");
                    }
                }
                Some(coordinator_frame::Msg::AckFeedback(acks)) => {
                    seen_task.lock().unwrap().ack_digests.extend(acks.digests);
                }
                Some(coordinator_frame::Msg::IfdCompletion(c)) => {
                    seen_task.lock().unwrap().ifd_completions.push(c);
                }
                Some(coordinator_frame::Msg::Shutdown(_)) => {
                    seen_task.lock().unwrap().shutdown = true;
                    break;
                }
            }
        }
        // Dropping the stream closes the channel — the coordinator
        // reads EOF, mirroring a clean eval-parent exit.
    });
    StubParent { seen }
}
