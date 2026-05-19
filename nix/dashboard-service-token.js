import crypto from 'crypto';
import fs from 'fs';

// Key file mounted from the rio-service-hmac Secret. Read once at
// worker init (njs caches the module-level binding); a key rotation
// requires a pod restart, same as every other rio component.
// Missing mount → empty string → scheduler in dev mode (verifier=
// None) accepts anything; with verifier set, the empty-key HMAC is
// rejected and the request fails closed (PermissionDenied).
let key;
try {
  const raw = fs.readFileSync('/etc/rio/hmac/service-hmac.key');
  // Mirror rio-auth load_key EXACTLY: strip one trailing CRLF or
  // LF at the byte level so a Secret created with `echo` (no -n)
  // or a YAML `|` block scalar still verifies. NOT .toString()
  // .replace() — the key is raw `openssl rand 32` bytes; a UTF-8
  // round-trip would corrupt it. lib/hmac-keys.nix appends LF to
  // the test fixture so vm-dashboard-k3s breaks if this drifts.
  let n = raw.length;
  if (n >= 2 && raw[n - 2] === 0x0d && raw[n - 1] === 0x0a) {
    n -= 2;
  } else if (n >= 1 && raw[n - 1] === 0x0a) {
    n -= 1;
  }
  key = raw.slice(0, n);
} catch (e) {
  key = Buffer.from("");
}

function b64url(buf) {
  return buf
    .toString('base64')
    .replace(/\+/g, '-')
    .replace(/\//g, '_')
    .replace(/=+$/, "");
}

function token(r) {
  const claims = JSON.stringify({
    caller: 'rio-dashboard',
    expiry_unix: Math.floor(Date.now() / 1000) + 60,
  });
  const body = Buffer.from(claims);
  const tag = crypto.createHmac('sha256', key).update(body).digest();
  return b64url(body) + '.' + b64url(tag);
}

export default { token };
