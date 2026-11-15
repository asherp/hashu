"""Reference simulation of Hashu redemption priority (hydraulic Talmud rule).

Each open redemption is a vessel: a lower chamber of half its size, a thin
pipe, then an upper chamber of half its size ending at the vessel's top. All
vessels share one water level; each tick that tick's hashes are poured in and
the level rises until they are used up. See README.md for the rule.

The hydraulic rule only sets each order's deadline: on arrival, the tick at
which the virtual level would reach its top if no one else redeemed. That
deadline is then fixed. Real hashes come from rigs, each serving one order at
a time, taken earliest deadline first with just enough rigs per order to stay
on pace (`deliver_rigs`); `deliver_edf` is the divisible-pool reference.

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
    done_at: Optional[int] = None  # tick the virtual level actually reached the top (drifts with later arrivals)
    deadline: Optional[int] = None  # hydraulic finish tick forecast on arrival, then fixed
    delivered: float = 0.0  # hashes actually delivered under EDF
    finished: Optional[int] = None  # actual finish tick under EDF
    started: Optional[int] = None  # tick it first received hashes

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


def forecast(orders: list[Order], h: float, t: int, rate: float, new: list[Order]) -> None:
    """Fix each new order's deadline: the tick the virtual level would reach its
    top if no one else redeemed and the hash rate held."""
    sim = [Order(o.id, o.size, o.base, o.top, o.arrived) for o in orders if o.done_at is None]
    left = {o.id for o in new}
    for tick in range(t, t + 100000):
        h, _ = pour(sim, h, rate)
        for o in sim:
            if o.done_at is None and o.filled(h) >= o.size - EPS:
                o.done_at = tick
                if o.id in left:
                    next(n for n in new if n.id == o.id).deadline = tick
                    left.discard(o.id)
        if not left:
            return


def admit(orders: list[Order], h: float, t: int, batch: list[tuple[str, float]], rate: float = 0.0) -> None:
    """Add same-tick arrivals as one batch sharing one top. Existing tops never move.

    With `rate`, also fix each newcomer's deadline (see `forecast`)."""
    if not batch:
        return
    open_ = [o for o in orders if o.done_at is None]
    top = h + max([s for _, s in batch] + [o.size - o.filled(h) for o in open_])
    # Tops never decrease with arrival order, so EDF serves orders first come, first served.
    assert all(top >= o.top - 1e-6 for o in open_), "newcomer top below an open order's top"
    new = [Order(oid, s, h, top, t) for oid, s in batch]
    orders += new
    if rate > 0:
        forecast(orders, h, t, rate, new)


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


def deliver_rigs(orders: list[Order], rigs: list[float], assign: dict[int, str], t: int) -> int:
    """Deliver through physical rigs, each serving one order at a time. Returns switches.

    Orders are taken earliest deadline first. Each gets just enough rigs to stay on
    pace for its fixed deadline (remaining ÷ ticks left), keeping the rigs it already
    has where possible; spare rigs then go to later orders, so small orders run in
    parallel with a large one instead of queueing behind it. Rigs nobody needs keep
    their current order, or help the earliest one. A rig whose order completes
    mid-tick passes its leftover hashes on in deadline order.
    """
    open_ = sorted((o for o in orders if o.finished is None), key=lambda o: (o.deadline, o.arrived, o.id))
    by_id = {o.id: o for o in open_}
    free, new = list(range(len(rigs))), {}
    for o in open_:
        need = (o.size - o.delivered) / max(1, o.deadline - t + 1)
        mine = [r for r in free if assign.get(r) == o.id]
        others = sorted((r for r in free if r not in mine), key=lambda r: -rigs[r])
        have = 0.0
        for r in mine + others:
            if have >= need - EPS:
                break
            new[r], have = o.id, have + rigs[r]
            free.remove(r)
    for r in free:
        if assign.get(r) in by_id:
            new[r] = assign[r]
        elif open_:
            new[r] = open_[0].id
    switches = sum(1 for r, oid in new.items() if assign.get(r) in by_id and assign[r] != oid)
    assign.clear()
    assign.update(new)
    spare = 0.0
    for r, oid in assign.items():
        o = by_id[oid]
        d = min(rigs[r], o.size - o.delivered)
        if d > 0 and o.started is None:
            o.started = t
        o.delivered += d
        spare += rigs[r] - d
    for o in open_:
        d = min(spare, o.size - o.delivered)
        if d > 0 and o.started is None:
            o.started = t
        o.delivered += d
        spare -= d
    return switches


