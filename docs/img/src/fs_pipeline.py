"""fin-stream: WebSocket -> RawTick -> TickNormalizer -> SpscRing -> bars / order book / analytics. Animated.

Data:
- tape-first-17.jsonl: the first 17 trades of `cargo run --example tape` (seed 7), each with the
  venue payload exactly as serde_json prints it and the NormalizedTick fields TickNormalizer
  returned (dumped 2026-09-28 with a throwaway example that reuses tape's VenueFeed).
- BAR: the 2-second OhlcvBar that OhlcvAggregator::feed returned when trade 17 arrived (same run,
  also printed by the tape example itself).
- ZS: ZScoreNormalizer::new(20), update(price) then normalize(price), per trade (same run).
- BOOK: OrderBook::apply results for the listed deltas (illustrative depth updates, real results).

Run: python fs_pipeline.py ../pipeline.svg
"""
import json
import os
import sys
from common import *

HERE = os.path.dirname(os.path.abspath(__file__))
TICKS = [json.loads(l) for l in open(os.path.join(HERE, "tape-first-17.jsonl"), encoding="utf-8")]
T0 = 1790260200000
ZS = [0.0000, 1.0000, 0.4580, 0.3046, 1.3742, 1.8858, 1.6614, 0.7120, 1.1329, 1.2507, 1.3291,
      -0.1420, 0.5966, -0.0615, 0.6692, -1.2285, -1.1769]
BAR = dict(o="64250.19", h="64272.65", l="64250.19", c="64254.78", vol="2.74672991", n=16)
BOOK = [
    ("seq 1  Bid 64254.50 x 0.80", "Ok   one side only, no mid yet", GREEN),
    ("seq 2  Ask 64255.00 x 0.35", "Ok   mid 64254.75  spread 0.50", GREEN),
    ("seq 3  Bid 64254.00 x 1.20", "Ok   mid 64254.75  spread 0.50", GREEN),
    ("seq 4  Ask 64255.50 x 0.90", "Ok   mid 64254.75  spread 0.50", GREEN),
    ("seq 5  Ask 64255.00 x 0", "Ok   level removed, spread 1.00", GREEN),
    ("seq 6  Bid 64256.00 x 0.5", "Err  BookCrossed: bid 64256.00 >= ask 64255.50", RED),
    ("seq 9  Bid 64254.50 x 1.0", "Err  SequenceGap: expected 7, got 9", RED),
]
DUR = 16.0
SCALE = 5.4  # animation seconds per second of exchange time


def at(tk):
    return 0.8 + (tk["ts"] - T0) / 1000.0 * SCALE


VCOL = {"Binance": AMBER, "Coinbase": BLUE, "Alpaca": GREEN, "Polygon": RED}
W, H = 960, 884
s = Svg(W, H, "Animated diagram of the fin-stream pipeline, replaying the first 17 trades of cargo run --example tape. "
        "WsManager::run reads text frames from the exchange WebSocket and sends each one down an mpsc channel, "
        "reconnecting with backoff when the socket closes. Each frame becomes a RawTick and "
        "TickNormalizer::normalize turns the Binance, Coinbase, Alpaca or Polygon JSON into one NormalizedTick with "
        "exact decimal price and quantity. The feed thread pushes the tick into SpscRing<NormalizedTick, 64> at slot "
        "tail & (N - 1); the main thread pops it at head & (N - 1). Popped ticks go to OhlcvAggregator, which returns "
        "the 14:30:00 two-second bar (open 64250.19, high 64272.65, low 64250.19, close 64254.78, 16 trades) when "
        "trade 17 arrives; to a ZScoreNormalizer; and, from separate depth messages, an OrderBook whose apply "
        "returns BookCrossed or SequenceGap errors on bad updates.")

s.text(32, 44, "How a trade gets from the exchange to your code", 24, font=SERIF)
s.text(32, 70, "the first 17 trades of cargo run --example tape, replayed about 5x slower than real time", 14, MUTED)

# ---- row 1: socket -> raw -> normalized ----------------------------------------------
y1, h1 = 92, 214
s.box(32, y1, 206, h1)
s.text(48, y1 + 28, "exchange WebSocket", 16, INK, weight="bold")
ws_lines = [("WsManager::run(tx, None)", INK), ("each text frame goes to", MUTED), ("mpsc::Sender<String>", INK),
            ("socket closed?", MUTED), ("sleep 0.5 s, 1 s, 2 s ...", INK), ("(max 30 s, 10 tries)", MUTED),
            ("ping every 20 s", MUTED)]
