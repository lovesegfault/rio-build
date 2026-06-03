//! `rio-cli keygen` — ed25519 narinfo signing-key operations.
//!
//! Two subcommands, both purely local (no scheduler/store connection,
//! no config):
//!
//! - `keygen new <name> <secret-file> <public-file>` — generate a
//!   keypair. Replaces `nix-store --generate-binary-cache-key` in the
//!   bootstrap Job (`nix/bootstrap-job.sh`) so the bootstrap image
//!   doesn't carry the Nix closure. Output is byte-compatible with
//!   what `nix-store` emits and what
//!   `rio-store/src/signing.rs::Signer::parse` and
//!   `parse_trusted_key_entry` accept:
//!   - secret: `{name}:{base64(seed ++ pubkey)}` — the 64-byte
//!     expanded keypair encoding, standard RFC 4648 alphabet with
//!     padding, no trailing newline. Written `0600`, never
//!     overwritten.
//!   - public: `{name}:{base64(pubkey)}` — 32 bytes; the
//!     `trusted-public-keys` entry format.
//! - `keygen derive-pub` — read a SECRET entry on stdin, write the
//!   derived PUBLIC entry to stdout (no trailing newline). The only
//!   secret-to-public mapping the bootstrap plumbing is allowed to
//!   use: the shell previously re-implemented this as
//!   `base64 -d | tail -c 32 | base64 -w0`, which published the
//!   private seed verbatim for 32-byte seed-only entries (round-16
//!   bug_023). stdin (never argv) so the secret cannot land in
//!   /proc/*/cmdline; output is the codec's canonical encoding so
//!   `cmp`-based pair-consistency checks see keygen-identical bytes.
//!
//! All byte-format knowledge lives in `rio_common::signing_keyfmt`;
//! this module is I/O glue. The seed comes straight from the OS
//! CSPRNG — the workspace deliberately leaves `ed25519-dalek`'s
//! `rand_core` feature off (keys elsewhere are always built from
//! supplied seed bytes), so the entropy read and the key derivation
//! are two explicit steps here.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use clap::Args;
use rand::Rng as _;
use rio_common::signing_keyfmt::SecretEntry;

#[derive(Args, Clone)]
pub(crate) struct KeygenArgs {
    #[command(subcommand)]
    cmd: KeygenCmd,
}

#[derive(clap::Subcommand, Clone)]
enum KeygenCmd {
    /// Generate a new signing keypair (refuses to overwrite an
    /// existing secret file — rotation must be explicit).
    New(NewArgs),
    /// Read a secret entry (`name:base64`) on stdin and print the
    /// derived public entry (`name:base64(pubkey)`) to stdout with no
    /// trailing newline. The only supported secret-to-public mapping;
    /// refuses internally inconsistent or malformed entries.
    DerivePub,
}

#[derive(Args, Clone)]
struct NewArgs {
    /// Key name (e.g. `rio-<bucket>`). Embedded in every signature
    /// (`Sig: {name}:{b64sig}`) so clients know which
    /// `trusted-public-keys` entry to verify against.
    name: String,
    /// Where to write the secret key (`{name}:{base64(seed++pubkey)}`,
    /// mode 0600). Refuses to overwrite an existing file — silently
    /// regenerating a cluster signing key invalidates every signature
    /// made under the old one.
    secret_key_file: PathBuf,
    /// Where to write the public key (`{name}:{base64(pubkey)}`).
    public_key_file: PathBuf,
}

/// Run the `keygen` subcommand. Sync — no RPC, no async I/O.
pub(crate) fn run(args: KeygenArgs) -> anyhow::Result<()> {
    match args.cmd {
        KeygenCmd::New(a) => run_new(a),
        KeygenCmd::DerivePub => run_derive_pub(&mut std::io::stdin(), &mut std::io::stdout()),
    }
}

/// `keygen derive-pub`: secret entry on `input`, canonical public
/// entry (no trailing newline) on `output`. Factored over generic
/// streams so tests drive it without a real stdin.
fn run_derive_pub(input: &mut impl Read, output: &mut impl Write) -> anyhow::Result<()> {
    let mut entry = String::new();
    input
        .read_to_string(&mut entry)
        .context("read secret entry from stdin")?;
    // Transport stripping only (CLI `--output text` / editor trailing
    // newlines); the codec itself is a pure byte contract.
    let entry = entry.trim_ascii();
    let secret = rio_common::signing_keyfmt::SecretEntry::parse(entry)
        .context("parse secret entry from stdin")?;
    output
        .write_all(secret.derive_pub().encode().as_bytes())
        .context("write public entry to stdout")?;
    Ok(())
}

