use std::cell::{Cell, RefCell};
use std::future::poll_fn;
use std::rc::Rc;
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use h2::server::SendResponse;
use h2::{RecvStream, SendStream};
use http::{HeaderMap, HeaderValue, Request, Response};
use hyper_util::rt::TokioIo;
use orchard::ValuePool;
use prost::Message;
use tokio::io::DuplexStream;
use tokio::sync::{Notify, mpsc};
use tokio::task::spawn_local;
use tokio::time::{Instant, sleep, timeout};
use tonic::transport::Endpoint;
use zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;
use zcash_client_backend::proto::service::{
    BlockId, BlockRange, GetSubtreeRootsArg, LightdInfo, RawTransaction, SendResponse as Sent,
    ShieldedProtocol, TxFilter,
};
use zcash_protocol::consensus::NetworkType;
use zecswap_chain::zcash::Lightwalletd;

use super::ledger::{Ledger, Outcome};
use crate::seed::Seed;

pub const CALL: u64 = 120;
pub const IDLE: u64 = 30;
const SLACK: u64 = 5;
const UNAVAILABLE: u32 = 14;
const NOT_FOUND: u32 = 5;
const UNIMPLEMENTED: u32 = 12;

#[derive(Clone, Copy, Debug)]
pub struct Service {
    pub errors: u64,
    pub slow: u64,
    pub stalls: u64,
    pub cuts: u64,
    pub lags: u64,
    pub deaf: u64,
    pub hangups: u64,
    pub misplaces: u64,
    pub backwards: bool,
    pub another: bool,
    pub quiet: bool,
}

impl Service {
    pub const HONEST: Self = Self {
        errors: 0,
        slow: 0,
        stalls: 0,
        cuts: 0,
        lags: 0,
        deaf: 0,
        hangups: 0,
        misplaces: 0,
        backwards: false,
        another: false,
        quiet: true,
    };
}

pub struct Server {
    ledger: Ledger,
    seed: Seed,
    service: Service,
    clears: Instant,
    calls: Cell<u64>,
    cleared: Cell<bool>,
    faults: Cell<u64>,
    turn: Cell<u64>,
    told: Cell<(u32, [u8; 32])>,
    most: Cell<u32>,
    stated: Cell<bool>,
    misplaced: Cell<u64>,
    failures: RefCell<Vec<String>>,
}

type Reply = Result<Vec<Bytes>, (u32, &'static str)>;

fn frame(message: &impl Message) -> Bytes {
    let mut out = BytesMut::with_capacity(5 + message.encoded_len());
    out.put_u8(0);
    out.put_u32(message.encoded_len() as u32);
    message.encode(&mut out).expect("room for the message");
    out.freeze()
}

fn status(code: u32, message: &str) -> HeaderMap {
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", HeaderValue::from(code));
    if !message.is_empty() {
        let message = HeaderValue::from_str(message).expect("a plain message");
        trailers.insert("grpc-message", message);
    }
    trailers
}

fn headers() -> Response<()> {
    Response::builder()
        .status(200)
        .header("content-type", "application/grpc")
        .body(())
        .expect("a response")
}

async fn read(mut body: RecvStream) -> Option<Bytes> {
    let mut all = BytesMut::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.ok()?;
        let _ = body.flow_control().release_capacity(chunk.len());
        all.put(chunk);
    }
    (all.len() >= 5).then(|| all.freeze().slice(5..))
}

impl Server {
    pub fn new(ledger: Ledger, seed: Seed, service: Service, clears: Instant) -> Rc<Self> {
        let told = ledger.tip();
        Rc::new(Self {
            ledger,
            seed,
            service,
            clears,
            calls: Cell::new(0),
            cleared: Cell::new(false),
            faults: Cell::new(0),
            turn: Cell::new(0),
            told: Cell::new(told),
            most: Cell::new(0),
            stated: Cell::new(false),
            misplaced: Cell::new(0),
            failures: RefCell::default(),
        })
    }

    pub fn told(&self) -> (u32, [u8; 32]) {
        self.told.get()
    }

    pub fn failures(&self) -> Vec<String> {
        self.failures.borrow().clone()
    }

    pub fn connect(self: &Rc<Self>) -> Lightwalletd {
        let (dial, mut dialled) = mpsc::unbounded_channel::<DuplexStream>();
        let server = self.clone();
        spawn_local(async move {
            while let Some(io) = dialled.recv().await {
                spawn_local(server.clone().connection(io));
            }
        });
        let connector = tower::service_fn(move |_: http::Uri| {
            let (near, far) = tokio::io::duplex(1 << 16);
            let sent = dial.send(far);
            async move {
                sent.map_err(|_| std::io::Error::other("lightwalletd is gone"))?;
                Ok::<_, std::io::Error>(TokioIo::new(near))
            }
        });
        let channel = Endpoint::from_static("http://lightwalletd.sim")
            .timeout(Duration::from_secs(CALL))
            .connect_with_connector_lazy(connector);
        CompactTxStreamerClient::new(channel.into())
    }

