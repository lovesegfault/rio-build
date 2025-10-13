// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

// Clippy really doesn't like our Error enum, and puts a warning on *everything*
// returning one if it's not silenced like this.
#![allow(clippy::result_large_err)]

use std::{
    fs::File,
    io::{BufRead, BufReader, BufWriter, Seek, Write},
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use backon::{BlockingRetryable, ExponentialBuilder, Retryable};
use bytesize::ByteSize;
use chrono::prelude::*;
use clap::Parser;
use gorgon_build_helper::{
    event_handler, helper::HelperCaller, set_event_handler, Event, EventHandler,
};
use gorgond_client::{
    api,
    events::{BuildEvent, BuildEventKind},
    models::{Build, BuildResult},
    Client, Uuid, Yoke,
};
use humantime::Duration;
use miette::{miette, Context, IntoDiagnostic, Result};
use object_store::ObjectStore;
use parking_lot::Mutex;
use reqwest::Url;
use tokio::runtime::Runtime;
use tokio_util::{io::SyncIoBridge, sync::CancellationToken};
use tracing::{debug, error, info, instrument, warn};

#[derive(Debug, Clone, Parser)]
pub struct Args {
    /// Base URL for the Gorgon API to talk to.
    #[arg(
        long,
        short = 'B',
        env = "GORGON_BASE_URL",
        default_value = "http://localhost:9999/"
    )]
    base_url: reqwest::Url,

    /// Arbitrary ID of this builder.
    #[arg(long, short, env = "GORGON_BUILD_ID")]
    id: String,

    /// Interval between heartbeats sent back to gorgond.
    #[arg(
        long,
        short = 'H',
        env = "GORGON_BUILD_HEARTBEAT_INTERVAL",
        default_value = "60s"
    )]
    heartbeat_interval: Duration,

    /// Compression level for event log files.
    #[arg(long, env = "GORGON_EVENT_BUFFER_SIZE", default_value = "5M")]
    event_buffer_size: ByteSize,
    /// Compression level for event log files.
    #[arg(long, env = "GORGON_EVENT_ZSTD_LEVEL", default_value = "9")]
    event_zstd_level: i32,
    /// Destination URL for log files, eg. "file:///tmp".
    #[arg(long, short = 'E', env = "GORGON_EVENT_STORE_URL")]
    event_store_url: Url,

    #[command(flatten)]
    helpers: gorgon_util_cli::HelperArgs,
    #[command(flatten)]
    common: gorgon_util_cli::Args,
}

struct WritingEventHandler<W: Write> {
    w: Mutex<Option<minicbor_io::Writer<W>>>,
    build_id: Uuid,
    next: &'static dyn EventHandler,
}
impl<W: Write> WritingEventHandler<W> {
    pub fn new(w: W, build_id: Uuid, next: &'static dyn EventHandler) -> Self {
        Self {
            w: Mutex::new(Some(minicbor_io::Writer::new(w))),
            build_id,
            next,
        }
    }
    fn shutdown(&self) -> W {
        self.w
            .lock()
            .take()
            .map(|w| w.into_parts().0)
            .expect("Attempted to shut down event handler twice!")
    }
}
impl<W: Write> EventHandler for WritingEventHandler<W> {
    fn handle(&self, mut event: Event) {
        if let Event::Build(ref mut event) = event {
            event.build_id = self.build_id;

            let mut w = self.w.lock();
            let w = w
                .as_mut()
                .expect("Event received after handler was shut down!");
            w.write(event)
                .into_diagnostic()
                .expect("Couldn't write event!?");
        }
        self.next.handle(event);
    }
}

fn init_event_store(args: &Args) -> Result<(Arc<Box<dyn ObjectStore>>, object_store::path::Path)> {
    let url = &args.event_store_url;
    debug!(%url, "Building object store...");
    let (event_store, event_dir_path) = object_store::parse_url(url)
        .into_diagnostic()
        .with_context(|| url.to_string())?;
    Ok((Arc::new(event_store), event_dir_path))
}

fn init_client(args: &Args) -> Result<Client> {
    Ok(Client::new(args.base_url.clone())?)
}

async fn request_build(args: &Args, client: &Client) -> Result<Yoke<Build<'static>>> {
    info!("Requesting something to build...");
    Ok(client
        .workers()
        .slug(&args.id)
        .internal()
        .allocate_build(api::WorkerInternalAllocateBuildRequest {})
        .await?
        .map_project(|rsp, _| rsp.build))
}

