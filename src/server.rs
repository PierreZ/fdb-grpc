use crate::pb::gateway_server::Gateway;
use crate::pb::request::RequestType;
use crate::pb::response::ResponseType;
use crate::pb::Error;
use crate::pb::GetReadVersionResponse;
use crate::pb::{Request as FDBRequest, Response as FDBResponse};
use async_stream::try_stream;
use foundationdb::{Database, FdbError};
use futures::Stream;
use futures_util::TryStreamExt;
use std::fmt::{Debug, Formatter};
use std::pin::Pin;
use std::time::Duration;
use std::time::SystemTime;
use tonic::Code;
use tonic::{Request, Response, Status, Streaming};
use tracing::{event, span, Level};

pub struct FDBGateway {
    db: Database,
}

impl Debug for FDBGateway {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "FDBGateway")
    }
}

impl FDBGateway {
    pub fn new(path: &str) -> Result<Self, FdbError> {
        let db = foundationdb::Database::from_path(path)?;

        Ok(FDBGateway { db })
    }
}

type FDBResult<T> = Result<Response<T>, Status>;
type ResponseStream = Pin<Box<dyn Stream<Item = Result<FDBResponse, Status>> + Send + Sync>>;

#[tonic::async_trait]
impl Gateway for FDBGateway {
    type OpenTransactionStream = ResponseStream;

    #[tracing::instrument]
    async fn open_transaction(
        &self,
        req: Request<Streaming<FDBRequest>>,
    ) -> FDBResult<Self::OpenTransactionStream> {
        let mut stream = req.into_inner();

        let trx = match self.db.create_trx() {
            Ok(trx) => trx,
            Err(fdb_error) => {
                let status = Status::new(Code::from(fdb_error.code()), "could not create trx");
                return Err(status);
            }
        };

        event!(Level::INFO, "transaction created");

        let started_transaction_time = SystemTime::now();

        let stream = try_stream! {
            while let Some(msg) = stream.try_next().await? {
                let span = span!(Level::INFO, "handling message",  request_type = ?msg.request_type);
                let _enter = span.enter();

                match msg.request_type {
                    None => {
                        // shut while if trx time > 5s
                        let since = started_transaction_time
                            .duration_since(SystemTime::now())
                            .unwrap()
                            .as_millis();
                        if (since > Duration::new(5, 0).as_millis()) {
                            break;
                        }
                    }
                    // handle ReadVersion
                    Some(RequestType::GetReadVersion(_)) => {

                        let _enter = span.enter();
                        match trx.get_read_version().await {
                            Ok(read_version) => {
                                event!(Level::INFO, "retrieved read_version");
                                let response_type = Some(ResponseType::GetReadVersionResponse(
                                    GetReadVersionResponse {
                                        read_version: read_version as u64,
                                    }
                                ));
                                yield FDBResponse{response_type, error: None};
                            },
                            Err(fdb_error) => yield FDBResponse{
                                response_type: None,
                                error: Some(Error{error_code: fdb_error.code()})
                            }
                        }
                    },

                    _ => continue,
                }
            }

        };

        Ok(Response::new(
            Box::pin(stream) as Self::OpenTransactionStream
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pb::gateway_client::GatewayClient;
    use crate::pb::gateway_server::GatewayServer;
    use crate::pb::ReadVersionRequest;
    use tokio::sync::mpsc;
    use tonic::transport::Server;

    #[tokio::test]
    async fn integration_test() -> Result<(), Box<dyn std::error::Error>> {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .init();

        let _network = unsafe { foundationdb::boot() };

        let addr = "127.0.0.1:50051".parse()?;
        let server = FDBGateway::new("/etc/foundationdb/fdb.cluster")?;

        tokio::spawn({
            async move {
                Server::builder()
                    .trace_fn(|_| tracing::info_span!("fdbGateway"))
                    .add_service(GatewayServer::new(server))
                    .serve(addr)
                    .await
                    .unwrap();
            }
        });

        let mut client = GatewayClient::connect("http://localhost:50051").await?;

        let (tx, rx) = mpsc::unbounded_channel();

        let get_read_version = FDBRequest {
            request_type: Some(RequestType::GetReadVersion(ReadVersionRequest {})),
        };

        tx.send(get_read_version)?;

        let result = client
            .open_transaction(Request::new(
                tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
            ))
            .await;

        if let Ok(mut response) = result.map(Response::into_inner) {
            assert!(response.message().await?.is_some());
        } else {
            assert!(false);
        }

        Ok(())
    }
}
