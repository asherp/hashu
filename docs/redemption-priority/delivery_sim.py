"""Reference simulation of Hashu redemption priority (hydraulic Talmud rule).

Each open redemption is a vessel: a lower chamber of half its size, a thin
pipe, then an upper chamber of half its size ending at the vessel's top. All
vessels share one water level; each tick that tick's hashes are poured in and
the level rises until they are used up. See README.md for the rule.

Run `python3 delivery_sim.py` to reproduce the test runs in README.md.
"""
from __future__ import annotations

import random
from dataclasses import dataclass
from typing import Optional

EPS = 1e-9


@dataclass
class Order:
    id: str
    size: float
    base: float  # water level when the order arrived
    top: float  # level at which the vessel is full
    arrived: int
    done_at: Optional[int] = None

    def filled(self, h: float) -> float:
        """Hashes delivered to this order at water level h."""
        x = h - self.base
        if x <= 0:
            return 0.0
        half = self.size / 2
        return min(self.size, min(x, half) + max(0.0, h - (self.top - half)))


def total(orders: list[Order], h: float) -> float:
    return sum(o.filled(h) for o in orders)


def pour(orders: list[Order], h: float, hashes: float) -> tuple[float, float]:
    """Raise the level from h by `hashes`. Returns (new level, idle hashes)."""
    open_ = [o for o in orders if o.done_at is None]
    if not open_:
        return h, hashes
    top = max(o.top for o in open_)
    avail = total(open_, top) - total(open_, h)
    if avail <= hashes:
        return top, hashes - avail
    lo, hi, target = h, top, total(open_, h) + hashes
    for _ in range(200):
        mid = (lo + hi) / 2
        lo, hi = (mid, hi) if total(open_, mid) < target else (lo, mid)
    return hi, 0.0


def admit(orders: list[Order], h: float, t: int, batch: list[tuple[str, float]]) -> None:
    """Add same-tick arrivals as one batch sharing one top. Existing tops never move."""
    if not batch:
        return
    open_ = [o for o in orders if o.done_at is None]
    top = h + max([s for _, s in batch] + [o.size - o.filled(h) for o in open_])
    orders += [Order(oid, s, h, top, t) for oid, s in batch]


def simulate(rate: float, arrivals: dict[int, list[tuple[str, float]]], ticks: int) -> list[Order]:
    orders: list[Order] = []
    h = 0.0
    for t in range(ticks):
        admit(orders, h, t, arrivals.get(t, []))
        h, _idle = pour(orders, h, rate)
        for o in orders:
            if o.done_at is None and o.filled(h) >= o.size - EPS:
                o.done_at = t
    return orders


def random_arrivals(seed: int, n: int = 8, horizon: int = 20) -> dict[int, list[tuple[str, float]]]:
    rng = random.Random(seed)
    arr: dict[int, list[tuple[str, float]]] = {}
    for i in range(n):
        t = rng.randint(0, horizon)
        arr.setdefault(t, []).append((chr(65 + i), rng.choice([5, 10, 20, 40, 60, 100])))
    return arr


def check_fcfs(orders: list[Order]) -> bool:
    """No order finishes before one that arrived earlier."""
    return all(not (b.arrived > a.arrived and b.done_at < a.done_at) for a in orders for b in orders)


if __name__ == "__main__":
    for seed in (7, 11):
        orders = simulate(15, random_arrivals(seed), 80)
        print(f"seed {seed}: first come, first served = {check_fcfs(orders)}")
        for o in sorted(orders, key=lambda o: (o.arrived, o.id)):
            print(f"  {o.id}  size {o.size:>3.0f}  arrived {o.arrived:>2}  finished {o.done_at:>2}")
    orders = simulate(10, {0: [("A", 60)], 5: [("B", 60)]}, 14)
    print("equal orders 5 ticks apart:", {o.id: o.done_at for o in orders})
