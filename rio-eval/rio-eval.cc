/* rio-eval — the ADR-024 P3b eval parent.
 *
 * Embeds nix's libexpr (the flake-pinned 2.34 components, NEVER the
 * ambient nix) plus the rio-evalstore Rust staticlib through the same
 * C++ store shim the rio:// plugin uses (shim.cc, compiled into this
 * binary — the static RegisterStoreImplementation makes "rio://"
 * openable). The process split (ADR-024 "process architecture"):
 *
 *   rio (coordinator, Rust)  ── socketpair fd 3 ──  rio-eval (this)
 *                                                    │ fork-no-exec
 *                                                    eval workers
 *
 * This main() does the pre-fork half: GC_DONT_GC, init nix libs (sans
 * the libmain signal thread — see main()), initGC, open
 * the rio:// store, build the EvalState, lock the flake + fetch inputs
 * ONCE (through the rio store — workers never re-fetch), then hand the
 * loop to the Rust side (rio_eval_parent_run), which forks workers and
 * calls back into evalAttr() per assigned attr. Everything after fork
 * runs in the worker: attr selection, libexpr forcing (drvs land in
 * the store's in-memory map via writeDerivation), skeleton assembly +
 * the final ResultFrame (rio_emit_result), and the blocking IFD relay
 * (rio_ifd_request via the shim's buildPaths hook).
 */

#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

#include "nix/cmd/common-eval-args.hh" // fetchSettings / evalSettings / flakeSettings globals + lookupFileArg
#include "nix/expr/attr-path.hh"
#include "nix/expr/eval-gc.hh"
#include "nix/expr/eval-settings.hh"
#include "nix/expr/eval.hh"
#include "nix/expr/get-drvs.hh"
#include "nix/flake/flake.hh"
#include "nix/flake/flakeref.hh"
#include "nix/flake/settings.hh"
#include "nix/store/filetransfer.hh" // fileTransferSettings
#include "nix/store/globals.hh"      // initLibStore
#include "nix/store/store-api.hh"
#include "nix/store/store-open.hh"
#include "nix/util/logging.hh" // Logger / activity types
#include "nix/util/file-system.hh"
#include "nix/util/signals.hh" // unix::saveSignalMask (via signals-impl.hh)

#include "rio_shim.hh"

