use std::mem;
use std::sync::Arc;

use hstreamdb::client::Client;
use hstreamdb::producer::FlushSettings;
use hstreamdb::{ChannelProviderSettings, CompressionType, Record, Stream};
use once_cell::sync::OnceCell;
use parking_lot::{Mutex, MutexGuard};
use rustler::types::atom::{error, ok};
use rustler::{resource, Atom, Encoder, Env, NifResult, ResourceArc, Term};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::oneshot;

mod runtime;

rustler::atoms! {
    compression_type, none, gzip, zstd,
    concurrency_limit, len, size
}

rustler::init!(
    "hstreamdb",
    [
        create_stream,
        start_producer,
        stop_producer,
        append,
        await_append_result
    ],
    load = load
);

#[derive(Debug)]
enum AppendResult {
    RecordId(String),
    Error(String),
}

struct AppendResultFuture(Mutex<Option<AppendResultType>>, OnceCell<AppendResult>);

type AppendResultType = oneshot::Receiver<Result<String, Arc<hstreamdb::Error>>>;
#[derive(Clone)]
pub struct NifAppender(UnboundedSender<Option<(Record, oneshot::Sender<AppendResultType>)>>);

fn load(env: Env, _: Term) -> bool {
    resource!(NifAppender, env);
    resource!(AppendResultFuture, env);
    env_logger::init();
    true
}

pub fn try_create_stream(
    url: String,
    stream_name: String,
    replication_factor: u32,
    backlog_duration: u32,
    shard_count: u32,
) -> hstreamdb::Result<()> {
    let (sender, receiver) = oneshot::channel();
    let future = async move {
        let xs = async move {
            let mut client = Client::new(
                url,
                ChannelProviderSettings {
                    concurrency_limit: None,
                },
            )
            .await?;
            client
                .create_stream(Stream {
                    stream_name,
                    replication_factor,
                    backlog_duration,
                    shard_count,
                })
                .await?;
            Ok::<(), hstreamdb::Error>(())
        };
        let xs = xs.await;
        sender.send(xs).unwrap()
    };
    _ = runtime::spawn(future);
    receiver.blocking_recv().unwrap()
}

#[rustler::nif]
pub fn create_stream(
    env: Env,
    url: String,
    stream_name: String,
    replication_factor: u32,
    backlog_duration: u32,
    shard_count: u32,
) -> NifResult<Term> {
    try_create_stream(
        url,
        stream_name,
        replication_factor,
        backlog_duration,
        shard_count,
    )
    .map(|()| ok().to_term(env))
    .map_err(|err| rustler::Error::Term(Box::new(err.to_string())))
}

pub fn try_start_producer(
    url: String,
    stream_name: String,
    settings: Term,
) -> hstreamdb::Result<ResourceArc<NifAppender>> {
    let (sender, receiver) = oneshot::channel();
    let (request_sender, request_receiver) =
        unbounded_channel::<Option<(Record, oneshot::Sender<AppendResultType>)>>();
    let (compression_type, concurrency_limit, flush_settings) = new_producer_settings(settings)?;
    let future = async move {
        let xs = async move {
            let mut client = Client::new(
                url,
                ChannelProviderSettings {
                    concurrency_limit: None,
                },
            )
            .await?;
            let (appender, producer) = client
                .new_producer(
                    stream_name,
                    compression_type,
                    flush_settings,
                    ChannelProviderSettings { concurrency_limit },
                )
                .await?;

            _ = tokio::spawn(async move {
                let mut request_receiver = request_receiver;
                let mut appender = appender;
                while let Some(record) = request_receiver.recv().await {
                    match record {
                        Some((record, result_sender)) => {
                            let result_receiver = appender.append(record).unwrap();
                            result_sender.send(result_receiver).unwrap()
                        }
                        None => request_receiver.close(),
                    }
                }
            });
            _ = tokio::spawn(async move { producer.start().await });
            Ok::<(), hstreamdb::Error>(())
        };
        let xs = xs.await;
        sender.send(xs).unwrap()
    };
    _ = runtime::spawn(future);

    receiver
        .blocking_recv()
        .unwrap()
        .map(|()| ResourceArc::new(NifAppender(request_sender)))
}

