/* rio:// store plugin — thin C++ shim over the rio-evalstore Rust core.
 *
 * Implements the nix::Store vtable (nix 2.34, the flake's `inputs.nix`
 * pin) and marshals every operation through the extern "C" surface in
 * rio_evalstore.h. Shape mirrors nix's own dummy-store.cc, especially the
 * whole-store-view accessor.
 *
 * Boundary discipline (ADR-024):
 *  - Callbacks never unwind across the FFI boundary: every trampoline
 *    catches everything, stashes the exception, returns nonzero; the
 *    original exception is rethrown after the Rust call returns.
 *  - Rust errors arrive as malloc'd messages and become nix::Error
 *    (nix::Unsupported for rc 2 — foreign paths / M1 functionality).
 *  - No threads, no global state beyond store registration: dlopen runs
 *    only the RegisterStoreImplementation constructor (fork-safety rule
 *    for nix-eval-jobs fork-no-exec workers).
 */

#include "nix/store/store-registration.hh"
#include "nix/store/store-api.hh"
#include "nix/store/derivations.hh"
#include "nix/store/content-address.hh"
#include "nix/store/globals.hh"
#include "nix/store/keys.hh"
#include "nix/store/path-info.hh"
#include "nix/store/realisation.hh"
#include "nix/util/callback.hh"
#include "nix/util/canon-path.hh"
#include "nix/util/serialise.hh"
#include "nix/util/source-accessor.hh"
#include "nix/util/source-path.hh"
#include "nix/fetchers/filtering-source-accessor.hh"

#include <nlohmann/json.hpp>

#include <cstring>
#include <string>
#include <variant>

#include "rio_evalstore.h"
#include "rio_shim.hh"

/* IFD relay hook (ADR-024 P3b). Set by the rio-eval worker via
 * rio_shim_set_ifd_handler(); nullptr in the plugin, where buildPaths
 * stays Unsupported. The handler relays the import-from-derivation to
 * the coordinator and blocks until it resolves (outputs imported into
 * the store before it returns). */
static rio_shim_ifd_fn rioIfdFn = nullptr;
static void * rioIfdCtx = nullptr;

extern "C" void rio_shim_set_ifd_handler(rio_shim_ifd_fn fn, void * ctx)
{
    rioIfdFn = fn;
    rioIfdCtx = ctx;
}

namespace nix {

namespace {

/* RAII for strings allocated by the Rust side. */
struct RioStr
{
    char * p = nullptr;

    RioStr() = default;
    RioStr(const RioStr &) = delete;
    RioStr & operator=(const RioStr &) = delete;

    ~RioStr()
    {
        rio_string_free(p);
    }

    bool has() const
    {
        return p != nullptr;
    }

    std::string str() const
    {
        return p ? std::string(p) : std::string();
    }
};

/* RAII for byte buffers allocated by the Rust side (rio_read_directory). */
struct RioBytes
{
    unsigned char * p = nullptr;
    size_t len = 0;

    RioBytes() = default;
    RioBytes(const RioBytes &) = delete;
    RioBytes & operator=(const RioBytes &) = delete;