fn run_new(args: NewArgs) -> anyhow::Result<()> {
    if args.name.is_empty() {
        bail!("key name must not be empty");
    }
    // The colon is THE separator in `name:base64` — a name containing
    // one would make the emitted file unparseable (split_once would
    // cut the name short and feed garbage to the base64 decoder).
    if args.name.contains(':') {
        bail!("key name must not contain ':' (it separates name from key material)");
    }

    // rand::rng() (ThreadRng), not the OS RNG directly: in the
    // rand_core 0.10 lineage the OS handle is fallible (TryCryptoRng)
    // while ThreadRng — ChaCha12 reseeded from OS entropy — satisfies
    // the infallible CryptoRng bound. Same choice, same rationale, as
    // the gateway's SSH host-key generation (rio-gateway/src/server/
    // keys.rs) and xtask's client-key generation (xtask/src/ssh.rs).
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);

    let (secret_entry, public_entry) = encode_keypair(&args.name, &seed)?;

    // Secret first, with create_new: the "already exists" refusal is
    // atomic (no stat-then-write TOCTOU) and happens before anything
    // touches the filesystem. A pre-existing PUBLIC file is overwritten
    // — it is derived data, and converging a stale pub onto a fresh
    // secret is exactly what the bootstrap Job's retry path needs.
    write_secret(&args.secret_key_file, &secret_entry)
        .with_context(|| format!("write secret key to {}", args.secret_key_file.display()))?;
    std::fs::write(&args.public_key_file, &public_entry)
        .with_context(|| format!("write public key to {}", args.public_key_file.display()))?;

    // The public entry is what operators paste into
    // `trusted-public-keys`; print it so the Job log carries it even
    // if nobody fetches `rio/signing-key-pub`.
    println!("{public_entry}");
    Ok(())
}

/// Derive the keypair from `seed` and render both file bodies via the
/// shared codec (`rio_common::signing_keyfmt` — the single owner of
/// the name:base64 byte contract; canonical encodings, no trailing
/// newline). Pure — keeps the format encoding separate from the I/O
/// and the entropy read.
fn encode_keypair(name: &str, seed: &[u8; 32]) -> anyhow::Result<(String, String)> {
    let entry = SecretEntry::from_seed(name, seed)?;
    Ok((entry.encode(), entry.derive_pub().encode()))
}

