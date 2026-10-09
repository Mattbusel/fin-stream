# fin-stream

[English](README.md) | [简体中文](README.zh-CN.md) | [日本語](README.ja.md) | 한국어

**Binance, Coinbase, Alpaca, Polygon의 실시간 체결 메시지를 받아 정확한 가격을 가진 하나의 틱 형식으로 통일하고, 락프리(lock-free) 링 버퍼로 스레드 간에 전달하며, OHLCV 캔들로 묶어 주는 Rust 라이브러리.**

트레이딩 봇, 시세 데이터 수집기, 백테스터, 암호화폐·주식 분석 도구를 Rust로 만들면서 거래소 연동 같은 배관 작업은 한 번만 하고 싶은 개발자를 위한 라이브러리입니다.

<p align="center">
  <a href="https://crates.io/crates/fin-stream"><img alt="crates.io 버전" src="https://img.shields.io/crates/v/fin-stream.svg"></a>
  <a href="https://docs.rs/fin-stream"><img alt="docs.rs" src="https://docs.rs/fin-stream/badge.svg"></a>
  <a href="https://gitlab.com/mattbusel/fin-stream/-/blob/main/LICENSE"><img alt="MIT 라이선스" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
</p>

<p align="center">
  <img alt="실제 터미널 세션: cargo run --example tape가 Binance, Coinbase, Alpaca, Polygon에서 온 BTC-USD 체결 40건을 약 5초 동안 실시간으로 흘려보낸다. 각 체결에는 매수·매도 방향, 가격, 수량 막대, 지연 시간이 표시되고, 2초마다 요약 봉이 나오며, 마지막에 거래소별 합계와 링 사용량이 나온다" src="assets/demo.gif" width="100%">
</p>

## 설치

```bash
cargo add fin-stream serde_json
```

라이브러리이므로 내려받거나 시스템에 설치할 것은 없습니다(Rust 1.81 이상,
CI에서 확인). `wss://` 피드의 TLS는 ring 프로바이더를 쓰는 rustls라서 OpenSSL도 cmake도 필요 없습니다.
`serde_json`은 `json!`으로 원시 페이로드를 만들 수 있게 하려고 넣은 것뿐입니다. `Cargo.toml`에 쓰려면
`fin-stream = "2.12"`. 먼저 돌아가는 걸 보고 싶다면? `git clone https://gitlab.com/mattbusel/fin-stream && cd fin-stream && cargo run --example tape`.

## 동작 원리

`WsManager`는 거래소 WebSocket 연결을 유지하고(끊기면 백오프하며 재연결) 텍스트 프레임을
하나씩 넘겨 줍니다. 프레임을 `RawTick`으로 감싸면, `TickNormalizer`가 네 거래소의 어떤 형식이든
가격과 수량이 `Decimal`인 하나의 `NormalizedTick`으로 바꿉니다. 피드 스레드가 틱을
`SpscRing`에 넣고, 여러분의 스레드가 꺼내며, 그다음에는 봉, 피처, 그 밖의 무엇으로든 보낼 수 있습니다.
호가창 깊이 메시지는 `OrderBook`으로 들어가며, 교차된 호가창과 시퀀스 번호 누락은 거부됩니다.

<p align="center"><img alt="tape 예제의 첫 체결 17건을 재생하는 fin-stream 파이프라인 애니메이션 다이어그램: WsManager가 WebSocket 텍스트 프레임을 mpsc 채널로 내려보낸다. 각 프레임은 RawTick이 되고, TickNormalizer가 Binance, Coinbase, Alpaca, Polygon의 JSON을 NormalizedTick으로 바꾼다. 피드 스레드가 이를 SpscRing 슬롯에 넣고 메인 스레드가 꺼낸다. 17번째 체결이 도착하면 OhlcvAggregator가 14:30:00 2초 봉(시가 64250.19, 고가 64272.65, 저가 64250.19, 종가 64254.78, 체결 16건)을 반환하고, ZScoreNormalizer가 가격을 z-점수로 바꾸며, OrderBook::apply는 잘못된 깊이 업데이트에 BookCrossed와 SequenceGap 오류를 반환한다" src="docs/img/pipeline.svg" width="100%"></p>