namespace {

RioEvalStore * gRio = nullptr;

/* The worker's channel fd, set by evalAttr before any forcing — the
 * IFD hook fires from arbitrarily deep inside libexpr. */
int gWorkerFd = -1;

extern "C" int ifdHandler(void * /*ctx*/, const char * drvPath, char ** err) noexcept
{
    if (gWorkerFd < 0) {
        /* IFD outside a worker (e.g. during the parent's flake lock)
         * is unsupported by design: the parent has no attr context to
         * relay under. The message must come from the Rust allocator —
         * the shim frees *err with rio_string_free. */
        *err = rio_string_dup("import-from-derivation outside an eval worker");
        return 1;
    }
    char * outJson = nullptr;
    int rc = rio_ifd_request(gRio, gWorkerFd, drvPath, &outJson, err);
    rio_string_free(outJson); /* outputs already imported store-side */
    return rc;
}

/* Forwards libnix fetch-activity start lines to fd 3 as Note frames so
 * the coordinator's renderer surfaces fetch progress during the
 * pre-fork lockFlake/callFlake warmup. Without it, a cold flake eval
 * is silent for the entire transitive-input fetch — indistinguishable
 * from a hang. Everything else passes through to the previous logger.
 *
 * Installed for the pre-fork warmup ONLY: after rio_eval_parent_run
 * forks, fd 3 belongs to the parent's poll loop and a worker writing
 * to it would corrupt framing; the previous logger is restored before
 * the parent loop starts. */
struct FetchNoteLogger : nix::Logger
{
    nix::Logger * prev;
    void log(nix::Verbosity lvl, std::string_view s) override { prev->log(lvl, s); }
    void logEI(const nix::ErrorInfo & ei) override { prev->logEI(ei); }
    void startActivity(
        nix::ActivityId act,
        nix::Verbosity lvl,
        nix::ActivityType type,
        const std::string & s,
        const Fields & fields,
        nix::ActivityId parent) override
    {
        if ((type == nix::actFileTransfer || type == nix::actFetchTree) && !s.empty()) {
            char * err = nullptr;
            rio_emit_note(3, s.c_str(), &err);
            rio_string_free(err);
        }
        prev->startActivity(act, lvl, type, s, fields, parent);
    }
};

struct EvalCtx
{
    nix::ref<nix::EvalState> state;
    nix::Value * vRoot;
    nix::Bindings * autoArgs;
    bool flakeMode;
    std::string system;
};

/* Attr-path candidates for one WorkItem attr. File mode: the attr
 * verbatim. Flake mode: the fragment, then the `nix build` fallback
 * prefixes; an empty fragment means the default package. */
std::vector<std::string> attrCandidates(const EvalCtx & ctx, const std::string & attr)
{
    if (!ctx.flakeMode)
        return {attr};
    if (attr.empty())
        return {
            "packages." + ctx.system + ".default",
            "defaultPackage." + ctx.system,
        };
    return {
        attr,
        "packages." + ctx.system + "." + attr,
        "legacyPackages." + ctx.system + "." + attr,
    };
}

/* Attr-path component for re-resolution by findAlongAttrPath: quote
 * names its dot-splitting parser would otherwise tear apart. Names it
 * cannot address at all — containing '"' (the parser has no escapes),
 * all digits (parsed as a list index), or empty — return nullopt; the
 * caller skips those with a warning. */
std::optional<std::string> attrPathComponent(std::string_view name)
{
    if (name.find('"') != std::string_view::npos
        || name.find_first_not_of("0123456789") == std::string_view::npos)
        return std::nullopt;
    if (name.find('.') != std::string_view::npos)
        return "\"" + std::string(name) + "\"";
    return std::string(name);
}

/* Enumerate the derivation children of an attrset value (one level,
 * descending into `recurseForDerivations = true` sub-attrsets). A child
 * that is neither a derivation nor a recursable attrset — or that fails
 * to force — lands in `skipped`; one odd entry must not kill the
 * expansion. */
void collectDrvChildren(
    nix::EvalState & state,
    nix::Value & v,
    const std::string & prefix,
    std::vector<std::string> & children,
    std::vector<std::string> & skipped)
{
    for (auto & a : v.attrs()->lexicographicOrder(state.symbols)) {
        std::string_view name{state.symbols[a->name]};
        if (name == "recurseForDerivations")
            continue;
        auto component = attrPathComponent(name);
        std::string path = prefix + "." + component.value_or(std::string(name));
        if (!component) {
            skipped.push_back(path);
            continue;
        }
        try {
            if (nix::getDerivation(state, *a->value, false)) {
                children.push_back(path);
                continue;
            }
            if (a->value->type() == nix::nAttrs) {
                auto r = a->value->attrs()->get(state.s.recurseForDerivations);
                if (r
                    && state.forceBool(
                        *r->value, r->pos, "while evaluating `recurseForDerivations`")) {
                    collectDrvChildren(state, *a->value, path, children, skipped);
                    continue;
                }
            }
            skipped.push_back(path);
        } catch (nix::Error &) {
            // The child's own eval error resurfaces if it is requested
            // explicitly; here it only costs the skip warning.
            skipped.push_back(path);
        }
    }
}

/* The resolved attr is an attrset, not a derivation: expand it into one
 * WorkItem per derivation child (reported via an AttrsetExpansion
 * frame) instead of failing. `resolved` is the candidate attr path that
 * matched — children are named relative to it so the coordinator's
 * WorkItems re-resolve verbatim. */
// r[impl bc.eval.attrset-expansion]
int expandAttrset(
    EvalCtx & ctx, const std::string & attr, const std::string & resolved, nix::Value & v, int workerFd)
{
    std::string prefix = resolved;
    nix::Value * cur = &v;
    std::vector<std::string> children;
    std::vector<std::string> skipped;

    /* `.#checks` style: descend into the eval system's entry first (the
     * same system the flake fragment fallbacks use). */
    auto sys = cur->attrs()->get(ctx.state->symbols.create(ctx.system));
    if (sys) {
        prefix += "." + ctx.system;
        if (nix::getDerivation(*ctx.state, *sys->value, false)) {
            children.push_back(prefix);
        } else if (sys->value->type() == nix::nAttrs) {
            cur = sys->value;
        } else {
            throw nix::Error(
                "attribute '%s' is neither a derivation nor an attrset of derivations", prefix);
        }
    }
    if (children.empty())
        collectDrvChildren(*ctx.state, *cur, prefix, children, skipped);
    if (children.empty()) {
        std::string hint = sys ? "" : " and no '" + ctx.system + "' entry";
        throw nix::Error(
            "attribute '%s' expanded to zero derivations (no derivation children below '%s'%s)",
            attr,
            prefix,
            hint);
    }

    std::vector<const char *> childPtrs, skippedPtrs;
    for (auto & c : children)
        childPtrs.push_back(c.c_str());
    for (auto & s : skipped)
        skippedPtrs.push_back(s.c_str());
    char * err = nullptr;
    if (rio_emit_expansion(
            workerFd,
            attr.c_str(),
            childPtrs.data(),
            childPtrs.size(),
            skippedPtrs.data(),
            skippedPtrs.size(),
            &err)) {
        std::string m = err ? err : "emit failed";
        rio_string_free(err);
        throw nix::Error("emitting expansion for '%s': %s", attr, m);
    }
    return 0;
}

/* Per-attr worker callback (runs in the FORKED CHILD). */
extern "C" int
evalAttr(void * ctxRaw, const char * attrC, int workerFd, char * errBuf, size_t errCap) noexcept
{
    auto * ctx = static_cast<EvalCtx *>(ctxRaw);
    gWorkerFd = workerFd;
    try {
        std::string attr(attrC);
        nix::Value * v = nullptr;
        std::string resolved;
        std::optional<std::string> firstError;
        for (auto & candidate : attrCandidates(*ctx, attr)) {
            try {
                v = nix::findAlongAttrPath(*ctx->state, candidate, *ctx->autoArgs, *ctx->vRoot)
                        .first;
                resolved = candidate;
                break;
            } catch (nix::Error & e) {
                if (!firstError)
                    firstError = e.msg();
            }
        }
        if (!v)
            throw nix::Error(
                "attribute '%s' not found: %s",
                attr,
                firstError.value_or("no candidates tried"));

        auto packageInfo = nix::getDerivation(*ctx->state, *v, false);
        if (!packageInfo) {
            // getDerivation already forced *v.
            if (v->type() == nix::nAttrs)
                return expandAttrset(*ctx, attr, resolved, *v, workerFd);
            throw nix::Error("attribute '%s' does not evaluate to a derivation", attr);
        }
        /* Forcing drvPath instantiates the derivation closure: every
         * drv lands in the rio store's in-memory map (writeDerivation)
         * and every local source tree takes the two-plane ingest. */
        auto drvPath = packageInfo->queryDrvPath();
        if (!drvPath)
            throw nix::Error("derivation '%s' has no drvPath", attr);
        auto full = ctx->state->store->printStorePath(*drvPath);

        char * err = nullptr;
        if (rio_emit_result(gRio, workerFd, attrC, full.c_str(), &err)) {
            std::string m = err ? err : "emit failed";
            rio_string_free(err);
            throw nix::Error("emitting result for '%s': %s", attr, m);
        }
        return 0;
    } catch (std::exception & e) {
        snprintf(errBuf, errCap, "%s", e.what());
        return 1;
    } catch (...) {
        snprintf(errBuf, errCap, "unknown eval failure for attr");
        return 1;
    }
}

[[noreturn]] void usage(const char * argv0)
{
    fprintf(
        stderr,
        "usage: %s --cas DIR (--file PATH | --flake REF) [--workers N] "
        "[--recycle-attrs N] [--recycle-rss-mb N]\n"
        "Spawned by `rio build` with the worker channel on fd 3 — not a "
        "user-facing command.\n",
        argv0);
    exit(2);
}

} // namespace

