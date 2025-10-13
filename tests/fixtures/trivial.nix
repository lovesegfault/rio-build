# Trivial derivation for fast testing
with import <nixpkgs> { };

runCommandNoCC "rio-test-trivial" { } ''
  echo "Building trivial test"
  mkdir -p $out
  echo "test output" > $out/result.txt
''