for i, (t, c) in enumerate(ws_lines):
    s.text(48, y1 + 56 + i * 21, t, 13, c)

s.arrow(240, y1 + 100, 262, y1 + 100)
bx, bw = 264, 396
s.box(bx, y1, bw, h1)
s.text(bx + 16, y1 + 28, 'RawTick::new(venue, "BTC-USD", json)', 15, INK, weight="bold")


def wrap_json(raw, width=46):
    out, cur = [], ""
    for part in raw.split(","):
        piece = part + ","
        if len(cur) + len(piece) > width and cur:
            out.append(cur)
            cur = piece
        else:
            cur += piece
    out.append(cur.rstrip(","))
    return out


for k, tk in enumerate(TICKS):
    a = at(tk)
    b = at(TICKS[k + 1]) if k + 1 < len(TICKS) else DUR
    g = [f'<g opacity="0">{show_between(a, b, DUR)}']
    s.add("".join(g))
    s.text(bx + 16, y1 + 56, f"#{k + 1}  {tk['venue']}", 15, VCOL[tk["venue"]], weight="bold")
    s.text(bx + bw - 16, y1 + 56, f"14:30:{(tk['ts'] - T0) / 1000:06.3f}", 13, MUTED, anchor="end")
    for i, line in enumerate(wrap_json(tk["raw"])[:6]):
        s.text(bx + 16, y1 + 84 + i * 20, line, 13, INK)
    s.add("</g>")
s.text(bx + 16, y1 + h1 - 12, "four venues, four spellings of one trade", 13, MUTED)

s.arrow(bx + bw + 2, y1 + 100, bx + bw + 24, y1 + 100)
nx, nw = 690, 238
s.box(nx, y1, nw, h1, stroke=GREEN, sw=1.6)
s.text(nx + 16, y1 + 28, "NormalizedTick", 16, GREEN, weight="bold")
fields = ["exchange", "price", "quantity", "side", "trade_id", "exchange_ts_ms"]
for i, f in enumerate(fields):
    s.text(nx + 16, y1 + 58 + i * 23, f, 13, MUTED)
for k, tk in enumerate(TICKS):
    a = at(tk) + 0.25
    b = at(TICKS[k + 1]) + 0.25 if k + 1 < len(TICKS) else DUR
    vals = [tk["venue"], tk["price"], tk["qty"], tk["side"].capitalize() if tk["side"] else "None",
            f'"{tk["trade_id"]}"', str(tk["ts"])]
    s.add(f'<g opacity="0">{show_between(a, b, DUR)}')
    for i, v in enumerate(vals):
        col = MUTED if v == "None" else INK
        s.text(nx + 128, y1 + 58 + i * 23, v, 13, col)
    s.add("</g>")
s.text(nx + 16, y1 + 196, "TickNormalizer::normalize(raw)?", 12, MUTED)
s.text(nx + nw, y1 + h1 + 20, "exact Decimal price and size, not f64", 13, GREEN, anchor="end")

# ---- row 2: the ring ------------------------------------------------------------------
y2 = 360
s.text(32, y2, "SpscRing<NormalizedTick, 64>", 16, INK, weight="bold")
s.text(32, y2 + 20, "slots allocated once in new(); push and pop are an atomic load, a slot write or read, and an atomic store", 12, MUTED)
NS = 19
sx0, sw_, sg = 32, 40, 4.8
sy = y2 + 40
for i in range(NS):
    x = sx0 + i * (sw_ + sg)
    s.add(f'<rect x="{x:.1f}" y="{sy}" width="{sw_}" height="40" rx="5" fill="{BOX}" stroke="{LINE}" stroke-width="1.2"/>')
    s.text(x + sw_ / 2, sy + 58, str(i), 12, MUTED, anchor="middle")
s.text(928, sy + 25, "... 63", 12, MUTED, anchor="end")