def simulate(rate, arrivals: dict[int, list[tuple[str, float]]], ticks: int,
             rigs: Optional[list[float]] = None, quote: float = 1.0) -> list[Order]:
    """Run the hydraulic deadlines and real delivery side by side.

    `rate` is hashes per tick, a number or a function of the tick. With `rigs`
    (hashes per tick for each rig; `rate` is then their sum) delivery goes through
    `deliver_rigs`; without it, the hash rate is one divisible pool served EDF.
    Deadlines are quoted on `quote` × the hash rate; below 1 leaves slack.
    """
    if rigs is not None:
        rate = sum(rigs)
    rate_at = rate if callable(rate) else (lambda _t: rate)
    orders: list[Order] = []
    assign: dict[int, str] = {}
    h = 0.0
    for t in range(ticks):
        r = rate_at(t)
        admit(orders, h, t, arrivals.get(t, []), r * quote)
        h, _idle = pour(orders, h, r)
        if rigs is None:
            deliver_edf(orders, r)
        else:
            deliver_rigs(orders, rigs, assign, t)
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
    """No order reaches `attr` before one that arrived earlier."""
    return all(not (b.arrived > a.arrived and getattr(b, attr) < getattr(a, attr)) for a in orders for b in orders)


def check_deadlines(orders: list[Order], attr: str = "deadline") -> bool:
    """Every order finished no later than its deadline (fixed, or `done_at` for the drifting one)."""
    return all(o.finished is not None and o.finished <= getattr(o, attr) for o in orders)


def random_rigs(rng: random.Random) -> list[float]:
    return [rng.choice([1, 2, 3, 5]) for _ in range(rng.choice([2, 3, 5, 8]))]


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


def fixed_deadline_test(seeds: int = 1000) -> dict[str, int]:
    """Constant hash rate: are fixed deadlines met, pooled and through rigs?"""
    c = {"runs": 0, "orders": 0, "pooled missed": 0, "rigs missed": 0,
         "deadlines out of arrival order": 0, "fixed later than drifting": 0}
    for seed in range(seeds):
        rng = random.Random(seed)
        arr = random_arrivals(seed, n=rng.randint(2, 12), horizon=rng.randint(5, 40))
        rigs = random_rigs(rng)
        pooled = simulate(sum(rigs), arr, 3000)
        via_rigs = simulate(None, arr, 3000, rigs=rigs)
        c["runs"] += 1
        c["orders"] += len(pooled)
        c["pooled missed"] += sum(o.finished > o.deadline for o in pooled)
        c["rigs missed"] += sum(o.finished > o.deadline for o in via_rigs)
        c["deadlines out of arrival order"] += not check_fcfs(pooled, "deadline")
        c["fixed later than drifting"] += sum(o.deadline > o.done_at for o in pooled)
    return c


