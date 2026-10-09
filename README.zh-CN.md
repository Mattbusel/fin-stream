# fin-stream

[English](README.md) | 简体中文 | [日本語](README.ja.md) | [한국어](README.ko.md)

**一个 Rust 实时行情库：接收 Binance、Coinbase、Alpaca 和 Polygon 的实时成交消息，统一成一种价格精确的 tick 格式，通过无锁环形缓冲区（ring buffer）在线程之间传递，再合成 OHLCV K 线。**

适合正在用 Rust 构建交易机器人、行情数据采集器、回测框架或加密货币与股票分析工具，希望交易所对接这类底层工作只做一次的开发者。

<p align="center">
  <a href="https://crates.io/crates/fin-stream"><img alt="crates.io 版本" src="https://img.shields.io/crates/v/fin-stream.svg"></a>
  <a href="https://docs.rs/fin-stream"><img alt="docs.rs" src="https://docs.rs/fin-stream/badge.svg"></a>
  <a href="https://gitlab.com/mattbusel/fin-stream/-/blob/main/LICENSE"><img alt="MIT 许可证" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
</p>

<p align="center">
  <img alt="真实的终端录屏：cargo run --example tape 在大约五秒内实时流式输出来自 Binance、Coinbase、Alpaca 和 Polygon 的 40 笔 BTC-USD 成交，每笔都带有方向、价格、数量条和延迟，每两秒输出一条汇总 K 线，最后是各交易所的合计和环形缓冲区使用情况" src="assets/demo.gif" width="100%">
</p>

## 安装

```bash
cargo add fin-stream serde_json
```

这是一个库，所以不需要下载或在系统里安装任何东西（需要 Rust 1.81 或更高版本，
在 CI 中验证）。`wss://` 行情的 TLS 使用 rustls 加 ring provider：不需要 OpenSSL，也不需要 cmake。
`serde_json` 只是为了让你能用 `json!` 构造原始消息。也可以写在 `Cargo.toml` 里：
`fin-stream = "2.12"`。想先看看效果？`git clone https://gitlab.com/mattbusel/fin-stream && cd fin-stream && cargo run --example tape`。

## 工作原理

`WsManager` 保持与交易所的 WebSocket 连接（断线后按退避策略重连），并把每个
文本帧交给你。你把帧包装成 `RawTick`；`TickNormalizer` 把四个交易所中任意一种格式
转换成统一的 `NormalizedTick`，其价格和数量都是 `Decimal`。行情线程把 tick 推入
`SpscRing`，你的线程把它们取出，之后可以送去合成 K 线、计算特征或做任何其他处理。
订单簿深度消息会进入 `OrderBook`，它会拒绝交叉的订单簿和序列号缺口。

<p align="center"><img alt="fin-stream 流水线的动画示意图，回放 tape 示例的前 17 笔成交：WsManager 通过 mpsc channel 向下传递 WebSocket 文本帧；每个帧变成一个 RawTick，TickNormalizer 把 Binance、Coinbase、Alpaca 或 Polygon 的 JSON 转换成 NormalizedTick；行情线程把它推入 SpscRing 的一个槽位，主线程把它取出；第 17 笔成交到达时，OhlcvAggregator 返回 14:30:00 的两秒 K 线（开 64250.19，高 64272.65，低 64250.19，收 64254.78，16 笔成交），ZScoreNormalizer 把价格转换成 z 分数，OrderBook::apply 对错误的深度更新返回 BookCrossed 和 SequenceGap 错误" src="docs/img/pipeline.svg" width="100%"></p>

## 为什么选 fin-stream

