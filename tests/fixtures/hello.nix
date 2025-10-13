# Simple test fixture - trivial package for fast builds
with import <nixpkgs> { };

runCommandNoCC "rio-test-hello" { } ''
  echo "Building test package"
  echo "Hello from Rio!" > $out
''