    async fn connection(self: Rc<Self>, io: DuplexStream) {
        let Ok(mut connection) = h2::server::handshake(io).await else {
            return;
        };
        let hangup = Rc::new(Notify::new());
        loop {
            tokio::select! {
                accepted = connection.accept() => match accepted {
                    Some(Ok((request, respond))) => {
                        spawn_local(self.clone().answer(request, respond, hangup.clone()));
                    }
                    _ => return,
                },
                () = hangup.notified() => return,
            }
        }
    }

    pub fn clear(&self) {
        self.cleared.set(true);
    }

    fn rough(&self) -> bool {
        Instant::now() < self.clears && !self.cleared.get()
    }

    pub fn misplaced(&self) -> u64 {
        self.misplaced.get()
    }

    pub fn faults(&self) -> u64 {
        self.faults.get()
    }

    fn rolls(&self, what: &str, call: u64, odds: u64) -> bool {
        let rolled = self.rough() && self.seed.chance(what, call, odds);
        self.faults.set(self.faults.get() + u64::from(rolled));
        rolled
    }

    async fn answer(
        self: Rc<Self>,
        request: Request<RecvStream>,
        mut respond: SendResponse<Bytes>,
        hangup: Rc<Notify>,
    ) {
        let call = self.calls.get();
        self.calls.set(call + 1);
        let path = request.uri().path().to_owned();
        let method = path.rsplit('/').next().unwrap_or_default();
        let Some(body) = read(request.into_body()).await else {
            return;
        };
        if self.rolls("lwd-slow", call, self.service.slow) {
            let wait = self.seed.between("lwd-slow-for", call, 1, 25);
            sleep(Duration::from_secs(wait)).await;
        }
        if self.rolls("lwd-hangup", call, self.service.hangups) {
            self.ledger
                .note(format!("lightwalletd hung up on {method}"));
            hangup.notify_one();
            return;
        }
        if self.rolls("lwd-error", call, self.service.errors) {
            self.ledger.note(format!("lightwalletd failed {method}"));
            let _ = respond.send_response(
                Response::builder()
                    .status(200)
                    .header("content-type", "application/grpc")
                    .header("grpc-status", UNAVAILABLE)
                    .body(())
                    .expect("a response"),
                true,
            );
            return;
        }
        let stall = self
            .rolls("lwd-stall", call, self.service.stalls)
            .then(|| self.seed.below("lwd-stall-kind", call, 3));
        if stall == Some(0) {
            self.ledger
                .note(format!("lightwalletd never answered {method}"));
            let gone = poll_fn(|cx| respond.poll_reset(cx));
            if timeout(Duration::from_secs(CALL + SLACK), gone)
                .await
                .is_err()
            {
                self.late(method, CALL);
            }
            return;
        }
        let reply = self.reply(method, body, call);
        let Ok(mut stream) = respond.send_response(headers(), false) else {
            return;
        };
        if let Some(kind) = stall {
            self.ledger
                .note(format!("lightwalletd stalled {method} in way {kind}"));
            return self.stall(method, &mut stream, kind, &reply).await;
        }
        let (messages, end) = match reply {
            Ok(messages) => (messages, status(0, "")),
            Err((code, why)) => (Vec::new(), status(code, why)),
        };
        let cut = method == "GetBlockRange" && self.rolls("lwd-cut", call, self.service.cuts);
        let keep = match cut {
            true => self
                .seed
                .below("lwd-cut-after", call, messages.len() as u64) as usize,
            false => messages.len(),
        };
        for message in messages.into_iter().take(keep) {
            if stream.send_data(message, false).is_err() {
                return;
            }
        }
        if cut {
            self.ledger
                .note(format!("lightwalletd cut {method} after {keep} blocks"));
        }
        let _ = stream.send_trailers(match cut {
            true => status(UNAVAILABLE, "cut"),
            false => end,
        });
    }

    fn late(&self, method: &str, limit: u64) {
        self.failures.borrow_mut().push(format!(
            "a call to {method} was still open {}s after lightwalletd went silent, past the {limit}s it may wait",
            limit + SLACK
        ));
    }

    async fn stall(&self, method: &str, stream: &mut SendStream<Bytes>, kind: u64, reply: &Reply) {
        let started = Instant::now();
        let (limit, every) = match kind {
            1 => (IDLE, None),
            _ => (CALL, Some(20)),
        };
        let whole = match reply {
            Ok(messages) => messages.first().cloned().unwrap_or_default(),
            Err(_) => Bytes::new(),
        };
        let mut sent = 0;
        loop {
            let left = (limit + SLACK).saturating_sub(started.elapsed().as_secs());
            if left == 0 {
                return self.late(method, limit);
            }
            let wait = Duration::from_secs(every.unwrap_or(left).min(left));
            let gone = poll_fn(|cx| stream.poll_reset(cx));
            if timeout(wait, gone).await.is_ok() {
                return;
            }
            let piece = match kind {
                2 if sent + 1 < whole.len() => {
                    sent += 1;
                    Some(whole.slice(sent - 1..sent))
                }
                _ => None,
            };
            if piece.is_some_and(|piece| stream.send_data(piece, false).is_err()) {
                return;
            }
        }
    }