#[instrument(name = "heartbeats", skip(client))]
async fn send_heartbeats(
    client: Client,
    build_id: Uuid,
    started_at: DateTime<Utc>,
    interval: tokio::time::Duration,
) {
    let build_internal = client.builds().id(build_id).internal();

    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut last_tick = tokio::time::Instant::now();
    loop {
        let tick = ticker.tick().await;
        let delay = Duration::from(tick.duration_since(last_tick));
        let heartbeat_at = Utc::now();
        debug!(%delay, %heartbeat_at, "Sending heartbeat...");
        last_tick = tick;

        let req = api::BuildInternalHeartbeatRequest {
            started_at,
            heartbeat_at,
        };
        if let Err(err) = build_internal
            .heartbeat(req)
            .await
            .context("Heartbeat Failed")
        {
            error!("{err:?}");
        }
    }
}

async fn end_build(
    client: &Client,
    build_id: Uuid,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
    result: BuildResult,
) -> Result<()> {
    let build_internal = client.builds().id(build_id).internal();
    let req = api::BuildInternalEndRequest {
        started_at,
        ended_at,
        result,
    };
    build_internal.end(req).await?;
    Ok(())
}

fn upload_logs(
    args: &Args,
    rt: Arc<Runtime>,
    mut f: File,
    store: Arc<impl ObjectStore>,
    path: object_store::path::Path,
) -> Result<()> {
    // Find out the total length of the file.
    let len = f
        .seek(std::io::SeekFrom::End(0))
        .map(ByteSize::b)
        .into_diagnostic()
        .context("Couldn't get event log length")?;

    || -> Result<(), (bool, miette::Report)> {
        // Wrap an object store writer in a zstd encoder. Because this is happening
        // after the build has finished, we can compress it pretty aggressively.
        let w = SyncIoBridge::new_with_handle(
            object_store::buffered::BufWriter::new(store.clone(), path.clone()),
            rt.handle().clone(),
        );
        type ZW = zstd::Encoder<'static, SyncIoBridge<object_store::buffered::BufWriter>>;
        let mut w: ZW = zstd::stream::Encoder::new(w, args.event_zstd_level)
            .into_diagnostic()
            .context("Couldn't create zstd encoder")
            .map_err(|err| (false, err))?;
        let buf_len = ZW::recommended_input_size();

        // Rewind the file and get ready to read it.
        f.seek(std::io::SeekFrom::Start(0))
            .into_diagnostic()
            .context("Couldn't rewind event log")
            .map_err(|err| (false, err))?;
        let mut r = BufReader::with_capacity(buf_len, &mut f);

        // Do the copy, tracking progress.
        let mut pos = ByteSize::b(0);
        (|| -> std::io::Result<()> {
            while !r.fill_buf()?.is_empty() {
                r.consume(w.write(r.buffer()).inspect(|len| pos += *len as u64)?);
                debug!(%pos, %len, "-> Uploading event log");
            }
            Ok(())
        })()
        .into_diagnostic()
        .with_context(|| format!("Copy failed after {pos}"))
        .map_err(|err| (true, err))?;

        debug!("-> Finishing upload...");
        w.finish()
            .into_diagnostic()
            .context("Couldn't finish zstd stream")
            .and_then(|mut w| {
                w.shutdown()
                    .into_diagnostic()
                    .context("Couldn't shut down upload")
            })
            .map_err(|err| (true, err))
    }
    .retry(ExponentialBuilder::default())
    .when(|(retry, _)| *retry)
    .notify(|(_, err), d| {
        let d = Duration::from(d);
        warn!("Failed to upload event log -- retrying in {d}...\n\n{err:?}");
    })
    .call()
    .map_err(|(_, err)| err)
}