def splitting_test(seeds: int = 200, ks=(2, 4, 10), rigs: bool = False) -> dict[str, dict[str, int]]:
    """Does splitting one order into k pieces move anyone's deadline or finish?

    Counts per mode ("same tick" or "one per tick"): cases, how many times another
    order's fixed deadline or actual finish moved earlier/later (by whether it
    arrived before, with or after the splitter), and how the splitter's own
    deadline and finish (its last piece) compare with the unsplit order.
    """
    out = {m: {} for m in ("same tick", "one per tick")}
    for seed in range(seeds):
        rng = random.Random(seed)
        arr = random_arrivals(seed, n=rng.randint(3, 10), horizon=rng.randint(5, 30))
        rate = rng.choice([5, 10, 15, 30])
        rr = random_rigs(rng) if rigs else None
        base = {o.id: o for o in simulate(rate, arr, 3000, rigs=rr)}
        victim = rng.choice(sorted(base))
        for k in ks:
            for spread, mode in ((False, "same tick"), (True, "one per tick")):
                c = out[mode]
                bump = lambda key: c.__setitem__(key, c.get(key, 0) + 1)
                bump("cases")
                runs = {o.id: o for o in simulate(rate, split(arr, victim, k, spread), 3000, rigs=rr)}
                for i, o in base.items():
                    if i == victim:
                        continue
                    when = "before" if o.arrived < base[victim].arrived else "with" if o.arrived == base[victim].arrived else "after"
                    for attr in ("deadline", "finished"):
                        d = getattr(runs[i], attr) - getattr(o, attr)
                        if d:
                            bump(f"other {attr} {'earlier' if d < 0 else 'later'} (arrived {when})")
                pieces = [o for i, o in runs.items() if i not in base]
                for attr in ("deadline", "finished"):
                    d = max(getattr(o, attr) for o in pieces) - getattr(base[victim], attr)
                    bump(f"splitter {attr} {'earlier' if d < 0 else 'later' if d > 0 else 'same'}")
    return {m: dict(sorted(c.items())) for m, c in out.items()}


if __name__ == "__main__":
    rigs = [5, 5, 3, 2]  # 15 hashes per tick in total
    for seed in (7, 11):
        pooled = simulate(15, random_arrivals(seed), 80)
        via = {o.id: o for o in simulate(None, random_arrivals(seed), 80, rigs=rigs)}
        print(f"seed {seed}: deadlines in arrival order = {check_fcfs(pooled, 'deadline')}, "
              f"pooled meets deadlines = {check_deadlines(pooled)}, rigs meet deadlines = {check_deadlines(list(via.values()))}")
        for o in sorted(pooled, key=lambda o: (o.arrived, o.id)):
            print(f"  {o.id}  size {o.size:>3.0f}  arrived {o.arrived:>2}  deadline {o.deadline:>2}  "
                  f"pooled finished {o.finished:>2}  rigs {rigs} finished {via[o.id].finished:>2}")
    orders = simulate(10, {0: [("A", 60)], 5: [("B", 60)]}, 14)
    print("equal orders 5 ticks apart (deadline, finished):", {o.id: (o.deadline, o.finished) for o in orders})
    orders = simulate(None, {0: [("W", 100)], 1: [("S", 5)], 2: [("T", 5)]}, 40, rigs=[5, 5])
    print("whale + two small orders, 2 rigs of 5 (deadline, started, finished):",
          {o.id: (o.deadline, o.started, o.finished) for o in orders})
    print("fixed deadlines, constant hash rate:", fixed_deadline_test())
    print("fixed deadlines quoted on 80% of hash rate:", {o.id: (o.deadline, o.started, o.finished) for o in
          simulate(None, {0: [("W", 100)], 1: [("S", 5)], 2: [("T", 5)]}, 40, rigs=[5, 5], quote=0.8)})
    ok = slipped = 0
    for seed in range(1000):
        rng = random.Random(seed)
        rates = [rng.choice([5, 10, 15, 30]) for _ in range(400)]
        arr = random_arrivals(seed, n=rng.randint(2, 12), horizon=rng.randint(5, 40))
        orders = simulate(lambda t: rates[t], arr, 400)
        ok += check_deadlines(orders, "done_at") and check_fcfs(orders, "finished")
        slipped += not check_deadlines(orders)
    print(f"varying hash rate: pooled EDF met the virtual finish in arrival order in {ok}/1000 runs; "
          f"fixed deadlines slipped in {slipped}/1000")
    for label, rr in (("pooled", False), ("rigs", True)):
        for mode, counts in splitting_test(rigs=rr).items():
            print(f"split, {label}, {mode}:", counts)
