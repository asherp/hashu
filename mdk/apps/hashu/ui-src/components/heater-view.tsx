import { useState } from 'react'
import { EmptyState, ErrorCard, Loader, SingleStatCard } from '@tetherto/mdk-react-devkit'
import type { HashuState, Heater } from '../data/hashu'

const fmt = (n: number | null | undefined, d = 1) => (n === null || n === undefined || !Number.isFinite(n) ? '—' : n.toFixed(d))
const hostOf = (url?: string) => (url || '').replace(/^stratum\+tcp:\/\//, '')

const MODE_LABEL: Record<string, string> = { sleep: 'Off (sleep)', eco: 'Eco', normal: 'Normal', high: 'Boost' }

export function HeaterCard ({ d, s, onOverride }: { d: Heater; s: HashuState; onOverride: (mode: string) => void }) {
  const poolName = d.onHashu ? 'Hashu proxy (selling THH)' : 'Home pool'
  return (
    <div className="hashu-card">
      <div className="hashu-card-head">
        <strong>{d.deviceId}</strong>
        <span className={`hashu-pill ${d.online ? (d.onHashu ? 'sell' : 'home') : 'off'}`}>
          {d.online ? (d.onHashu ? 'Selling hashrate' : 'Mining for you') : 'Offline'}
        </span>
      </div>
      <div className="hashu-grid">
        <div><label>Room</label><b>{fmt(d.roomC)} °C</b><small>target {s.thermostat.setpointC} °C</small></div>
        <div><label>Heat</label><b>{fmt(d.powerW, 0)} W</b><small>{MODE_LABEL[d.mode] || d.mode}</small></div>
        <div><label>Hashrate</label><b>{fmt(d.hashrateThs, 1)} TH/s</b><small>{d.sharesAccepted} shares</small></div>
        <div><label>Pool</label><b>{poolName}</b><small>{hostOf(d.pool?.url)} · {d.pool?.user}</small></div>
      </div>
      <p className="hashu-why">Pool: {d.target.poolReason}. Heat: {d.target.modeReason}.</p>
      <div className="hashu-row">
        <span>Control:</span>
        {(['auto', 'home', 'hashu', 'off'] as const).map((m) => (
          <button key={m} className={d.override === m ? 'on' : ''} onClick={() => onOverride(m)}>{m}</button>
        ))}
      </div>
    </div>
  )
}

export function HashuView ({ view, setSettings, redeem, compact }: {
  view: any
  setSettings: (b: unknown) => Promise<void>
  redeem: (b: unknown) => Promise<void>
  compact?: boolean
}) {
  const [amount, setAmount] = useState(1)
  const [err, setErr] = useState<string | null>(null)
  if (view.status === 'loading') return <Loader />
  if (view.status === 'error') return <ErrorCard title="Could not load Hashu" error={view.message} />
  const s: HashuState = view.data
  if (!s.kernelConnected && s.devices.length === 0) return <EmptyState description="Waiting for the MDK Kernel." size="sm" />
  const e = s.fleet.economics
  const run = (p: Promise<void>) => p.then(() => setErr(null)).catch((x: any) => setErr(x?.body?.error || x?.message || String(x)))

  const stats = (
    <div className="hashu-summary">
      <SingleStatCard name="Sats per kWh of heat" value={Math.round(e.satsPerKwh)} />
      <SingleStatCard name="Electric heater" value={0} />
      <SingleStatCard name="Heating bill paid back" value={`${fmt(e.billOffsetPct, 0)}%`} />
      <SingleStatCard name="THH sold" value={fmt(s.totals.thhSold, 2)} />
      <SingleStatCard name="Heat delivered (kWh)" value={fmt(s.totals.heatKwh, 3)} />
    </div>
  )
  if (compact) return stats

  return (
    <div className="hashu-stack">
      {!s.hashu.reachable && (
        <div className="hashu-alert">Hashu proxy unreachable at {s.hashu.controlUrl} ({s.hashu.error}). Heaters fall back to the home pool.</div>
      )}
      {err && <div className="hashu-alert">{err}</div>}
      {stats}
      <p className="hashu-note">
        Every watt a miner draws ends up as heat, exactly like an electric heater. This one also earns {s.prices.satsPerThh} sats per THH
        it sells: {fmt(e.joulesPerTh, 1)} J/TH means {fmt(e.kwhPerThh, 4)} kWh per THH, so about {Math.round(e.satsPerKwh)} sats per kWh,
        against electricity at {s.prices.electricitySatsPerKwh} sats/kWh.
      </p>

      {s.devices.map((d) => (
        <HeaterCard key={d.deviceId} d={d} s={s} onOverride={(m) => run(setSettings({ override: { [d.deviceId]: m } }))} />
      ))}

      <div className="hashu-card">
        <div className="hashu-card-head"><strong>Thermostat</strong>
          <button className={s.thermostat.enabled ? 'on' : ''} onClick={() => run(setSettings({ thermostat: { enabled: !s.thermostat.enabled } }))}>
            {s.thermostat.enabled ? 'on' : 'off'}
          </button>
        </div>
        <div className="hashu-row">
          <span>Target {s.thermostat.setpointC} °C</span>
          <button onClick={() => run(setSettings({ thermostat: { setpointC: s.thermostat.setpointC - 0.5 } }))}>−</button>
          <button onClick={() => run(setSettings({ thermostat: { setpointC: s.thermostat.setpointC + 0.5 } }))}>+</button>
          <span>Never above {s.thermostat.maxRoomC} °C, even while selling.</span>
        </div>
      </div>

      <div className="hashu-card">
        <div className="hashu-card-head"><strong>THH redemptions</strong><span>{s.hashu.activeRedemptions} open</span></div>
        {s.hashu.redemptions.length === 0 && <p className="hashu-why">None yet.</p>}
        {s.hashu.redemptions.map((r) => (
          <div key={r.id} className="hashu-redemption">
            <span>{r.id.slice(0, 8)}</span>
            <span>{r.amount_thh} THH → {r.worker_name} @ {hostOf(r.pool_url)}</span>
            <div className="hashu-bar"><div style={{ width: `${Math.round(r.progress * 100)}%` }} /></div>
            <span>{r.state === 'completed' ? 'filled' : `${Math.round(r.progress * 100)}%`}</span>
          </div>
        ))}
        {s.allowDemoRedeem && (
          <div className="hashu-row">
            <span>Demo: redeem</span>
            <input type="number" min={1} max={100} value={amount} onChange={(ev) => setAmount(Number(ev.target.value) || 1)} />
            <span>THH to {s.pools.redeem.user} @ {hostOf(s.pools.redeem.url)}</span>
            <button onClick={() => run(redeem({ amountThh: amount }))}>Redeem</button>
          </div>
        )}
      </div>

      <div className="hashu-card">
        <div className="hashu-card-head"><strong>What happened</strong></div>
        <ul className="hashu-events">
          {s.events.map((ev, i) => (
            <li key={i} className={ev.kind}>
              <time>{new Date(ev.at).toLocaleTimeString()}</time> {ev.deviceId ? <b>{ev.deviceId}</b> : null} {ev.text}
            </li>
          ))}
        </ul>
      </div>
    </div>
  )
}