    ~RioBytes()
    {
        rio_bytes_free(p, len);
    }
};

/* Source -> rio_read_cb trampoline state. */
struct SourceCtx
{
    Source & source;
    std::exception_ptr ex;
};

extern "C" int rioSourceRead(void * ctx, unsigned char * buf, size_t cap, size_t * nRead) noexcept
{
    auto * c = static_cast<SourceCtx *>(ctx);
    try {
        *nRead = c->source.read(reinterpret_cast<char *>(buf), cap);
        return 0;
    } catch (EndOfFile &) {
        *nRead = 0;
        return 0;
    } catch (...) {
        c->ex = std::current_exception();
        return 1;
    }
}

/* Sink -> rio_write_cb trampoline state. */
struct SinkCtx
{
    Sink & sink;
    std::exception_ptr ex;
};

extern "C" int rioSinkWrite(void * ctx, const unsigned char * data, size_t len) noexcept
{
    auto * c = static_cast<SinkCtx *>(ctx);
    try {
        c->sink({reinterpret_cast<const char *>(data), len});
        return 0;
    } catch (...) {
        c->ex = std::current_exception();
        return 1;
    }
}

/* std::string -> rio_write_cb trampoline (collecting small reads). */
struct StringSinkCtx
{
    std::string out;
    std::exception_ptr ex;
};

extern "C" int rioStringWrite(void * ctx, const unsigned char * data, size_t len) noexcept
{
    auto * c = static_cast<StringSinkCtx *>(ctx);
    try {
        c->out.append(reinterpret_cast<const char *>(data), len);
        return 0;
    } catch (...) {
        c->ex = std::current_exception();
        return 1;
    }
}

/* Cross-check path callback trampoline. */
struct PathCbCtx
{
    fun<std::string(const nlohmann::json &)> compute;
    std::exception_ptr ex;
};

extern "C" int rioPathCompute(void * ctx, const char * hashesJson, char * outPath, size_t outCap) noexcept
{
    auto * c = static_cast<PathCbCtx *>(ctx);
    try {
        auto path = c->compute(nlohmann::json::parse(hashesJson));
        if (path.size() + 1 > outCap)
            return 1;
        std::memcpy(outPath, path.c_str(), path.size() + 1);
        return 0;
    } catch (...) {
        c->ex = std::current_exception();
        return 1;
    }
}

/* Map a Rust return code to control flow. Stashed callback exceptions
 * take precedence — they are the original cause. */
void checkRc(int rc, const RioStr & err, std::string_view op, std::exception_ptr stashed = nullptr)
{
    if (stashed)
        std::rethrow_exception(stashed);
    if (rc == RIO_OK)
        return;
    auto msg = err.has() ? err.str() : std::string("unknown rio-evalstore failure");
    if (rc == RIO_UNSUPPORTED)
        throw Unsupported("'%s' in rio store: %s", std::string(op), msg);
    throw Error("'%s' in rio store: %s", std::string(op), msg);
}

int caMethodFor(ContentAddressMethod method, std::string_view op)
{
    switch (method.raw) {
    case ContentAddressMethod::Raw::Flat:
        return RIO_CA_FLAT;
    case ContentAddressMethod::Raw::NixArchive:
        return RIO_CA_NAR;
    case ContentAddressMethod::Raw::Text:
        return RIO_CA_TEXT;
    default:
        throw Unsupported("'%s' in rio store: content-address method '%s'", std::string(op), method.render());
    }
}

} // namespace

struct RioStoreConfig : public std::enable_shared_from_this<RioStoreConfig>, virtual StoreConfig
{
    RioStoreConfig(const Params & params)
        : StoreConfig(params)
    {
    }

    RioStoreConfig(std::string_view scheme, std::string_view authority, const Params & params)
        : RioStoreConfig(params)
    {
        if (!authority.empty())
            throw UsageError("`%s` store URIs must not contain an authority part %s", scheme, authority);
    }

    Setting<std::string> casDir{
        this,
        "",
        "cas",
        R"(
          Path of the client CAS directory backing this store.
          Defaults to `$XDG_CACHE_HOME/rio/cas`.
        )"};

    static const std::string name()
    {
        return "rio Eval Store";
    }

    static std::string doc()
    {
        return R"(
          Client-side eval store for the rio native build protocol
          (ADR-024). Backed by a persistent content-addressed cache; no
          daemon, no network in M0.
        )";
    }

    static StringSet uriSchemes()
    {
        return {"rio"};
    }

    ref<Store> openStore() const override;

    StoreReference getReference() const override
    {
        return {
            .variant =
                StoreReference::Specified{
                    .scheme = *uriSchemes().begin(),
                },
            .params = getQueryParams(),
        };
    }
};

struct RioStore : virtual Store
{
    using Config = RioStoreConfig;

    ref<const Config> config;

    RioEvalStore * rio = nullptr;

    /* Accessor serving store-object reads from the client CAS. With an
     * empty `fixedBase` it is the whole-store view (first path component
     * selects the object); with `fixedBase` set it scopes one object. */
    struct RioAccessor : SourceAccessor
    {
        RioStore & store;
        std::string fixedBase;

        RioAccessor(RioStore & store, std::string fixedBase = "")
            : store(store)
            , fixedBase(std::move(fixedBase))
        {
        }

