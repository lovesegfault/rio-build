# Remediation 18: SSH hardening

**Parent:** [phase4a.md §2.13](../phase4a.md#213-ssh-hardening)
**Findings:** `ssh-session-errors-swallowed`, `ssh-unbounded-channels`, `ssh-no-keepalive`, `ssh-nagle-enabled`, `ssh-auth-methods-all`
**Blast radius:** P1 — undiagnosable hangs, slow per-opcode RTT, half-open sockets pinning goroutine-equivalents forever, unbounded per-connection growth
**Effort:** ~2 h implementation + 1 integration-test cycle

---

## Ground truth

§2.13 cites `rio-gateway/src/ssh/server.rs:136`. There is no `ssh/` subdirectory;
the file is `rio-gateway/src/server.rs`. Line 136 is `impl russh::server::Server
for GatewayServer` — the correct `impl` block.

`russh 0.57.1` trait defaults (from `server/mod.rs`):

| Hook / field | Default | Problem |
|---|---|---|
| `Server::handle_session_error` | `{}` (no-op, :839) | Errors from any `?` in handler methods vanish |
| `Config::methods` | `MethodSet::all()` (:110) | Advertises `none`, `password`, `hostbased`, `keyboard-interactive` we don't support |
| `Config::keepalive_interval` | `None` (:122) | Half-open TCP (NLB idle reset, client kernel panic) never detected |
| `Config::nodelay` | `false` (:124) | Nagle on a request/response protocol = ~40ms per round-trip |
| `Handler::auth_none` | `Ok(Auth::reject())` (:218) | Correct result, but `mark_real_connection()` never fires |
| `Handler::auth_publickey_offered` | `Ok(Auth::Accept)` (:247) | Accepts *every* offered key → client computes signatures for unknown keys |

Plus two session-layer unbounded-growth sites not listed in §2.13 but adjacent:

- `handler/mod.rs:92` `temp_roots: HashSet<StorePath>` — insert-only, never read
- `session.rs:83` `wire::read_u64(reader).await` — no inter-opcode idle timeout

---

## 1. `handle_session_error` — stop swallowing

`russh::server::Server::handle_session_error` is called from `run_on_socket`'s
accept loop (russh `server/mod.rs:926`) whenever a connection task sends an
error up the `error_tx` channel. Two producers: connection setup failure
(`:891`) and session future returning `Err` (`:911`). Both paths today emit a
`debug!` in russh and then call our empty default. From the operator's side:
`nix copy` hangs 30s, client sees `Broken pipe`, server logs nothing above
`DEBUG`.

**Metric:** reuse `rio_gateway_errors_total` (already `describe_counter!`'d in
`lib.rs:53-56`, already in `observability.md:72`) with a new `type="session"`
label. §2.13's suggested `rio_gateway_ssh_session_errors_total` would be a new
metric name — no reason to split the series when the existing one is already
`type`-labelled.

```diff
--- a/rio-gateway/src/server.rs
+++ b/rio-gateway/src/server.rs
@@ -136,6 +136,22 @@ impl russh::server::Server for GatewayServer {
     type Handler = ConnectionHandler;

+    // r[impl gw.conn.session-error-visible]
+    /// russh default is a no-op (`server/mod.rs:839`). Called from the
+    /// accept loop when a connection task reports an error — both
+    /// connection-setup failure and any `?`-propagated error from a
+    /// `Handler` method. Without this override, `session.channel_success(..)?`
+    /// failures (and every other `?` in this file's `Handler` impl) drop
+    /// the connection with zero server-side signal.
+    ///
+    /// NOTE: this is on `Server`, not `Handler` — it runs on the accept
+    /// loop task, so `self.peer_addr` is not available. The error itself
+    /// is the only context we get.
+    fn handle_session_error(&mut self, error: <Self::Handler as Handler>::Error) {
+        error!(error = %error, "SSH session error");
+        metrics::counter!("rio_gateway_errors_total", "type" => "session").increment(1);
+    }
+
     fn new_client(&mut self, peer_addr: Option<SocketAddr>) -> Self::Handler {
```

---

## 2. Channel limit — `Ok(false)` past 4

`channel_open_session` currently logs and returns `Ok(true)` unconditionally.
`self.sessions` is a per-connection `HashMap<ChannelId, ChannelSession>`. Each
`ChannelSession` holds two spawned tasks + two 256 KiB duplex buffers — a
malicious or buggy client opening channels in a loop is ~0.5 MiB + 2 tasks per
iteration with no ceiling.

`Ok(false)` makes russh send `SSH_MSG_CHANNEL_OPEN_FAILURE` to the client. The
connection stays alive; the client's `channel_open_session()` call returns an
error it can handle (or not — that's on them).

**Why 4:** matches Nix's default `max-jobs`. A well-behaved `nix build -j4`
opens at most 4 channels. Anything past that is either misconfiguration or
abuse.

**Subtlety — `sessions.len()` vs channels opened.** `self.sessions` is
populated in `exec_request`, not `channel_open_session`. A client could open 100
session channels and send `exec` on none of them, and `sessions.len()` stays 0.
But: a channel without `exec` does nothing — no tasks spawned, no buffers. The
expensive state is the `ChannelSession`, and that's exactly what
`sessions.len()` counts. Gating on `sessions.len()` is correct for the resource
we care about.

```diff
--- a/rio-gateway/src/server.rs
+++ b/rio-gateway/src/server.rs
@@ -21,6 +21,14 @@ use tracing::{Instrument, debug, error, info, trace, warn};

 use crate::session::run_protocol;

+/// Max active protocol sessions per SSH connection. Matches Nix's
+/// default `max-jobs` — a well-behaved `nix build -j4` opens at most
+/// this many channels. Each session = 2 spawned tasks + 2×256 KiB
+/// duplex buffers, so this bounds per-connection memory at ~2 MiB.
+///
+/// Counted via `self.sessions.len()` — see `channel_open_session`.
+const MAX_CHANNELS_PER_CONNECTION: usize = 4;
+
 /// Load or generate an SSH host key.
```

```diff
@@ -269,10 +277,26 @@ impl Handler for ConnectionHandler {
     async fn channel_open_session(
         &mut self,
         channel: russh::Channel<Msg>,
         _session: &mut Session,
     ) -> Result<bool, Self::Error> {
         let channel_id = channel.id();
+        // r[impl gw.conn.channel-limit]
+        // Gate on sessions.len(), not "channels ever opened" — a channel
+        // without an `exec_request` has no ChannelSession, no spawned
+        // tasks, no buffers. Only exec'd channels consume resources.
+        // This DOES mean a client can burst 5 opens before the first
+        // exec lands; russh's event loop serializes handler calls so
+        // in practice exec-after-open is the common interleaving.
+        if self.sessions.len() >= MAX_CHANNELS_PER_CONNECTION {
+            warn!(
+                peer = ?self.peer_addr,
+                active = self.sessions.len(),
+                limit = MAX_CHANNELS_PER_CONNECTION,
+                "rejecting SSH channel open: per-connection limit reached"
+            );
+            metrics::counter!("rio_gateway_errors_total", "type" => "channel_limit").increment(1);
+            return Ok(false);
+        }
         info!(channel = ?channel_id, "SSH session channel opened");
         Ok(true)
     }
```

---

## 3. russh `Config` — keepalive, nodelay, methods

Three one-line field sets plus `auth_rejection_time_initial` (bonus — OpenSSH
clients send `none` first as a probe; without this override the 1s constant-time
rejection delay applies to a probe we know is benign).

```diff
--- a/rio-gateway/src/server.rs
+++ b/rio-gateway/src/server.rs
@@ -13,7 +13,8 @@ use rio_proto::StoreServiceClient;
 use russh::keys::ssh_key::rand_core::OsRng;
 use russh::keys::{Algorithm, PrivateKey, PublicKey};
 use russh::server::{Auth, Handler, Msg, Server as _, Session};
-use russh::{ChannelId, CryptoVec};
+use russh::{ChannelId, CryptoVec, MethodKind, MethodSet};
 use tokio::io::{AsyncReadExt, AsyncWriteExt};

@@ -116,9 +117,28 @@ impl GatewayServer {
     pub async fn run(mut self, host_key: PrivateKey, addr: SocketAddr) -> anyhow::Result<()> {
         let config = russh::server::Config {
             keys: vec![host_key],
-            inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
+            // r[impl gw.conn.keepalive]
+            // keepalive_max defaults to 3 (russh server/mod.rs:123).
+            // 30s × 3 unanswered = connection dropped at ~90s. Catches
+            // half-open TCP: NLB idle-timeout RST that never reached
+            // us, client kernel panic, cable pull. Without this, a
+            // half-open connection holds its ConnectionHandler (and
+            // all its ChannelSessions) until inactivity_timeout — 1h.
+            keepalive_interval: Some(std::time::Duration::from_secs(30)),
+            // Keep inactivity_timeout as a backstop; keepalive is primary.
+            inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
+            // r[impl gw.conn.nodelay]
+            // Worker protocol is small-request/small-response ping-pong
+            // (opcode u64 + a few strings, then STDERR_LAST + result).
+            // Nagle buffers the response waiting for more bytes that
+            // won't come until the client sends the NEXT opcode —
+            // which it won't until it sees this response. ~40ms/RTT.
+            nodelay: true,
+            // Only advertise publickey. Default MethodSet::all()
+            // includes none/password/hostbased/keyboard-interactive
+            // which we reject anyway — advertising them wastes a
+            // client round-trip per rejected method.
+            methods: MethodSet::from(&[MethodKind::PublicKey][..]),
             auth_rejection_time: std::time::Duration::from_secs(1),
+            // OpenSSH sends `none` first to probe available methods
+            // (RFC 4252 §5.2). That probe is not an attack; skip the
+            // constant-time delay for it. Subsequent real rejections
+            // (unknown pubkey) still get the full 1s.
+            auth_rejection_time_initial: Some(std::time::Duration::from_millis(10)),
             ..Default::default()
         };
```

**On `MethodSet::from(&[MethodKind::PublicKey][..])`:** russh's `From` impl is
for `&[MethodKind]` (slice), not `[MethodKind; N]` (array) — `auth.rs:83`. The
`[..]` coerces. Without it: `the trait From<&[MethodKind; 1]> is not
implemented`.

---

## 4. `auth_none` + `auth_publickey_offered` — complete the `auth_*` coverage

### 4a. `auth_none` → `mark_real_connection`

`mark_real_connection` is called from `auth_password` and `auth_publickey`
today. But OpenSSH's canonical flow is: `none` probe → server replies with
`methods that can continue` → `publickey`. The `none` probe is the **first**
auth callback — without overriding it, a client that only sends `none` (then
disconnects, or gets the method list and gives up) never increments
`rio_gateway_connections_total{result="new"}` and the `connections_active`
gauge's increment/decrement pairing in `ConnectionHandler::Drop` (`:210-224`)
stays balanced at zero. A real SSH-speaking client that never authenticates is
invisible.

The comment block at `server.rs:140-143` already documents the intent
("Defer logging/metrics to `mark_real_connection()`, called from the first
`auth_*` callback") — `auth_none` is exactly that first callback and we forgot
it.

```diff
--- a/rio-gateway/src/server.rs
+++ b/rio-gateway/src/server.rs
@@ -227,6 +227,18 @@ impl Handler for ConnectionHandler {
     type Error = anyhow::Error;

+    // r[impl gw.conn.real-connection-marker]
+    /// OpenSSH clients send `none` first (RFC 4252 §5.2 probe). This is
+    /// the FIRST auth callback for a well-behaved client — the earliest
+    /// point we can distinguish "real SSH client" from "TCP probe."
+    /// Without this override, `mark_real_connection` only fires on
+    /// `auth_password`/`auth_publickey`, missing clients that probe and
+    /// disconnect (or probe, see `publickey` in the method list, and
+    /// then fail key offering in §4b below before ever reaching
+    /// `auth_publickey`).
+    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
+        self.mark_real_connection();
+        Ok(Auth::reject())
+    }
+
     async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
```

### 4b. `auth_publickey_offered` → reject unknown keys early

russh's default (`server/mod.rs:247`) is `Ok(Auth::Accept)` — "yes, go compute
a signature and send it, I'll verify." For every key the client offers. An
`ssh-agent` with 10 keys means 10 signature round-trips when only key #7 is in
`authorized_keys`.

OpenSSH's `sshd` checks `authorized_keys` at the **offered** stage and rejects
unknown keys before the client signs. Matching that:

```diff
--- a/rio-gateway/src/server.rs
+++ b/rio-gateway/src/server.rs
@@ (after auth_none, before auth_password)
+    /// russh default accepts every offered key, forcing the client to
+    /// compute a signature we'll then reject in `auth_publickey`. Check
+    /// `authorized_keys` here instead — unknown key → reject before
+    /// signature, saving the client a round-trip per ssh-agent key.
+    ///
+    /// DO NOT set `self.tenant_name` here. The client hasn't proven
+    /// ownership yet (no signature). `auth_publickey` does the final
+    /// match-and-set after russh verifies the signature.
+    ///
+    /// No `mark_real_connection()` — `auth_none` always fires first
+    /// for OpenSSH clients. A non-OpenSSH client that skips the `none`
+    /// probe and goes straight to publickey is covered by the
+    /// `auth_publickey` call that follows on accept.
+    async fn auth_publickey_offered(
+        &mut self,
+        _user: &str,
+        key: &PublicKey,
+    ) -> Result<Auth, Self::Error> {
+        let known = self
+            .authorized_keys
+            .iter()
+            .any(|authorized| authorized.key_data() == key.key_data());
+        if known {
+            Ok(Auth::Accept)
+        } else {
+            debug!(peer = ?self.peer_addr, "offered key not in authorized_keys");
+            Ok(Auth::reject())
+        }
+    }
+
```

The `authorized_keys` scan is duplicated between `auth_publickey_offered` and
`auth_publickey`. Fine — it's a `Vec` of at most a few dozen keys, and the
semantics differ (`offered`: does the key exist? `publickey`: which entry
matched, for the comment). Not worth a helper.

---

## 5. `temp_roots` — delete the field

`SessionContext::temp_roots` (`handler/mod.rs:92`) is `insert`-only
(`opcodes_read.rs:180`). No reader anywhere in the crate. Why it exists:
`wopAddTempRoot` is a local-daemon GC concept — "I'm about to use this path,
don't GC it mid-session." rio's GC is store-side with explicit pins; a
gateway-session-scoped in-memory set is meaningless to it.

**Growth pattern:** `nix copy` of a large closure calls `wopAddTempRoot` once
per path before each upload. 10k-path closure → 10k `StorePath` entries ×
~50 bytes each = ~500 KiB per session. Not catastrophic per-session, but it's
pure waste, and the "never-read" part means the next engineer to touch it
will assume it does something.

**Decision: delete, not cap.** Capping is lying — it pretends the set has
semantics worth preserving. It doesn't. The opcode handler becomes a true
no-op (read path, validate, ack).

```diff
--- a/rio-gateway/src/handler/mod.rs
+++ b/rio-gateway/src/handler/mod.rs
@@ -88,7 +88,6 @@
 pub struct SessionContext {
     pub store_client: StoreServiceClient<Channel>,
     pub scheduler_client: SchedulerServiceClient<Channel>,
     pub options: Option<ClientOptions>,
-    pub temp_roots: HashSet<StorePath>,
     pub drv_cache: HashMap<StorePath, Derivation>,
@@ -111,7 +110,6 @@ impl SessionContext {
         Self {
             store_client,
             scheduler_client,
             options: None,
-            temp_roots: HashSet::new(),
             drv_cache: HashMap::new(),
@@ -180,7 +178,7 @@ pub async fn handle_opcode<R, W>(
         Some(WorkerOp::AddTempRoot) => {
-            handle_add_temp_root(reader, &mut stderr, &mut ctx.temp_roots).await
+            handle_add_temp_root(reader, &mut stderr).await
         }
```

```diff
--- a/rio-gateway/src/handler/opcodes_read.rs
+++ b/rio-gateway/src/handler/opcodes_read.rs
@@ -167,24 +167,26 @@
 // r[impl gw.opcode.mandatory-set]
-/// wopAddTempRoot (11): Register a temporary GC root.
+/// wopAddTempRoot (11): read-and-ack no-op.
+///
+/// Temp roots are a local-daemon concept — "don't GC this path while
+/// my session holds it." rio's GC is store-side with explicit pins
+/// (see `rio-store/src/gc/`); a gateway-session-scoped set is invisible
+/// to it. We MUST still consume the path off the wire and ack with `1`,
+/// or the client desyncs.
+///
+/// Previously this inserted into a `HashSet<StorePath>` that nothing
+/// read — unbounded growth on `nix copy` of large closures.
 #[instrument(skip_all)]
 pub(super) async fn handle_add_temp_root<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
     reader: &mut R,
     stderr: &mut StderrWriter<&mut W>,
-    temp_roots: &mut HashSet<StorePath>,
 ) -> anyhow::Result<()> {
     let path_str = wire::read_string(reader).await?;
     debug!(path = %path_str, "wopAddTempRoot");

-    match StorePath::parse(&path_str) {
-        Ok(path) => {
-            temp_roots.insert(path);
-        }
-        Err(e) => {
-            warn!(path = %path_str, error = %e, "invalid store path in wopAddTempRoot, ignoring");
-        }
-    }
+    // Still validate — a malformed path is a client bug worth logging,
+    // even though we do nothing with the result.
+    if let Err(e) = StorePath::parse(&path_str) {
+        warn!(path = %path_str, error = %e, "invalid store path in wopAddTempRoot, ignoring");
+    }

     stderr.finish().await?;
     wire::write_u64(stderr.inner_mut(), 1).await?;
     Ok(())
 }
```

Also drop the now-unused `use std::collections::HashSet` in `handler/mod.rs`
(clippy `--deny warnings` will catch it if forgotten).

`docs/src/components/gateway.md:474` mentions "the `wopAddTempRoot` set" in
the session-state note — update to "and tracks `wopAddTempRoot` calls as a
no-op" in the same commit.

---

## 6. Inter-opcode idle timeout

`session.rs:83` reads the next opcode with no timeout. A client that completes
handshake + `wopSetOptions`, then stops sending (bug, or deliberate
slowloris-style), holds the session forever. `inactivity_timeout` (1h) and
keepalive (§3 above) catch *dead* clients; this catches *alive-but-idle* ones.

**Why this is safe for long builds:** the timeout wraps only `wire::read_u64`
— the read of the NEXT opcode. `handle_opcode` runs outside the timeout.
`wopBuildDerivation` blocking for 3 hours inside `handle_opcode` is fine; the
timer doesn't start until the handler returns and we loop back to `read_u64`.

**10 minutes:** generous. A real Nix client's inter-opcode gap is milliseconds
(it's a synchronous loop). Even accounting for client-side GC pauses, network
hiccups, and human `^Z`/`fg`, 10 minutes of silence between opcodes is
pathological.

```diff
--- a/rio-gateway/src/session.rs
+++ b/rio-gateway/src/session.rs
@@ -14,6 +14,13 @@ use tracing::{debug, error, info, warn};

 use crate::handler::{self, SessionContext};

+/// Max time to wait for the NEXT opcode after the previous one completes.
+/// Catches alive-but-idle clients (handshake, then silence). Does NOT
+/// bound per-opcode duration — `handle_opcode` runs outside this timer,
+/// so a 3-hour `wopBuildDerivation` is unaffected. Real Nix clients'
+/// inter-opcode gap is milliseconds.
+const OPCODE_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
+
@@ -82,7 +89,15 @@
     loop {
-        let opcode = match wire::read_u64(reader).await {
-            Ok(op) => op,
+        let opcode = match tokio::time::timeout(
+            OPCODE_IDLE_TIMEOUT,
+            wire::read_u64(reader),
+        )
+        .await
+        {
+            Ok(Ok(op)) => op,
+            Err(_elapsed) => {
+                warn!(timeout = ?OPCODE_IDLE_TIMEOUT, "idle timeout waiting for next opcode");
+                metrics::counter!("rio_gateway_errors_total", "type" => "idle_timeout").increment(1);
+                // Best-effort: try to tell the client why. If the
+                // write fails, the connection is already dead.
+                let mut stderr = StderrWriter::new(&mut *writer);
+                let _ = stderr
+                    .error(&StderrError::simple(
+                        "rio-gateway",
+                        format!("idle timeout: no opcode received in {OPCODE_IDLE_TIMEOUT:?}"),
+                    ))
+                    .await;
+                return Ok(());
+            }
-            Err(wire::WireError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
+            Ok(Err(wire::WireError::Io(e))) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                 debug!("client disconnected (EOF)");
@@ (end of EOF arm)
                 return Ok(());
             }
-            Err(e) => {
+            Ok(Err(e)) => {
                 metrics::counter!("rio_gateway_errors_total", "type" => "wire_read").increment(1);
```

---

## Tests

These exercise the SSH layer — `GatewaySession` (`tests/common/mod.rs`) bypasses
SSH entirely via `DuplexStream` → `run_protocol`. We need a real russh client.
**New file:** `rio-gateway/tests/ssh_hardening.rs`.

### Harness

```rust
// rio-gateway/tests/ssh_hardening.rs
//
// Exercises the SSH layer (russh Config + Server/Handler overrides).
// GatewaySession in tests/common/ bypasses SSH via DuplexStream; these
// tests need a real TCP socket + russh::client.

use std::sync::Arc;

use russh::client;
use russh::keys::{Algorithm, PrivateKey};

struct AcceptAllClient;
impl client::Handler for AcceptAllClient {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        _key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// Spawn a GatewayServer on 127.0.0.1:0 with one authorized key.
/// Returns (bound addr, the client's private key, server task).
/// Mock gRPC backends — these tests don't reach opcodes.
async fn spawn_ssh_server() -> anyhow::Result<(
    std::net::SocketAddr,
    PrivateKey,
    tokio::task::JoinHandle<()>,
)> {
    // ... spawn_mock_store + spawn_mock_scheduler (from rio-test-support)
    // ... PrivateKey::random for client; .public_key() into authorized_keys
    // ... TcpListener::bind("127.0.0.1:0"), local_addr(), hand to run_on_socket
    // Host key: ephemeral PrivateKey::random.
    // Config: the REAL config from §3 above — this is what we're testing.
    todo!("harness scaffold — fill during implementation")
}
```

### T1 — 5th channel rejected

```rust
// r[verify gw.conn.channel-limit]
#[tokio::test]
async fn test_fifth_channel_rejected() -> anyhow::Result<()> {
    let (addr, client_key, _srv) = spawn_ssh_server().await?;

    let config = Arc::new(client::Config::default());
    let mut session = client::connect(config, addr, AcceptAllClient).await?;
    assert!(
        session
            .authenticate_publickey(
                "nix",
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(client_key), None)
            )
            .await?
            .success()
    );

    // Open 4 channels, send exec on each (populates self.sessions).
    // Must hold the channel handles — dropping them closes the channel
    // and shrinks sessions.len().
    let mut chans = Vec::new();
    for _ in 0..4 {
        let ch = session.channel_open_session().await?;
        ch.exec(true, "nix-daemon --stdio").await?;
        chans.push(ch);
    }

    // 5th: russh server sends SSH_MSG_CHANNEL_OPEN_FAILURE → client
    // returns Err(ChannelOpenFailure).
    let fifth = session.channel_open_session().await;
    assert!(
        matches!(fifth, Err(russh::Error::ChannelOpenFailure(_))),
        "expected ChannelOpenFailure, got {fifth:?}"
    );

    // Closing one frees a slot.
    chans.pop().unwrap().close().await?;
    // Give the server a tick to process channel_close → sessions.remove.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let sixth = session.channel_open_session().await;
    assert!(sixth.is_ok(), "slot should free after close: {sixth:?}");

    Ok(())
}
```

### T2 — half-open dropped within ~90s

```rust
// r[verify gw.conn.keepalive]
//
// Time budget: keepalive_interval=30s × keepalive_max=3 = 90s ceiling.
// We give 100s and assert the server-side handler dropped. Uses
// tokio::time::pause — NOT start_paused (see lang-gotchas:
// `start_paused` + real TCP = spurious deadline). Pause AFTER the
// connection is established, then advance.
//
// "Half-open" simulation: authenticate successfully, then into_raw_fd
// the client socket and leak it. From the server's perspective: TCP
// established, peer alive at the kernel level (ACKs), but SSH layer
// silent. Exactly what keepalive is for — kernel TCP keepalive would
// NOT fire (kernel still ACKs); SSH-layer keepalive DOES.
#[tokio::test]
async fn test_half_open_dropped_by_keepalive() -> anyhow::Result<()> {
    // Can't use tokio::time::pause with a real russh client task running
    // (its internal timers would fire). Instead: drastically shorten
    // the config for this test — spawn_ssh_server takes a Config
    // override. keepalive_interval=100ms, keepalive_max=3 → ~300ms.
    //
    // ACTUALLY — spawn_ssh_server as scaffolded hardcodes the config.
    // Add a spawn_ssh_server_with_config(cfg: russh::server::Config)
    // variant. Production config stays in server.rs; test uses a
    // scaled-down clone (same structure, faster timers).
    let cfg = russh::server::Config {
        keepalive_interval: Some(std::time::Duration::from_millis(100)),
        keepalive_max: 3,
        // ... rest as production
        ..test_config()
    };
    let (addr, client_key, srv) = spawn_ssh_server_with_config(cfg).await?;

    // Connect + auth. Then: take the underlying TcpStream, stop
    // polling the russh client future. Server's keepalive requests go
    // unanswered at the SSH layer → connection closed.
    //
    // russh::client::Handle doesn't expose the raw stream. Alternative:
    // connect with a custom Handler that blocks forever in some
    // callback? No — keepalive replies are handled internally by russh.
    //
    // SIMPLEST: connect, auth, then std::mem::forget the client Handle.
    // The client task stops being polled (no executor wakes it), russh
    // server's keepalive requests get no SSH-layer reply.
    //
    // WRONG — tokio spawns the client task; forgetting the Handle
    // doesn't stop the task.
    //
    // CORRECT: raw TcpStream. Do the SSH handshake manually up through
    // auth (or: connect with russh, then abort the client's background
    // task via JoinHandle). russh::client::connect spawns internally;
    // we don't get the JoinHandle.
    //
    // Pragmatic approach: use russh's lower-level API. OR — accept this
    // as a metric-assert test: connect normally, then instead of
    // simulating half-open, DROP the client and assert the server
    // processes disconnect cleanly. That tests the happy path but not
    // keepalive.
    //
    // DECISION: raw-TCP approach. Connect a TcpStream, speak SSH
    // handshake + auth by hand (or via russh::client::connect_stream
    // over a controllable transport), then stop reading. Enough of the
    // complexity that this test gets its own helper module. Defer the
    // full impl to the implementation PR; this plan records the intent
    // and the 90s→~300ms scaling trick.

    todo!("see block comment — raw-TCP or connect_stream approach")
}
```

**T2 fallback if raw-TCP proves brittle:** metric-assert. The keepalive
disconnect path increments `rio_gateway_errors_total{type="session"}` (via §1's
`handle_session_error`). A test that connects, blackholes, waits 400ms (scaled
config), then scrapes the metrics endpoint for `type="session"` delta ≥ 1 is
less direct but sufficient as an `r[verify gw.conn.keepalive]` — it proves the
full chain (keepalive fires → connection errors → error surfaces to operator).

### T3 — `auth_none` counted, `auth_publickey_offered` rejects unknown

Unit-level — no real SSH needed. Construct `ConnectionHandler` directly (fields
are all crate-visible or add a `#[cfg(test)]` constructor), call `auth_none`,
assert `auth_attempted == true`. Call `auth_publickey_offered` with a fresh
random key not in `authorized_keys`, assert `Auth::Reject`.

### T4 — idle timeout fires

Via `GatewaySession` (DuplexStream harness) + `tokio::time::pause`. Handshake,
`wopSetOptions`, then `tokio::time::advance(OPCODE_IDLE_TIMEOUT + 1s)`, read
the stream: expect `STDERR_ERROR` with "idle timeout" in the message. This one
is safe for `pause` because `GatewaySession` has no real TCP — it's all
in-memory duplex.

### T5 — `temp_roots` gone

Compile-time check (field deleted → any stray use fails `cargo build`). Existing
wire test at `tests/wire_opcodes/opcodes_read.rs:194` already asserts
`wopAddTempRoot` returns `1` — still passes with the no-op handler.

---

## Spec markers to add

`docs/src/components/gateway.md`, in the Connection Lifecycle section (after
`r[gw.conn.exec-request]` at :498):

```markdown
r[gw.conn.session-error-visible]
Any error propagated from an SSH handler method (via `?`) is logged at
`error!` and increments `rio_gateway_errors_total{type="session"}`. The
russh default swallows these silently.

r[gw.conn.channel-limit]
A single SSH connection may open at most `MAX_CHANNELS_PER_CONNECTION`
(default 4) active protocol sessions. Additional `channel_open_session`
requests receive `SSH_MSG_CHANNEL_OPEN_FAILURE`. The limit matches Nix's
default `max-jobs`.

r[gw.conn.keepalive]
The gateway sends SSH keepalive requests every 30 seconds. After 3
consecutive unanswered keepalives (~90 s), the connection is closed.
This detects half-open TCP that kernel-level keepalive would not.

r[gw.conn.nodelay]
TCP_NODELAY is set on all accepted sockets. The worker protocol's
small-request/small-response pattern interacts pathologically with
Nagle's algorithm (~40 ms added per round-trip).

r[gw.conn.real-connection-marker]
`rio_gateway_connections_total{result="new"}` and
`rio_gateway_connections_active` count connections that reached the SSH
authentication layer (any `auth_*` callback). TCP probes that close
before the SSH handshake are logged at `trace!` only.
```

`r[impl ...]` annotations: in diffs above. `r[verify ...]`: on T1/T2/T3/T4.
