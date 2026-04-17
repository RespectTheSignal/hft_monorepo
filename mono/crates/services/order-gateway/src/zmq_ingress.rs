//! strategy ZMQ PUSH → order-gateway PULL ingress.
//!
//! 128B [`hft_protocol::order_wire::OrderRequestWire`] 를 받아
//! [`hft_exchange_api::OrderRequest`] 로 정규화한 뒤 기존 router channel 로 fan-in 한다.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{Sender, TrySendError};
use hft_config::{AppConfig, ShmBackendKind, ZmqConfig};
use hft_exchange_api::{CancellationToken, OrderRequest, OrderSide, OrderType, TimeInForce};
use hft_protocol::order_wire::{
    OrderRequestWire, WireError, ORDER_REQUEST_WIRE_SIZE, ORDER_TYPE_LIMIT, ORDER_TYPE_MARKET,
    TIF_FOK, TIF_GTC, TIF_IOC,
};
use hft_shm::{exchange_from_u8, Backing, LayoutSpec, Role, SharedRegion, SubKind, SymbolTable};
use hft_telemetry::{counter_inc, CounterKey};
use hft_types::Symbol;
use hft_zmq::Context as ZmqContext;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::{IngressEnvelope, IngressMeta};

#[derive(Debug)]
enum IngressDecodeError {
    Invalid(anyhow::Error),
    UnknownSymbol(u32),
}

impl From<WireError> for IngressDecodeError {
    fn from(value: WireError) -> Self {
        Self::Invalid(anyhow!(value))
    }
}

fn build_v2_backing(cfg: &AppConfig) -> Result<Backing> {
    match cfg.shm.backend {
        ShmBackendKind::DevShm => Ok(Backing::DevShm {
            path: cfg.shm.shared_path.clone(),
        }),
        ShmBackendKind::Hugetlbfs => Ok(Backing::Hugetlbfs {
            path: cfg.shm.shared_path.clone(),
        }),
        ShmBackendKind::PciBar => Ok(Backing::PciBar {
            path: cfg.shm.shared_path.clone(),
        }),
        ShmBackendKind::LegacyMultiFile => {
            anyhow::bail!("legacy_multi_file backend does not use shared-region backing")
        }
    }
}

fn build_layout_spec(cfg: &AppConfig) -> LayoutSpec {
    LayoutSpec {
        quote_slot_count: cfg.shm.quote_slot_count,
        trade_ring_capacity: cfg.shm.trade_ring_capacity,
        symtab_capacity: cfg.shm.symbol_table_capacity,
        order_ring_capacity: cfg.shm.order_ring_capacity,
        n_max: cfg.shm.n_max,
    }
}

/// order-gateway ZMQ ingress 가 사용할 symbol table 을 연다.
pub fn open_order_ingress_symtab(cfg: &AppConfig) -> Result<Arc<SymbolTable>> {
    match cfg.shm.backend {
        ShmBackendKind::LegacyMultiFile => {
            let symtab =
                SymbolTable::open_or_create(&cfg.shm.symtab_path, cfg.shm.symbol_table_capacity)
                    .with_context(|| {
                        format!(
                            "SymbolTable::open_or_create({})",
                            cfg.shm.symtab_path.display()
                        )
                    })?;
            Ok(Arc::new(symtab))
        }
        ShmBackendKind::DevShm | ShmBackendKind::Hugetlbfs | ShmBackendKind::PciBar => {
            let backing = build_v2_backing(cfg)?;
            let spec = build_layout_spec(cfg);
            let shared = SharedRegion::open_view(backing, spec, Role::OrderGateway)
                .context("SharedRegion::open_view(order ingress symtab)")?;
            let sub = shared
                .sub_region(SubKind::Symtab)
                .context("sub_region(Symtab)")?;
            let symtab = SymbolTable::open_from_region(sub)
                .map_err(|e| anyhow!("SymbolTable::open_from_region: {e}"))?;
            Ok(Arc::new(symtab))
        }
    }
}

fn wire_text_tag(wire: &OrderRequestWire) -> Result<&str, IngressDecodeError> {
    let len = wire
        .text_tag
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(wire.text_tag.len());
    std::str::from_utf8(&wire.text_tag[..len])
        .map_err(|e| IngressDecodeError::Invalid(anyhow!("text_tag is not valid UTF-8: {e}")))
}

fn decode_order_type(code: u8) -> Result<OrderType, IngressDecodeError> {
    match code {
        ORDER_TYPE_LIMIT => Ok(OrderType::Limit),
        ORDER_TYPE_MARKET => Ok(OrderType::Market),
        other => Err(IngressDecodeError::Invalid(anyhow!(
            "unsupported order_type={other} for OrderRequest normalization"
        ))),
    }
}