#[rustler::nif]
pub fn start_producer<'a>(
    env: Env<'a>,
    url: String,
    stream_name: String,
    settings: Term,
) -> NifResult<Term<'a>> {
    try_start_producer(url, stream_name, settings)
        .map(|x| Encoder::encode(&(ok(), x), env))
        .map_err(|err| rustler::Error::Term(Box::new(err.to_string())))
}

#[rustler::nif]
fn stop_producer(producer: ResourceArc<NifAppender>) -> Atom {
    let producer = &producer.0;
    producer.send(None).unwrap_or(());
    ok()
}

#[rustler::nif]
fn append(
    producer: ResourceArc<NifAppender>,
    partition_key: String,
    raw_payload: String,
) -> ResourceArc<AppendResultFuture> {
    let record = Record {
        partition_key,
        payload: hstreamdb::Payload::RawRecord(raw_payload.into_bytes()),
    };
    let producer = &producer.0;
    let (sender, receiver) = oneshot::channel();
    producer.send(Some((record, sender))).unwrap();
    let receiver = receiver.blocking_recv().unwrap();
    ResourceArc::new(AppendResultFuture(
        Mutex::new(Some(receiver)),
        OnceCell::new(),
    ))
}

#[rustler::nif]
fn await_append_result(env: Env, x: ResourceArc<AppendResultFuture>) -> Term {
    use crate::AppendResult::*;
    let result = &x.1;

    if result.get().is_none() {
        let receiver: &Mutex<_> = &x.0;
        let mut receiver: MutexGuard<Option<_>> = receiver.lock();
        let receiver = mem::take(&mut (*receiver));
        let append_result: Result<String, Arc<_>> = receiver.unwrap().blocking_recv().unwrap();
        let append_result = match append_result {
            Ok(record_id) => RecordId(record_id),
            Err(err) => Error(err.to_string()),
        };
        result.set(append_result).unwrap()
    }

    let result = match result.get().unwrap() {
        RecordId(record_id) => (ok(), record_id.to_string()),
        Error(err) => (error(), err.to_string()),
    };
    result.encode(env)
}

fn atom_to_compression_type(compression_type: Atom) -> Option<CompressionType> {
    if compression_type == none() {
        Some(CompressionType::None)
    } else if compression_type == gzip() {
        Some(CompressionType::Gzip)
    } else if compression_type == zstd() {
        Some(CompressionType::Zstd)
    } else {
        None
    }
}

fn new_producer_settings(
    proplists: Term,
) -> hstreamdb::Result<(CompressionType, Option<usize>, FlushSettings)> {
    let proplists = proplists
        .into_list_iterator()
        .map_err(|err| hstreamdb::Error::BadArgument(format!("{err:?}")))?;
    let mut concurrency_limit_v = None;
    let mut len_v = usize::MAX;
    let mut size_v = usize::MAX;
    let mut compression_type_v: Atom = none();

    for x in proplists {
        if x.is_tuple() {
            let (k, v): (Atom, Term) = x
                .decode()
                .map_err(|err| hstreamdb::Error::BadArgument(format!("{err:?}")))?;
            if k == concurrency_limit() {
                concurrency_limit_v = Some(
                    v.decode()
                        .map_err(|err| hstreamdb::Error::BadArgument(format!("{err:?}")))?,
                );
            } else if k == len() {
                len_v = v
                    .decode()
                    .map_err(|err| hstreamdb::Error::BadArgument(format!("{err:?}")))?;
            } else if k == size() {
                size_v = v
                    .decode()
                    .map_err(|err| hstreamdb::Error::BadArgument(format!("{err:?}")))?;
            } else if k == compression_type() {
                compression_type_v = v
                    .decode()
                    .map_err(|err| hstreamdb::Error::BadArgument(format!("{err:?}")))?;
            }
        }
    }

    if len_v == usize::MAX && size_v == usize::MAX {
        len_v = 0;
        size_v = 0;
    }
    let compression_type_v = atom_to_compression_type(compression_type_v).ok_or_else(|| {
        hstreamdb::Error::BadArgument(format!(
            "no match for compression type `{compression_type_v:?}`"
        ))
    })?;

    Ok((
        compression_type_v,
        concurrency_limit_v,
        FlushSettings {
            len: len_v,
            size: size_v,
        },
    ))
}