POP = 0.55  # visual delay; the real consumer pops within microseconds
for k, tk in enumerate(TICKS):
    x = sx0 + k * (sw_ + sg)
    a, b = at(tk) + 0.35, at(tk) + 0.35 + POP
    col = VCOL[tk["venue"]]
    s.add(f'<rect x="{x:.1f}" y="{sy}" width="{sw_}" height="40" rx="5" fill="{col}" opacity="0">{show_between(a, b, DUR)}</rect>')
    s.add(f'<rect x="{x:.1f}" y="{sy}" width="{sw_}" height="40" rx="5" fill="{col}" opacity="0">'
          f'<animate attributeName="opacity" values="0;0;0.16;0.16" keyTimes="0;{b / DUR:.4f};{b / DUR + 0.0005:.4f};1" '
          f'dur="{DUR}s" repeatCount="indefinite"/></rect>')
    s.add(f'<g opacity="0">{show_between(a, b, DUR)}')
    s.text(x + sw_ / 2, sy + 25, tk["venue"][0], 14, BOX, anchor="middle", weight="bold")
    s.add("</g>")


# tail / head counters
tail_steps = [(at(tk) + 0.35, k + 1) for k, tk in enumerate(TICKS)]
head_steps = [(at(tk) + 0.35 + POP, k + 1) for k, tk in enumerate(TICKS)]
s.text(32, sy + 84, "feed thread:", 13, MUTED)
s.text(128, sy + 84, "tx.push(tick)?  writes slot tail & (N - 1), then tail += 1", 13, INK)
s.text(32, sy + 104, "main thread:", 13, MUTED)
s.text(128, sy + 104, "rx.pop()       reads slot head & (N - 1), then head += 1", 13, INK)
for name, steps, yy in (("tail", tail_steps, sy + 84), ("head", head_steps, sy + 104)):
    for j, (t, v) in enumerate(steps):
        t_end = steps[j + 1][0] if j + 1 < len(steps) else DUR
        s.add(f'<g opacity="0">{show_between(t, t_end, DUR)}')
        s.text(928, yy, f"{name} = {v}", 13, INK, anchor="end", weight="bold")
        s.add("</g>")
s.text(32, sy + 124, "Pops are slowed here so you can see them; a full ring returns Err(RingBufferFull), never blocks or drops silently.", 12, MUTED)

# ---- row 3: consumers -----------------------------------------------------------------
y3 = 570
cw, chh = 290, 284
xs = [32, 335, 638]

# 1. bars
x = xs[0]
s.box(x, y3, cw, chh)
s.text(x + 14, y3 + 26, "OhlcvAggregator", 15, INK, weight="bold")
s.text(x + 14, y3 + 46, "Timeframe::Seconds(2)", 12, MUTED)
s.text(x + 14, y3 + 68, "agg.feed(&tick)?", 13, INK)
run = []
o = h = l = None
for k, tk in enumerate(TICKS[:16]):
    p = float(tk["price"])
    o = tk["price"] if o is None else o
    h = p if h is None else max(h, p)
    l = p if l is None else min(l, p)
    run.append((at(tk) + 0.35 + POP, o, h, l, tk["price"], k + 1))
close_t = at(TICKS[16]) + 0.35 + POP
for j, (t, o_, h_, l_, c_, n_) in enumerate(run):
    t_end = run[j + 1][0] if j + 1 < len(run) else close_t
    s.add(f'<g opacity="0">{show_between(t, t_end, DUR)}')
    s.text(x + 150, y3 + 68, "-> Ok(vec![])", 13, MUTED)
    s.text(x + 14, y3 + 96, "open bar 14:30:00, still filling", 12, MUTED)
    rows = [("open", o_), ("high", f"{h_:.2f}"), ("low", f"{l_:.2f}"), ("close", c_), ("trades", str(n_))]
    for i, (kk, vv) in enumerate(rows):
        s.text(x + 14, y3 + 122 + i * 20, kk, 13, MUTED)
        s.text(x + 90, y3 + 122 + i * 20, vv, 13, INK)
    s.add("</g>")
s.add(f'<g opacity="0">{show_between(close_t, DUR, DUR)}')
s.text(x + 150, y3 + 68, "-> Ok(vec![bar])", 13, GREEN, weight="bold")
s.text(x + 14, y3 + 96, "trade 17 (14:30:02.076) closed it:", 12, GREEN)
rows = [("open", BAR["o"]), ("high", BAR["h"]), ("low", BAR["l"]), ("close", BAR["c"]), ("trades", f"{BAR['n']}   vol {BAR['vol']}")]
for i, (kk, vv) in enumerate(rows):
    s.text(x + 14, y3 + 122 + i * 20, kk, 13, MUTED)
    s.text(x + 90, y3 + 122 + i * 20, vv, 13, GREEN)
