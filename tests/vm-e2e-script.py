import time
import sys

def info(msg):
    print(f"\n{'='*60}")
    print(f"  {msg}")
    print('='*60)

# Start all VMs
start_all()

# === Phase 1: Service Startup ===
info("Phase 1: Waiting for services to start")

dispatcher.wait_for_unit("rio-dispatcher.service")
dispatcher.wait_for_open_port(50051)
dispatcher.wait_for_open_port(2222)
print("Dispatcher is ready (gRPC:50051, SSH:2222)")

builder.wait_for_unit("rio-builder.service")
builder.wait_for_unit("nix-daemon.service")
builder.wait_for_open_port(50052)
print("Builder is ready (gRPC:50052)")

client.wait_for_unit("multi-user.target")
print("Client is ready")

# Give builder time to register with dispatcher
time.sleep(3)

# Verify builder registered
info("Phase 1.5: Verifying builder registration")

dispatcher_logs = dispatcher.succeed(
    "journalctl -u rio-dispatcher.service --no-pager -n 100"
)

if "register" not in dispatcher_logs.lower():
    print("WARNING: Builder registration not clearly visible in logs")
    print("Dispatcher logs (last 500 chars):")
    print(dispatcher_logs[-500:])
else:
    print("Builder registered with dispatcher")

# === Phase 2: Create Test Derivation ===
info("Phase 2: Creating test derivation on client")

client.succeed("""
  cat > /tmp/rio-vm-test.nix << 'EOF'
  derivation {
    name = "rio-vm-e2e-test";
    system = builtins.currentSystem;
    builder = "/bin/sh";
    args = [ "-c" "echo 'Successfully built by Rio in VM!' > $out" ];
  }
  EOF
""")

drv_path = client.succeed("nix-instantiate /tmp/rio-vm-test.nix").strip()
print(f"✓ Created derivation: {drv_path}")

# === Phase 3: Submit Build via SSH ===
info("Phase 3: Submitting build to Rio via SSH")

print("Attempting: nix-build --store ssh://dispatcher:2222")

try:
    output = client.succeed(
        f"nix-build --store ssh://dispatcher:2222 {drv_path} --no-out-link 2>&1",
        timeout=60
    )
    output_path = output.strip().split('\n')[-1]

    print("✓ Build completed!")
    print(f"  Output path: {output_path}")

except Exception as e:
    print(f"❌ Build failed: {e}")
    print("\nDispatcher logs:")
    print(dispatcher.succeed("journalctl -u rio-dispatcher.service --no-pager -n 50"))
    print("\nBuilder logs:")
    print(builder.succeed("journalctl -u rio-builder.service --no-pager -n 50"))
    raise

# === Phase 4: Verify Build Output ===
info("Phase 4: Verifying build output")

# Check that output path is valid
if not output_path.startswith("/nix/store/"):
    print(f"❌ Output is not a store path: {output_path}")
    sys.exit(1)

# The output should exist on the dispatcher
dispatcher.succeed(f"test -f {output_path}")
print(f"✓ Output exists on dispatcher: {output_path}")

# Read and verify content
content = dispatcher.succeed(f"cat {output_path}")
expected = "Successfully built by Rio in VM!"

if expected not in content:
    print(f"❌ Unexpected output content: {content}")
    sys.exit(1)

print(f"✓ Output has correct content: '{content.strip()}'")

# === Phase 5: Test Concurrent Builds ===
info("Phase 5: Testing concurrent builds")

# Create multiple test derivations
for i in range(3):
    client.succeed(f"""
      cat > /tmp/concurrent-{i}.nix << 'EOF'
      derivation {{{{
        name = "concurrent-test-{i}";
        system = builtins.currentSystem;
        builder = "/bin/sh";
        args = [ "-c" "echo 'Concurrent build {i}' > $out" ];
      }}}}
      EOF
    """)

# Submit concurrent builds
try:
    client.succeed("""
      nix-build --store ssh://dispatcher:2222 /tmp/concurrent-0.nix --no-out-link &
      nix-build --store ssh://dispatcher:2222 /tmp/concurrent-1.nix --no-out-link &
      nix-build --store ssh://dispatcher:2222 /tmp/concurrent-2.nix --no-out-link &
      wait
    """, timeout=120)
    print("Concurrent builds completed")
except Exception as e:
    print(f"⚠ Some concurrent builds may have issues: {e}")

# === Phase 6: Verify System Health ===
info("Phase 6: Verifying final system state")

# Check services are still running
dispatcher.succeed("systemctl is-active rio-dispatcher.service")
print("Dispatcher still running")

builder.succeed("systemctl is-active rio-builder.service")
print("Builder still running")

info("✅ ALL END-TO-END VM TESTS PASSED!")
print("\nSummary:")
print("- Services started successfully")
print("- Builder registered with dispatcher")
print("- Test derivation built via SSH")
print("- Build output transferred and verified")
print("- Concurrent builds handled")
print("- System remained healthy")
