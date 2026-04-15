# Module Wiring — 누가 누구를 부르는가

각 crate 는 "뭘 만들지" 가 아니라 "다른 crate 와 어떤 인터페이스로 맞물리는지"
가 고정돼야 통합 마찰이 없다. 이 문서가 그 인터페이스의 단일 소스.

## 의존성 그래프 (compile-time)

```
                 ┌──────────────┐
                 │  hft-types   │  enum ExchangeId, BookTicker, Trade, MarketEvent, DataRole
                 └──────┬───────┘
                        │
          ┌─────────────┼─────────────┐
          │             │             │
    ┌─────▼─────┐ ┌─────▼──────┐ ┌────▼──────┐
    │  hft-time │ │hft-protocol│ │hft-config │
    │  Clock    │ │ wire/topic │ │  Figment  │
    └─────┬─────┘ └─────┬──────┘ └────┬──────┘
          │             │             │
          └──────┬──────┴──────┬──────┘
                 │             │
         ┌───────▼─────┐ ┌─────▼────────┐
         │hft-telemetry│ │hft-exchange- │
         │ tracing+OTel│ │   api (trait)│
         └───────┬─────┘ └─────┬────────┘
                 │             │
   ┌─────────────┼─────────────┼──────────────┐
   │             │             │              │
┌──▼────┐  ┌─────▼────┐  ┌─────▼─────┐  ┌─────▼──────┐
│hft-zmq│  │hft-shm   │  │hft-storage│  │hft-exchange│
│       │  │hft-ipc   │  │           │  │-<vendor>   │
└──┬────┘  └──┬───────┘  └────┬──────┘  └─────┬──────┘
   │          │               │               │
   └──────────┴───────────────┴───────────────┘
                      │
            ┌─────────┼───────────┐
            │                     │
     ┌──────▼─────────┐  ┌────────▼──────────┐
     │  services/*    │  │    tools/*        │
     │  (binaries)    │  │  (binaries)       │
     └────────────────┘  └───────────────────┘
```

규칙: **아래에서 위로만 참조**. 상위 crate 가 하위 crate 를 건드리는 역방향 참조는 금지.
우회가 필요하면 trait 을 하위에 두고 상위가 구현하는 방향으로.

## 데이터 흐름 (runtime)

```
 Exchange WS                                                                   
     │                                                                         
     │  (JSON frame)                                                           
     ▼                                                                         
┌────────────────────┐                                                         
│ hft-exchange-<v>   │  stamps.exchange_server_ms, ws_received_ms              
│  .stream()         │                                                         
│  → emit(MarketEvent)                                                         
└────┬───────────────┘                                                         
     │  (MarketEvent + LatencyStamps)                                          
     ▼                                                                         
┌──────────────────────┐                                                       
│ services/publisher   │                                                       
│  - serialize (wire)  │  stamps.serialized_ms                                 
│  - hft-zmq PUSH      │  stamps.pushed_ms                                     
└────┬─────────────────┘                                                       
     │  (inproc/ipc ZMQ PUSH-PULL)                                             
     ▼                                                                         
┌──────────────────────┐                                                       
│ publisher aggregator │                                                       
│  - PULL drain batch  │                                                       
│  - PUB (tcp)         │  stamps.published_ms                                  
└────┬─────────────────┘                                                       
     │  (tcp ZMQ PUB-SUB)                                                      
     ├──────────────────────┐                                                  
     ▼                      ▼                                                  
┌────────────────┐  ┌──────────────────┐                                      
│ services/      │  │ hft-storage      │                                      
│ subscriber     │  │  (bg runtime)    │                                      
│  stamps.       │  │  → QuestDB ILP   │                                      
│  subscribed_ms │  └──────────────────┘                                      
└────┬───────────┘                                                             
     │ (in-process callback or inproc ZMQ)                                     
     ▼                                                                         
┌───────────────────┐                                                          
│ services/strategy │  stamps.consumed_ms                                      
│  → decision       │                                                          
└────┬──────────────┘                                                          
     │ (OrderRequest)                                                          
     ▼                                                                         
┌───────────────────┐                                                          
│ services/         │                                                          
│ order-gateway     │  hft-exchange-<v>::ExchangeExecutor 경유                 
└───────────────────┘                                                          
```

## 각 경계의 타입

| 경계 | 전달 타입 | 소유권 |
|---|---|---|
| exchange → publisher | `MarketEvent` enum (`hft-types`) + `LatencyStamps` (`hft-time`) | move |
| publisher worker → aggregator | 와이어 바이트 `[u8; 120]` 또는 `[u8; 128]` (`hft-protocol`) | move (zmq) |
| aggregator → subscriber/storage | 와이어 바이트 + 토픽 문자열 | move (zmq) |
| subscriber → strategy | `MarketEvent` 재구성 + `LatencyStamps` | move (in-process ring buffer or inproc zmq) |
| strategy → order-gateway | `OrderRequest` (`hft-exchange-api`) | move |
| order-gateway → exchange | `ExchangeExecutor::place_order` 호출 | borrow (`&self`) |

## Config 흐름

```
  env vars + TOML + supabase  →  hft-config::load_all()  →  Arc<AppConfig>
                                                                │
                        ┌───────────────┬─────────────┬─────────┴────┐
                        ▼               ▼             ▼              ▼
                   publisher        subscriber     strategy      order-gateway
```

**Config 는 startup 에만 로드**. runtime reload 없음. 값이 바뀌어야 하면 재시작.
(이유: hot path 에서 Config 접근 시 `Arc` deref 외에 어떤 lock 도 허용 안 함.)

## Observability 흐름

```
  각 crate  →  tracing::span!() / event!()   →   hft_telemetry (subscriber)
                                                       │
                                         ┌─────────────┼──────────────┐
                                         ▼             ▼              ▼
                                      stdout       OTel OTLP     HDR histogram
                                      (async)    (bg runtime)    (메모리 + dump)
```

hot path 가 찍는 건 span enter/exit 뿐. 실제 포매팅·전송은 모두 bg thread.

## 금지된 참조

- `services/*` 가 다른 `services/*` 를 직접 참조하지 않는다 (ZMQ 로만 통신)
- `tools/*` 는 `services/*` 를 참조하지 않는다 (core 와 transport 만 사용)
- `hft-exchange-<vendor>` 끼리는 서로 참조하지 않는다 (같은 trait 만 구현)
- `hft-storage` 가 `hft-zmq` 에 의존하는 건 OK (SUB 해서 읽음). 하지만 publisher 가 `hft-storage` 에 의존하는 건 금지
