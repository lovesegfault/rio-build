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
#include "nix/store/path-info.hh"
#include "nix/store/realisation.hh"
#include "nix/util/callback.hh"
#include "nix/util/canon-path.hh"
#include "nix/util/serialise.hh"
#include "nix/util/source-accessor.hh"
#include "nix/util/source-path.hh"

#include <nlohmann/json.hpp>

#include <cstring>
#include <string>

#include "rio_evalstore.h"

namespace nix {

namespace {

/* RAII for strings allocated by the Rust side. */
struct RioStr
{
    char * p = nullptr;

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
            RioStr out, err;
            int rc = rio_lstat(store.rio, base.c_str(), rel.c_str(), &out.p, &err.p);
            checkRc(rc, err, "lstat");
            if (!out.has())
                return std::nullopt;
            auto j = nlohmann::json::parse(out.str());
            auto type = j.at("type").get<std::string>();
            Stat st;
            if (type == "regular") {
                st.type = tRegular;
                st.fileSize = j.at("size").get<uint64_t>();
                st.isExecutable = j.at("executable").get<bool>();
            } else if (type == "symlink") {
                st.type = tSymlink;
            } else {
                st.type = tDirectory;
            }
            return st;
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
            RioStr out, err;
            int rc = rio_read_directory(store.rio, base.c_str(), rel.c_str(), &out.p, &err.p);
            checkRc(rc, err, "readDirectory");
            DirEntries entries;
            for (auto & [name, kind] : nlohmann::json::parse(out.str()).items()) {
                auto k = kind.get<std::string>();
                entries.emplace(
                    name, k == "regular" ? tRegular : k == "symlink" ? tSymlink : tDirectory);
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
        /* Stat-fingerprint shortcut: a physical source whose fingerprint
         * matches a prior ingest (same method + refs) skips the dump
         * entirely. Only valid with the default filter (a custom filter
         * changes content independently of the file stats) and only for
         * regular files (directory mtimes don't reflect child edits). */
        std::optional<std::filesystem::path> phys;
        int caM = -1;
        if (&filter == &defaultPathFilter && hashAlgo == HashAlgorithm::SHA256
            && method.raw != ContentAddressMethod::Raw::Git) {
            if (auto p = path.accessor->getPhysicalPath(path.path)) {
                auto st = path.accessor->maybeLstat(path.path);
                if (st && st->type == SourceAccessor::tRegular) {
                    phys = std::move(p);
                    caM = caMethodFor(method, "addToStore");
                }
            }
        }

        if (phys) {
            auto refs = refsJson(references).dump();
            RioStr out, err;
            int rc = rio_fingerprint_lookup(rio, phys->string().c_str(), caM, refs.c_str(), &out.p, &err.p);
            if (rc == RIO_OK && out.has())
                return parseStorePath(out.str());
            /* miss or lookup failure → fall through to a real ingest */
        }

        auto result = Store::addToStore(name, path, method, hashAlgo, references, filter, repair);

        if (phys) {
            auto refs = refsJson(references).dump();
            RioStr err;
            /* Best-effort: a failed record only loses the shortcut. */
            rio_fingerprint_record(
                rio, phys->string().c_str(), caM, refs.c_str(), printStorePath(result).c_str(), &err.p);
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

    /* -- unsupported ------------------------------------------------------ */

    void registerDrvOutput(const Realisation & output) override
    {
        unsupported("registerDrvOutput");
    }
};

ref<Store> RioStoreConfig::openStore() const
{
    return make_ref<RioStore>(ref{shared_from_this()});
}

static RegisterStoreImplementation<RioStoreConfig> regRioStore;

} // namespace nix
