#!/usr/bin/env python3
"""Mock IMDSv2 for nix/tests/nixos-node.nix.

Serves just enough of the EC2 instance-metadata API for `nodeadm init
--skip run --daemon kubelet` to complete without AWS. Endpoints derived
from awslabs/amazon-eks-ami nodeadm at the rev pinned in nix/pins.nix
(aws-sdk-go-v2/feature/ec2/imds path constants + nodeadm/internal/aws/
imds/imds.go IMDSProperty values). The user-data NodeConfig sets
featureGates.InstanceIdNodeName=true so nodeadm skips the EC2
DescribeInstances call for PrivateDnsName (which would need real AWS
credentials and a real EC2 endpoint).

Unmatched paths 404 and log to stderr — visible in the VM test output —
so a future nodeadm bump that adds an endpoint surfaces the gap instead
of silently retrying.
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

TOKEN = "mock-imds-token"
MAC = "06:00:00:00:00:01"
PRIVATE_IP = "10.0.0.1"

INSTANCE_IDENTITY = json.dumps({
    "accountId": "000000000000",
    "architecture": "x86_64",
    "availabilityZone": "us-west-2a",
    "imageId": "ami-00000000000000000",
    "instanceId": "i-0123456789abcdef0",
    "instanceType": "m6i.large",
    "pendingTime": "2026-01-01T00:00:00Z",
    "privateIp": PRIVATE_IP,
    "region": "us-west-2",
    "version": "2017-09-30",
})

# Karpenter-shaped MIME-multipart NodeConfig (one part). nodeadm's
# ParseMaybeMultipart() accepts bare YAML too, but the production path
# is multipart so exercise that.
#
# certificateAuthority is a syntactically-valid throwaway PEM (kubelet
# never validates it against a real apiserver here — kubelet starts,
# fails TLS-bootstrap to 127.0.0.1:6443, restarts; the test only asserts
# kubelet.service forked). cidr is IPv4 → nodeadm's getNodeIp() reads
# meta-data/local-ipv4 (not the macs/.../ipv6s path).
USER_DATA = b"""\
MIME-Version: 1.0
Content-Type: multipart/mixed; boundary="BOUNDARY"

--BOUNDARY
Content-Type: application/node.eks.aws

---
apiVersion: node.eks.aws/v1alpha1
kind: NodeConfig
spec:
  cluster:
    name: rio-vmtest
    apiServerEndpoint: https://127.0.0.1:6443
    certificateAuthority: LS0tLS1CRUdJTiBDRVJUSUZJQ0FURS0tLS0tCk1JSUJpVENDQVRPZ0F3SUJBZ0lVZERQd2FHaDlXU0l2NEk4Z2JTVkQ1aXN2THJzd0RRWUpLb1pJaHZjTkFRRUwKQlFBd0dERVdNQlFHQTFVRUF3d05jbWx2TFhadGRHVnpkQzFqWVRBZ0Z3MHlOakEwTURjd05EVTBORGRhR0E4eQpNVEkyTURNeE5EQTBOVFEwTjFvd0dERVdNQlFHQTFVRUF3d05jbWx2TFhadGRHVnpkQzFqWVRCY01BMEdDU3FHClNJYjNEUUVCQVFVQUEwc0FNRWdDUVFDbEVsZGJCZEhidVkxTDllTGF2MklObWlIdlBsY09NZHA1RFNReDVEMk4KbE91T0l5MXl1L2xZQllXcXJ0c2xiTkpoR3VVT2pLbnJnYlcyajZocUYzNWpBZ01CQUFHalV6QlJNQjBHQTFVZApEZ1FXQkJUbklYdnk0cng2bUlTRzFlKzVtdkQ3bmYrMTZ6QWZCZ05WSFNNRUdEQVdnQlRuSVh2eTRyeDZtSVNHCjFlKzVtdkQ3bmYrMTZ6QVBCZ05WSFJNQkFmOEVCVEFEQVFIL01BMEdDU3FHU0liM0RRRUJDd1VBQTBFQUxBZkgKUDlYcWlEd1hzUzFiMUNDN3pNLzVoRzJLcFU3TTBPZG8zNERoeWtBcjBPTDhnTGc3ajE0MEZyRzFkamhHV2F6bQpPYWgzSnIrNG90dlFJSW9SWGc9PQotLS0tLUVORCBDRVJUSUZJQ0FURS0tLS0tCg==
    cidr: 10.100.0.0/16
  kubelet:
    config:
      clusterDNS: [10.100.0.10]
      maxPods: 110
    flags:
      - --node-labels=rio.build/vmtest=true
  featureGates:
    InstanceIdNodeName: true

--BOUNDARY--
"""

# nodeadm init proper only reads `mac` and `local-ipv4` from meta-data/.
# The macs/ subtree + services/domain + placement/* are hit by
# nodeadm-internal (udev/boot-book) and the aws-sdk credential chain
# respectively; mocked so a 404 on a future code-path move doesn't
# wedge the unit-level Restart=on-failure loop.
META_DATA = {
    "mac": MAC,
    "local-ipv4": PRIVATE_IP,
    "services/domain": "amazonaws.com",
    "placement/region": "us-west-2",
    "placement/availability-zone": "us-west-2a",
    "instance-id": "i-0123456789abcdef0",
    "instance-type": "m6i.large",
    "network/interfaces/macs/": MAC + "/",
    f"network/interfaces/macs/{MAC}/device-number": "0",
    f"network/interfaces/macs/{MAC}/network-card": "0",
    f"network/interfaces/macs/{MAC}/local-ipv4s": PRIVATE_IP,
    f"network/interfaces/macs/{MAC}/vpc-ipv4-cidr-block": "10.0.0.0/16",
    f"network/interfaces/macs/{MAC}/subnet-ipv4-cidr-block": "10.0.0.0/24",
}


class Handler(BaseHTTPRequestHandler):
    def _send(self, body, *, status=200, headers=()):
        if isinstance(body, str):
            body = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        for k, v in headers:
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    # IMDSv2 token. aws-sdk-go-v2 requires the TTL response header to be
    # a parseable int (api_op_GetToken.go) — without it the SDK treats
    # the token as invalid and falls back to v1, which nodeadm's client
    # may have disabled.
    def do_PUT(self):
        if self.path == "/latest/api/token":
            self._send(
                TOKEN,
                headers=[("X-Aws-Ec2-Metadata-Token-Ttl-Seconds", "21600")],
            )
        else:
            self._unknown()

    def do_GET(self):
        if self.path == "/latest/user-data":
            self._send(USER_DATA)
        elif self.path == "/latest/dynamic/instance-identity/document":
            self._send(INSTANCE_IDENTITY)
        elif self.path.startswith("/latest/meta-data/"):
            key = self.path.removeprefix("/latest/meta-data/")
            if key in META_DATA:
                self._send(META_DATA[key])
            else:
                self._unknown()
        else:
            self._unknown()

    def _unknown(self):
        print(f"mock-imds: UNHANDLED {self.command} {self.path}", file=sys.stderr, flush=True)
        self._send("not found", status=404)

    def log_message(self, fmt, *args):
        sys.stderr.write(f"mock-imds: {self.address_string()} {fmt % args}\n")


if __name__ == "__main__":
    HTTPServer(("169.254.169.254", 80), Handler).serve_forever()
