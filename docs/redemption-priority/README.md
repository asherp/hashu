# Hashu Redemption Priority: Hydraulic Talmud Rule

How the mint shares its hash rate between redemptions that are open at the same time.

- [`delivery_sim.py`](delivery_sim.py): reference implementation and the test runs below (`python3 delivery_sim.py`).
- [`simulator.html`](simulator.html): interactive simulator. Open it in a browser; it has no build step or dependencies beyond Google Fonts.

## Problem

The mint issues hashu vouchers up to hash rate × time horizon (e.g. 12 hours). Up to 100% may be sold. Redemptions don't arrive together, but bursts can exceed the instantaneous hash rate. We need a deterministic, fair rule for metering out hashes to open redemptions so users get predictable delivery and the mint can estimate time to delivery.

## Delivery mechanism

- Each rig connected to the mint is a lane: it has its own hash rate and serves one order at a time. One order can be served by several rigs, and users may split their orders; either way service stays first come, first served.
- How many orders run in parallel depends on how many rigs are free.
- Real rigs have a startup cost when they switch orders, and production will use rented hash rate. Switching delays are left for later (see Open questions).

## Deadlines, not delivery

The hydraulic rule below only marks **when** each order should be complete. How the hashes flow in between doesn't matter as long as each order's integral reaches its size by its deadline. So the mint runs the hydraulic rule as a virtual clock to set deadlines, and the rigs deliver the real hashes to meet them. Nothing is paused at the 50% mark, which removed the incentive to game the halfway line.

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

## Fixed deadlines

An order's top T is a water level fixed on arrival, but the wall-clock time the virtual level reaches it drifts later as new orders arrive. So on arrival the mint forecasts that time, assuming no one else redeems and the quoted hash rate holds, and **fixes it as the order's deadline**. It never moves after that.

- Fixed deadlines run in arrival order, and each is no later than the drifting one (`fixed_deadline_test()`, 1,000 random runs).
- Quoted on 100% of hash rate, the fixed deadline works out to exactly when first come, first served would finish the order. That leaves no slack: every rig is needed to keep every promise, so small orders wait behind a large one.
- Quoted on a smaller share, say 80% of hash rate, deadlines are a little later and the spare rigs serve later orders in parallel. In the whale example (2 rigs of 5, a 100 order then two 5s), at 100% the small orders start at tick 10 after the whale; at 80% they start at ticks 7 and 8 and finish then, and the whale still beats its deadline.
- Deadlines hold as long as the quoted hash rate does. With hash rate changing at random every tick, fixed deadlines slipped in 734 of 1,000 runs, so the quote should use dependable hash rate, not peak.

## Delivery: rigs, earliest deadline first

`deliver_rigs()` reassigns rigs once a tick:

- Open orders are taken in deadline order (which is arrival order). Each gets just enough rigs to stay on pace for its deadline (remaining ÷ ticks left), keeping the rigs it already has where possible, fastest rigs first.
- Spare rigs go to later orders, so they run in parallel instead of queueing. Rigs nobody needs keep their current order or help the earliest one.
- A rig whose order completes mid-tick passes its leftover hashes on in deadline order.
- Result: in 1,000 random runs (2 to 8 rigs of 1 to 5 hashes a tick, constant hash rate), no order missed its fixed deadline. The pooled reference (`deliver_edf()`, all hash rate as one divisible pool) also missed none.
- Strictly one order per rig, with no rig ever helping another order, can't keep the deadlines: a large order on one small rig takes as long as it takes. That's why an order may hold several rigs, and why users splitting an order is fine.

## ETAs from assigned rigs (proxy router)

A deadline is really an ETA from the rigs assigned to an order: put a large rig on a large order and the order has an expected delivery time, remaining ÷ assigned hash rate. The mint only issues a finite number of hashes, so how fast it fills orders is bounded by the hash rate it can muster: everything owed clears no sooner than owed ÷ total hash rate, and issuance is capped at hash rate × horizon.

