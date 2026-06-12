//! Pipeline internals: shared work deque, byte-budget semaphore, reader
//! threads, chunk workers, and the NAR-sha256 spine.
//!
//! # Thread roles
//!
//! - **Readers** (R, spawned per ingest) pop work items — directories for
//!   readdir, files for open+read — from one shared deque. A reader that
//!   dequeues a directory lists it (sorted, NAR order) and pushes the
//!   children; this is what makes *discovery* parallel, the load-bearing
//!   property on true-cold trees.
//! - **The spine** (the calling thread) performs a DFS in exact NAR order,
//!   emitting canonical framing via [`rio_nix::nar::frame`] interleaved
//!   with file contents into a SHA-256 tee.
//! - **Chunk workers** (W) consume completed file buffers from an mpsc
//!   channel and produce per-file blake3 + FastCDC chunk lists.
//!
//! # Progress guarantee (why the spine can steal)
//!
//! Readers acquire the byte budget *before* reading, and a buffer's charge
//! is released only when both planes have dropped it. Because reads
//! complete out of NAR order, the budget can fill up entirely with files
//! the spine has not reached yet while the spine's *next* file is still
//! queued and unable to acquire budget — a deadlock on adversarial tree
//! shapes (e.g. a slow readdir whose children sort before an already-read
//! sibling subtree). The escape hatch: when the spine reaches a node whose
//! IO has not *started*, it claims the node and does the IO inline,
//! bypassing the budget. A stolen read runs plane 2 inline too — handing
//! it to the chunk workers would park a budget-exempt buffer in their
//! channel, and with the workers behind, steal after steal would grow
//! that queue without bound. Kept inline, the stolen buffer dies with the
//! spine's visit, so the memory bound only widens by the one file the
//! spine is processing — and every wait the spine ever blocks on is an
//! in-flight reader operation that finishes without further dependencies,
//! so the pipeline always makes progress.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;

use fastcdc::v2020::FastCDC;
use rio_common::limits::{FASTCDC_AVG_BYTES, FASTCDC_MAX_BYTES, FASTCDC_MIN_BYTES};
use rio_nix::nar::{MAX_DIRECTORY_ENTRIES, MAX_NAR_DEPTH, frame};
use sha2::{Digest, Sha256};

use super::{
    IngestChunk, IngestConfig, IngestDir, IngestEntry, IngestError, IngestFile, IngestNode,
    IngestResult, IngestSymlink,
};

/// What the discovery `lstat` said a node is. Fixed before the node enters
/// the deque; the NAR header is written from these values, and a file
/// whose read disagrees with `size` fails the ingest
/// ([`IngestError::SizeChanged`]).
#[derive(Clone, Copy)]
enum Kind {
    File { size: u64, executable: bool },
    Dir,
}

/// Lifecycle of a file/dir node. Symlinks never become nodes — readlink
/// is one cheap syscall, done inline at discovery.
enum State {
    /// In the deque; IO not started. The spine may steal it.
    Pending,
    /// A reader (or the spine) is doing the IO. Always resolves to
    /// `FileReady`/`DirReady`/`Failed` — the IO needs nothing from the
    /// rest of the pipeline once started.
    Reading,
    /// File read complete; buffer parked for the spine.
    FileReady(Arc<FileBuf>),
    /// The spine hashed the contents and dropped its buffer reference
    /// (the chunk worker may still hold one).
    Consumed,
    /// Directory listed; children discovered and queued.
    DirReady(Arc<Vec<ChildEntry>>),
    /// IO failed; the error is in [`Shared::error_slot`].
    Failed,
}

/// A file or directory awaiting/undergoing/holding its IO result.
struct Node {
    /// Filesystem location, for the IO syscalls and error messages.
    path: PathBuf,
    /// Depth below the ingest root (root = 0), for the MAX_NAR_DEPTH bound.
    depth: usize,
    kind: Kind,
    /// Set (under the `state` lock) the moment anyone claims the IO.
    /// Read lock-free by the budget's abort predicate — a reader parked
    /// on the budget for a node the spine stole must give up, not wait
    /// for budget that will never be needed.
    taken: AtomicBool,
    state: Mutex<State>,
    cond: Condvar,
    /// Plane-2 output, written by a chunk worker.
    chunks: Mutex<Option<ChunkOutput>>,
}

impl Node {
    fn new(path: PathBuf, depth: usize, kind: Kind) -> Self {
        Self {
            path,
            depth,
            kind,
            taken: AtomicBool::new(false),
            state: Mutex::new(State::Pending),
            cond: Condvar::new(),
            chunks: Mutex::new(None),
        }
    }

