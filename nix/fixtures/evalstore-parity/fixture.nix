# Local parity fixture for the rio:// eval-store plugin — NO nixpkgs, NO
# network. Exercises every eval-time store op the plugin intercepts:
#   - builtins.toFile          → addToStoreFromDump (Text method)
#   - ./src-dir reference      → addToStore(SourcePath) → NAR dump
#   - derivation { … }         → writeDerivation (ATerm + drv JSON capture)
#   - __structuredAttrs        → the highest drvPath-divergence-risk shape
#   - plain → structured edge  → inputDrvs in the second derivation
#
# `system` is a constant: nothing is built, and both parity runs must
# evaluate identical terms on any host.
let
  builderScript = builtins.toFile "builder.sh" ''
    echo hello > "$out"
  '';

  src = ./src-dir;

  plain = derivation {
    name = "rio-parity-plain";
    system = "x86_64-linux";
    builder = "/bin/sh";
    args = [
      "-e"
      builderScript
    ];
    inherit src;
    someVar = "with chars needing escapes: \" \\ \n \${notInterpolated}";
  };

  structured = derivation {
    name = "rio-parity-structured";
    system = "x86_64-linux";
    builder = "/bin/sh";
    args = [
      "-e"
      builderScript
    ];
    __structuredAttrs = true;
    inherit src;
    depends = plain;
    nested = {
      ints = [
        1
        2
        3
      ];
      attrs.deep = "value";
      bool = true;
      nullValue = null;
    };
    listOfPaths = [ src ];
  };
in
{
  paths = {
    plain = plain.drvPath;
    structured = structured.drvPath;
    source = "${src}";
    toFile = "${builderScript}";
  };
}