`deliver_router()` places rigs the way `hashu-proxy/src/router.rs` (#60) does:

- A rig stays on its order until the order completes; there is no rebalancing.
- Free rigs are placed biggest first. Tiers: an order with no rig for 2 minutes (oldest first), then any order with no rig, then orders that already have rigs. Within a tier, best fit: the order whose remaining need per minute is the smallest that still fits the rig, so large rigs land on large orders.
- An order whose rigs already finish it within a minute takes no more.

`router_test()` (150 random runs, 970 orders, 2 to 8 rigs of 1 to 8 hashes a tick):

- **The ETA from assigned rigs was never missed** (0 of 970). Rigs never leave an order before it completes and more can join, so once an order has rigs its ETA can only improve.
- **A forecast made on arrival, before rigs are assigned, slipped for 172 of 970 orders** (980 ticks in all), because later arrivals can take freed rigs first: unserved orders outrank served ones, and best fit ignores arrival order. 231 times an order finished before one that arrived earlier.
- A large order that arrives when every rig is free takes all of them and keeps them, so later small orders wait until it finishes (the simulator's "Whale arrives late" scenario shows this). A per-order cap on rigs, or reserving the smallest rigs for small orders, would keep a lane open.

So the honest promise has two stages: a forecast (≈) while an order waits for rigs, and a firm ETA once rigs are on it. The worst-case wait is bounded by everything owed ahead of it ÷ total hash rate.

## Half first: urgent first halves, flexible second halves

Before the halfway mark, redeemers want delivery as soon as possible; once they hold half, they focus on what they have, so the end date can flex. This is close to Aumann and Maschler's reading of the Talmud rule: what a claimant holds uncontested is secure, and only the rest is in dispute. `deliver_half_first()`:

- Each tick, every open order's unmet first half outranks every second half.
- First halves share the rigs: each rig goes to the first half given the least so far this tick, so a new order gets a rig as soon as there are as many rigs as first halves (the hydraulic rule's lower chamber, "share the gains"). With `share=False` first halves go oldest first instead.
- Second halves go oldest first, on whatever rigs first halves don't need. A rig on a second half moves when a new first half needs it.

`compare_test()`: the same 400 random runs (2,967 orders, 2 to 8 rigs of 1 to 8 hashes a tick) under each scheduler. Times are mean ticks from arrival.

| Scheduler | First hash | Half delivered | Finished | Small orders (≤ 10) finished | Large orders (≥ 60) finished | Missed Talmud deadline (100% quote) |
| --- | --- | --- | --- | --- | --- | --- |
| Proxy router | 4.9 | 8.9 | 11.7 | 7.5 | 18.0 | 822 |
| Talmud deadlines | 6.9 | 9.9 | 12.0 | 9.4 | 16.0 | 0 |
| Half first, oldest first | 2.5 | 4.9 | 16.6 | 13.5 | 21.4 | 1,439 |
| Half first, shared | 1.4 | 4.9 | 17.3 | 14.7 | 21.2 | 1,439 |

- Half first does what it's meant to: hashes start flowing about 3.5 times sooner than with the router, and half arrives about twice as fast.
- The cost lands on end dates: orders finish about 50% later on average, and small orders, which the router often finishes in one go, take about twice as long.
- The last order finishes at the same time under every scheduler (the total is fixed by hash rate), so this only moves who waits.
- Half the orders miss a Talmud deadline quoted on 100%, so with half first the promise has to be two times: "half by X" (fast, and only first halves compete for it) and "all by Y", a backstop such as the clear-all-owed bound or a deadline quoted on a lower share of hash rate.
- Gaming: stopping after half and re-redeeming the change would win first-half priority on half of the remainder. Mark change from a stopped order as second-phase (for example its own keyset) so re-redeeming it doesn't jump ahead. Splitting an order up front doesn't help, since the total first-half work is the same.

## Time to delivery

- With Talmud deadlines (`deliver_rigs`), the fixed deadline is the promise: shown on arrival and never moved unless hash rate falls below the quote. With router placement, the promise is the ETA from assigned rigs (see above).
- The live ETA is usually earlier, since rigs often run ahead of deadlines. Later arrivals never delay an earlier order past its deadline.
- If hash rate holds, everything is delivered within the window; the rule only sets the order of filling.

## Does splitting an order help?

Splitting is allowed; it stays first come, first served. `splitting_test()` takes 200 random scenarios, picks one order and replaces it with 2, 4 or 10 equal pieces, either all in the same tick or one per tick, delivered pooled and through random rigs (600 cases each).

| Split | Deadlines | Orders that arrived **before** the splitter | Splitter's finish | Later or same-tick orders' finish |
| --- | --- | --- | --- | --- |
| Same tick, pooled | No one's moved | Untouched | Never changed | Never changed |
| Same tick, rigs | No one's moved | Untouched | Earlier in 45 cases, later in 8 (rig granularity) | Moved 48 times, both ways, all within their deadlines |
| One per tick, pooled | Splitter's later in 377 cases | Untouched | Same or later, never earlier | Moved both ways |
| One per tick, rigs | Splitter's later in 375 cases | Untouched | Later in 386 cases, earlier once | Moved both ways |

- With fixed deadlines, splitting within one tick changes no one's deadline, the splitter's included: the pieces share the batch top, and the forecast is the same.
- No order that arrived before the splitter is ever affected, deadline or finish.
- Through rigs, splitting can land the pieces on spare rigs and finish a little sooner, or a little later; others' finishes shift too, but never past their fixed deadlines.
- Spreading pieces over time pushes the splitter's deadline later, since later pieces queue behind whoever arrived in between.
- Earlier drifting-deadline versions of this note found the hydraulic deadline gameable by splitting (a batch's top uses the largest single size). With deadlines fixed on arrival from the forecast, that no longer happens.

## Test run

Hash rate 15 per tick, 8 random orders (sizes 5 to 100) arriving over 20 ticks, two seeds. "Deadline" is the fixed deadline (quoted on 100%). "Pooled" is the divisible-pool reference; "Rigs" is delivery through four rigs of 5, 5, 3 and 2. Every order met its deadline both ways, and deadlines run in arrival order. Separately, two equal 60-unit orders at rate 10, five ticks apart: deadlines 5 and 11, finished at 5 and 11.

| Seed | Order | Size | Arrived (tick) | Deadline (tick) | Pooled finished | Rigs finished |
| --- | --- | --- | --- | --- | --- | --- |
| 7 | C | 5 | 1 | 5 | 5 | 1 |
| 7 | F | 60 | 1 | 5 | 5 | 5 |
| 7 | H | 40 | 2 | 7 | 7 | 7 |
| 7 | G | 5 | 6 | 8 | 8 | 8 |
| 7 | A | 10 | 10 | 10 | 10 | 10 |
| 7 | E | 60 | 11 | 14 | 14 | 14 |
| 7 | B | 100 | 12 | 21 | 21 | 21 |
| 7 | D | 5 | 17 | 21 | 21 | 21 |
| 11 | G | 5 | 5 | 5 | 5 | 5 |
| 11 | D | 10 | 6 | 6 | 6 | 6 |
| 11 | A | 60 | 14 | 21 | 21 | 21 |
| 11 | B | 40 | 14 | 21 | 21 | 21 |
| 11 | H | 20 | 14 | 21 | 21 | 21 |
| 11 | C | 60 | 16 | 28 | 28 | 28 |
| 11 | E | 40 | 16 | 28 | 28 | 28 |
| 11 | F | 60 | 20 | 32 | 32 | 32 |

Observation: at a 100% quote the fixed deadline is the first-come-first-served finish, so delivery has little room to beat it; rigs only get ahead through granularity (seed 7's order C, 5 units, finishes at tick 1 on a spare rig instead of 5). A lower quote gives rigs room to run small orders alongside large ones.

## Units in the simulator

The simulator shows the virtual vessels with a bar beside each for what the rigs have really delivered, a panel of rigs and the order each is serving, and the finish and fixed deadline in the table. Controls set the number of rigs, their sizes (equal, or big to small), the rig assignment (proxy router, half first, or Talmud deadlines), and the share of hash rate deadlines are quoted on. In router mode the ETA column shows "by X" once an order has rigs (firm) and "≈ X" while it waits. It uses one-minute ticks, hash rate in TH/s and order sizes in TH/s·h (1 TH/s sustained for an hour = 3.6 × 10¹⁵ hashes ≈ 3,274 TiH). Its default 900 TH/s delivers 15 TH/s·h a minute, matching the 15-per-tick test run above. Finish times there are shown as elapsed minutes, one higher than the tick indices in the table.

## Open questions

- Tick length (affects smoothness only; splits change only on arrivals).
- What share of hash rate to quote deadlines on. Lower means later promises but more spare rigs for parallel service and more headroom when hash rate dips.
- What to do when hash rate drops and deadlines can't all be met. Deadlines are fixed, so a shortfall means some get missed. Serving in deadline order puts the whole miss on the latest orders; the hydraulic rule could instead be used to spread it.
- Handling rigs going offline mid-window. Later: rig-side scheduling for rented hash rate, including buying hash rate as orders arrive vs keeping a reserve or mining continuously, and a switching delay of some number of ticks.

## Stop-and-restart: holding change until the epoch ends

Holders can stop a redemption anytime and get the remainder back as change. Under half first, re-redeeming that change right away would win first-half priority on half of it, so change must not be redeemable within the epoch it was issued.

**Time-based keysets don't work with stock cdk 0.16.** Checked against the cdk 0.16.0 source:

- One active keyset per unit. `rotate_keyset` calls `set_active_keyset(unit, id)`, and outputs must use an active keyset (`verification.rs`). So change and fresh purchases in the same epoch are signed by the same keyset and can't be told apart; there is no second active "change keyset".
- Melt change must be in the inputs' unit (`melt_saga` and `verify_transaction_balanced` return `UnitMismatch`), so change can't be issued in a separate "change" unit either.
- Even with a fork that allowed it, a change-only keyset marks those proofs as change (a smaller anonymity set), and the mint would also need to stop swaps from moving change into the regular keyset before the epoch ends.

**What works instead: delay the change.** Change is only signed when cdk finalizes the melt, and that happens when `HashrateMelter::check_outgoing_payment` (`hashu-mint/src/melt.rs`) reports `Paid` for a `Settled` redemption. If it keeps reporting `Pending` until a release time, the change simply doesn't exist yet:

- Release at the end of the epoch the stop happened in (epochs could match the stock top-up hour), or after a cooldown equal to how long the stopped order's remaining hashes would have taken at current hash rate. The cooldown version makes stopping and restarting exactly neutral.
- Works with stock wallets and the non-custodial checkout change flow: the browser already polls the melt quote and unblinds the change when it's paid.
- No privacy cost: change is signed by the normal active keyset, indistinguishable from purchases.
- Cost: the holder's change is locked until release. It's hashrate either way, so the only loss is not being able to redeem it immediately, which is the point.
- Splitting an order up front needs no rule: the total first-half work is the same either way.

## References

- Aumann, R. J. & Maschler, M. (1985). Game theoretic analysis of a bankruptcy problem from the Talmud. *Journal of Economic Theory*, 36(2), 195–213.
- Kaminski, M. M. (2000). 'Hydraulic' rationing. *Mathematical Social Sciences*, 40(2), 131–155.
- Babylonian Talmud, Bava Metzia 2a (contested garment) and Ketubot 93a (estate division).