int main(int argc, char ** argv)
{
    /* Boehm must never collect: workers are recycled instead (process
     * exit IS the GC — ADR-024; measured 1.19-1.39x RSS). Must be set
     * before initGC(). */
    setenv("GC_DONT_GC", "1", 1);

    std::string casDir;
    std::string file;
    std::string flakeRef;
    std::string optsJson = "{";
    bool firstOpt = true;
    auto addOpt = [&](const char * key, const std::string & val) {
        if (!firstOpt)
            optsJson += ",";
        firstOpt = false;
        optsJson += std::string("\"") + key + "\":" + val;
    };
    for (int i = 1; i < argc; i++) {
        std::string arg = argv[i];
        auto next = [&]() -> std::string {
            if (i + 1 >= argc)
                usage(argv[0]);
            return argv[++i];
        };
        if (arg == "--cas")
            casDir = next();
        else if (arg == "--file")
            file = next();
        else if (arg == "--flake")
            flakeRef = next();
        else if (arg == "--workers")
            addOpt("max_workers", next());
        else if (arg == "--recycle-attrs")
            addOpt("recycle_attrs", next());
        else if (arg == "--recycle-rss-mb")
            addOpt("recycle_rss_mb", next());
        else
            usage(argv[0]);
    }
    optsJson += "}";
    if (casDir.empty() || (file.empty() == flakeRef.empty()))
        usage(argv[0]);

    try {
        /* NOT initNix(): that starts the detached signal-handler
         * thread, and the fork-safety rule requires zero live threads
         * at every fork(2) (r[bc.evalparent.fork-safety] — glibc's
         * atfork handlers only cover malloc; any other lock that
         * thread held at the fork instant would deadlock the worker).
         * Reproduce the pieces rio-eval needs by hand:
         *   - initLibStore: config load, NSS preload, curl_global_init;
         *   - save the inherited signal mask (so restoreSignals() in
         *     nix-spawned helpers like git restores it), then BLOCK the
         *     terminal/pipe signals on this thread. Blocked, not
         *     handled: rio-eval's lifecycle is the coordinator channel
         *     (EOF ends it), Ctrl-C must not kill the eval mid-detach,
         *     and a blocked SIGPIPE turns relay writes into EPIPE;
         *   - SIGCHLD back to SIG_DFL in case the spawner ignored it
         *     (SIG_IGN survives exec and would break waitpid reaping).
         *
         * TODO: in flake mode a remote input fetch starts nix's curl
         * FileTransfer worker thread, which has no shutdown API and
         * outlives the pre-fork warmup — a residual fork-safety gap
         * until nix grows a way to tear that singleton down. Path
         * flakes and --file mode never start it. */

        /* initLibStore opens the local-store db lock under
         * NIX_STATE_DIR (default /nix/var/nix), failing on any
         * read-only-nix-store / unprivileged-container client even
         * though rio-eval's only store is rio:// and nothing here
         * touches the local store. Default the state dir to a
         * per-user XDG path so an unset env doesn't surface that
         * lock as "Read-only file system". */
        if (!std::getenv("NIX_STATE_DIR")) {
            const char * base = std::getenv("XDG_STATE_HOME");
            std::filesystem::path dir = base && *base
                ? std::filesystem::path(base)
                : std::filesystem::path(std::getenv("HOME") ?: "/tmp") / ".local/state";
            dir /= "rio-eval/nix";
            std::filesystem::create_directories(dir);
            setenv("NIX_STATE_DIR", dir.c_str(), 1);
        }
        nix::initLibStore(true);
        nix::unix::saveSignalMask();
        {
            sigset_t set;
            sigemptyset(&set);
            sigaddset(&set, SIGINT);
            sigaddset(&set, SIGTERM);
            sigaddset(&set, SIGHUP);
            sigaddset(&set, SIGPIPE);
            if (sigprocmask(SIG_BLOCK, &set, nullptr))
                throw nix::SysError("blocking signals");
            struct sigaction act = {};
            act.sa_handler = SIG_DFL;
            if (sigaction(SIGCHLD, &act, nullptr))
                throw nix::SysError("resetting SIGCHLD");
        }
        nix::initGC();

        /* The eval store, build store and IFD store are all the rio
         * store: drvs stay in process memory, sources take the
         * two-plane ingest, and buildPaths routes through the IFD
         * relay hook. */
        auto store = nix::openStore("rio://?cas=" + casDir);
        gRio = nix::rioShimStoreHandle(*store);
        if (!gRio) {
            fprintf(stderr, "rio-eval: opened store is not a rio:// store\n");
            return 1;
        }
        rio_shim_set_ifd_handler(&ifdHandler, nullptr);

        auto state = nix::make_ref<nix::EvalState>(
            nix::LookupPath{}, store, nix::fetchSettings, nix::evalSettings);
        nix::Bindings & autoArgs = *state->buildBindings(0).finish();

        EvalCtx ctx{
            .state = state,
            .vRoot = nullptr,
            .autoArgs = &autoArgs,
            .flakeMode = !flakeRef.empty(),
            .system = nix::settings.thisSystem.get(),
        };

        /* Pre-fork warmup: parse + lock + fetch ONCE; workers inherit
         * the forced top-level value COW (sharing by fork order).
         * Fetch progress goes to fd 3 as Note frames so the
         * coordinator surfaces it; restored before the parent loop
         * starts (see FetchNoteLogger). Tighten stalled-download from
         * the 300s default so a stuck tarball has a hard ceiling. */
        auto prevLogger = std::move(nix::logger);
        {
            auto fnl = std::make_unique<FetchNoteLogger>();
            fnl->prev = prevLogger.get();
            nix::logger = std::move(fnl);
        }
        nix::fileTransferSettings.stalledDownloadTimeout = 60;
        if (ctx.flakeMode) {
            auto parsed = nix::parseFlakeRef(
                nix::fetchSettings, flakeRef, nix::absPath(std::filesystem::path(".")));
            nix::flake::LockFlags lockFlags;
            /* The eval parent must never mutate the user's tree; a
             * needed-but-missing lock entry stays in memory. */
            lockFlags.writeLockFile = false;
            auto locked = nix::flake::lockFlake(nix::flakeSettings, *state, parsed, lockFlags);
            ctx.vRoot = state->allocValue();
            nix::flake::callFlake(*state, locked, *ctx.vRoot);
            state->forceAttrs(*ctx.vRoot, nix::noPos, "while forcing the flake's outputs");
        } else {
            nix::Value vTop;
            state->evalFile(nix::lookupFileArg(*state, file), vTop);
            ctx.vRoot = state->allocValue();
            state->autoCallFunction(autoArgs, vTop, *ctx.vRoot);
            state->forceAttrs(*ctx.vRoot, nix::noPos, "while forcing the top-level attrset");
        }
        nix::logger = std::move(prevLogger);

        /* Hand the loop to Rust: fork workers, relay frames, recycle,
         * crash-requeue — until the coordinator's Shutdown drains. */
        char * err = nullptr;
        if (rio_eval_parent_run(gRio, 3, optsJson.c_str(), &evalAttr, &ctx, &err)) {
            fprintf(stderr, "rio-eval: %s\n", err ? err : "parent loop failed");
            rio_string_free(err);
            return 1;
        }
        return 0;
    } catch (nix::Error & e) {
        fprintf(stderr, "rio-eval: %s\n", e.msg().c_str());
        return 1;
    } catch (std::exception & e) {
        fprintf(stderr, "rio-eval: %s\n", e.what());
        return 1;
    }
}
