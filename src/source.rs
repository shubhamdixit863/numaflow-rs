use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::error::Error::SourceError;
use crate::error::{Error, ErrorKind};
use crate::shared::{self, prost_timestamp_from_utc};
use crate::source::proto::{AckRequest, AckResponse, ReadRequest, ReadResponse};
use chrono::{DateTime, Utc};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{async_trait, Request, Response, Status, Streaming};
use tracing::{error, info};

const DEFAULT_MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;
const DEFAULT_SOCK_ADDR: &str = "/var/run/numaflow/source.sock";
const DEFAULT_SERVER_INFO_FILE: &str = "/var/run/numaflow/sourcer-server-info";
// TODO: use batch-size, blocked by https://github.com/numaproj/numaflow/issues/2026
const DEFAULT_CHANNEL_SIZE: usize = 1000;

/// Source Proto definitions.
pub mod proto {
    tonic::include_proto!("source.v1");
}

struct SourceService<T> {
    handler: Arc<T>,
    shutdown_tx: Sender<()>,
    cancellation_token: CancellationToken,
}

// FIXME: remove async_trait
#[async_trait]
/// Trait representing a [user defined source](https://numaflow.numaproj.io/user-guide/sources/overview/).
///
/// ## Example
/// Please refer to [simple source](https://github.com/numaproj/numaflow-rs/tree/main/examples/simple-source) for an example.
///
/// ## NOTE
/// The standard convention for both [`Sourcer::read`] and [`Sourcer::ack`] is that they should be mutable,
/// since they have to update some state. Unfortunately the SDK provides only a shared reference of self and thus makes it immutable. This is because
/// gRPC [tonic] provides only a shared reference for its traits. This means, the implementer for trait will have to use [SharedState] pattern to mutate
/// the values as recommended in [issue-427]. This might change in future as async traits evolves.
///
/// [user-defined source]: https://numaflow.numaproj.io/user-guide/sources/overview/
/// [tonic]: https://github.com/hyperium/tonic/
/// [SharedState]: https://tokio.rs/tokio/tutorial/shared-state
/// [issue-427]: https://github.com/hyperium/tonic/issues/427
pub trait Sourcer {
    /// Reads the messages from the source and sends them to the transmitter.
    async fn read(&self, request: SourceReadRequest, transmitter: Sender<Message>);
    /// Acknowledges the message that has been processed by the user-defined source.
    async fn ack(&self, offset: Offset);
    /// Returns the number of messages that are yet to be processed by the user-defined source.
    async fn pending(&self) -> usize;
    /// Returns the partitions associated with the source. This will be used by the platform to determine
    /// the partitions to which the watermark should be published. Some sources might not have the concept of partitions.
    /// Kafka is an example of source where a reader can read from multiple partitions.
    /// If None is returned, Numaflow replica-id will be returned as the partition.
    async fn partitions(&self) -> Option<Vec<i32>>;
}

/// A request from the gRPC client (numaflow) to the user's [`Sourcer::read`].
pub struct SourceReadRequest {
    /// The number of messages to be read.
    pub count: usize,
    /// Request timeout in milliseconds.
    pub timeout: Duration,
}

/// The offset of the message.
pub struct Offset {
    /// Offset value in bytes.
    pub offset: Vec<u8>,
    /// Partition ID of the message.
    pub partition_id: i32,
}