s.add("</g>")
s.text(x + 14, y3 + chh - 12, "a bar comes back when the next one starts", 12, MUTED)

# 2. analytics: z-score
x = xs[1]
s.box(x, y3, cw, chh)
s.text(x + 14, y3 + 26, "ZScoreNormalizer::new(20)", 15, INK, weight="bold")
s.text(x + 14, y3 + 46, "z.update(price); z.normalize(price)?", 12, MUTED)
gx0, gx1, gy0, gy1 = x + 30, x + cw - 16, y3 + 70, y3 + 180
zmin, zmax = -2.2, 2.2


def zy(z):
    return gy1 - (z - zmin) / (zmax - zmin) * (gy1 - gy0)


for zz in (-2, 0, 2):
    s.add(f'<line x1="{gx0}" x2="{gx1}" y1="{zy(zz):.1f}" y2="{zy(zz):.1f}" stroke="{LINE}" stroke-width="{1.2 if zz == 0 else 0.8}"/>')
    s.text(gx0 - 6, zy(zz) + 4, f"{zz:+d}" if zz else "0", 12, MUTED, anchor="end")
step = (gx1 - gx0) / 16
for k, z in enumerate(ZS):
    t = at(TICKS[k]) + 0.35 + POP
    px_, py_ = gx0 + k * step, zy(z)
    if k:
        pz = ZS[k - 1]
        s.add(f'<line x1="{gx0 + (k - 1) * step:.1f}" y1="{zy(pz):.1f}" x2="{px_:.1f}" y2="{py_:.1f}" stroke="{BLUE}" '
              f'stroke-width="1.8" opacity="0">{show_between(t, DUR, DUR)}</line>')
    s.add(f'<circle cx="{px_:.1f}" cy="{py_:.1f}" r="3.5" fill="{BLUE}" opacity="0">{show_between(t, DUR, DUR)}</circle>')
for k, z in enumerate(ZS):
    t = at(TICKS[k]) + 0.35 + POP
    t_end = at(TICKS[k + 1]) + 0.35 + POP if k + 1 < len(ZS) else DUR
    s.add(f'<g opacity="0">{show_between(t, t_end, DUR)}')
    s.text(x + 14, y3 + 206, f"trade {k + 1}: {TICKS[k]['price']}  ->  z = {z:+.4f}", 13, INK)
    s.add("</g>")
s.text(x + 14, y3 + chh - 12, "prices in, a model-ready feature out", 12, MUTED)

# 3. order book
x = xs[2]
s.box(x, y3, cw, chh)
s.text(x + 14, y3 + 26, "OrderBook", 15, INK, weight="bold")
s.text(x + 14, y3 + 46, "depth messages: book.apply(delta)?", 12, MUTED)
bt0, bstep = 1.0, 1.9
for j, (d, res, col) in enumerate(BOOK):
    t = bt0 + j * bstep
    yy = y3 + 72 + j * 21
    s.add(f'<g opacity="0">{show_between(t, DUR, DUR)}')
    s.text(x + 14, yy, d, 12, INK)
    s.add("</g>")
for j, (d, res, col) in enumerate(BOOK):
    t = bt0 + j * bstep
    t_end = bt0 + (j + 1) * bstep if j + 1 < len(BOOK) else DUR
    s.add(f'<g opacity="0">{show_between(t, t_end, DUR)}')
    head, _, tail = res.partition(": ")
    s.text(x + 14, y3 + 72 + 7 * 21 + 8, head + (":" if tail else ""), 13, col, weight="bold")
    if tail:
        s.text(x + 14, y3 + 72 + 7 * 21 + 27, tail, 13, col, weight="bold")
    s.add("</g>")
s.text(x + 14, y3 + chh - 12, "example deltas, real apply() results", 12, MUTED)

# arrows from ring to consumers
for xx in (xs[0] + cw / 2, xs[1] + cw / 2):
    s.arrow(xx, sy + 136, xx, y3 - 4)

s.save(sys.argv[1])
