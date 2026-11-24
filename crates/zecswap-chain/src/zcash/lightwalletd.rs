use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use http_body::{Body, Frame};
use tokio::time::{Instant, Sleep};
use tonic::body::Body as TonicBody;
use tonic::codegen::{Bytes, http};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tower_service::Service;
use zcash_client_backend::proto::service::{
    BlockId, ChainSpec, Empty, RawTransaction, SendResponse, TreeState, TxFilter,
    compact_tx_streamer_client::CompactTxStreamerClient,
};
use zcash_protocol::TxId;
use zcash_protocol::consensus::{BlockHeight, NetworkType};

use crate::Error;

const STREAM_IDLE: Duration = Duration::from_secs(30);
const RPC_TOTAL: Duration = Duration::from_secs(120);

pub type Lightwalletd = CompactTxStreamerClient<DeadlineChannel>;

#[derive(Clone)]
pub struct DeadlineChannel(Channel);

impl From<Channel> for DeadlineChannel {
    fn from(channel: Channel) -> Self {
        Self(channel)
    }
}

impl Service<http::Request<TonicBody>> for DeadlineChannel {
    type Response = http::Response<TonicBody>;
    type Error = tonic::transport::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<TonicBody>) -> Self::Future {
        let deadline = Instant::now() + RPC_TOTAL;
        let response = self.0.call(request);
        Box::pin(async move {
            Ok(response
                .await?
                .map(|body| TonicBody::new(DeadlineBody::new(body, deadline))))
        })
    }
}

struct DeadlineBody {
    inner: TonicBody,
    idle: Pin<Box<Sleep>>,
    total: Pin<Box<Sleep>>,
    ended: bool,
}

impl DeadlineBody {
    fn new(inner: TonicBody, deadline: Instant) -> Self {
        Self {
            inner,
            idle: Box::pin(tokio::time::sleep(STREAM_IDLE)),
            total: Box::pin(tokio::time::sleep_until(deadline)),
            ended: false,
        }
    }
}

impl Body for DeadlineBody {
    type Data = Bytes;
    type Error = tonic::Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.ended {
            return Poll::Ready(None);
        }
        if self.total.as_mut().poll(cx).is_ready() || self.idle.as_mut().poll(cx).is_ready() {
            self.ended = true;
            return Poll::Ready(Some(Err(tonic::Status::deadline_exceeded(
                "lightwalletd response deadline exceeded",
            ))));
        }
        match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if frame.data_ref().is_some_and(|data| !data.is_empty()) {
                    self.idle.as_mut().reset(Instant::now() + STREAM_IDLE);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(result) => {
                self.ended = true;
                Poll::Ready(result)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub async fn connect(url: &str) -> Result<Lightwalletd, Error> {
    Ok(CompactTxStreamerClient::new(
        endpoint(url)?.connect().await?.into(),
    ))
}

pub fn connect_lazy(url: &str) -> Result<Lightwalletd, Error> {
    Ok(CompactTxStreamerClient::new(
        endpoint(url)?.connect_lazy().into(),
    ))
}

fn endpoint(url: &str) -> Result<Endpoint, Error> {
    let mut endpoint = Endpoint::from_shared(url.to_owned())
        .map_err(|e| Error::Config(format!("lightwalletd URL {url}: {e}")))?
        .connect_timeout(Duration::from_secs(15))
        .timeout(RPC_TOTAL)
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_timeout(Duration::from_secs(20))
        .keep_alive_while_idle(true);
    if url.starts_with("https://") {
        endpoint = endpoint.tls_config(ClientTlsConfig::new().with_webpki_roots())?;
    }
    Ok(endpoint)
}

pub(crate) async fn check_network(
    client: &mut Lightwalletd,
    network: NetworkType,
) -> Result<(), Error> {
    let expected = match network {
        NetworkType::Main => "main",
        NetworkType::Test => "test",
        NetworkType::Regtest => "regtest",
    };
    let served = client
        .get_lightd_info(Empty {})
        .await?
        .into_inner()
        .chain_name;
    if served != expected {
        return Err(Error::Config(format!(
            "lightwalletd serves Zcash's {served} network, not {expected}"
        )));
    }
    Ok(())
}

pub(crate) async fn chain_tip(client: &mut Lightwalletd) -> Result<BlockHeight, Error> {
    let block = client
        .get_latest_block(ChainSpec::default())
        .await?
        .into_inner();
    u32::try_from(block.height)
        .map(BlockHeight::from_u32)
        .map_err(|_| Error::Wallet(format!("implausible chain tip {}", block.height)))
}

pub(crate) async fn tree_state(
    client: &mut Lightwalletd,
    height: BlockHeight,
) -> Result<TreeState, Error> {
    let block = BlockId {
        height: u32::from(height).into(),
        hash: vec![],
    };
    Ok(client.get_tree_state(block).await?.into_inner())
}

pub(crate) async fn broadcast(
    client: &mut Lightwalletd,
    txid: TxId,
    data: Vec<u8>,
) -> Result<(), Error> {
    let response = client
        .send_transaction(RawTransaction {
            data: data.clone(),
            height: 0,
        })
        .await?
        .into_inner();
    confirm_submission(response, &data, async {
        client
            .get_transaction(TxFilter {
                hash: txid.as_ref().to_vec(),
                ..Default::default()
            })
            .await
            .map(|response| response.into_inner())
    })
    .await
}

async fn confirm_submission(
    response: SendResponse,
    data: &[u8],
    lookup: impl Future<Output = Result<RawTransaction, tonic::Status>>,
) -> Result<(), Error> {
    if response.error_code == 0 {
        return Ok(());
    }
    if let Ok(Ok(known)) = tokio::time::timeout(Duration::from_secs(5), lookup).await
        && !data.is_empty()
        && known.data == data
        && known.height != u64::MAX
    {
        return Ok(());
    }
    Err(Error::Rejected {
        code: response.error_code,
        message: response.error_message,
    })
}