        std::pair<std::string, std::string> split(const CanonPath & path)
        {
            if (!fixedBase.empty())
                return {fixedBase, std::string(path.rel())};
            if (path.isRoot())
                return {"", ""};
            std::string base(*path.begin());
            auto rest = path.removePrefix(CanonPath{base});
            return {base, std::string(rest.rel())};
        }

        std::optional<Stat> maybeLstat(const CanonPath & path) override
        {
            auto [base, rel] = split(path);
            if (base.empty())
                return Stat{.type = tDirectory};
            /* Hot path: a plain out-struct — no allocation, no parse
             * (this op alone was ~13% of warm-eval cycles as JSON). */
            RioStat rst{};
            RioStr err;
            int rc = rio_lstat(store.rio, base.c_str(), rel.c_str(), &rst, &err.p);
            checkRc(rc, err, "lstat");
            switch (rst.kind) {
            case RIO_NODE_MISSING:
                return std::nullopt;
            case RIO_NODE_REGULAR: {
                Stat st;
                st.type = tRegular;
                st.fileSize = rst.size;
                st.isExecutable = rst.executable != 0;
                return st;
            }
            case RIO_NODE_SYMLINK:
                return Stat{.type = tSymlink};
            default:
                return Stat{.type = tDirectory};
            }
        }

        bool pathExists(const CanonPath & path) override
        {
            return maybeLstat(path).has_value();
        }

        DirEntries readDirectory(const CanonPath & path) override
        {
            auto [base, rel] = split(path);
            if (base.empty())
                return {};
            RioBytes out;
            RioStr err;
            int rc = rio_read_directory(
                store.rio, base.c_str(), rel.c_str(), &out.p, &out.len, &err.p);
            checkRc(rc, err, "readDirectory");
            /* Flat layout (see rio_evalstore.h): u32 count, then per
             * entry u8 kind, u32 name_len, raw name bytes. Walked, not
             * parsed — produced in-process by the paired Rust core.
             * memcpy because the u32s are unaligned. */
            const unsigned char * q = out.p;
            auto rd32 = [&q] {
                uint32_t v;
                std::memcpy(&v, q, 4);
                q += 4;
                return v;
            };
            uint32_t count = rd32();
            DirEntries entries;
            for (uint32_t i = 0; i < count; i++) {
                unsigned char kind = *q++;
                uint32_t nameLen = rd32();
                std::string name(reinterpret_cast<const char *>(q), nameLen);
                q += nameLen;
                entries.emplace(
                    std::move(name),
                    kind == RIO_NODE_REGULAR  ? tRegular
                    : kind == RIO_NODE_SYMLINK ? tSymlink
                                               : tDirectory);
            }
            return entries;
        }

        std::string readLink(const CanonPath & path) override
        {
            auto [base, rel] = split(path);
            if (base.empty())
                throw Error("'%s' is not a symlink", showPath(path));
            RioStr out, err;
            int rc = rio_read_link(store.rio, base.c_str(), rel.c_str(), &out.p, &err.p);
            checkRc(rc, err, "readLink");
            return out.str();
        }

        void readFile(const CanonPath & path, Sink & sink, fun<void(uint64_t)> sizeCallback) override
        {
            auto [base, rel] = split(path);
            if (base.empty())
                throw Error("'%s' is not a regular file", showPath(path));
            auto st = maybeLstat(path);
            if (!st || st->type != tRegular)
                throw Error("'%s' is not a regular file", showPath(path));
            sizeCallback(st->fileSize.value_or(0));
            SinkCtx sctx{sink};
            RioStr err;
            int rc = rio_read_file(store.rio, base.c_str(), rel.c_str(), &rioSinkWrite, &sctx, &err.p);
            checkRc(rc, err, "readFile", sctx.ex);
        }
    };

    ref<RioAccessor> wholeStoreView;

    RioStore(ref<const Config> config)
        : Store{*config}
        , config(config)
        , wholeStoreView(make_ref<RioAccessor>(*this))
    {
        RioStr err;
        int rc = rio_store_open(config->casDir.get().empty() ? nullptr : config->casDir.get().c_str(), &rio, &err.p);
        checkRc(rc, err, "open");
        wholeStoreView->setPathDisplay(config->storeDir);
    }