    fn reply(&self, method: &str, body: Bytes, call: u64) -> Reply {
        let malformed = |_| (UNIMPLEMENTED, "malformed");
        let stated = self.stated.replace(method == "GetTreeState");
        match method {
            "GetLightdInfo" => {
                let real = self.ledger.kind();
                let other = match real {
                    NetworkType::Main => NetworkType::Test,
                    _ => NetworkType::Main,
                };
                let name = match if self.service.another { other } else { real } {
                    NetworkType::Main => "main",
                    _ => "test",
                };
                Ok(vec![frame(&LightdInfo {
                    chain_name: name.into(),
                    block_height: u64::from(self.ledger.tip().0),
                    ..Default::default()
                })])
            }
            "GetLatestBlock" => {
                let turn = self.turn.get();
                self.turn.set(turn + 1);
                let rolled = self.rolls("lwd-lag", call, self.service.lags);
                let lag = match (self.service.backwards && self.rough(), rolled) {
                    (true, _) => 2 * (turn % 2) as u32,
                    (false, true) => self.seed.between("lwd-lag-by", call, 1, 2) as u32,
                    (false, false) => 0,
                };
                let floor = match self.service.backwards {
                    true => 0,
                    false => self.most.get(),
                };
                let (height, hash) = self.ledger.latest(lag, floor);
                let behind = self.ledger.tip().0 - height;
                if behind > 0 {
                    self.ledger
                        .note(format!("lightwalletd named a tip {behind} behind"));
                }
                self.told.set((height, hash));
                self.most.set(self.most.get().max(height));
                Ok(vec![frame(&BlockId {
                    height: u64::from(height),
                    hash: hash.to_vec(),
                })])
            }
            "GetBlockRange" => {
                let range = BlockRange::decode(body).map_err(malformed)?;
                let end = |id: Option<BlockId>| id.map_or(0, |id| id.height as u32);
                let (start, end) = (end(range.start), end(range.end));
                let blocks = (start..=end).map(|height| self.ledger.compact(height));
                let blocks: Option<Vec<_>> = blocks.collect();
                let blocks = blocks.ok_or((NOT_FOUND, "no such block"))?;
                Ok(blocks.iter().map(frame).collect())
            }
            "GetTreeState" => {
                let asked = BlockId::decode(body).map_err(malformed)?.height as u32;
                let misplaced = stated && self.rolls("lwd-misplaces", call, self.service.misplaces);
                let height = match misplaced.then(|| self.seed.below("lwd-misplaced", call, 3)) {
                    None => asked,
                    Some(0) => asked - 1,
                    Some(1) => asked + 1,
                    Some(_) => self.ledger.tip().0,
                };
                if height != asked {
                    self.misplaced.set(self.misplaced.get() + 1);
                    self.ledger
                        .note("lightwalletd answered the tree state of another height".into());
                }
                let state = self.ledger.tree_state(height);
                Ok(vec![frame(&state.ok_or((NOT_FOUND, "no such block"))?)])
            }
            "GetSubtreeRoots" => {
                let asked = GetSubtreeRootsArg::decode(body).map_err(malformed)?;
                let pool = match asked.shielded_protocol() {
                    ShieldedProtocol::Sapling => return Ok(Vec::new()),
                    ShieldedProtocol::Orchard => ValuePool::Orchard,
                    ShieldedProtocol::Ironwood => ValuePool::Ironwood,
                };
                let roots = self.ledger.shard_roots(pool, asked.start_index);
                Ok(roots.iter().map(frame).collect())
            }
            "SendTransaction" => {
                let raw = RawTransaction::decode(body).map_err(malformed)?;
                let outcome = self.ledger.accept(&raw.data, self.service.quiet);
                if self.rolls("lwd-deaf", call, self.service.deaf) {
                    self.ledger
                        .note("lightwalletd lost its answer to the broadcast".into());
                    return Err((UNAVAILABLE, "lost"));
                }
                Ok(vec![frame(&match outcome {
                    Outcome::Took => Sent::default(),
                    Outcome::Refused(why) => Sent {
                        error_code: why.code(),
                        error_message: format!("{why:?}"),
                    },
                })])
            }
            "GetTransaction" => {
                let filter = TxFilter::decode(body).map_err(malformed)?;
                let txid: [u8; 32] = filter.hash.try_into().map_err(|_| (NOT_FOUND, "unknown"))?;
                let held = self.ledger.holds(&txid);
                let held = held.map(|(data, mined)| (data, mined.map_or(0, u64::from)));
                let orphan = self.ledger.orphan(&txid).map(|data| (data, u64::MAX));
                let (data, height) = held.or(orphan).ok_or((NOT_FOUND, "unknown"))?;
                Ok(vec![frame(&RawTransaction { data, height })])
            }
            _ => Err((UNIMPLEMENTED, "unimplemented")),
        }
    }
}
