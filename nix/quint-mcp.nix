# Hermetic packaging of the quint-llm-kit MCP servers for dev-shell use.
#
# Two servers, both wired through the project-scoped .mcp.json so any
# Claude Code session started inside the dev shell gets them without a
# per-developer registration step (the same mechanism also serves the
# tracey MCP server):
#
#   quint-kb-mcp   — search/browse the kit's curated Quint knowledge base
#                    (builtin-operator docs, pattern cards, worked example
#                    specs, templates, posts) over stdio MCP.
#   quint-lsp-mcp  — mcp-language-server bridging
#                    @informalsystems/quint-language-server, giving
#                    LSP-grade diagnostics/navigation for the .qnt models.
#
# Upstream runs both impurely: a first-launch `npm run build && npm run
# setup` in ~/.cache, an embedding-model download from HuggingFace at
# setup AND query time, and the LSP via `npx`. Here everything is done at
# build time instead — the TypeScript build, the search-index generation,
# and the embedding model are vendored into the store — so both servers
# run offline from read-only store paths. The KB server resolves its data
# relative to its own install location (import.meta.url), which is what
# makes the read-only layout workable.
#
# Bumping the kit: update kitRev + its hash + npmDepsHash together (the
# npmDepsHash belongs to that rev's mcp-servers/kb/package-lock.json).
# Bumping the language server: edit nix/quint-mcp/lsp-shim/package.json,
# regenerate its package-lock.json (`npm install --package-lock-only`),
# and update lspShimNpmDepsHash.
{
  lib,
  stdenv,
  fetchFromGitHub,
  fetchurl,
  buildNpmPackage,
  nodejs,
  python3,
  pkg-config,
  vips,
  makeWrapper,
  writeShellApplication,
  autoPatchelfHook,
  linkFarm,
  mcp-language-server,
}:
let
  kitRev = "520e563613c25abac4c631ea2aa3181ba76ba193";
  kitSrc = fetchFromGitHub {
    owner = "informalsystems";
    repo = "quint-llm-kit";
    rev = kitRev;
    hash = "sha256-aFgG23IHCKYSMgtxcEthf1bSRm09SSM9776sldX8JFg=";
  };

  # The sentence-transformer the KB's semantic search embeds with,
  # vendored from a pinned HuggingFace revision so neither the index
  # build nor the running server ever reaches the network. The layout
  # mirrors transformers.js' localModelPath convention:
  # <models>/<modelId>/<file>.
  miniLmRev = "751bff37182d3f1213fa05d7196b954e230abad9";
  miniLmFile =
    name: hash:
    fetchurl {
      url = "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/${miniLmRev}/${name}";
      inherit hash;
      name = builtins.baseNameOf name;
    };
  miniLmModel = linkFarm "all-MiniLM-L6-v2-vendored" [
    {
      name = "Xenova/all-MiniLM-L6-v2/config.json";
      path = miniLmFile "config.json" "sha256-cTUUn3z/oaVzRmxuTYQj7XO2L9IzLFdb9zig0DP3Dfc=";
    }
    {
      name = "Xenova/all-MiniLM-L6-v2/tokenizer.json";
      path = miniLmFile "tokenizer.json" "sha256-2g55kzue1ReYo64niT08X6SiARJs73VYYpbfm00sYqA=";
    }
    {
      name = "Xenova/all-MiniLM-L6-v2/tokenizer_config.json";
      path = miniLmFile "tokenizer_config.json" "sha256-kmHn15tEyBlcHK2itFPlWwCuuB6QemZkl0tNd3YXKrM=";
    }
    {
      name = "Xenova/all-MiniLM-L6-v2/special_tokens_map.json";
      path = miniLmFile "special_tokens_map.json" "sha256-ttNGvjZqfR1IMy28n987+JYLXYeVIrd5ndulnnYjfuM=";
    }
    {
      name = "Xenova/all-MiniLM-L6-v2/onnx/model_quantized.onnx";
      path = miniLmFile "onnx/model_quantized.onnx" "sha256-r9tvGg5FtxXQu5sRdy8DLDmbq9I7/DH+0cFwr8hIvbE=";
    }
  ];

  quint-kb-mcp = buildNpmPackage {
    pname = "quint-kb-mcp";
    version = "0.1.0-${builtins.substring 0 7 kitRev}";

    src = kitSrc;
    sourceRoot = "${kitSrc.name}/mcp-servers/kb";

    npmDepsHash = "sha256-wxugLBO1QtSgq0ap9rmg9heRX0xjpHjqekRY2X4f25A=";

    nativeBuildInputs = [
      python3 # node-gyp builds (hnswlib-node, sharp)
      pkg-config
      makeWrapper
      autoPatchelfHook
    ];
    buildInputs = [
      vips # sharp builds against the system libvips (no prebuilt download)
      stdenv.cc.cc.lib # libstdc++ for the prebuilt onnxruntime / built .node addons
    ];

    # The default buildPhase runs `npm run build` (tsc → dist/). After
    # that, stage the vendored model where the indexers expect it and
    # generate every search index offline. LD_LIBRARY_PATH lets the
    # prebuilt onnxruntime native backend load inside the sandbox; if it
    # ever fails to, transformers.js falls back to its WASM backend and
    # the indices still build (slower).
    postBuild = ''
      mkdir -p data
      cp -r --no-preserve=mode,ownership ${miniLmModel} data/models
      export HOME="$TMPDIR"
      export LD_LIBRARY_PATH=${
        lib.makeLibraryPath [
          stdenv.cc.cc.lib
          vips
        ]
      }
      npm run setup
    '';

    # The upstream package has no bin entry (it is started as `node
    # dist/server.js`), so the stock npm-install phase has nothing to
    # install; lay out the runtime tree by hand instead.
    installPhase = ''
      runHook preInstall

      # Drop build-only tooling (tsx, typescript, jest) from the runtime
      # closure; the server only needs the compiled dist/ + indices.
      npm prune --omit=dev --ignore-scripts

      # Foreign-platform prebuilt binaries would only bloat the closure
      # and trip autoPatchelf.
      rm -rf node_modules/onnxruntime-node/bin/napi-v3/darwin \
             node_modules/onnxruntime-node/bin/napi-v3/win32 \
             node_modules/onnxruntime-node/bin/napi-v3/linux/arm64

      mkdir -p $out/lib/quint-kb-mcp $out/bin
      cp -r dist kb data node_modules package.json $out/lib/quint-kb-mcp/
      makeWrapper ${nodejs}/bin/node $out/bin/quint-kb-mcp \
        --add-flags $out/lib/quint-kb-mcp/dist/server.js

      runHook postInstall
    '';

    # GPU/execution-provider stubs inside onnxruntime reference libraries
    # we do not ship (CUDA et al.); the CPU path never dlopens them.
    autoPatchelfIgnoreMissingDeps = true;

    meta = {
      description = "MCP server over the quint-llm-kit knowledge base (offline build: vendored embedding model + indices)";
      homepage = "https://github.com/informalsystems/quint-llm-kit";
      license = lib.licenses.mit;
      mainProgram = "quint-kb-mcp";
    };
  };

  quint-language-server = buildNpmPackage {
    pname = "quint-language-server";
    version = "0.19.0";

    # A two-file shim package (package.json + package-lock.json) whose
    # only dependency is the published language server — buildNpmPackage
    # needs a lockfile, and the npm tarball does not ship one.
    src = ./quint-mcp/lsp-shim;
    npmDepsHash = "sha256-bH9wnyMDSBhJSar0pd8MKLyqKuPrP7bT98YYZIAZ2sw=";

    dontNpmBuild = true;
    nativeBuildInputs = [ makeWrapper ];

    installPhase = ''
      runHook preInstall
      mkdir -p $out/lib $out/bin
      cp -r node_modules $out/lib/node_modules
      makeWrapper ${nodejs}/bin/node $out/bin/quint-language-server \
        --add-flags $out/lib/node_modules/@informalsystems/quint-language-server/out/src/server.js
      runHook postInstall
    '';

    meta = {
      description = "Quint language server (pinned npm release, hermetic install)";
      homepage = "https://github.com/informalsystems/quint";
      license = lib.licenses.asl20;
      mainProgram = "quint-language-server";
    };
  };

  quint-lsp-mcp = writeShellApplication {
    name = "quint-lsp-mcp";
    runtimeInputs = [
      mcp-language-server
      quint-language-server
    ];
    text = ''
      # LSP-over-MCP for the Quint models: mcp-language-server bridging
      # quint-language-server. The workspace defaults to the model
      # directory of the repo the session was started in; override with
      # RIO_QUINT_LSP_WORKSPACE for out-of-tree experiments.
      exec mcp-language-server \
        --workspace "''${RIO_QUINT_LSP_WORKSPACE:-$PWD/docs/spec/models}" \
        --lsp quint-language-server -- --stdio
    '';
  };
in
{
  inherit quint-kb-mcp quint-language-server quint-lsp-mcp;
}