impl<T> SourceService<T>
where
    T: Sourcer + Send + Sync + 'static,
{
    /// writes a read batch returned by the user-defined handler to the client (numaflow).
    async fn write_a_batch(
        grpc_resp_tx: Sender<Result<ReadResponse, Status>>,
        mut udsource_rx: Receiver<Message>,
    ) -> crate::error::Result<()> {
        // even though we use bi-di; the user-defined source sees this as a 1/2 duplex
        // server side streaming. this means that the below while loop will terminate
        // after every batch of read has been returned.
        while let Some(resp) = udsource_rx.recv().await {
            grpc_resp_tx
                .send(Ok(ReadResponse {
                    result: Some(proto::read_response::Result {
                        payload: resp.value,
                        offset: Some(proto::Offset {
                            offset: resp.offset.offset,
                            partition_id: resp.offset.partition_id,
                        }),
                        event_time: prost_timestamp_from_utc(resp.event_time),
                        keys: resp.keys,
                        headers: Default::default(),
                    }),
                    status: None,
                }))
                .await
                .map_err(|e| SourceError(ErrorKind::InternalError(e.to_string())))?;
        }

        // send end of transmission on success
        grpc_resp_tx
            .send(Ok(ReadResponse {
                result: None,
                status: Some(proto::read_response::Status {
                    eot: true,
                    code: 0,
                    error: 0,
                    msg: None,
                }),
            }))
            .await
            .map_err(|e| SourceError(ErrorKind::InternalError(e.to_string())))?;

        Ok(())
    }

    /// Invokes the user-defined source handler to get a read batch and streams it to the numaflow
    /// (client).
    async fn forward_a_batch(
        handler_fn: Arc<T>,
        grpc_resp_tx: Sender<Result<ReadResponse, Status>>,
        request: proto::read_request::Request,
    ) -> crate::error::Result<()> {
        // tx,rx pair for sending data over to user-defined source
        let (stx, srx) = mpsc::channel::<Message>(DEFAULT_CHANNEL_SIZE);

        // spawn the rx side so that when the handler is invoked, we can stream the handler's read data
        // to the gprc response stream.
        let grpc_writer_handle: JoinHandle<Result<(), Error>> =
            tokio::spawn(async move { Self::write_a_batch(grpc_resp_tx, srx).await });

        // spawn the handler, it will stream the data to tx passed which will be streamed to the client
        // by the above task.
        handler_fn
            .read(
                SourceReadRequest {
                    count: request.num_records as usize,
                    timeout: Duration::from_millis(request.timeout_in_ms as u64),
                },
                stx,
            )
            .await;

        // wait for the spawned grpc writer to end
        grpc_writer_handle
            .await
            .map_err(|e| SourceError(ErrorKind::InternalError(e.to_string())))?
            .map_err(|e| SourceError(ErrorKind::InternalError(e.to_string())))?;

        Ok(())
    }
}