    ~RioStore()
    {
        rio_store_free(rio);
    }

    std::string baseName(const StorePath & path)
    {
        return std::string(path.to_string());
    }

    nlohmann::json refsJson(const StorePathSet & references)
    {
        auto arr = nlohmann::json::array();
        for (auto & r : references)
            arr.push_back(printStorePath(r));
        return arr;
    }

    /* -- queries -------------------------------------------------------- */

    bool isValidPathUncached(const StorePath & path) override
    {
        int valid = 0;
        RioStr err;
        int rc = rio_is_valid_path(rio, baseName(path).c_str(), &valid, &err.p);
        checkRc(rc, err, "isValidPath");
        return valid != 0;
    }

    void queryPathInfoUncached(
        const StorePath & path, Callback<std::shared_ptr<const ValidPathInfo>> callback) noexcept override
    {
        try {
            RioStr out, err;
            int rc = rio_query_path_info(rio, baseName(path).c_str(), &out.p, &err.p);
            checkRc(rc, err, "queryPathInfo");
            if (!out.has()) {
                callback(nullptr);
                return;
            }
            auto j = nlohmann::json::parse(out.str());
            auto narHash =
                Hash::parseNonSRIUnprefixed(j.at("nar_hash").get<std::string>(), HashAlgorithm::SHA256);
            auto info = std::make_shared<ValidPathInfo>(path, UnkeyedValidPathInfo(*this, narHash));
            info->narSize = j.at("nar_size").get<uint64_t>();
            for (auto & r : j.at("references"))
                info->references.insert(parseStorePath(r.get<std::string>()));
            if (j.contains("ca"))
                info->ca = ContentAddress::parse(j.at("ca").get<std::string>());
            callback(std::move(info));
        } catch (...) {
            callback.rethrow();
        }
    }

    void queryRealisationUncached(
        const DrvOutput &, Callback<std::shared_ptr<const UnkeyedRealisation>> callback) noexcept override
    {
        /* No realisations in the client CAS (M0 has no builds). */
        callback(nullptr);
    }

    std::optional<StorePath> queryPathFromHashPart(const std::string & hashPart) override
    {
        RioStr out, err;
        int rc = rio_query_path_from_hash_part(rio, hashPart.c_str(), &out.p, &err.p);
        checkRc(rc, err, "queryPathFromHashPart");
        if (!out.has())
            return std::nullopt;
        return parseStorePath(out.str());
    }

    std::optional<TrustedFlag> isTrustedClient() override
    {
        return Trusted;
    }

    /* Substitution into rio:// (flake-input fetchToStore → ensurePath →
     * PathSubstitutionGoal on this store) checks signatures via the
     * destination store's pathInfoIsUntrusted. The base Store impl
     * returns true unconditionally, so without this override every
     * substitute is rejected ("not signed by any of the keys in
     * 'trusted-public-keys'") — including CA -source paths and paths
     * signed by cache.nixos.org's default key. Mirror LocalStore: honor
     * require-sigs and the configured trusted-public-keys. */
    bool pathInfoIsUntrusted(const ValidPathInfo & info) override
    {
        return settings.requireSigs && !info.checkSignatures(*this, getDefaultPublicKeys());
    }

    bool realisationIsUntrusted(const Realisation & realisation) override
    {
        return settings.requireSigs
               && !realisation.checkSignatures(realisation.id, getDefaultPublicKeys());
    }

    /* -- writes --------------------------------------------------------- */

    void addToStore(const ValidPathInfo & info, Source & source, RepairFlag repair, CheckSigsFlag checkSigs) override
    {
        nlohmann::json j{
            {"path", printStorePath(info.path)},
            {"nar_hash", info.narHash.to_string(HashFormat::Base16, false)},
            {"nar_size", info.narSize},
            {"references", refsJson(info.references)},
        };
        if (info.ca)
            j["ca"] = info.ca->render();
        SourceCtx sctx{source};
        RioStr err;
        int rc = rio_add_nar(rio, j.dump().c_str(), &rioSourceRead, &sctx, &err.p);
        checkRc(rc, err, "addToStore", sctx.ex);
    }

