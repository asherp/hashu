"""Reference simulation of Hashu redemption priority (hydraulic Talmud rule).

Each open redemption is a vessel: a lower chamber of half its size, a thin
pipe, then an upper chamber of half its size ending at the vessel's top. All
vessels share one water level; each tick that tick's hashes are poured in and
the level rises until they are used up. See README.md for the rule.

The hydraulic rule only sets each order's deadline: the water level at its
top, and the tick at which that virtual level would reach it. Hashes are
actually delivered earliest-deadline-first (EDF), which never finishes an
order later than the hydraulic rule would and never pauses one at halfway.

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
    done_at: Optional[int] = None  # hydraulic (virtual) finish tick: the deadline
    delivered: float = 0.0  # hashes actually delivered under EDF
    finished: Optional[int] = None  # actual finish tick under EDF

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
    # Tops never decrease with arrival order, so EDF serves orders first come, first served.
    assert all(top >= o.top - 1e-6 for o in open_), "newcomer top below an open order's top"
    orders += [Order(oid, s, h, top, t) for oid, s in batch]


def deliver_edf(orders: list[Order], hashes: float) -> float:
    """Deliver real hashes earliest deadline (lowest top) first. Returns idle hashes.

    Ties go to the earlier arrival; orders from the same batch share in
    proportion to what they still need, so they finish together.
    """
    key = lambda o: (round(o.top, 6), o.arrived)  # equal tops can differ by float error
    queue = sorted((o for o in orders if o.finished is None), key=key)
    i = 0
    while hashes > EPS and i < len(queue):
        group = [o for o in queue[i:] if key(o) == key(queue[i])]
        need = sum(o.size - o.delivered for o in group)
        d = min(hashes, need)
        for o in group:
            o.delivered += d * (o.size - o.delivered) / need
        hashes -= d
        i += len(group)
    return hashes


def simulate(rate, arrivals: dict[int, list[tuple[str, float]]], ticks: int) -> list[Order]:
    """Run the hydraulic deadlines and EDF delivery side by side.

    `rate` is hashes per tick, a number or a function of the tick.
    """
    rate_at = rate if callable(rate) else (lambda _t: rate)
    orders: list[Order] = []
    h = 0.0
    for t in range(ticks):
        admit(orders, h, t, arrivals.get(t, []))
        r = rate_at(t)
        h, _idle = pour(orders, h, r)
        deliver_edf(orders, r)
        for o in orders:
            if o.done_at is None and o.filled(h) >= o.size - EPS:
                o.done_at = t
            if o.finished is None and o.delivered >= o.size - 1e-7:
                o.finished = t
    return orders


def random_arrivals(seed: int, n: int = 8, horizon: int = 20) -> dict[int, list[tuple[str, float]]]:
    rng = random.Random(seed)
    arr: dict[int, list[tuple[str, float]]] = {}
    for i in range(n):
        t = rng.randint(0, horizon)
        arr.setdefault(t, []).append((chr(65 + i), rng.choice([5, 10, 20, 40, 60, 100])))
    return arr


def check_fcfs(orders: list[Order], attr: str = "done_at") -> bool:
    """No order finishes before one that arrived earlier."""
    return all(not (b.arrived > a.arrived and getattr(b, attr) < getattr(a, attr)) for a in orders for b in orders)


def check_deadlines(orders: list[Order]) -> bool:
    """EDF finishes every order no later than its hydraulic deadline."""
    return all(o.finished is not None and o.finished <= o.done_at for o in orders)


def split(arrivals, oid: str, k: int, spread: bool = False):
    """Replace order `oid` with k equal pieces, in one batch or one per tick."""
    out: dict[int, list[tuple[str, float]]] = {}
    for t, batch in arrivals.items():
        for i, s in batch:
            if i != oid:
                out.setdefault(t, []).append((i, s))
                continue
            for j in range(k):
                out.setdefault(t + (j if spread else 0), []).append((f"{i}{j}", s / k))
    return out


def splitting_test(seeds: int = 500, ks=(2, 4, 10)) -> dict[str, dict[str, int]]:
    """Does splitting one order into k pieces move anyone's deadline or finish?

    Counts per mode ("same tick" or "one per tick"): cases, how many times another
    order's deadline or EDF finish moved earlier/later, and how the splitter's
    own deadline and finish (its last piece) compare with the unsplit order.
    """
    out = {m: {} for m in ("same tick", "one per tick")}
    for seed in range(seeds):
        rng = random.Random(seed)
        arr = random_arrivals(seed, n=rng.randint(3, 10), horizon=rng.randint(5, 30))
        rate = rng.choice([5, 10, 15, 30])
        base = {o.id: o for o in simulate(rate, arr, 600)}
        victim = rng.choice(sorted(base))
        for k in ks:
            for spread, mode in ((False, "same tick"), (True, "one per tick")):
                c = out[mode]
                bump = lambda key: c.__setitem__(key, c.get(key, 0) + 1)
                bump("cases")
                runs = {o.id: o for o in simulate(rate, split(arr, victim, k, spread), 600)}
                for i in base:
                    if i == victim:
                        continue
                    for attr, name in (("done_at", "deadline"), ("finished", "finish")):
                        d = getattr(runs[i], attr) - getattr(base[i], attr)
                        if d:
                            bump(f"other {name} {'earlier' if d < 0 else 'later'}")
                pieces = [o for i, o in runs.items() if i not in base]
                for attr, name in (("done_at", "deadline"), ("finished", "finish")):
                    d = max(getattr(o, attr) for o in pieces) - getattr(base[victim], attr)
                    bump(f"splitter {name} {'earlier' if d < 0 else 'later' if d > 0 else 'same'}")
    return out


if __name__ == "__main__":
    for seed in (7, 11):
        orders = simulate(15, random_arrivals(seed), 80)
        print(f"seed {seed}: first come, first served = {check_fcfs(orders)}, "
              f"EDF meets every deadline = {check_deadlines(orders)}")
        for o in sorted(orders, key=lambda o: (o.arrived, o.id)):
            print(f"  {o.id}  size {o.size:>3.0f}  arrived {o.arrived:>2}  "
                  f"deadline {o.done_at:>2}  EDF finished {o.finished:>2}")
    orders = simulate(10, {0: [("A", 60)], 5: [("B", 60)]}, 14)
    print("equal orders 5 ticks apart:", {o.id: (o.done_at, o.finished) for o in orders})
    ok = 0
    for seed in range(2000):
        rng = random.Random(seed)
        rates = [rng.choice([5, 10, 15, 30]) for _ in range(400)]
        arr = random_arrivals(seed, n=rng.randint(2, 12), horizon=rng.randint(5, 40))
        ok += check_deadlines(orders := simulate(lambda t: rates[t], arr, 400)) and check_fcfs(orders, "finished")
    print(f"random runs with varying hash rate where EDF met every deadline in arrival order: {ok}/2000")
    for mode, counts in splitting_test().items():
        print(f"split into pieces, {mode}:", dict(sorted(counts.items())))
