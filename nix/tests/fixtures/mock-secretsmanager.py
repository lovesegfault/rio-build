# Stdlib-only Secrets Manager mock for the prod-parity VM fixture.
#
# The bootstrap Job runs the REAL awscli2 against this endpoint
# (AWS_ENDPOINT_URL + dummy creds in the Job env) so the script's
# exception discrimination is exercised against GENUINE wire shapes:
# the CLI maps the JSON `__type` below to the exact
# "An error occurred (ResourceNotFoundException) ..." stderr the
# script's secret_state() classifies. Before this mock existed the
# fixture had no credentials at all, every describe-secret died with
# NoCredentials, and the old raw `if aws describe` guard collapsed
# that into "missing" — the scenario asserted "describe returned
# not-found" while actually exercising "describe failed somehow"
# (round-17 composition find; the round-17 fail-closed probe exposed
# it by correctly refusing to guess).
#
# Faithful on the axes the script discriminates (per the
# bootstrap-idempotent harness philosophy: control flow and bytes,
# not AWS semantics): __type strings, DeletedDate presence,
# SecretString/SecretBinary round-trip, create-vs-put existence
# semantics. Everything else is minimal.
import json
import socket
from http.server import BaseHTTPRequestHandler, HTTPServer

SECRETS = {}  # name -> {"SecretString": str} | {"SecretBinary": b64-str}


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(n) or b"{}")
        op = self.headers.get("X-Amz-Target", "").split(".")[-1]
        sid = body.get("SecretId") or body.get("Name")

        def send(code, obj):
            data = json.dumps(obj).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/x-amz-json-1.1")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def err(t, msg):
            send(400, {"__type": t, "message": msg})

        def payload(b):
            return {k: b[k] for k in ("SecretString", "SecretBinary") if k in b}

        arn = f"arn:aws:secretsmanager:vm-test:000000000000:secret:{sid}-mock00"
        if op == "DescribeSecret":
            if sid in SECRETS:
                # No DeletedDate field: a live secret. The script's
                # `--query DeletedDate --output text` prints "None".
                send(200, {"Name": sid, "ARN": arn})
            else:
                err(
                    "ResourceNotFoundException",
                    "Secrets Manager can't find the specified secret.",
                )
        elif op == "CreateSecret":
            if sid in SECRETS:
                err(
                    "ResourceExistsException",
                    f"The operation failed because the secret {sid} "
                    f"already exists.",
                )
            else:
                SECRETS[sid] = payload(body)
                send(200, {"Name": sid, "ARN": arn})
        elif op == "PutSecretValue":
            if sid in SECRETS:
                SECRETS[sid] = payload(body)
                send(200, {"Name": sid, "ARN": arn})
            else:
                err(
                    "ResourceNotFoundException",
                    "Secrets Manager can't find the specified secret.",
                )
        elif op == "GetSecretValue":
            if sid in SECRETS:
                send(200, {"Name": sid, "ARN": arn, **SECRETS[sid]})
            else:
                err(
                    "ResourceNotFoundException",
                    "Secrets Manager can't find the specified secret.",
                )
        else:
            err("InvalidRequestException", f"unmocked operation {op}")

    def log_message(self, fmt, *args):  # quiet; journald gets enough
        pass


class V6Server(HTTPServer):
    address_family = socket.AF_INET6


if __name__ == "__main__":
    V6Server(("::", 5000), Handler).serve_forever()