pub fn run(args: Args) -> miette::Result<()> {
    // We need to make some async API calls, but most of what this program does is very
    // sync-shaped. So instead of this being an async program that spends most of its
    // runtime doing `block_in_place` or `spawn_blocking`, just make a runtime on the
    // current thread and `block_on` the occasional async block.
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name_fn(|| {
                static THREAD_ID: AtomicUsize = AtomicUsize::new(0);
                let id = THREAD_ID.fetch_add(1, Ordering::SeqCst);
                format!("gorgon-tokio-{id:02}")
            })
            .build()
            .into_diagnostic()
            .context("Couldn't create async runtime")?,
    );

    // Parse the event destination URL, to make sure it's vaguely valid.
    let (event_store, event_dir_path) =
        init_event_store(&args).context("Couldn't create object store")?;

    // Ask gorgond for something to do.
    let client = init_client(&args).context("Couldn't build gorgond client")?;
    let build_ = rt
        .block_on(request_build(&args, &client))
        .context("Couldn't get something to build; lost and without purpose")?;
    let build = build_.get();
    info!(id = %build.id, task=build_.backing_cart(), " -> Got a task!");

    // Test that the log storage backend seems to work.
    let event_path = event_dir_path.child(format!("{}.cbor.zst", build.id));
    rt.block_on(event_store.put(&event_path, object_store::PutPayload::new()))
        .into_diagnostic()
        .context("Couldn't create events log file in destination")?;
    info!(%event_path, %event_store, "Log storage backend is working!");

    // We'll write events to a temporary file to begin with.
    let event_handler = {
        let f = tempfile::tempfile()
            .into_diagnostic()
            .context("Couldn't create temporary file for event storage")?;
        let bw = BufWriter::with_capacity(args.event_buffer_size.as_u64() as _, f);
        let next = event_handler();
        Box::leak(Box::new(WritingEventHandler::new(bw, build.id, next)))
    };
    // SAFETY: Safe as long as no events have been emitted.
    unsafe { set_event_handler(event_handler) }

    // Find builder.
    let caller = args.helpers.caller()?;
    let builder = caller.helper(format!("gorgon-build-{}", build.spec.kind))?;
    let mut build_task = builder
        .with(&args.helpers)
        .with_arg("-")
        .with_stdin(Stdio::piped())
        .into_task(None);

    // Start the builder process...
    info!("Starting build...");
    let mut build_child = build_task
        .start(args.common.level())
        .context("Couldn't start build")?;

    // ...feed it the build context over stdin...
    let build_child_stdin = build_child
        .child
        .stdin
        .as_mut()
        .ok_or(miette!("Builder process has no stdin!?"))?;
    serde_json::to_writer_pretty(build_child_stdin, &build)
        .into_diagnostic()
        .context("Couldn't write Build to builder's stdin")?;

    // ...start communicating with gorgond...
    let started_at = Utc::now();
    event_handler.handle(Event::Build(BuildEvent::from(BuildEventKind::Started)));
    let cancel_heartbeats = CancellationToken::new();
    let join_heartbeats = rt.spawn(cancel_heartbeats.child_token().run_until_cancelled_owned(
        send_heartbeats(
            client.clone(),
            build.id,
            started_at,
            *args.heartbeat_interval,
        ),
    ));

    // ...and wait for the build to finish - note that the build failing does not mean we fail.
    let result = match build_child.wait_checked() {
        Err(err) => {
            error!("{:?}", miette::Report::new(err).context("Build Failed"));
            BuildResult::Failure
        }
        Ok(_) => {
            info!("Build Succeeded!");
            BuildResult::Success
        }
    };
    let ended_at = Utc::now();
    event_handler.handle(Event::Build(BuildEvent::from(BuildEventKind::Stopped(
        result,
    ))));

    // Upload event log.
    info!(%event_store, %event_path, "-> Uploading event log...");
    let event_file = event_handler
        .shutdown()
        .into_inner()
        .into_diagnostic()
        .context("Couldn't flush event log file")?;
    upload_logs(&args, rt.clone(), event_file, event_store, event_path)
        .context("Couldn't upload event log")?;

    // Report build completion to gorgond - only once this is done will it stop trying to
    // reassign the job if we stop sending heartbeats.
    info!("-> Reporting build completion...");
    rt.block_on(
        (|| end_build(&client, build.id, started_at, ended_at, result))
            .retry(ExponentialBuilder::default())
            .notify(|err, d| {
                let d = Duration::from(d);
                warn!("Failed to report build completion -- retrying in {d}...\n\n{err:?}");
            }),
    )
    .context("Couldn't report build completion")?;

    // And stop reporting in.
    cancel_heartbeats.cancel();
    if let Err(err) = rt.block_on(join_heartbeats).into_diagnostic() {
        warn!("{:?}", err.context("Heartbeat task panicked!"))
    }

    info!("All done - goodbye!");
    Ok(())
}

fn main() {
    let args = Args::parse();
    gorgon_util_cli::logger(args.common).init();
    gorgon_util_cli::main(args.common, || run(args));
}