    /// Atomically claim this node's IO. False means someone else (a
    /// reader or the spine) got there first — the caller skips it.
    fn claim(&self) -> bool {
        let mut st = self.state.lock().expect("node lock");
        if matches!(*st, State::Pending) {
            *st = State::Reading;
            self.taken.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// Publish a state transition and wake the spine if it is parked here.
    fn resolve(&self, state: State) {
        *self.state.lock().expect("node lock") = state;
        self.cond.notify_all();
    }
}

/// A discovered directory entry, in NAR (byte-lex) order.
struct ChildEntry {
    name: Vec<u8>,
    child: Child,
}

/// Either an async node (file/dir) or an inline-resolved symlink.
enum Child {
    Node(Arc<Node>),
    Symlink(Vec<u8>),
}

/// Plane-2 result for one file.
struct ChunkOutput {
    digest: [u8; 32],
    chunks: Vec<IngestChunk>,
}

/// A read file's bytes plus its byte-budget charge. Both planes hold an
/// `Arc`; the charge returns to the budget when the last plane drops it —
/// that is what makes the budget simultaneously the prefetch window and
/// the tee bound.
struct FileBuf {
    data: Vec<u8>,
    /// 0 for spine-stolen reads (they bypass the budget).
    charge: u64,
    budget: Arc<Budget>,
}

impl Drop for FileBuf {
    fn drop(&mut self) {
        self.budget.release(self.charge);
    }
}

/// One unit of plane-2 work.
struct ChunkJob {
    node: Arc<Node>,
    buf: Arc<FileBuf>,
}

/// The byte-budget semaphore.
struct Budget {
    total: u64,
    avail: Mutex<u64>,
    cond: Condvar,
}

impl Budget {
    /// Block until `size` bytes are admitted; returns the charge to
    /// release later. `None` when `abort()` turned true while waiting
    /// (node stolen by the spine, or the ingest failed).
    ///
    /// `abort` must not take any pipeline lock (it is called under the
    /// budget lock; the only other budget-lock user that holds a node
    /// lock is `FileBuf::drop`, giving a single node→budget lock order).
    fn acquire(&self, size: u64, abort: impl Fn() -> bool) -> Option<u64> {
        let mut avail = self.avail.lock().expect("budget lock");
        loop {
            if abort() {
                return None;
            }
            if size > self.total {
                // Oversized file: admitted alone when the budget is
                // fully free, charged the whole budget so nothing else
                // is admitted alongside it.
                if *avail == self.total {
                    *avail = 0;
                    return Some(self.total);
                }
            } else if *avail >= size {
                *avail -= size;
                return Some(size);
            }
            avail = self.cond.wait(avail).expect("budget lock");
        }
    }

    fn release(&self, charge: u64) {
        if charge == 0 {
            return;
        }
        *self.avail.lock().expect("budget lock") += charge;
        self.cond.notify_all();
    }

    /// Force parked acquirers to re-evaluate their abort predicate
    /// (spine steal, ingest failure). Lock-then-notify so a waiter that
    /// checked the predicate just before the state change is already
    /// parked and receives the wakeup.
    fn wake(&self) {
        drop(self.avail.lock().expect("budget lock"));
        self.cond.notify_all();
    }
}

/// Work deque + termination accounting.
struct QueueState {
    deque: VecDeque<Arc<Node>>,
    /// Items pushed but not yet finished (still queued OR in flight).
    /// Readers exit when this hits zero with an empty deque.
    outstanding: usize,
}

/// Everything the pipeline threads share.
struct Shared {
    queue: Mutex<QueueState>,
    queue_cond: Condvar,
    budget: Arc<Budget>,
    /// Fast failure flag; the actual error lives in `error_slot`.
    error: AtomicBool,
    /// First failure wins; later ones are dropped.
    error_slot: Mutex<Option<IngestError>>,
}

impl Shared {
    fn failed(&self) -> bool {
        self.error.load(Ordering::Acquire)
    }

    /// Record the first failure and wake every parked thread so the
    /// whole pipeline winds down (readers exit, spine aborts, budget
    /// waiters give up).
    fn fail(&self, e: IngestError) {
        {
            let mut slot = self.error_slot.lock().expect("error slot lock");
            if slot.is_none() {
                *slot = Some(e);
            }
        }
        self.error.store(true, Ordering::Release);
        // Lock-then-notify: a reader that checked the flag before the
        // store is already parked on the condvar and gets the wakeup.
        drop(self.queue.lock().expect("queue lock"));
        self.queue_cond.notify_all();
        self.budget.wake();
    }
}

/// Spine abort marker: the ingest failed and the real error is already
/// recorded in [`Shared::error_slot`].
struct Abort;

/// `Write` sink for the spine: SHA-256 + byte count, never fails.
struct TeeHasher {
    sha: Sha256,
    written: u64,
}

impl Write for TeeHasher {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sha.update(buf);
        self.written += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The frame emitters return `io::Result` for real sinks; the spine's
/// sink is an in-memory hasher whose `Write` impl cannot fail.
fn emit(r: io::Result<()>) {
    r.expect("sha256 tee sink is infallible");
}

/// Entry point — see [`super::ingest_tree`].
pub(super) fn run(root: &Path, config: &IngestConfig) -> Result<IngestResult, IngestError> {
    let readers = config.reader_threads.max(1);
    let workers = config.chunk_workers.max(1);
    let shared = Shared {
        queue: Mutex::new(QueueState {
            deque: VecDeque::new(),
            outstanding: 0,
        }),
        queue_cond: Condvar::new(),
        budget: Arc::new(Budget {
            total: config.byte_budget.max(1),
            avail: Mutex::new(config.byte_budget.max(1)),
            cond: Condvar::new(),
        }),
        error: AtomicBool::new(false),
        error_slot: Mutex::new(None),
    };

    // Discover the root on the calling thread (one lstat; readlink if it
    // is a symlink). Errors here never start a thread.
    let root_child = discover(root, 0)?;
    if let Child::Node(n) = &root_child {
        let mut q = shared.queue.lock().expect("queue lock");
        q.deque.push_back(Arc::clone(n));
        q.outstanding = 1;
    }

    let (chunk_tx, chunk_rx) = mpsc::channel::<ChunkJob>();
    let chunk_rx = Mutex::new(chunk_rx);

    // FORK SAFETY: every thread lives inside this scope; the scope joins
    // them all before `run` returns, on success and on error alike.
    let spine_out = thread::scope(|s| {
        for _ in 0..readers {
            let tx = chunk_tx.clone();
            let shared = &shared;
            s.spawn(move || reader_loop(shared, &tx));
        }
        for _ in 0..workers {
            s.spawn(|| chunk_worker(&shared, &chunk_rx));
        }
        // Only the readers send chunk jobs (the spine chunks stolen
        // reads inline). Once they exit — outstanding==0 on success,
        // the error flag on failure — and drop their senders, the
        // chunk workers see the disconnect, drain, and exit.
        drop(chunk_tx);

        let mut tee = TeeHasher {
            sha: Sha256::new(),
            written: 0,
        };
        emit(frame::magic(&mut tee));
        spine_child(&shared, &mut tee, &root_child).map(|()| (tee.sha, tee.written))
    });

    // All threads are joined; results and errors are final.
    if let Some(e) = shared.error_slot.lock().expect("error slot lock").take() {
        return Err(e);
    }
    let (sha, nar_size) = spine_out
        .map_err(|Abort| ())
        .expect("spine aborts only after recording an error");
    Ok(IngestResult {
        nar_sha256: sha.finalize().into(),
        nar_size,
        root: assemble(&root_child),
    })
}

/// lstat one path and turn it into a `Child`. Symlinks resolve inline;
/// files and directories become deque-able nodes.
fn discover(path: &Path, depth: usize) -> Result<Child, IngestError> {
    if depth > MAX_NAR_DEPTH {
        return Err(IngestError::TooDeep {
            path: path.to_path_buf(),
            depth,
        });
    }
    let meta = fs::symlink_metadata(path).map_err(|e| IngestError::Stat {
        path: path.to_path_buf(),
        source: e,
    })?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        let target = fs::read_link(path).map_err(|e| IngestError::ReadLink {
            path: path.to_path_buf(),
            source: e,
        })?;
        use std::os::unix::ffi::OsStringExt;
        Ok(Child::Symlink(target.into_os_string().into_vec()))
    } else if ft.is_file() {
        use std::os::unix::fs::PermissionsExt;
        Ok(Child::Node(Arc::new(Node::new(
            path.to_path_buf(),
            depth,
            Kind::File {
                size: meta.len(),
                executable: meta.permissions().mode() & 0o111 != 0,
            },
        ))))
    } else if ft.is_dir() {
        Ok(Child::Node(Arc::new(Node::new(
            path.to_path_buf(),
            depth,
            Kind::Dir,
        ))))
    } else {
        Err(IngestError::UnsupportedFileType {
            path: path.to_path_buf(),
        })
    }
}

/// List a directory node: readdir, byte-lex sort (NAR entry order),
/// lstat + discover each child.
fn list_dir(node: &Node) -> Result<Vec<ChildEntry>, IngestError> {
    let read_dir_err = |e| IngestError::ReadDir {
        path: node.path.clone(),
        source: e,
    };
    let mut names: Vec<std::ffi::OsString> = Vec::new();
    for entry in fs::read_dir(&node.path).map_err(read_dir_err)? {
        names.push(entry.map_err(read_dir_err)?.file_name());
    }
    // Same producer-side bound as rio-nix's dump: a NAR every consumer
    // rejects must not be emitted.
    if names.len() >= MAX_DIRECTORY_ENTRIES {
        return Err(IngestError::TooManyEntries {
            path: node.path.clone(),
            count: names.len(),
        });
    }
    {
        use std::os::unix::ffi::OsStrExt;
        names.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    }
    let mut children = Vec::with_capacity(names.len());
    for name in names {
        let child = discover(&node.path.join(&name), node.depth + 1)?;
        use std::os::unix::ffi::OsStringExt;
        children.push(ChildEntry {
            name: name.into_vec(),
            child,
        });
    }
    Ok(children)
}

/// Queue a listing's file/dir children for the readers. Pushed to the
/// FRONT in reverse order: the deque behaves as a DFS stack, so readers
/// process the tree in approximate NAR order and the byte budget
/// prefetches the files the spine needs next instead of distant subtrees.
fn push_children(shared: &Shared, children: &[ChildEntry]) {
    let mut q = shared.queue.lock().expect("queue lock");
    for e in children.iter().rev() {
        if let Child::Node(n) = &e.child {
            q.deque.push_front(Arc::clone(n));
            q.outstanding += 1;
        }
    }
    drop(q);
    shared.queue_cond.notify_all();
}

/// Read one file fully, enforcing the discovery-time length (the NAR
/// header is already committed to it once the spine writes it).
fn read_file(path: &Path, expected: u64) -> Result<Vec<u8>, IngestError> {
    let read_err = |e| IngestError::ReadFile {
        path: path.to_path_buf(),
        source: e,
    };
    let mut f = fs::File::open(path).map_err(read_err)?;
    let mut data = Vec::with_capacity(expected as usize);
    f.read_to_end(&mut data).map_err(read_err)?;
    if data.len() as u64 != expected {
        return Err(IngestError::SizeChanged {
            path: path.to_path_buf(),
            expected,
            actual: data.len() as u64,
        });
    }
    Ok(data)
}

/// Reader thread body: pop work until the tree is finished or the ingest
/// failed.
fn reader_loop(shared: &Shared, chunk_tx: &mpsc::Sender<ChunkJob>) {
    while let Some(node) = pop(shared) {
        process_item(shared, chunk_tx, &node);
        finish_item(shared);
    }
}

fn pop(shared: &Shared) -> Option<Arc<Node>> {
    let mut q = shared.queue.lock().expect("queue lock");
    loop {
        if shared.failed() {
            return None;
        }
        if let Some(n) = q.deque.pop_front() {
            return Some(n);
        }
        if q.outstanding == 0 {
            return None;
        }
        q = shared.queue_cond.wait(q).expect("queue lock");
    }
}

/// Mark one popped item finished; the last one wakes readers parked on
/// an empty deque so they can exit.
fn finish_item(shared: &Shared) {
    let mut q = shared.queue.lock().expect("queue lock");
    q.outstanding -= 1;
    if q.outstanding == 0 {
        drop(q);
        shared.queue_cond.notify_all();
    }
}

fn process_item(shared: &Shared, chunk_tx: &mpsc::Sender<ChunkJob>, node: &Arc<Node>) {
    match node.kind {
        Kind::Dir => {
            if !node.claim() {
                return; // stolen by the spine
            }
            match list_dir(node) {
                Ok(children) => {
                    let children = Arc::new(children);
                    push_children(shared, &children);
                    node.resolve(State::DirReady(children));
                }
                Err(e) => {
                    // fail() before resolve(): the spine wakes on the
                    // node condvar and must find the error recorded.
                    shared.fail(e);
                    node.resolve(State::Failed);
                }
            }
        }
        Kind::File { size, .. } => {
            // Budget BEFORE claim: a budget-parked reader must leave the
            // node stealable, or the spine would wait forever on a node
            // whose reader waits on budget only the spine can free.
            if node.taken.load(Ordering::Acquire) || shared.failed() {
                return;
            }
            let abort = || node.taken.load(Ordering::Acquire) || shared.failed();
            let Some(charge) = shared.budget.acquire(size, abort) else {
                return;
            };
            if !node.claim() {
                shared.budget.release(charge);
                return; // stolen while we waited on the budget
            }
            match read_file(&node.path, size) {
                Ok(data) => {
                    let buf = Arc::new(FileBuf {
                        data,
                        charge,
                        budget: Arc::clone(&shared.budget),
                    });
                    send_chunk_job(chunk_tx, node, &buf);
                    node.resolve(State::FileReady(buf));
                }
                Err(e) => {
                    shared.budget.release(charge);
                    shared.fail(e);
                    node.resolve(State::Failed);
                }
            }
        }
    }
}

fn send_chunk_job(chunk_tx: &mpsc::Sender<ChunkJob>, node: &Arc<Node>, buf: &Arc<FileBuf>) {
    chunk_tx
        .send(ChunkJob {
            node: Arc::clone(node),
            buf: Arc::clone(buf),
        })
        // The receiver outlives every sender: workers exit only on
        // channel disconnect, which needs all senders dropped first.
        .expect("chunk workers outlive all senders");
}

/// `IngestChunk::len` is `u32`: FastCDC never emits a chunk longer than
/// `FASTCDC_MAX_BYTES`.
const _: () = assert!(FASTCDC_MAX_BYTES <= u32::MAX as usize);

/// Plane 2 for one file: whole-file blake3 + FastCDC chunk list, same
/// constants and call shape as rio-builder's fused output walk so chunks
/// dedup across the builder and the eval store. Called by the chunk
/// workers and, for stolen reads, inline by the spine.
fn chunk_buf(data: &[u8]) -> ChunkOutput {
    let digest = *blake3::hash(data).as_bytes();
    let mut chunks = Vec::new();
    if !data.is_empty() {
        // FastCDC::new panics on empty input; a zero-byte file has an
        // empty chunk run by contract.
        for c in FastCDC::new(
            data,
            FASTCDC_MIN_BYTES,
            FASTCDC_AVG_BYTES,
            FASTCDC_MAX_BYTES,
        ) {
            chunks.push(IngestChunk {
                digest: *blake3::hash(&data[c.offset..c.offset + c.length]).as_bytes(),
                offset: c.offset as u64,
                len: c.length as u32,
            });
        }
    }
    ChunkOutput { digest, chunks }
}

/// Chunk worker body: run plane 2 over reader-read buffers.
fn chunk_worker(shared: &Shared, rx: &Mutex<mpsc::Receiver<ChunkJob>>) {
    loop {
        // Holding the mutex across recv is deliberate: idle workers
        // queue on the mutex instead of the channel — same dispatch.
        let job = match rx.lock().expect("chunk rx lock").recv() {
            Ok(j) => j,
            Err(_) => return, // all senders dropped: pipeline drained
        };
        if shared.failed() {
            continue; // drop the buffer (releases budget), keep draining
        }
        *job.node.chunks.lock().expect("chunks lock") = Some(chunk_buf(&job.buf.data));
    }
}

/// Emit one child (symlink inline, file/dir via [`spine_node`]).
fn spine_child(shared: &Shared, w: &mut TeeHasher, child: &Child) -> Result<(), Abort> {
    if shared.failed() {
        return Err(Abort);
    }
    match child {
        Child::Symlink(target) => {
            emit(frame::node_open(w));
            emit(frame::symlink(w, target));
            emit(frame::node_close(w));
            Ok(())
        }
        Child::Node(node) => spine_node(shared, w, node),
    }
}

/// Emit one file/dir node in canonical NAR token order.
fn spine_node(shared: &Shared, w: &mut TeeHasher, node: &Arc<Node>) -> Result<(), Abort> {
    emit(frame::node_open(w));
    match node.kind {
        Kind::File { size, executable } => {
            let buf = spine_wait_file(shared, node)?;
            emit(frame::regular_header(w, executable, size));
            emit(w.write_all(&buf.data));
            emit(frame::contents_padding(w, size));
        }
        Kind::Dir => {
            let children = spine_wait_dir(shared, node)?;
            emit(frame::directory_open(w));
            for e in children.iter() {
                emit(frame::entry_open(w, &e.name));
                spine_child(shared, w, &e.child)?;
                emit(frame::entry_close(w));
            }
        }
    }
    emit(frame::node_close(w));
    Ok(())
}

/// Block until this file's buffer is available, stealing the read if it
/// has not started (see the module-level progress guarantee).
fn spine_wait_file(shared: &Shared, node: &Arc<Node>) -> Result<Arc<FileBuf>, Abort> {
    let mut st = node.state.lock().expect("node lock");
    loop {
        match &*st {
            State::Pending => {
                *st = State::Reading;
                node.taken.store(true, Ordering::Release);
                drop(st);
                // A reader may be parked on the budget for this node —
                // make it re-check its abort predicate and move on.
                shared.budget.wake();
                let Kind::File { size, .. } = node.kind else {
                    unreachable!("spine_wait_file is only called for file nodes")
                };
                return match read_file(&node.path, size) {
                    Ok(data) => {
                        // charge 0: the spine consumes this buffer right
                        // now; budgeting it could deadlock against the
                        // very files it is meant to evict. Plane 2 runs
                        // inline for the same reason — a chunk job would
                        // park this budget-exempt buffer in the workers'
                        // queue, unbounded across repeated steals.
                        *node.chunks.lock().expect("chunks lock") = Some(chunk_buf(&data));
                        let buf = Arc::new(FileBuf {
                            data,
                            charge: 0,
                            budget: Arc::clone(&shared.budget),
                        });
                        node.resolve(State::Consumed);
                        Ok(buf)
                    }
                    Err(e) => {
                        shared.fail(e);
                        node.resolve(State::Failed);
                        Err(Abort)
                    }
                };
            }
            State::Reading => {
                st = node.cond.wait(st).expect("node lock");
            }
            State::FileReady(buf) => {
                let buf = Arc::clone(buf);
                // Drop the node's buffer reference now — keeping it
                // until assembly would hold every file of the tree in
                // memory and defeat the tee bound.
                *st = State::Consumed;
                return Ok(buf);
            }
            State::Failed => return Err(Abort),
            State::Consumed | State::DirReady(_) => {
                unreachable!("spine visits each node exactly once")
            }
        }
    }
}

/// Block until this directory's listing is available, stealing the
/// readdir if it has not started.
fn spine_wait_dir(shared: &Shared, node: &Arc<Node>) -> Result<Arc<Vec<ChildEntry>>, Abort> {
    let mut st = node.state.lock().expect("node lock");
    loop {
        match &*st {
            State::Pending => {
                *st = State::Reading;
                node.taken.store(true, Ordering::Release);
                drop(st);
                return match list_dir(node) {
                    Ok(children) => {
                        let children = Arc::new(children);
                        // Still queue the children: the readers prefetch
                        // them while the spine descends.
                        push_children(shared, &children);
                        node.resolve(State::DirReady(Arc::clone(&children)));
                        Ok(children)
                    }
                    Err(e) => {
                        shared.fail(e);
                        node.resolve(State::Failed);
                        Err(Abort)
                    }
                };
            }
            State::Reading => {
                st = node.cond.wait(st).expect("node lock");
            }
            State::DirReady(children) => return Ok(Arc::clone(children)),
            State::Failed => return Err(Abort),
            State::Consumed | State::FileReady(_) => {
                unreachable!("spine_wait_dir is only called for dir nodes")
            }
        }
    }
}

/// Fold the resolved skeleton into the public tree. Only called after a
/// fully successful run with all threads joined, so every file has its
/// chunk output and every directory its listing.
fn assemble(child: &Child) -> IngestNode {
    match child {
        Child::Symlink(target) => IngestNode::Symlink(IngestSymlink {
            target: target.clone(),
        }),
        Child::Node(node) => match node.kind {
            Kind::File { size, executable } => {
                let out = node
                    .chunks
                    .lock()
                    .expect("chunks lock")
                    .take()
                    .expect("chunk plane finished before threads joined");
                IngestNode::File(IngestFile {
                    digest: out.digest,
                    size,
                    executable,
                    chunks: out.chunks,
                })
            }
            Kind::Dir => {
                let children = {
                    let st = node.state.lock().expect("node lock");
                    match &*st {
                        State::DirReady(c) => Arc::clone(c),
                        _ => unreachable!("successful spine resolved every directory"),
                    }
                };
                IngestNode::Dir(IngestDir {
                    entries: children
                        .iter()
                        .map(|e| IngestEntry {
                            name: e.name.clone(),
                            node: assemble(&e.child),
                        })
                        .collect(),
                })
            }
        },
    }
}
