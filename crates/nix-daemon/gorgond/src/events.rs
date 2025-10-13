// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use std::io::{BufReader, BufWriter};

use bytesize::ByteSize;
use clap::{Parser, Subcommand};
use clio::{Input, Output};
use gorgond_client::events;
use miette::{Context, IntoDiagnostic, Result};
use ownable::traits::IntoOwned;
use serde::Serialize;

#[derive(Debug, Clone, Parser)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Parser)]
pub struct InputOutputArgs {
    /// Output buffer size.
    #[arg(long, short = 'b', default_value = "5M")]
    pub output_buffer: ByteSize,

    /// Output file, or "-" for stdout.
    #[arg(long, short)]
    pub output: Output,

    /// Input buffer size.
    #[arg(long, short = 'B', default_value = "5M")]
    pub input_buffer: ByteSize,

    /// Input file, or "-" for stdin.
    pub input: Input,
}
impl InputOutputArgs {
    fn into_buffered(self) -> (BufReader<Input>, BufWriter<Output>) {
        (
            BufReader::with_capacity(self.input_buffer.as_u64() as usize, self.input),
            BufWriter::with_capacity(self.output_buffer.as_u64() as usize, self.output),
        )
    }
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    Json2cbor(Json2cbor),
    Cbor2json(Cbor2json),
}

pub fn run(args: Args) -> Result<()> {
    match args.command {
        Command::Json2cbor(args) => args.run(),
        Command::Cbor2json(args) => args.run(),
    }
}

/// Converts an event log from JSON to CBOR.
///
/// Input: an uncompressed stream of JSON objects.
///
/// Output: an uncompressed stream of length-delimited CBOR values.
#[derive(Debug, Clone, Parser)]
pub struct Json2cbor {
    #[command(flatten)]
    in_out: InputOutputArgs,
}
impl Json2cbor {
    pub fn run(self) -> Result<()> {
        let (input, output) = self.in_out.into_buffered();
        let mut w = minicbor_io::Writer::new(output);
        let events = serde_json::Deserializer::from_reader(input)
            .into_iter::<events::BuildEvent>()
            .enumerate();
        for (i, res) in events {
            res.into_diagnostic()
                .context("Couldn't parse event")
                .and_then(|e| w.write(e).into_diagnostic().context("Couldn't write event"))
                .with_context(|| format!("events[{i}]"))?;
        }
        Ok(())
    }
}

/// Converts an event log from JSON to CBOR.
///
/// Input: an uncompressed stream of JSON objects.
///
/// Output: an uncompressed stream of length-delimited CBOR values.
#[derive(Debug, Clone, Parser)]
pub struct Cbor2json {
    #[command(flatten)]
    in_out: InputOutputArgs,
}
impl Cbor2json {
    pub fn run(self) -> Result<()> {
        let (input, output) = self.in_out.into_buffered();
        let mut w = serde_json::Serializer::new(output);
        let mut r = minicbor_io::Reader::new(input);
        let events = std::iter::from_fn(|| {
            r.read::<events::BuildEvent>()
                .map(|e| e.into_owned())
                .transpose()
        })
        .enumerate();
        for (i, res) in events {
            res.into_diagnostic()
                .context("Couldn't parse event")
                .and_then(|e| {
                    e.serialize(&mut w)
                        .into_diagnostic()
                        .context("Couldn't write event")
                })
                .with_context(|| format!("events[{i}]"))?;
        }
        Ok(())
    }
}