#[async_trait]
impl<T> proto::source_server::Source for SourceService<T>
where
    T: Sourcer + Send + Sync + 'static,
{
    type ReadFnStream = ReceiverStream<Result<ReadResponse, Status>>;

    async fn read_fn(
        &self,
        request: Request<Streaming<ReadRequest>>,
    ) -> Result<Response<Self::ReadFnStream>, Status> {
        let mut sr = request.into_inner();
        // we have to call the handler over and over for each ReadRequest
        let handler_fn = Arc::clone(&self.handler);

        // tx (read from client), rx (write to client) pair for gRPC response
        let (tx, rx) = mpsc::channel::<Result<ReadResponse, Status>>(DEFAULT_CHANNEL_SIZE);

        // this _tx ends up writing to the client side
        let grpc_tx = tx.clone();

        let cln_token = self.cancellation_token.clone();

        // this is the top-level stream consumer and this task will only exit when stream is closed (which
        // will happen when server and client are shutting down).
        let grpc_read_handle: JoinHandle<Result<(), Error>> = tokio::spawn(async move {
            loop {
                tokio::select! {
                    // for each ReadRequest message, the handler will be called and a batch of messages
                    // will be sent over to the client.
                    read_request = sr.message() => {
                        let read_request = read_request
                            .map_err(|e| SourceError(ErrorKind::InternalError(e.to_string())))?
                            .ok_or_else(|| SourceError(ErrorKind::InternalError("Stream closed".to_string())))?;

                        let request = read_request.request.ok_or_else(|| SourceError(ErrorKind::InternalError("Stream closed".to_string())))?;

                        // start the ud-source rx asynchronously and start populating the gRPC
                        // response, so it can be streamed to the gRPC client (numaflow).
                        let grpc_resp_tx = grpc_tx.clone();

                        // let's forward a batch for this request
                        Self::forward_a_batch(handler_fn.clone(), grpc_resp_tx, request).await?
                    }
                    _ = cln_token.cancelled() => {
                        info!("Cancellation token triggered, shutting down");
                        break;
                    }
                }
            }
            Ok(())
        });

        let shutdown_tx = self.shutdown_tx.clone();
        // spawn so we can return the recv stream to client.
        tokio::spawn(async move {
            // wait for the grpc read handle; if there are any errors, we set the gRPC Status to failure
            // which will close the stream with failure.
            if let Err(e) = grpc_read_handle.await {
                error!("shutting down the gRPC channel, {}", e);
                tx.send(Err(Status::internal(e.to_string())))
                    .await
                    .map_err(|e| SourceError(ErrorKind::InternalError(e.to_string())))
                    .expect("writing error to grpc response channel should never fail");

                // if there are any failures, we propagate those failures so that the server can shutdown.
                shutdown_tx
                    .send(())
                    .await
                    .expect("write to shutdown channel should never fail");
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn ack_fn(
        &self,
        request: Request<Streaming<AckRequest>>,
    ) -> Result<Response<AckResponse>, Status> {
        let mut ack_stream = request.into_inner();
        while let Some(ack_request) = ack_stream.message().await? {
            // the request is not there send back status as invalid argument
            let Some(request) = ack_request.request else {
                return Err(Status::invalid_argument("request is empty"));
            };

            let Some(offset) = request.offset else {
                return Err(Status::invalid_argument("offset is not present"));
            };

            self.handler
                .ack(Offset {
                    offset: offset.clone().offset,
                    partition_id: offset.partition_id,
                })
                .await;
        }
        Ok(Response::new(AckResponse {
            result: Some(proto::ack_response::Result { success: Some(()) }),
        }))
    }

    async fn pending_fn(&self, _: Request<()>) -> Result<Response<proto::PendingResponse>, Status> {
        // invoke the user-defined source's pending handler
        let pending = self.handler.pending().await;

        Ok(Response::new(proto::PendingResponse {
            result: Some(proto::pending_response::Result {
                count: pending as i64,
            }),
        }))
    }

    async fn partitions_fn(
        &self,
        _request: Request<()>,
    ) -> Result<Response<proto::PartitionsResponse>, Status> {
        let partitions = self.handler.partitions().await.unwrap_or_else(|| {
            vec![std::env::var("NUMAFLOW_REPLICA")
                .unwrap_or_default()
                .parse::<i32>()
                .unwrap_or_default()]
        });
        Ok(Response::new(proto::PartitionsResponse {
            result: Some(proto::partitions_response::Result { partitions }),
        }))
    }

    async fn is_ready(&self, _: Request<()>) -> Result<Response<proto::ReadyResponse>, Status> {
        Ok(Response::new(proto::ReadyResponse { ready: true }))
    }
}

/// Message is the response from the user's [`Sourcer::read`]
pub struct Message {
    /// The value passed to the next vertex.
    pub value: Vec<u8>,
    /// Offset of the message. When the message is acked, the offset is passed to the user's [`Sourcer::ack`].
    pub offset: Offset,
    /// The time at which the message was generated.
    pub event_time: DateTime<Utc>,
    /// Keys of the message.
    pub keys: Vec<String>,
    /// Headers of the message.
    pub headers: HashMap<String, String>,
}

/// gRPC server for starting a [`Sourcer`] service
#[derive(Debug)]
pub struct Server<T> {
    sock_addr: PathBuf,
    max_message_size: usize,
    server_info_file: PathBuf,
    svc: Option<T>,
}

impl<T> Server<T> {
    /// Creates a new gRPC `Server` instance
    pub fn new(source_svc: T) -> Self {
        Server {
            sock_addr: DEFAULT_SOCK_ADDR.into(),
            max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
            server_info_file: DEFAULT_SERVER_INFO_FILE.into(),
            svc: Some(source_svc),
        }
    }

    /// Set the unix domain socket file path used by the gRPC server to listen for incoming connections.
    /// Default value is `/var/run/numaflow/source.sock`
    pub fn with_socket_file(mut self, file: impl Into<PathBuf>) -> Self {
        self.sock_addr = file.into();
        self
    }

    /// Get the unix domain socket file path where gRPC server listens for incoming connections. Default value is `/var/run/numaflow/source.sock`
    pub fn socket_file(&self) -> &std::path::Path {
        self.sock_addr.as_path()
    }

    /// Set the maximum size of an encoded and decoded gRPC message. The value of `message_size` is in bytes. Default value is 64MB.
    pub fn with_max_message_size(mut self, message_size: usize) -> Self {
        self.max_message_size = message_size;
        self
    }

    /// Get the maximum size of an encoded and decoded gRPC message in bytes. Default value is 64MB.
    pub fn max_message_size(&self) -> usize {
        self.max_message_size
    }

    /// Change the file in which numflow server information is stored on start up to the new value. Default value is `/var/run/numaflow/sourcer-server-info`
    pub fn with_server_info_file(mut self, file: impl Into<PathBuf>) -> Self {
        self.server_info_file = file.into();
        self
    }

    /// Get the path to the file where numaflow server info is stored. Default value is `/var/run/numaflow/sourcer-server-info`
    pub fn server_info_file(&self) -> &std::path::Path {
        self.server_info_file.as_path()
    }

    /// Starts the gRPC server. When message is received on the `shutdown` channel, graceful shutdown of the gRPC server will be initiated.
    pub async fn start_with_shutdown(
        &mut self,
        shutdown_rx: oneshot::Receiver<()>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        T: Sourcer + Send + Sync + 'static,
    {
        let listener = shared::create_listener_stream(
            &self.sock_addr,
            &self.server_info_file,
            shared::ServerInfo::default(),
        )?;
        let handler = self.svc.take().unwrap();
        let (internal_shutdown_tx, internal_shutdown_rx) = mpsc::channel(1);
        let cln_token = CancellationToken::new();

        let source_service = SourceService {
            handler: Arc::new(handler),
            shutdown_tx: internal_shutdown_tx,
            cancellation_token: cln_token.clone(),
        };

        let source_svc = proto::source_server::SourceServer::new(source_service)
            .max_decoding_message_size(self.max_message_size)
            .max_decoding_message_size(self.max_message_size);

        let shutdown = shared::shutdown_signal(internal_shutdown_rx, Some(shutdown_rx));

        // will call cancel_token.cancel() on drop of _drop_guard
        let _drop_guard = cln_token.drop_guard();

        tonic::transport::Server::builder()
            .add_service(source_svc)
            .serve_with_incoming_shutdown(listener, shutdown)
            .await?;

        Ok(())
    }

    /// Starts the gRPC server. Automatically registers singal handlers for SIGINT and SIGTERM and initiates graceful shutdown of gRPC server when either one of the singal arrives.
    pub async fn start(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        T: Sourcer + Send + Sync + 'static,
    {
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        self.start_with_shutdown(shutdown_rx).await
    }
}

impl<C> Drop for Server<C> {
    // Cleanup the socket file when the server is dropped so that when the server is restarted, it can bind to the
    // same address. UnixListener doesn't implement Drop trait, so we have to manually remove the socket file.
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.sock_addr);
    }
}

#[cfg(test)]
mod tests {
    use super::{proto, Message, Offset, SourceReadRequest};
    use crate::source;
    use chrono::Utc;
    use std::collections::{HashMap, HashSet};
    use std::error::Error;
    use std::time::Duration;
    use std::vec;
    use tempfile::TempDir;
    use tokio::net::UnixStream;
    use tokio::sync::mpsc::Sender;
    use tokio::sync::{mpsc, oneshot};
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::transport::Uri;
    use tonic::Request;
    use tower::service_fn;
    use uuid::Uuid;

    // A source that repeats the `num` for the requested count
    struct Repeater {
        num: usize,
        yet_to_ack: std::sync::RwLock<HashSet<String>>,
    }

    impl Repeater {
        fn new(num: usize) -> Self {
            Self {
                num,
                yet_to_ack: std::sync::RwLock::new(HashSet::new()),
            }
        }
    }

    #[tonic::async_trait]
    impl source::Sourcer for Repeater {
        async fn read(&self, request: SourceReadRequest, transmitter: Sender<Message>) {
            let event_time = Utc::now();
            let mut message_offsets = Vec::with_capacity(request.count);

            for i in 0..request.count {
                let mut headers = HashMap::new();
                headers.insert(String::from("x-txn-id"), String::from(Uuid::new_v4()));
                // we assume timestamp in nanoseconds would be unique on each read operation from our source
                let offset = format!("{}-{}", event_time.timestamp_nanos_opt().unwrap(), i);
                transmitter
                    .send(Message {
                        value: self.num.to_le_bytes().to_vec(),
                        event_time,
                        offset: Offset {
                            offset: offset.clone().into_bytes(),
                            partition_id: 0,
                        },
                        keys: vec![],
                        headers,
                    })
                    .await
                    .unwrap();
                message_offsets.push(offset)
            }
            self.yet_to_ack.write().unwrap().extend(message_offsets)
        }

        async fn ack(&self, offset: Offset) {
            self.yet_to_ack
                .write()
                .unwrap()
                .remove(&String::from_utf8(offset.offset).unwrap());
        }

        async fn pending(&self) -> usize {
            // The pending function should return the number of pending messages that can be read from the source.
            // However, for this source the pending messages will always be 0.
            // For testing purposes, we return the number of messages that are not yet acknowledged as pending.
            self.yet_to_ack.read().unwrap().len()
        }

        async fn partitions(&self) -> Option<Vec<i32>> {
            Some(vec![2])
        }
    }

    #[tokio::test]
    async fn source_server() -> Result<(), Box<dyn Error>> {
        let tmp_dir = TempDir::new()?;
        let sock_file = tmp_dir.path().join("source.sock");
        let server_info_file = tmp_dir.path().join("sourcer-server-info");

        let mut server = source::Server::new(Repeater::new(8))
            .with_server_info_file(&server_info_file)
            .with_socket_file(&sock_file)
            .with_max_message_size(10240);

        assert_eq!(server.max_message_size(), 10240);
        assert_eq!(server.server_info_file(), server_info_file);
        assert_eq!(server.socket_file(), sock_file);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move { server.start_with_shutdown(shutdown_rx).await });

        tokio::time::sleep(Duration::from_millis(50)).await;

        // https://github.com/hyperium/tonic/blob/master/examples/src/uds/client.rs
        // https://github.com/hyperium/tonic/blob/master/examples/src/uds/client.rs
        let channel = tonic::transport::Endpoint::try_from("http://[::]:50051")?
            .connect_with_connector(service_fn(move |_: Uri| {
                // https://rust-lang.github.io/async-book/03_async_await/01_chapter.html#async-lifetimes
                let sock_file = sock_file.clone();
                async move {
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                        UnixStream::connect(sock_file).await?,
                    ))
                }
            }))
            .await?;

        let mut client = proto::source_client::SourceClient::new(channel);

        // Test read_fn with bidirectional streaming
        let (read_tx, read_rx) = mpsc::channel(4);
        let read_request = proto::ReadRequest {
            request: Some(proto::read_request::Request {
                num_records: 5,
                timeout_in_ms: 1000,
            }),
        };
        read_tx.send(read_request).await.unwrap();
        drop(read_tx); // Close the sender to indicate no more requests

        let mut response_stream = client
            .read_fn(Request::new(ReceiverStream::new(read_rx)))
            .await?
            .into_inner();
        let mut response_values = Vec::new();

        while let Some(response) = response_stream.message().await? {
            if let Some(status) = response.status {
                if status.eot {
                    break;
                }
            }

            if let Some(result) = response.result {
                response_values.push(result);
            }
        }
        assert_eq!(response_values.len(), 5);

        // Test pending_fn
        let pending_before_ack = client.pending_fn(Request::new(())).await?.into_inner();
        assert_eq!(pending_before_ack.result.unwrap().count, 5);

        // Test ack_fn with client-side streaming
        let (ack_tx, ack_rx) = mpsc::channel(10);
        for resp in response_values.iter() {
            let ack_request = proto::AckRequest {
                request: Some(proto::ack_request::Request {
                    offset: Some(proto::Offset {
                        offset: resp.offset.clone().unwrap().offset,
                        partition_id: resp.offset.clone().unwrap().partition_id,
                    }),
                }),
            };
            ack_tx.send(ack_request).await.unwrap();
        }
        drop(ack_tx); // Close the sender to indicate no more requests

        let ack_response = client
            .ack_fn(Request::new(ReceiverStream::new(ack_rx)))
            .await?
            .into_inner();
        assert!(ack_response.result.unwrap().success.is_some());

        let pending_after_ack = client.pending_fn(Request::new(())).await?.into_inner();
        assert_eq!(pending_after_ack.result.unwrap().count, 0);

        let partitions = client.partitions_fn(Request::new(())).await?.into_inner();
        assert_eq!(partitions.result.unwrap().partitions, vec![2]);

        shutdown_tx.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(task.is_finished(), "gRPC server is still running");
        Ok(())
    }
}