    StorePath addToStoreFromDump(
        Source & dump,
        std::string_view name,
        FileSerialisationMethod dumpMethod = FileSerialisationMethod::NixArchive,
        ContentAddressMethod hashMethod = FileIngestionMethod::NixArchive,
        HashAlgorithm hashAlgo = HashAlgorithm::SHA256,
        const StorePathSet & references = StorePathSet(),
        RepairFlag repair = NoRepair) override
    {
        if (hashAlgo != HashAlgorithm::SHA256)
            throw Unsupported("rio store only supports sha256 content addressing in M0");
        int caM = caMethodFor(hashMethod, "addToStoreFromDump");
        int dumpM = dumpMethod == FileSerialisationMethod::Flat ? RIO_DUMP_FLAT : RIO_DUMP_NAR;

        /* The cross-check: nix's own path computation for the hashes the
         * Rust core measured. The core compares this against rio-nix's
         * computation and hard-fails on divergence. */
        PathCbCtx pctx{
            .compute =
                [&](const nlohmann::json & h) {
                    auto narHash = Hash::parseNonSRIUnprefixed(
                        h.at("nar_sha256").get<std::string>(), HashAlgorithm::SHA256);
                    auto contentHash = Hash::parseNonSRIUnprefixed(
                        h.at("content_sha256").get<std::string>(), HashAlgorithm::SHA256);
                    auto caHash =
                        hashMethod.raw == ContentAddressMethod::Raw::NixArchive ? narHash : contentHash;
                    auto info = ValidPathInfo::makeFromCA(
                        *this,
                        name,
                        ContentAddressWithReferences::fromParts(
                            hashMethod,
                            std::move(caHash),
                            StoreReferences{
                                .others = references,
                                .self = false,
                            }),
                        narHash);
                    return printStorePath(info.path);
                },
        };
        SourceCtx sctx{dump};
        RioStr out, err;
        int rc = rio_add_from_dump(
            rio,
            std::string(name).c_str(),
            dumpM,
            caM,
            refsJson(references).dump().c_str(),
            &rioSourceRead,
            &sctx,
            &rioPathCompute,
            &pctx,
            &out.p,
            &err.p);
        if (sctx.ex)
            std::rethrow_exception(sctx.ex);
        checkRc(rc, err, "addToStoreFromDump", pctx.ex);
        return parseStorePath(nlohmann::json::parse(out.str()).at("path").get<std::string>());
    }

