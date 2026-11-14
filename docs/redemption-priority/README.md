# Hashu Redemption Priority: Hydraulic Talmud Rule

How the mint shares its hash rate between redemptions that are open at the same time.

- [`delivery_sim.py`](delivery_sim.py): reference implementation and the test runs below (`python3 delivery_sim.py`).
- [`simulator.html`](simulator.html): interactive simulator. Open it in a browser; it has no build step or dependencies beyond Google Fonts.

## Problem

The mint issues hashu vouchers up to hash rate × time horizon (e.g. 12 hours). Up to 100% may be sold. Redemptions don't arrive together, but bursts can exceed the instantaneous hash rate. We need a deterministic, fair rule for metering out hashes to open redemptions so users get predictable delivery and the mint can estimate time to delivery.

## Delivery mechanism

- Each rig connected to the mint has its own hash rate and serves one order at a time; one order can be served by several rigs.
- For now the model treats the rigs as one divisible pool that switches between orders instantly: the rule decides each order's share per tick, and rig assignment just carries out that split. Delivery then depends on total hash rate, not rig count.
- Real rigs have a startup cost when they switch orders, and production will use rented hash rate. Rig-side scheduling is left for later (see Open questions).

## The rule: Kaminski's hydraulic form of the Talmud (contested garment) rule

- Each order is a vessel: a lower chamber of half its size, a thin pipe, then an upper chamber of half its size ending at the vessel's top T.
- All vessels share one water level h; each tick's hashes are poured in and h rises until they are used up.
- A lone order has no pipe (top = base + size), so an uncontested claim fills straight up.
- New orders enter with their base at the current water level. Same-tick arrivals are one batch sharing one top.
- A batch's top = water level + the largest of (each newcomer's size, each open order's remaining). Existing tops never move.
- Below halfway, everyone open gains equally (Talmud: share the gains). Above halfway, remaining amounts are equalised (Talmud: share the losses).
- Because a newcomer's top is at least every open order's remaining finish line, no order finishes before one that arrived earlier (first come, first served), while an equal order arriving later finishes later by about the delay.
- Delayed arrival differs continuously from simultaneous arrival.

Delivered amount for an order of size c, base b and top T at water level h:

```
filled = min(c, min(h − b, c/2) + max(0, h − (T − c/2)))    (zero when h ≤ b)
```

Each tick, the new level is found by bisection on h so that the total delivered rises by exactly that tick's hashes (or to the highest open top, with the rest idle).

## Time to delivery

- Optimistic estimate: assume no new redemptions.
- Worst case: assume all unredeemed vouchers in the window are redeemed immediately. This is bounded because issuance is capped. Base it on dependable hash rate rather than peak, and update it if new vouchers are issued when rigs join.
- If hash rate holds, everything is delivered within the window; the rule only sets the order of filling.

## Test run

Hash rate 15 per tick, 8 random orders (sizes 5 to 100) arriving over 20 ticks, two seeds. No order finished before an earlier arrival. Separately, two equal 60-unit orders at rate 10, five ticks apart, finished at ticks 6 and 11.

| Seed | Order | Size | Arrived (tick) | Finished (tick) |
| --- | --- | --- | --- | --- |
| 7 | C | 5 | 1 | 8 |
| 7 | F | 60 | 1 | 8 |
| 7 | H | 40 | 2 | 8 |
| 7 | G | 5 | 6 | 8 |
| 7 | A | 10 | 10 | 10 |
| 7 | E | 60 | 11 | 18 |
| 7 | B | 100 | 12 | 21 |
| 7 | D | 5 | 17 | 21 |
| 11 | G | 5 | 5 | 5 |
| 11 | D | 10 | 6 | 6 |
| 11 | A | 60 | 14 | 29 |
| 11 | B | 40 | 14 | 29 |
| 11 | H | 20 | 14 | 29 |
| 11 | C | 60 | 16 | 31 |
| 11 | E | 40 | 16 | 31 |
| 11 | F | 60 | 20 | 32 |

Observation: small orders arriving while a large order is open line up with its top and finish alongside it rather than ahead of it.

## Units in the simulator

The simulator uses one-minute ticks, hash rate in TH/s and order sizes in TH/s·h (1 TH/s sustained for an hour = 3.6 × 10¹⁵ hashes ≈ 3,274 TiH). Its default 900 TH/s delivers 15 TH/s·h a minute, matching the 15-per-tick test run above. Finish times there are shown as elapsed minutes, one higher than the tick indices in the table.

## Open questions

- Tick length (affects smoothness only; splits change only on arrivals).
- Whether small late orders should ride along with a large open order or finish behind earlier ones.
- Handling rigs going offline mid-window. Later: rig-side scheduling for rented hash rate, including buying hash rate as orders arrive vs keeping a reserve or mining continuously, and a switching delay of some number of ticks.

**Note: stop-and-restart.** Holders can stop a redemption anytime and get the remainder back as change. Re-redeeming as a fresh bucket would regain lower-half priority and jump ahead of waiting orders. Proposed fix: stamp change with the water level where the old bucket would have resumed (via a keyset per water-level band, bands wide enough to keep anonymity sets), and base the re-redeemed bucket at that level. Water level already accounts for new arrivals, so stopping and restarting is neutral: no faster, no slower.

## References

- Aumann, R. J. & Maschler, M. (1985). Game theoretic analysis of a bankruptcy problem from the Talmud. *Journal of Economic Theory*, 36(2), 195–213.
- Kaminski, M. M. (2000). 'Hydraulic' rationing. *Mathematical Social Sciences*, 40(2), 131–155.
- Babylonian Talmud, Bava Metzia 2a (contested garment) and Ketubot 93a (estate division).
