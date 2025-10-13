# Derivation that always fails (for error testing)
with import <nixpkgs> { };

runCommandNoCC "rio-test-failing" { } ''
  echo "This build will fail" >&2
  exit 1
''