    StorePath addToStore(
        std::string_view name,
        const SourcePath & path,
        ContentAddressMethod method = ContentAddressMethod::Raw::NixArchive,
        HashAlgorithm hashAlgo = HashAlgorithm::SHA256,
        const StorePathSet & references = StorePathSet(),
        PathFilter & filter = defaultPathFilter,
        RepairFlag repair = NoRepair) override
    {
        /* A physical source is one whose raw on-disk tree IS what nix is
         * adding: a filesystem accessor whose subtree view equals raw fs,
         * and the all-pass filter (a custom filter changes content
         * independently of the tree). The filter check must be by closure
         * TYPE, not address: fetchToStore passes a local COPY of
         * defaultPathFilter (`auto filter2 = filter ? *filter :
         * defaultPathFilter`), so an address compare never matches the
         * real eval flow. defaultPathFilter's lambda closure type is
         * unique to its definition site, so target_type() identifies it
         * and every copy of it — and nothing else.
         *
         * The accessor check must be by TYPE too. getPhysicalPath()
         * answers "where is THIS path on disk", NOT "is the subtree
         * identical": FilteringSourceAccessor (the abstract base of every
         * nix accessor that hides subtree entries — git workdir's
         * AllowListSourceAccessor, GitExportIgnoreSourceAccessor)
         * delegates it to `next` after access-checking only the queried
         * path, so a tracked-files view over a dirty worktree returns the
         * worktree root and a raw walk then ingests every gitignored
         * file. Gating it out routes filtered sources through dumpPath
         * (honours accessor + filter), same as stock nix. The remaining
         * getPhysicalPath() overrides in the pinned nix are
         * PosixSourceAccessor and the routing wrappers (Mounted, Union)
         * around it — non-filtering by construction; for the one
         * composition nix actually builds (rootFS = posix ∪ storeFS
         * mounted at /nix/store) a non-store-dir physical path is
         * posix-only at every sub-path.
         *
         * TODO: a local-git workdir flake with `?submodules=1` wraps the
         * AllowList in a MountedSourceAccessor (git.cc
         * getAccessorFromWorkdir); the outer-type cast then misses and
         * the raw walk still leaks gitignored content. Submodules default
         * off so the dogfood path is fixed; closing this gap needs either
         * a recursive accessor-chain probe or a getPhysicalPath()
         * contract change upstream. */
        std::string nameStr(name);
        std::optional<std::filesystem::path> phys;
        if (filter.get_fn().target_type() == defaultPathFilter.get_fn().target_type()
            && hashAlgo == HashAlgorithm::SHA256 && method.raw != ContentAddressMethod::Raw::Git
            && !dynamic_cast<FilteringSourceAccessor *>(&*path.accessor))
            phys = path.accessor->getPhysicalPath(path.path);

        auto refs = refsJson(references).dump();

        /* Stat-fingerprint shortcut: a physical source whose fingerprint
         * matches a prior ingest (same name + method + refs) skips the
         * ingest entirely. Regular files validate against the root stat;
         * directory trees validate against a tree-level record on the
         * Rust side (digest over a sorted lstat-only walk — directory
         * mtimes alone don't reflect child edits, so the whole tree is
         * stat-walked, still no reads/hashing/NAR). Never for repair
         * (repair means re-ingest unconditionally). */
        bool fingerprintable = false;
        int caM = -1;
        if (phys && repair == NoRepair) {
            auto st = path.accessor->maybeLstat(path.path);
            if (st
                && (st->type == SourceAccessor::tRegular
                    || st->type == SourceAccessor::tDirectory)) {
                fingerprintable = true;
                caM = caMethodFor(method, "addToStore");
            }
        }

        if (fingerprintable) {
            RioStr out, err;
            int rc = rio_fingerprint_lookup(
                rio, phys->string().c_str(), nameStr.c_str(), caM, refs.c_str(), &out.p, &err.p);
            if (rc == RIO_OK && out.has())
                return parseStorePath(out.str());
            /* miss or lookup failure → fall through to a real ingest */
        }

        auto result = [&]() -> StorePath {
            if (!phys || method.raw != ContentAddressMethod::Raw::NixArchive)
                /* Non-filesystem accessor (fetchTree, filtered source) or
                 * flat/text addressing: generic NAR-dump ingest — those
                 * bytes have no other local home, so they land as
                 * FETCHED content records. */
                return Store::addToStore(name, path, method, hashAlgo, references, filter, repair);

            /* Direct two-plane ingest (ADR-024 not-a-mirror): one walk of
             * the origin tree feeds the NAR-sha256 spine and the chunk
             * plane; NO file content is copied into the CAS. Same hard
             * path cross-check as addToStoreFromDump. */
            PathCbCtx pctx{
                .compute =
                    [&](const nlohmann::json & h) {
                        auto narHash = Hash::parseNonSRIUnprefixed(
                            h.at("nar_sha256").get<std::string>(), HashAlgorithm::SHA256);
                        auto info = ValidPathInfo::makeFromCA(
                            *this,
                            nameStr,
                            ContentAddressWithReferences::fromParts(
                                method,
                                Hash(narHash),
                                StoreReferences{
                                    .others = references,
                                    .self = false,
                                }),
                            narHash);
                        return printStorePath(info.path);
                    },
            };
            RioStr out, err;
            int rc = rio_add_source_tree(
                rio,
                phys->string().c_str(),
                nameStr.c_str(),
                refs.c_str(),
                &rioPathCompute,
                &pctx,
                &out.p,
                &err.p);
            checkRc(rc, err, "addToStore", pctx.ex);
            return parseStorePath(nlohmann::json::parse(out.str()).at("path").get<std::string>());
        }();

        if (fingerprintable) {
            RioStr err;
            /* Best-effort: a failed record only loses the shortcut. */
            rio_fingerprint_record(
                rio,
                phys->string().c_str(),
                nameStr.c_str(),
                caM,
                refs.c_str(),
                printStorePath(result).c_str(),
                &err.p);
        }
        return result;
    }