- **四个交易所，一种 tick 类型**：Binance 和 Coinbase（加密货币）以及 Alpaca 和 Polygon（美股）
  都会变成同样的 `NormalizedTick`，价格和数量都是精确的 `Decimal`。
  [`barter-data`](https://crates.io/crates/barter-data) 支持更多加密货币交易所，但没有
  股票行情；可以用下面的 `barter` feature 把 fin-stream 的股票 tick 交给 barter 代码。
- **不会掉线的连接循环**：`WsManager` 按退避策略重连，每次成功建立连接后都会重置重试
  额度，替换掉悄无声息的半开连接，并在你丢弃接收端后立即停止。这些路径都是针对一个真实的本地
  WebSocket 服务器测试的，而不是 mock。
- **不可信的输入绝不会 panic**：JSON 归一化器和 FIX 4.2 解析器都用随机消息和随机字节串
  做了属性测试（property testing）。
- **实测数据，包括别人更强的地方**（[bench/competitors](bench/competitors/README.md)）：
  在单线程上推入和取出时，`SpscRing` 每秒大约完成 12 亿次操作，
  领先于 `rtrb`、`ringbuf` 和 crossbeam 的 `ArrayQueue`。跨两个线程时，`rtrb` 和
  `ringbuf` 更快（每秒大约 1.46 亿和 1.39 亿个元素，对比 1.21 亿）。
- **可观测**：启用 `metrics` feature 后，连接数、重连数、消息数、字节数、K 线数
  和迟到的 tick 都会成为你所安装的 recorder（Prometheus、StatsD、
  OpenTelemetry）上的计数器。

## Feature 开关

| feature | 默认 | 新增内容 |
|---|---|---|
| `fin-primitives` | 是 | `Tick::try_from(&NormalizedTick)` 转换到 [fin-primitives](https://crates.io/crates/fin-primitives)（K 线、700+ 个指标、订单簿、风控），并可对其错误使用 `?` |
| `metrics` | 否 | 通过 [`metrics`](https://crates.io/crates/metrics) facade 输出计数器；列表见 `telemetry` 模块文档 |
| `grpc` | 否 | 一个 tonic gRPC 服务端，把 tick 流式推送给远程客户端（生成的代码已提交到仓库，不需要 `protoc`） |
| `barter` | 否 | 为 [barter-data](https://crates.io/crates/barter-data) 策略提供 `PublicTrade::try_from(&NormalizedTick)` |

## 示例

[`examples/`](examples/) 中附带四个程序。无需网络，无需 API 密钥，每次运行的成交都一样。
在终端里它们会实时流式输出；通过管道时则立即全部打印。`NO_COLOR=1` 可以关闭颜色。

| 运行 | 得到 |
|---|---|
| `cargo run --example tape` | 四个交易所的成交在行情线程上归一化，经由 `SpscRing` 传递，以实时逐笔成交（time-and-sales）的形式打印，并合成 2 秒 K 线 |
| `cargo run --example normalize` | 每个交易所发送的同一笔成交、各自转换成的那个 `NormalizedTick`，以及三个带类型的拒绝错误 |
| `cargo run --example feed_health` | `HealthMonitor` 监控四路行情：其中一路变得安静、变为过期、触发熔断，然后恢复 |
| `cargo run --example replay` | 一个录制好的 NDJSON 文件通过实时行情 trait 回放，合成 30 秒 K 线 |

**`tape`** 就是本页顶部的录屏：来自四个交易所的成交，统一成一种格式，每根 2 秒 K 线在收盘的那一刻打印出来。

**`feed_health`**（真实输出，运行于 2026-09-28，`NO_COLOR=1`）：

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

（`│` 心跳，`·` 安静，`▒` 过期，`█` 熔断打开。为节省篇幅，省略了表头和最后几行。）

<details>
<summary><b>tape</b>（完整运行）、<b>normalize</b> 和 <b>replay</b> 的输出</summary>

<br>

<p align="center"><img alt="cargo run --example tape 的输出：来自四个交易所的 40 笔 BTC-USD 成交，包含时间、交易所、方向、按涨跌着色的价格、数量条和延迟，每两秒一行 K 线汇总，最后是各交易所的 tick 数、K 线数和环形缓冲区使用情况的汇总" src="assets/term-tape.png" width="840"></p>

<p align="center"><img alt="cargo run --example normalize 的输出：同一笔成交在四个交易所的原始消息，以及各自生成的归一化价格、数量、方向和交易所时间戳，然后是三条格式错误的消息及其 StreamError 信息" src="assets/term-normalize.png" width="840"></p>

<p align="center"><img alt="cargo run --example replay 的输出：由 600 个录制的 tick 生成的二十根 30 秒 K 线，在固定的价格轴上画成横向蜡烛图，附带收盘价、相对开盘的涨跌和成交量" src="assets/term-replay.png" width="760"></p>

</details>

## 3 步上手

**1. 新建项目并添加 crate**

```bash
cargo new tick-demo && cd tick-demo
cargo add fin-stream serde_json
```

**2. 把下面的代码放进 `src/main.rs`**

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

**3. 运行**

```bash
cargo run
```

你会看到：

```text
Binance   sell    0.40 BTC @ 64251.30
Coinbase  buy    0.012 BTC @ 64250.10
Alpaca    n/a     0.05 BTC @ 64252.0
Polygon   n/a      0.2 BTC @ 64249.75
no price  -> Tick parse error from Binance: missing field 'p'
```

四个交易所用四种不同的方式描述一笔成交；你得到的是每笔一个 `NormalizedTick`，价格和数量都是精确的
十进制数。不说明哪一方是主动方的交易所会显示 `n/a`，而不是瞎猜；格式错误的消息则是一个
可以 match 的错误。这段程序会原样由本仓库的 `cargo test --doc` 编译并运行。

## 文档

| 阅读 | 内容 |
|---|---|
| [docs.rs/fin-stream](https://docs.rs/fin-stream) | 每个类型和方法 |
| [docs/EXAMPLES.md](docs/EXAMPLES.md) | 简短的用法示例：环形缓冲区、K 线、归一化器、订单簿、行情健康度、交易时段、Lorentz 特征 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 每个模块的作用、支持的交易所及其消息字段、设计规则、基准测试数据、如何新增交易所 |
| [docs/REFERENCE.md](docs/REFERENCE.md) | 模块指南（多路行情与 NBBO 聚合、熔断器、行情质量、异常检测、回放、FIX 4.2、gRPC、OFI、VPIN、市场微观结构、市场状态等）、数学、API 签名、每一种 `StreamError` |
| [docs/TESTING.md](docs/TESTING.md) | 运行测试和基准测试、当前测试状态 |
| [CHANGELOG.md](CHANGELOG.md) | 每个版本的改动 |
| [项目网站](https://fin-stream-rs.vercel.app/) | 以网页形式呈现的同一份概览 |

可选的 gRPC 服务端在 `grpc` feature 之后；metrics、互操作等其他功能
见上面的 feature 表。

> 这是一个研究和工程用的库。它不会下单，这里的任何内容都不构成投资建议。

## 参与贡献

欢迎提交 issue 和 pull request。公开项需要 `///` 文档（`#![deny(missing_docs)]`），
可能失败的代码要返回 `Result<_, StreamError>`，新行为需要配套测试。提交 PR 前请运行 `cargo fmt`、
`cargo clippy` 和 `cargo test --doc`。要新增交易所，请按照
[新增交易所适配器](docs/ARCHITECTURE.md#adding-a-new-exchange-adapter)一节操作。

## 许可证与相关项目

MIT，见 [LICENSE](LICENSE)。可以与 [fin-primitives](https://gitlab.com/mattbusel/fin-primitives)
（经过校验的价格和数量类型、订单簿、指标、风控）搭配使用：用
`fin_primitives::tick::Tick::try_from(&tick)` 转换 tick。`lorentz` 模块来自
狭义相对论金融建模（Special Relativity Financial Modeling）的工作：[Special-Relativity-in-Financial-Modeling](https://gitlab.com/mattbusel/Special-Relativity-in-Financial-Modeling)、
[srfm-python](https://gitlab.com/mattbusel/srfm-python)、[srfm-paper-impl](https://gitlab.com/mattbusel/srfm-paper-impl) 和 [srfm-lab](https://gitlab.com/mattbusel/srfm-lab)。