## 왜 fin-stream인가

- **거래소 네 곳, 틱 타입 하나**: Binance와 Coinbase(암호화폐), Alpaca와 Polygon(미국 주식)이
  모두 가격과 수량이 정확한 `Decimal`인 같은 `NormalizedTick`이 됩니다.
  [`barter-data`](https://crates.io/crates/barter-data)는 더 많은 암호화폐 거래소를 지원하지만
  주식 피드는 없습니다. fin-stream의 주식 틱을 barter 코드에 넘기려면 아래의 `barter` feature를 쓰세요.
- **끊기지 않는 연결 루프**: `WsManager`는 백오프하며 재연결하고, 연결이 성립할 때마다 재시도
  한도를 초기화하며, 조용히 반쯤 열린 채 멈춘(half-open) 연결을 교체하고, 수신 측을 drop하는 즉시
  멈춥니다. 이 경로들은 목(mock)이 아니라 로컬에서 띄운 실제 WebSocket 서버를 상대로 테스트했습니다.
- **신뢰할 수 없는 입력에도 panic하지 않음**: JSON 정규화기와 FIX 4.2 파서는
  무작위 페이로드와 무작위 바이트열로 속성 기반 테스트(property testing)를 거쳤습니다.
- **측정 결과, 다른 쪽이 이기는 부분까지 포함**([bench/competitors](bench/competitors/README.md)):
  한 스레드에서 push와 pop을 할 때 `SpscRing`은 초당 약 12억 번의 연산을 처리해
  `rtrb`, `ringbuf`, crossbeam의 `ArrayQueue`를 앞섰습니다. 두 스레드 사이에서는 `rtrb`와
  `ringbuf`가 더 빨랐습니다(초당 약 1억 4,600만 개와 1억 3,900만 개, 대 1억 2,100만 개).
- **관측 가능**: `metrics` feature를 켜면 연결, 재연결, 메시지, 바이트, 봉,
  늦게 도착한 틱이 여러분이 설치한 레코더(Prometheus, StatsD,
  OpenTelemetry)의 카운터가 됩니다.

## Feature 플래그

| feature | 기본값 | 추가되는 것 |
|---|---|---|
| `fin-primitives` | 예 | [fin-primitives](https://crates.io/crates/fin-primitives)(봉, 700개 이상의 지표, 호가창, 리스크)로의 `Tick::try_from(&NormalizedTick)`, 그리고 그 오류에 `?` 사용 |
| `metrics` | 아니요 | [`metrics`](https://crates.io/crates/metrics) 파사드를 통한 카운터; 목록은 `telemetry` 모듈 문서에 있음 |
| `grpc` | 아니요 | 원격 클라이언트에 틱을 스트리밍하는 tonic gRPC 서버(생성된 코드가 저장소에 포함되어 있어 `protoc` 불필요) |
| `barter` | 아니요 | [barter-data](https://crates.io/crates/barter-data) 전략용 `PublicTrade::try_from(&NormalizedTick)` |

## 예제

[`examples/`](examples/)에 프로그램 4개가 들어 있습니다. 네트워크도 API 키도 필요 없고, 실행할 때마다 같은 체결이 나옵니다.
터미널에서는 실시간으로 흘러가고, 파이프로 넘기면 즉시 모두 출력됩니다. `NO_COLOR=1`로 색을 끌 수 있습니다.

| 실행 명령 | 결과 |
|---|---|
| `cargo run --example tape` | 네 거래소의 체결을 피드 스레드에서 정규화하고 `SpscRing`을 거쳐 실시간 체결 내역(time-and-sales)으로 출력하며 2초 봉으로 묶음 |
| `cargo run --example normalize` | 각 거래소가 보내는 형태 그대로의 체결 한 건, 각각이 변환된 하나의 `NormalizedTick`, 그리고 타입이 있는 거부 세 건 |
| `cargo run --example feed_health` | 피드 네 개를 지켜보는 `HealthMonitor`: 하나가 조용해지고, stale 상태가 되고, 서킷이 열렸다가 회복됨 |
| `cargo run --example replay` | 녹화된 NDJSON 파일을 라이브 피드 트레이트를 통해 흘려 30초 봉으로 묶음 |

**`tape`**는 이 페이지 맨 위의 녹화 화면입니다: 네 거래소의 체결이 하나의 형식으로, 2초 봉이 마감되는 순간마다 출력됩니다.

**`feed_health`**(실제 출력, 2026-09-28 실행, `NO_COLOR=1`):

```text
  binance   ││││││││││││││││││││││││││││││││││││││││││││││││  healthy   96 beats, last  0.0s ago
  coinbase  │││││││││····▒▒█████████││││││││││││││││││││││││  healthy   33 beats, last  0.0s ago
  alpaca    ·││·││·││·││·││·││·││·││·││·││·│···▒▒███████████  open      21 beats, last  8.2s ago
  polygon   ·····│·····│·····│·····│·····│·····│·····│·····│  healthy    8 beats, last  0.0s ago
            0s        5s        10s       15s       20s

  t+ 7.0s  coinbase  Feed 'coinbase' is stale: last tick was 2500ms ago (threshold: 2000ms)
  t+ 8.0s  coinbase  circuit OPEN after 3 stale checks
  t+12.5s  coinbase  heartbeat received, circuit closed
```

(`│` 하트비트, `·` 조용함, `▒` stale, `█` 서킷 열림. 길이 때문에 헤더와 마지막 몇 줄은 생략했습니다.)

<details>
<summary><b>tape</b>(전체 실행), <b>normalize</b>, <b>replay</b> 출력</summary>

<br>

<p align="center"><img alt="cargo run --example tape의 출력: 네 거래소에서 온 BTC-USD 체결 40건이 시간, 거래소, 방향, 틱 방향에 따라 색이 입혀진 가격, 수량 막대, 지연 시간과 함께 나오고, 2초마다 봉 요약 줄이, 마지막에 거래소별 틱 수, 봉 수, 링 사용량 요약이 나온다" src="assets/term-tape.png" width="840"></p>

<p align="center"><img alt="cargo run --example normalize의 출력: 체결 한 건에 대한 네 거래소의 페이로드와 각각에서 나온 정규화된 가격, 수량, 방향, 거래소 타임스탬프, 이어서 잘못된 형식의 페이로드 세 건과 그 StreamError 메시지" src="assets/term-normalize.png" width="840"></p>

<p align="center"><img alt="cargo run --example replay의 출력: 녹화된 틱 600개로 만든 30초 봉 스무 개를 고정된 가격 축 위에 가로 캔들로 그리고, 종가, 세션 시가 대비 변화, 거래량을 표시" src="assets/term-replay.png" width="760"></p>

</details>

## 3단계로 사용하기

**1. 프로젝트를 만들고 크레이트 추가하기**

```bash
cargo new tick-demo && cd tick-demo
cargo add fin-stream serde_json
```

**2. 이 코드를 `src/main.rs`에 넣기**

```rust
use fin_stream::tick::{Exchange, RawTick, TickNormalizer};
use serde_json::json;

fn main() -> Result<(), fin_stream::StreamError> {
    let normalizer = TickNormalizer::new();

    // One BTC trade each, in the JSON shape each exchange really sends.
    let trades = [
        (Exchange::Binance, json!({"p": "64251.30", "q": "0.40", "m": true, "t": 7, "T": 1790260201205u64})),
        (Exchange::Coinbase, json!({"price": "64250.10", "size": "0.012", "side": "buy"})),
        (Exchange::Alpaca, json!({"p": 64252.0, "s": 0.05, "i": 99})),
        (Exchange::Polygon, json!({"p": 64249.75, "s": 0.2, "i": "p-1"})),
    ];

    // Four formats in, one tick type out.
    for (venue, payload) in trades {
        let tick = normalizer.normalize(RawTick::new(venue, "BTC-USD", payload))?;
        let side = tick.side.map_or("n/a".to_string(), |s| s.to_string());
        println!("{:<9} {:<5} {:>6} BTC @ {}", tick.exchange.to_string(), side, tick.quantity, tick.price);
    }

    // Bad input is a typed error, not a panic.
    let broken = RawTick::new(Exchange::Binance, "BTC-USD", json!({"q": "1"}));
    println!("no price  -> {}", normalizer.normalize(broken).unwrap_err());
    Ok(())
}
```

**3. 실행하기**

```bash
cargo run
```

이렇게 출력됩니다:

```text
Binance   sell    0.40 BTC @ 64251.30
Coinbase  buy    0.012 BTC @ 64250.10
Alpaca    n/a     0.05 BTC @ 64252.0
Polygon   n/a      0.2 BTC @ 64249.75
no price  -> Tick parse error from Binance: missing field 'p'
```

네 거래소는 체결 하나를 네 가지 다른 방식으로 표현하지만, 여러분은 각각에 대해 가격과 수량이 정확한
10진수인 `NormalizedTick` 하나를 받습니다. 어느 쪽이 공격적인 주문(aggressor)이었는지 알려 주지 않는 거래소는
추측하지 않고 `n/a`로 표시하며, 형식이 잘못된 메시지는 match로 처리할 수 있는 오류가 됩니다. 이 프로그램
그대로가 이 저장소의 `cargo test --doc`에서 컴파일되고 실행됩니다.

## 문서

| 문서 | 내용 |
|---|---|
| [docs.rs/fin-stream](https://docs.rs/fin-stream) | 모든 타입과 메서드 |
| [docs/EXAMPLES.md](docs/EXAMPLES.md) | 짧은 레시피: 링 버퍼, 봉, 정규화기, 호가창, 피드 상태, 거래 세션, Lorentz 피처 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 각 모듈이 하는 일, 지원 거래소와 그 와이어 필드, 설계 규칙, 벤치마크 수치, 거래소 추가하기 |
| [docs/REFERENCE.md](docs/REFERENCE.md) | 모듈 가이드(멀티 피드와 NBBO 집계, 서킷 브레이커, 피드 품질, 이상 탐지, 리플레이, FIX 4.2, gRPC, OFI, VPIN, 시장 미시구조, 시장 국면 등), 수학, API 시그니처, 모든 `StreamError` |
| [docs/TESTING.md](docs/TESTING.md) | 테스트와 벤치마크 실행, 현재 테스트 상태 |
| [CHANGELOG.md](CHANGELOG.md) | 버전별 변경 사항 |
| [프로젝트 사이트](https://fin-stream-rs.vercel.app/) | 같은 개요를 웹 페이지로 |

선택 사항인 gRPC 서버는 `grpc` feature로 켭니다. 메트릭, 상호 운용 등 나머지는
위의 feature 표에 있습니다.

> 연구 및 엔지니어링용 라이브러리입니다. 주문을 내지 않으며, 여기 있는 어떤 내용도 투자 조언이 아닙니다.

## 기여하기

이슈와 풀 리퀘스트를 환영합니다. 공개 항목에는 `///` 문서가 필요하고(`#![deny(missing_docs)]`),
실패할 수 있는 코드는 `Result<_, StreamError>`를 반환해야 하며, 새 동작에는 테스트가 필요합니다. PR을 열기 전에 `cargo fmt`,
`cargo clippy`, `cargo test --doc`을 실행하세요. 거래소를 추가하려면
[새 거래소 어댑터 추가하기](docs/ARCHITECTURE.md#adding-a-new-exchange-adapter)를 따르세요.

## 라이선스와 관련 프로젝트

MIT, [LICENSE](LICENSE)를 참고하세요. [fin-primitives](https://gitlab.com/mattbusel/fin-primitives)
(검증된 가격·수량 타입, 호가창, 지표, 리스크)와 함께 쓰기 좋습니다: 틱은
`fin_primitives::tick::Tick::try_from(&tick)`으로 변환하세요. `lorentz` 모듈은
특수 상대성 이론 금융 모델링(Special Relativity Financial Modeling) 작업에서 나왔습니다: [Special-Relativity-in-Financial-Modeling](https://gitlab.com/mattbusel/Special-Relativity-in-Financial-Modeling),
[srfm-python](https://gitlab.com/mattbusel/srfm-python), [srfm-paper-impl](https://gitlab.com/mattbusel/srfm-paper-impl), [srfm-lab](https://gitlab.com/mattbusel/srfm-lab).