    StorePath writeDerivation(const Derivation & drv, RepairFlag repair = NoRepair) override
    {
        auto nixPath = nix::computeStorePath(*this, drv);
        auto aterm = drv.unparse(*this, false);
        nlohmann::json j = drv;
        auto drvJson = j.dump();
        RioStr out, err;
        int rc = rio_write_derivation(
            rio,
            std::string(nixPath.name()).c_str(),
            reinterpret_cast<const unsigned char *>(aterm.data()),
            aterm.size(),
            reinterpret_cast<const unsigned char *>(drvJson.data()),
            drvJson.size(),
            printStorePath(nixPath).c_str(),
            &out.p,
            &err.p);
        checkRc(rc, err, "writeDerivation");
        return parseStorePath(out.str());
    }

    /* -- reads ----------------------------------------------------------- */

    Derivation readDerivation(const StorePath & drvPath) override
    {
        StringSinkCtx ctx;
        RioStr err;
        int rc = rio_read_file(rio, baseName(drvPath).c_str(), "", &rioStringWrite, &ctx, &err.p);
        checkRc(rc, err, "readDerivation", ctx.ex);
        return parseDerivation(*this, std::move(ctx.out), Derivation::nameFromPath(drvPath));
    }

    Derivation readInvalidDerivation(const StorePath & drvPath) override
    {
        return readDerivation(drvPath);
    }

    void narFromPath(const StorePath & path, Sink & sink) override
    {
        SinkCtx sctx{sink};
        RioStr err;
        int rc = rio_nar_from_path(rio, baseName(path).c_str(), &rioSinkWrite, &sctx, &err.p);
        checkRc(rc, err, "narFromPath", sctx.ex);
    }

    ref<SourceAccessor> getFSAccessor(bool requireValidPath = true) override
    {
        return wholeStoreView;
    }

    std::shared_ptr<SourceAccessor> getFSAccessor(const StorePath & path, bool requireValidPath = true) override
    {
        if (!isValidPathUncached(path))
            return nullptr;
        return std::make_shared<RioAccessor>(*this, baseName(path));
    }

    /* -- IFD (ADR-024 P3b) ------------------------------------------------ */

    /* Import-from-derivation reaches the eval store as buildPaths
     * (EvalState::realiseContext → buildStore->buildPaths). In the
     * rio-eval worker the registered hook relays the request to the
     * coordinator and BLOCKS until the build resolves and the outputs
     * are imported; in the plugin (no hook) this stays Unsupported. */
    void buildPaths(
        const std::vector<DerivedPath> & paths,
        BuildMode buildMode = bmNormal,
        std::shared_ptr<Store> evalStore = nullptr) override
    {
        if (!rioIfdFn)
            unsupported("buildPaths");
        for (auto & p : paths) {
            auto * built = std::get_if<DerivedPath::Built>(&p.raw());
            if (!built)
                throw Unsupported("'buildPaths' in rio store: non-derivation path '%s'", p.to_string(*this));
            auto drvPath = printStorePath(built->drvPath->getBaseStorePath());
            RioStr err;
            if (rioIfdFn(rioIfdCtx, drvPath.c_str(), &err.p))
                throw Error(
                    "import-from-derivation '%s' failed: %s",
                    drvPath,
                    err.has() ? err.str() : std::string("IFD relay failed"));
        }
    }

    /* -- unsupported ------------------------------------------------------ */

    void registerDrvOutput(const Realisation & output) override
    {
        unsupported("registerDrvOutput");
    }
};

/* Handle accessor for the rio-eval binary: the embedder opens the
 * store through nix's registration ("rio://...") and needs the Rust
 * core handle for the P3b FFI calls (emit/IFD/parent-run). C++
 * linkage — both TUs compile into one binary. */
RioEvalStore * rioShimStoreHandle(Store & store)
{
    auto * s = dynamic_cast<RioStore *>(&store);
    return s ? s->rio : nullptr;
}

ref<Store> RioStoreConfig::openStore() const
{
    return make_ref<RioStore>(ref{shared_from_this()});
}

static RegisterStoreImplementation<RioStoreConfig> regRioStore;

} // namespace nix
