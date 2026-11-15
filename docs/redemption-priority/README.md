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

## Deadlines, not delivery

The hydraulic rule below only marks **when** each order should be complete. How the hashes flow in between doesn't matter as long as each order's integral reaches its size by its deadline. So the mint runs the hydraulic rule as a virtual clock to set deadlines, and delivers the real hashes **earliest deadline first** (EDF). Nothing is paused at the 50% mark, which removed the incentive to game the halfway line.

## Deadline rule: Kaminski's hydraulic form of the Talmud (contested garment) rule

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

An order's deadline is its top T, a water level fixed on arrival. In wall-clock terms it is the tick at which the virtual level reaches T. That tick isn't fixed on arrival, since later arrivals slow the virtual level.

## Delivery: earliest deadline first

- Each tick, the real hashes go to the open order with the lowest top. Ties go to the earlier arrival; orders that arrived in the same tick share in proportion to what they still need, so they finish together.
- A newcomer's top is never below an open order's top (checked as an assertion in every run below), so EDF serves orders in arrival order. In practice the Talmud deadlines turn delivery into first come, first served, with same-tick batches sharing.
- The head of the queue only changes when an order completes, so each order gets one continuous run of hashes. That also means rigs only switch orders at completions, which suits real rigs with a switching cost.
- EDF never finishes an order later than its hydraulic deadline: the virtual system and EDF receive the same arrivals and the same hashes, and EDF always works on the order the virtual system would finish first. `delivery_sim.py` checks this in 2,000 random runs with the hash rate changing every tick: every order met its deadline, in arrival order.
- Both systems deliver the same total each tick while anything is open, so the cumulative hashes delivered are identical; only who gets them when differs.

## Time to delivery

- Under EDF, later arrivals always queue behind, so an order's finish time is set the moment it arrives: the work ahead of it divided by hash rate. Optimistic and worst-case estimates coincide, and only a change in hash rate moves the ETA. Base it on dependable hash rate rather than peak.
- The hydraulic deadline is a looser bound. Its wall-clock time drifts later as new orders arrive, so it's not what the wallet should show.
- If hash rate holds, everything is delivered within the window; the rule only sets the order of filling.

## Does splitting an order help?

`splitting_test()` takes 500 random scenarios, picks one order and replaces it with 2, 4 or 10 equal pieces, either all in the same tick or one per tick (3,000 cases).

| Split | Anyone else's actual finish moved | Splitter's actual finish | Hydraulic deadlines |
| --- | --- | --- | --- |
| Same tick (1,500 cases) | Never | Never changed | Splitter's deadline earlier in 206 cases; other orders' deadlines moved 982 times (753 later, 229 earlier) |
| One piece per tick (1,500 cases) | Other orders finished earlier 822 times, later 319 times | Later in 962 cases, never earlier | Shift both ways |

- Splitting within one tick changes nothing that is delivered: the pieces sit in the same place in the queue as the whole order would.
- Spreading pieces over time only hurts the splitter: orders arriving in between go ahead of the later pieces. The 319 "later" moves come from mint idle time, when the splitter held back work and the rigs had nothing to do; that's the same as the splitter simply arriving late.
- The hydraulic deadlines themselves **are** gameable: a large order split into small pieces gets a lower top, because a batch's top uses the largest single size. That's harmless while deadlines only rank orders and ETAs come from EDF, but it means the hydraulic deadline should never be shown or promised as a delivery time.

## Test run

Hash rate 15 per tick, 8 random orders (sizes 5 to 100) arriving over 20 ticks, two seeds. "Deadline" is the hydraulic finish; "EDF" is when the order actually finished. Every order met its deadline and no order finished before an earlier arrival. Separately, two equal 60-unit orders at rate 10, five ticks apart: deadlines 6 and 11, EDF finished at 5 and 11.

| Seed | Order | Size | Arrived (tick) | Deadline (tick) | EDF finished (tick) |
| --- | --- | --- | --- | --- | --- |
| 7 | C | 5 | 1 | 8 | 5 |
| 7 | F | 60 | 1 | 8 | 5 |
| 7 | H | 40 | 2 | 8 | 7 |
| 7 | G | 5 | 6 | 8 | 8 |
| 7 | A | 10 | 10 | 10 | 10 |
| 7 | E | 60 | 11 | 18 | 14 |
| 7 | B | 100 | 12 | 21 | 21 |
| 7 | D | 5 | 17 | 21 | 21 |
| 11 | G | 5 | 5 | 5 | 5 |
| 11 | D | 10 | 6 | 6 | 6 |
| 11 | A | 60 | 14 | 29 | 21 |
| 11 | B | 40 | 14 | 29 | 21 |
| 11 | H | 20 | 14 | 29 | 21 |
| 11 | C | 60 | 16 | 31 | 28 |
| 11 | E | 40 | 16 | 31 | 28 |
| 11 | F | 60 | 20 | 32 | 32 |

Observation: small orders arriving while a large order is open get the same or a later deadline and, under EDF, wait behind it. In the hydraulic model they would at least receive some hashes while waiting; under EDF they receive none until the orders ahead finish, but never finish later.

## Units in the simulator

The simulator shows the virtual vessels with a bar beside each for what EDF has really delivered, and both finish times in the table. It uses one-minute ticks, hash rate in TH/s and order sizes in TH/s·h (1 TH/s sustained for an hour = 3.6 × 10¹⁵ hashes ≈ 3,274 TiH). Its default 900 TH/s delivers 15 TH/s·h a minute, matching the 15-per-tick test run above. Finish times there are shown as elapsed minutes, one higher than the tick indices in the table.

## Open questions

- Tick length (affects smoothness only; splits change only on arrivals).
- Small orders queue behind a large earlier one. If that wait is too long, the fix is a cap on order size or a separate lane for small orders, not a change to the deadline rule.
- What to do when hash rate drops and deadlines can't all be met. EDF keeps arrival order, so the latest orders absorb the whole shortfall; the hydraulic rule could instead be used to spread it.
- Handling rigs going offline mid-window. Later: rig-side scheduling for rented hash rate, including buying hash rate as orders arrive vs keeping a reserve or mining continuously, and a switching delay of some number of ticks.

**Note: stop-and-restart.** Holders can stop a redemption anytime and get the remainder back as change. Under EDF, a re-redeemed remainder simply joins the back of the queue, so stopping and restarting can't jump ahead of anyone; it only costs the holder their place. The earlier concern was that, under the pure hydraulic split, re-redeeming as a fresh bucket would regain lower-half priority. The fix proposed then, kept here in case delivery ever follows the hydraulic split directly: stamp change with the water level where the old bucket would have resumed (via a keyset per water-level band, bands wide enough to keep anonymity sets), and base the re-redeemed bucket at that level. Water level already accounts for new arrivals, so stopping and restarting is neutral: no faster, no slower.

## References

- Aumann, R. J. & Maschler, M. (1985). Game theoretic analysis of a bankruptcy problem from the Talmud. *Journal of Economic Theory*, 36(2), 195–213.
- Kaminski, M. M. (2000). 'Hydraulic' rationing. *Mathematical Social Sciences*, 40(2), 131–155.
- Babylonian Talmud, Bava Metzia 2a (contested garment) and Ketubot 93a (estate division).