/// Create `path` with mode 0600 and write `contents`. Fails if the
/// file already exists.
fn write_secret(path: &Path, contents: &str) -> anyhow::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow::anyhow!(
                    "refusing to overwrite existing secret key (delete it first if you really \
                     mean to rotate the cluster signing key)"
                )
            } else {
                e.into()
            }
        })?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use ed25519_dalek::{Signer as _, SigningKey, Verifier as _, VerifyingKey};

    use super::*;

    /// The format contract `rio-store::signing::Signer::parse` and
    /// `parse_trusted_key_entry` depend on: secret decodes to 64 bytes
    /// whose tail 32 equal the public file's 32, and a signature made
    /// with the seed verifies against the published public key.
    #[test]
    fn round_trip_format() {
        let dir = tempfile::tempdir().unwrap();
        let sec_path = dir.path().join("key.sec");
        let pub_path = dir.path().join("key.pub");
        run_new(NewArgs {
            name: "rio-test-1".into(),
            secret_key_file: sec_path.clone(),
            public_key_file: pub_path.clone(),
        })
        .unwrap();

        let b64 = base64::engine::general_purpose::STANDARD;

        let sec = std::fs::read_to_string(&sec_path).unwrap();
        let (sec_name, sec_b64) = sec.split_once(':').expect("secret is name:base64");
        assert_eq!(sec_name, "rio-test-1");
        let sec_bytes = b64.decode(sec_b64).expect("secret base64 decodes");
        assert_eq!(sec_bytes.len(), 64, "secret is the 64-byte expanded form");
        // No trailing newline — byte-compatible with nix-store's output
        // and with `--secret-string file://` round-trips.
        assert!(!sec.ends_with('\n'));

        let pubkey = std::fs::read_to_string(&pub_path).unwrap();
        let (pub_name, pub_b64) = pubkey.split_once(':').expect("public is name:base64");
        assert_eq!(pub_name, "rio-test-1");
        let pub_bytes = b64.decode(pub_b64).expect("public base64 decodes");
        assert_eq!(pub_bytes.len(), 32, "public key is 32 bytes");
        assert!(!pubkey.ends_with('\n'));

        assert_eq!(
            &sec_bytes[32..],
            pub_bytes.as_slice(),
            "secret's trailing 32 bytes are the public key"
        );

        // Sign with the seed (what rio-store's Signer does after
        // parsing the first 32 bytes), verify with the published key
        // (what a nix client with trusted-public-keys does).
        let seed: [u8; 32] = sec_bytes[..32].try_into().unwrap();
        let sig = SigningKey::from_bytes(&seed).sign(b"fingerprint");
        let vk = VerifyingKey::from_bytes(&pub_bytes.try_into().unwrap()).unwrap();
        vk.verify(b"fingerprint", &sig)
            .expect("signature made with the seed verifies against the published public key");

        // The secret file is operator-eyes-only.
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&sec_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "secret key file is mode 0600");
    }

    /// A pre-existing secret file makes the command fail without
    /// modifying it (rotation must be explicit, never accidental).
    #[test]
    fn refuses_to_overwrite_secret() {
        let dir = tempfile::tempdir().unwrap();
        let sec_path = dir.path().join("key.sec");
        let pub_path = dir.path().join("key.pub");
        std::fs::write(&sec_path, "existing").unwrap();

        let err = run_new(NewArgs {
            name: "rio-test-1".into(),
            secret_key_file: sec_path.clone(),
            public_key_file: pub_path.clone(),
        })
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("refusing to overwrite"),
            "error names the refusal: {err:#}"
        );
        assert_eq!(
            std::fs::read_to_string(&sec_path).unwrap(),
            "existing",
            "existing secret untouched"
        );
        assert!(!pub_path.exists(), "no public key written on refusal");
    }

    /// A `:` in the name would corrupt the `name:base64` framing.
    #[test]
    fn rejects_colon_in_name() {
        let dir = tempfile::tempdir().unwrap();
        let err = run_new(NewArgs {
            name: "bad:name".into(),
            secret_key_file: dir.path().join("k.sec"),
            public_key_file: dir.path().join("k.pub"),
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("must not contain ':'"));
    }

    /// Distinct invocations produce distinct keys (the seed actually
    /// comes from the RNG, not a fixed array).
    #[test]
    fn keys_are_random() {
        let dir = tempfile::tempdir().unwrap();
        for n in ["a", "b"] {
            run_new(NewArgs {
                name: "k".into(),
                secret_key_file: dir.path().join(format!("{n}.sec")),
                public_key_file: dir.path().join(format!("{n}.pub")),
            })
            .unwrap();
        }
        assert_ne!(
            std::fs::read_to_string(dir.path().join("a.sec")).unwrap(),
            std::fs::read_to_string(dir.path().join("b.sec")).unwrap(),
        );
    }

    /// `derive-pub` on a keygen-emitted secret reproduces the keygen
    /// public file byte-for-byte (the bootstrap re-derive contract:
    /// cmp-equal, no trailing newline).
    #[test]
    fn derive_pub_matches_keygen_output() {
        let dir = tempfile::tempdir().unwrap();
        let sec_path = dir.path().join("key.sec");
        let pub_path = dir.path().join("key.pub");
        run_new(NewArgs {
            name: "rio-test-1".into(),
            secret_key_file: sec_path.clone(),
            public_key_file: pub_path.clone(),
        })
        .unwrap();

        // Transport fidelity: feed the secret with a trailing newline
        // (what `aws --output text` emits) — output must still be the
        // canonical newline-free entry.
        let mut input = std::fs::read(&sec_path).unwrap();
        input.push(b'\n');
        let mut out = Vec::new();
        run_derive_pub(&mut input.as_slice(), &mut out).unwrap();
        assert_eq!(
            out,
            std::fs::read(&pub_path).unwrap(),
            "derive-pub output is byte-identical to the keygen public file"
        );
        assert!(!out.ends_with(b"\n"));
    }

    /// bug_023 regression: a 32-byte seed-only secret entry derives the
    /// PUBLIC key — the output never contains the seed in any window.
    #[test]
    fn derive_pub_seed_only_never_emits_seed() {
        let b64 = base64::engine::general_purpose::STANDARD;
        let seed = [0x5Au8; 32];
        let seed_b64 = b64.encode(seed);
        let entry = format!("rio-byo:{seed_b64}");

        let mut out = Vec::new();
        run_derive_pub(&mut entry.as_bytes(), &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();

        let expected = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
        assert_eq!(out, format!("rio-byo:{}", b64.encode(expected)));
        assert!(
            !out.contains(&seed_b64),
            "published entry must not contain the seed's base64"
        );
    }

    /// The 64-byte stale-tail arm: derive-pub refuses an internally
    /// inconsistent expanded entry instead of publishing either side.
    #[test]
    fn derive_pub_refuses_stale_tail() {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut expanded = [0u8; 64];
        expanded[..32].copy_from_slice(&[0x33u8; 32]);
        expanded[32..].copy_from_slice(&[0x44u8; 32]); // not derive(seed)
        let entry = format!("rio-t:{}", b64.encode(expanded));

        let mut out = Vec::new();
        let err = run_derive_pub(&mut entry.as_bytes(), &mut out).unwrap_err();
        assert!(
            format!("{err:#}").contains("internally inconsistent"),
            "error names the inconsistency: {err:#}"
        );
        assert!(out.is_empty(), "nothing published on refusal");
    }
}
