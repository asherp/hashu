# Home heater Worker (MDK)

An MDK Worker for a home-mining heater, or a Bitaxe, over the AxeOS HTTP API
(`GET /api/system/info`, `PATCH /api/system`, `POST /api/system/restart`).

| Telemetry | Unit |
|---|---|
| `hashrate_rt` | TH/s |
| `power` | W (= heat output) |
| `temperature` | °C, ASIC |
| `room_temperature` | °C, null if the device has no ambient sensor |
| `power_mode` | sleep, eco, normal, high |
| `pool` | `{ url, user }` |
| `shares_accepted` | count |

| Command | Params |
|---|---|
| `setupPools` | `pools: [{ url, user, pass }]`, first entry used; patches the stratum settings and restarts the stratum session |
| `setPowerMode` | `mode: sleep \| eco \| normal \| high` (ASIC frequency 0/400/525/600 MHz) |
| `reboot` | |

## Mock

`mock/server.js` (started per device with `mock: true`) is a ~40 TH/s, 1300 W
heater. It serves the AxeOS subset **and runs a real Stratum V1 client**
against whatever pool it is set to, submitting shares at the rate its
hashrate implies at the pool's difficulty. Pointed at the Hashu proxy, those
shares are routed to the redeemer's pool and credited to the open
redemption, so redemptions really fill with no hardware. The shares carry no
real proof of work, so the demo uses `../../scripts/fake-pool.js`, which
accepts every share. The mock also models the room the heater warms.

## Real hardware

- **Bitaxe**: point `opts.host` at it and drop `mock: true`. The AxeOS API is
  the same; `sleep` maps to frequency 0, which real firmware may refuse, and
  `room_temperature` reads null (the app then runs in eco).
- **Heatbit**: closed firmware with no LAN API. Set its pool to the Hashu
  proxy once in the Heatbit app; the proxy does the routing on its own.

```bash
mdk validate worker --path workers/heater --smoke   # from mdk/
```