fn decode_tif(code: u8) -> Result<TimeInForce, IngressDecodeError> {
    match code {
        TIF_GTC => Ok(TimeInForce::Gtc),
        TIF_IOC => Ok(TimeInForce::Ioc),
        TIF_FOK => Ok(TimeInForce::Fok),
        other => Err(IngressDecodeError::Invalid(anyhow!(
            "unsupported tif={other} for OrderRequest normalization"
        ))),
    }
}

fn decode_side(code: u8) -> Result<OrderSide, IngressDecodeError> {
    match code {
        0 => Ok(OrderSide::Buy),
        1 => Ok(OrderSide::Sell),
        other => Err(IngressDecodeError::Invalid(anyhow!("unknown side={other}"))),
    }
}

fn normalize_wire_request(
    buf: &[u8],
    symtab: &SymbolTable,
) -> Result<IngressEnvelope, IngressDecodeError> {
    if buf.len() != ORDER_REQUEST_WIRE_SIZE {
        return Err(IngressDecodeError::Invalid(anyhow!(
            "unexpected order wire size: {}",
            buf.len()
        )));
    }

    let mut raw = [0u8; ORDER_REQUEST_WIRE_SIZE];
    raw.copy_from_slice(buf);
    let wire = OrderRequestWire::decode(&raw)?;
    let exchange = exchange_from_u8(wire.exchange as u8).ok_or_else(|| {
        IngressDecodeError::Invalid(anyhow!("invalid exchange={}", wire.exchange))
    })?;
    let (resolved_exchange, symbol_name) = symtab
        .resolve(wire.symbol_id)
        .ok_or(IngressDecodeError::UnknownSymbol(wire.symbol_id))?;
    if resolved_exchange != exchange {
        return Err(IngressDecodeError::Invalid(anyhow!(
            "symbol_id={} belongs to {:?}, not {:?}",
            wire.symbol_id,
            resolved_exchange,
            exchange
        )));
    }

    let strategy_tag = wire_text_tag(&wire)?;
    let client_id: Arc<str> = Arc::from(if strategy_tag.is_empty() {
        format!("wire-{}", wire.client_seq)
    } else {
        format!("{strategy_tag}-{}", wire.client_seq)
    });

    let order_type = decode_order_type(wire.order_type)?;
    let price = match order_type {
        OrderType::Limit => Some(wire.price),
        OrderType::Market => None,
    };

    Ok((
        OrderRequest {
            exchange,
            symbol: Symbol::new(&symbol_name),
            side: decode_side(wire.side)?,
            order_type,
            qty: wire.size as f64,
            price,
            reduce_only: wire.reduce_only(),
            tif: decode_tif(wire.tif)?,
            client_seq: wire.client_seq,
            origin_ts_ns: wire.origin_ts_ns,
            client_id,
        },
        IngressMeta {
            origin_vm_id: None,
            text_tag: wire.text_tag,
        },
    ))
}

pub fn start_zmq_order_ingress(
    endpoint: &str,
    zmq_cfg: &ZmqConfig,
    symtab: Arc<SymbolTable>,
    req_tx: Sender<IngressEnvelope>,
    cancel: CancellationToken,
) -> Result<JoinHandle<()>> {
    start_zmq_order_ingress_with_context(
        ZmqContext::new(),
        endpoint,
        zmq_cfg.clone(),
        symtab,
        req_tx,
        cancel,
    )
}

