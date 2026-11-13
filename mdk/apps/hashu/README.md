# Hashu Heater (MDK app)

Sells a home miner's hashrate as `TiH` and keeps the room comfortable. All
telemetry and control go through the MDK Kernel (`pullTelemetry`,
`sendCommand`); the app never talks to a miner directly.

Every 2 s the controller (`plugin/lib/controller.js`, started by `plugin/main.js`):

1. reads the Hashu proxy's control API (`GET /status`) for open redemptions,
2. reads each heater's telemetry through the Kernel,
3. decides pool and heat level (`plugin/lib/policy.js`, unit tested), and
4. sends `setupPools` / `setPowerMode` through the Kernel when a heater is not where it should be.

| Situation | Pool | Heat |
|---|---|---|
| No open redemption | home pool | thermostat: sleep above setpoint + 0.5 °C, heat below setpoint − 0.5 °C |
| Redemption open | Hashu proxy | keep mining; boost if the room is cold |
| Room at the ceiling (`maxRoomC`) | unchanged | sleep, even mid-redemption, until 1 °C below the ceiling |
| Room sensor missing | unchanged | eco (never flat out blind) |
| Hashu proxy unreachable | home pool | thermostat |
| Manual override | `home` / `hashu` / `off` | `off` sleeps |

It also shows sats earned per kWh of heat against a plain electric heater
(0 sats/kWh for the same heat): `sats/kWh = (msatPerPih / 1024 / 1000) /
kWh per TiH`, where a TiH is 2^40 hashes, so `kWh per TiH = J/TH × 1.0995 / 3.6e6`.

## Routes

| Route | What |
|---|---|
| `GET /apps/hashu/api/hashu/state` | heaters, decisions and reasons, redemptions, economics, event log |
| `POST /apps/hashu/api/hashu/settings` | `{ thermostat: { setpointC, maxRoomC, enabled }, override: { <deviceId>: auto\|home\|hashu\|off } }` |
| `POST /apps/hashu/api/hashu/redeem` | demo only: open a redemption on the proxy, standing in for a Cashu melt (`allowDemoRedeem`) |

## Config (`mdk-app.yaml`, overridden per stack in `mdk.yaml`)

| Key | Default |
|---|---|
| `hashuControlUrl` | `http://127.0.0.1:3334` (`hashu proxy run --control-bind`) |
| `proxyPool` | `stratum+tcp://127.0.0.1:3333`, the Hashu proxy |
| `homePool` | `stratum+tcp://216.230.225.61:3333` (bitcoin++ Seoul Testnet4 pool) |
| `redeemPool` | pool and worker the demo Redeem button uses |
| `thermostat` | `{ enabled: true, setpointC: 21, hysteresisC: 0.5, maxRoomC: 26 }` |
| `msatPerPih` | `625` msat per PiH (1,024 TiH), ≈ 2 sats per TH-hour (about mainnet hashprice) |
| `electricitySatsPerKwh` | `150` |

## Develop

```bash
npm test                 # policy unit tests
npm run build:ui         # ui-src/ -> ui/ (commit ui/)
npm run typecheck
mdk validate app hashu   # from mdk/
```