fn start_zmq_order_ingress_with_context(
    ctx: ZmqContext,
    endpoint: &str,
    zmq_cfg: ZmqConfig,
    symtab: Arc<SymbolTable>,
    req_tx: Sender<IngressEnvelope>,
    cancel: CancellationToken,
) -> Result<JoinHandle<()>> {
    let endpoint = endpoint.to_string();
    Ok(tokio::task::spawn_blocking(move || {
        let mut pull = match ctx.pull(&endpoint, &zmq_cfg) {
            Ok(sock) => sock,
            Err(e) => {
                error!(
                    target: "order_gateway::zmq_ingress",
                    endpoint = %endpoint,
                    error = %e,
                    "failed to bind PULL socket"
                );
                return;
            }
        };
        info!(
            target: "order_gateway::zmq_ingress",
            endpoint = %endpoint,
            "ZMQ order ingress started"
        );

        loop {
            if cancel.is_cancelled() {
                break;
            }

            match pull.recv_bytes_timeout(50) {
                Ok(Some(payload)) => match normalize_wire_request(&payload, &symtab) {
                    Ok((req, meta)) => match req_tx.try_send((req, meta)) {
                        Ok(()) => {}
                        Err(TrySendError::Full((req, _meta))) => {
                            counter_inc(CounterKey::OrderGatewayRouterQueueFull);
                            warn!(
                                target: "order_gateway::zmq_ingress",
                                client_id = %req.client_id,
                                "order router channel full — dropping ZMQ-ingested order"
                            );
                        }
                        Err(TrySendError::Disconnected(_)) => {
                            error!(
                                target: "order_gateway::zmq_ingress",
                                "order router channel disconnected — stopping ZMQ ingress"
                            );
                            break;
                        }
                    },
                    Err(IngressDecodeError::Invalid(err)) => {
                        counter_inc(CounterKey::OrderGatewayInvalidWire);
                        debug!(
                            target: "order_gateway::zmq_ingress",
                            error = %err,
                            "ZMQ order wire decode failed — skipping"
                        );
                    }
                    Err(IngressDecodeError::UnknownSymbol(symbol_id)) => {
                        counter_inc(CounterKey::OrderGatewayUnknownSymbol);
                        debug!(
                            target: "order_gateway::zmq_ingress",
                            symbol_id,
                            "ZMQ order wire symbol_id not found — skipping"
                        );
                    }
                },
                Ok(None) => continue,
                Err(e) => {
                    counter_inc(CounterKey::OrderGatewayInvalidWire);
                    warn!(
                        target: "order_gateway::zmq_ingress",
                        endpoint = %endpoint,
                        error = %e,
                        "ZMQ order ingress recv failed"
                    );
                }
            }
        }

        info!(target: "order_gateway::zmq_ingress", "ZMQ order ingress stopped");
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        start_with_arc, NoopExecutor, RetryPolicy, Route, RoutingTable, DEDUP_CACHE_CAP_DEFAULT,
    };
    use hft_order_egress::{OrderEgress, SubmitOutcome, ZmqOrderEgress};
    use hft_protocol::{order_request_to_order_request_wire, OrderEgressMeta, WireLevel};
    use hft_telemetry::counters_snapshot;
    use hft_types::ExchangeId;

    fn counter_value(key: &str) -> u64 {
        counters_snapshot()
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
            .unwrap_or(0)
    }

    fn sample_req() -> OrderRequest {
        OrderRequest {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            qty: 1.0,
            price: Some(100.5),
            reduce_only: true,
            tif: TimeInForce::Ioc,
            client_seq: 7,
            origin_ts_ns: 123,
            client_id: Arc::from("v8-7"),
        }
    }

    fn sample_meta() -> OrderEgressMeta<'static> {
        OrderEgressMeta {
            strategy_tag: "v8",
            level: WireLevel::Close,
            symbol_id: Some(0),
            symbol_idx: None,
            quantize: None,
        }
    }

    #[test]
    fn normalize_wire_request_keeps_reduce_only_and_client_seq() {
        let dir = tempfile::tempdir().unwrap();
        let sym_path = dir.path().join("symtab");
        let symtab = SymbolTable::open_or_create(&sym_path, 16).unwrap();
        let symbol_id = symtab.get_or_intern(ExchangeId::Gate, "BTC_USDT").unwrap();

        let mut meta = sample_meta();
        meta.symbol_id = Some(symbol_id);
        let req = sample_req();
        let wire = order_request_to_order_request_wire(&req, &meta).unwrap();
        let mut buf = [0u8; ORDER_REQUEST_WIRE_SIZE];
        wire.encode(&mut buf);

        let (normalized, meta) = normalize_wire_request(&buf, &symtab).unwrap();
        assert_eq!(normalized.exchange, ExchangeId::Gate);
        assert_eq!(normalized.symbol.as_str(), "BTC_USDT");
        assert!(normalized.reduce_only);
        assert_eq!(normalized.client_seq, 7);
        assert_eq!(normalized.origin_ts_ns, 123);
        assert_eq!(normalized.client_id.as_ref(), "v8-7");
        assert_eq!(meta.origin_vm_id, None);
        assert_eq!(&meta.text_tag[..2], b"v8");
    }

    #[test]
    fn normalize_wire_request_rejects_unknown_symbol_id() {
        let dir = tempfile::tempdir().unwrap();
        let sym_path = dir.path().join("symtab");
        let symtab = SymbolTable::open_or_create(&sym_path, 16).unwrap();

        let mut meta = sample_meta();
        meta.symbol_id = Some(9);
        let req = sample_req();
        let wire = order_request_to_order_request_wire(&req, &meta).unwrap();
        let mut buf = [0u8; ORDER_REQUEST_WIRE_SIZE];
        wire.encode(&mut buf);

        assert!(matches!(
            normalize_wire_request(&buf, &symtab),
            Err(IngressDecodeError::UnknownSymbol(9))
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zmq_ingress_inproc_roundtrip_to_request_channel() {
        let ctx = ZmqContext::new();
        let endpoint = "inproc://order-gateway-zmq-ingress-test";
        let dir = tempfile::tempdir().unwrap();
        let sym_path = dir.path().join("symtab");
        let symtab = Arc::new(SymbolTable::open_or_create(&sym_path, 16).unwrap());
        let symbol_id = symtab.get_or_intern(ExchangeId::Gate, "BTC_USDT").unwrap();

        let (tx, rx) = crossbeam_channel::bounded(4);
        let cancel = CancellationToken::new();
        let handle = start_zmq_order_ingress_with_context(
            ctx.clone(),
            endpoint,
            ZmqConfig {
                hwm: 16,
                linger_ms: 0,
                drain_batch_cap: 1,
                push_endpoint: String::new(),
                pub_endpoint: String::new(),
                sub_endpoint: String::new(),
                order_ingress_bind: Some(endpoint.into()),
                result_egress_bind: None,
                result_heartbeat_interval_ms: 0,
            },
            symtab.clone(),
            tx,
            cancel.clone(),
        )
        .unwrap();

        let sender = ZmqOrderEgress::connect_with_context(
            ctx,
            &hft_config::order_egress::ZmqOrderEgressConfig {
                endpoint: endpoint.into(),
                send_hwm: 16,
                linger_ms: 0,
                reconnect_interval_ms: 10,
                reconnect_interval_max_ms: 10,
            },
        )
        .unwrap();

        let mut meta = sample_meta();
        meta.symbol_id = Some(symbol_id);
        assert_eq!(
            sender.try_submit(&sample_req(), &meta).unwrap(),
            SubmitOutcome::Sent
        );

        let (got, meta) = rx
            .recv_timeout(std::time::Duration::from_millis(500))
            .unwrap();
        assert_eq!(got.client_id.as_ref(), "v8-7");
        assert!(got.reduce_only);
        assert_eq!(got.client_seq, 7);
        assert_eq!(got.origin_ts_ns, 123);
        assert_eq!(meta.origin_vm_id, None);
        assert_eq!(&meta.text_tag[..2], b"v8");

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zmq_ingress_routes_into_noop_executor_and_emits_counter() {
        let ctx = ZmqContext::new();
        let endpoint = "inproc://order-gateway-zmq-router-test";
        let dir = tempfile::tempdir().unwrap();
        let sym_path = dir.path().join("symtab");
        let symtab = Arc::new(SymbolTable::open_or_create(&sym_path, 16).unwrap());
        let symbol_id = symtab.get_or_intern(ExchangeId::Gate, "BTC_USDT").unwrap();

        let (req_tx, req_rx) = crossbeam_channel::bounded::<IngressEnvelope>(8);
        let (ack_tx, ack_rx) = crossbeam_channel::bounded::<hft_exchange_api::OrderAck>(8);

        let mut routing = RoutingTable::new();
        routing.insert(
            ExchangeId::Gate,
            Route::Rust(Arc::new(NoopExecutor::new(ExchangeId::Gate))),
        );
        let gateway = start_with_arc(
            Arc::new(routing),
            req_rx,
            Some(ack_tx),
            None,
            DEDUP_CACHE_CAP_DEFAULT,
            RetryPolicy::default(),
        )
        .unwrap();

        let cancel = gateway.cancel.clone();
        let ingress = start_zmq_order_ingress_with_context(
            ctx.clone(),
            endpoint,
            ZmqConfig {
                hwm: 16,
                linger_ms: 0,
                drain_batch_cap: 1,
                push_endpoint: String::new(),
                pub_endpoint: String::new(),
                sub_endpoint: String::new(),
                order_ingress_bind: Some(endpoint.into()),
                result_egress_bind: None,
                result_heartbeat_interval_ms: 0,
            },
            symtab.clone(),
            req_tx,
            cancel.clone(),
        )
        .unwrap();

        let sender = ZmqOrderEgress::connect_with_context(
            ctx,
            &hft_config::order_egress::ZmqOrderEgressConfig {
                endpoint: endpoint.into(),
                send_hwm: 16,
                linger_ms: 0,
                reconnect_interval_ms: 10,
                reconnect_interval_max_ms: 10,
            },
        )
        .unwrap();

        let before = counter_value("order_gateway_routed_ok");
        let mut meta = sample_meta();
        meta.symbol_id = Some(symbol_id);
        assert_eq!(
            sender.try_submit(&sample_req(), &meta).unwrap(),
            SubmitOutcome::Sent
        );

        let ack = tokio::task::spawn_blocking(move || {
            ack_rx.recv_timeout(std::time::Duration::from_millis(1000))
        })
        .await
        .unwrap()
        .expect("router should ack noop order");
        assert_eq!(ack.exchange, ExchangeId::Gate);
        assert!(ack.exchange_order_id.starts_with("noop-"));
        assert_eq!(ack.client_id.as_ref(), "v8-7");

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while counter_value("order_gateway_routed_ok") <= before
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(counter_value("order_gateway_routed_ok") > before);

        gateway.shutdown();
        let _ = ingress.await;
        gateway.join().await;
    }
}
