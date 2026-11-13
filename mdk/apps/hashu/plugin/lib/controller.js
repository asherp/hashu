'use strict'

// The heater controller: one loop, started by main.js, read by the routes.
//
// Every tick it
//   1. asks the Hashu proxy's control API whether a TiH redemption is open,
//   2. reads each heater's telemetry through the MDK Kernel (pullTelemetry),
//   3. decides pool and heat level (policy.js), and
//   4. sends setupPools / setPowerMode through the Kernel (sendCommand)
//      when the device is not where it should be.
//
// It never talks to a miner directly: telemetry and control go through MDK.

const { TIH_PER_TH_HOUR, choosePool, chooseHeat, economics, samePool } = require('./policy')

const DEFAULTS = {
  hashuControlUrl: 'http://127.0.0.1:3334',
  proxyPool: { url: 'stratum+tcp://127.0.0.1:3333', user: 'hashu.heater', pass: 'x' },
  homePool: { url: 'stratum+tcp://127.0.0.1:3340', user: 'home.heater', pass: 'x' },
  redeemPool: { url: 'stratum+tcp://127.0.0.1:3341', user: 'buyer.heater' },
  thermostat: { enabled: true, setpointC: 21, hysteresisC: 0.5, maxRoomC: 26 },
  msatPerPih: 625, // ≈ 2 sats per THH
  electricitySatsPerKwh: 150,
  tickMs: 2000,
  commandHoldMs: 8000,
  allowDemoRedeem: true
}

const state = {
  started: false,
  config: { ...DEFAULTS },
  hashu: { reachable: null, error: null, status: null, checkedAt: null },
  devices: {}, // deviceId -> view
  overrides: {}, // deviceId -> 'auto' | 'home' | 'hashu' | 'off'
  totals: { heatKwh: 0, tihProduced: 0, tihSold: 0, since: Date.now() },
  events: [],
  pending: {}, // `${deviceId}:${command}` -> { target, at }
  capabilityCache: {},
  lastTickAt: null,
  lastError: null
}

let client = null
let logger = console
let timer = null

function event (kind, text, deviceId) {
  state.events.unshift({ at: Date.now(), kind, text, deviceId })
  state.events.length = Math.min(state.events.length, 60)
  logger.info && logger.info(`[hashu] ${deviceId ? deviceId + ': ' : ''}${text}`)
}

function merge (base, over) {
  const out = { ...base }
  for (const [k, v] of Object.entries(over || {})) {
    out[k] = v && typeof v === 'object' && !Array.isArray(v) && base[k] && typeof base[k] === 'object'
      ? { ...base[k], ...v }
      : v
  }
  return out
}

async function fetchHashu () {
  const url = state.config.hashuControlUrl.replace(/\/$/, '') + '/status'
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(1500) })
    if (!res.ok) throw new Error(`HTTP ${res.status}`)
    const status = await res.json()
    if (state.hashu.reachable === false) event('recover', 'Hashu proxy reachable again')
    state.hashu = { reachable: true, error: null, status, checkedAt: Date.now() }
  } catch (err) {
    if (state.hashu.reachable !== false) event('fault', `Hashu proxy unreachable (${err.cause?.code || err.message})`)
    state.hashu = { reachable: false, error: err.cause?.code || err.message, status: state.hashu.status, checkedAt: Date.now() }
  }
}

// Telemetry values come back keyed by channel; accept either { name: value }
// or { name: { value } } so a Worker change does not break the app.
function pick (t, name) {
  if (!t) return undefined
  const src = t.metrics || t.telemetry || t.values || t
  const v = src[name]
  if (v && typeof v === 'object' && 'value' in v) return v.value
  return v
}

async function isHeater (deviceId) {
  if (deviceId in state.capabilityCache) return state.capabilityCache[deviceId]
  try {
    const { capabilities } = await client.getCapabilities(deviceId)
    const cmds = (capabilities?.commands || []).map((c) => c.name)
    const tel = (capabilities?.telemetry || []).map((c) => c.name)
    const ok = cmds.includes('setupPools') && cmds.includes('setPowerMode') && tel.includes('pool')
    state.capabilityCache[deviceId] = ok
    return ok
  } catch {
    return false
  }
}

async function command (deviceId, name, params, target, why) {
  const key = `${deviceId}:${name}`
  const p = state.pending[key]
  if (p && p.target === target && Date.now() - p.at < state.config.commandHoldMs) return
  state.pending[key] = { target, at: Date.now() }
  try {
    const res = await client.sendCommand(deviceId, name, params)
    if (res && res.status === 'REJECTED') {
      event('fault', `${name} rejected: ${res.error}`, deviceId)
    } else {
      event('command', `${name} → ${target} (${why})`, deviceId)
    }
  } catch (err) {
    event('fault', `${name} failed: ${err.message}`, deviceId)
  }
}

async function tickDevice (deviceId, redemptionOpen, dtH) {
  const cfg = state.config
  let t
  try {
    t = await client.pullTelemetry(deviceId, 'metrics')
  } catch (err) {
    const prev = state.devices[deviceId]
    if (!prev || prev.online) event('fault', `telemetry failed: ${err.message}`, deviceId)
    state.devices[deviceId] = { ...(prev || { deviceId }), online: false, error: err.message }
    return
  }
  const hashrateThs = Number(pick(t, 'hashrate_rt')) || 0
  const powerW = Number(pick(t, 'power')) || 0
  const rawRoom = pick(t, 'room_temperature')
  const roomC = typeof rawRoom === 'number' ? rawRoom : (rawRoom == null ? null : Number(rawRoom))
  const pool = pick(t, 'pool') || null
  const mode = pick(t, 'power_mode') || 'normal'
  const override = state.overrides[deviceId] || 'auto'

  const poolChoice = choosePool({ redemptionOpen, override })
  const heatChoice = chooseHeat({ roomC, current: mode, redemptionOpen, override, thermostat: cfg.thermostat })
  const targetPool = poolChoice.pool === 'hashu' ? cfg.proxyPool : cfg.homePool
  const onHashu = samePool(pool, cfg.proxyPool)

  state.totals.heatKwh += (powerW / 1000) * dtH
  state.totals.tihProduced += hashrateThs * dtH * TIH_PER_TH_HOUR

  const prevDev = state.devices[deviceId]
  const joulesPerTh = hashrateThs > 0 && powerW > 0 ? powerW / hashrateThs : prevDev?.joulesPerTh ?? null
  state.devices[deviceId] = {
    deviceId,
    joulesPerTh,
    online: true,
    error: null,
    hashrateThs,
    powerW,
    chipC: Number(pick(t, 'temperature')) || null,
    roomC: Number.isFinite(roomC) ? roomC : null,
    mode,
    pool,
    onHashu,
    sharesAccepted: Number(pick(t, 'shares_accepted')) || 0,
    override,
    target: { pool: poolChoice.pool, poolReason: poolChoice.reason, mode: heatChoice.mode, modeReason: heatChoice.reason },
    economics: economics({ powerW, hashrateThs, joulesPerTh, msatPerPih: cfg.msatPerPih, electricitySatsPerKwh: cfg.electricitySatsPerKwh })
  }

  if (!samePool(pool, targetPool)) {
    await command(deviceId, 'setupPools', { pools: [targetPool] }, poolChoice.pool, poolChoice.reason)
  }
  if (mode !== heatChoice.mode) {
    await command(deviceId, 'setPowerMode', { mode: heatChoice.mode }, heatChoice.mode, heatChoice.reason)
  }
}

async function tick () {
  const now = Date.now()
  const dtH = state.lastTickAt ? (now - state.lastTickAt) / 3.6e6 : 0
  state.lastTickAt = now
  await fetchHashu()
  const st = state.hashu.status
  const redemptionOpen = state.hashu.reachable ? (st.active_redemptions || 0) > 0 : null
  if (st && Array.isArray(st.redemptions)) {
    state.totals.tihSold = st.redemptions.reduce((s, r) => s + Math.min(r.filled_tih || 0, r.amount_tih || 0), 0)
    for (const r of st.redemptions) {
      if (!state.pending[`open:${r.id}`]) {
        state.pending[`open:${r.id}`] = { at: now }
        event('redemption', `redemption ${r.id.slice(0, 8)} open: ${r.amount_tih} TiH for ${r.worker_name} @ ${r.pool_url}`)
      }
      if (r.state === 'completed' && !state.pending[`done:${r.id}`]) {
        state.pending[`done:${r.id}`] = { at: now }
        event('redemption', `redemption ${r.id.slice(0, 8)} filled: ${r.amount_tih} TiH delivered to ${r.worker_name}`)
      }
    }
  }
  let workers
  try {
    workers = (await client.listWorkers()).workers || []
    state.lastError = null
  } catch (err) {
    state.lastError = err.message
    return
  }
  const ids = workers.flatMap((w) => w.deviceIds || [])
  for (const id of ids) {
    if (await isHeater(id)) await tickDevice(id, redemptionOpen, dtH)
  }
}

function start (config, mdkClient, log) {
  if (state.started) return
  state.started = true
  state.config = merge(DEFAULTS, config || {})
  client = mdkClient
  if (log) logger = log
  event('info', `controller started: home pool ${state.config.homePool.url}, Hashu proxy ${state.config.proxyPool.url}`)
  const loop = async () => {
    try { await tick() } catch (err) { state.lastError = err.message }
    timer = setTimeout(loop, state.config.tickMs)
  }
  timer = setTimeout(loop, 500)
}

function stop () { clearTimeout(timer); state.started = false }

function view () {
  const cfg = state.config
  const devices = Object.values(state.devices)
  const powerW = devices.reduce((s, d) => s + (d.online ? d.powerW : 0), 0)
  const hashrateThs = devices.reduce((s, d) => s + (d.online ? d.hashrateThs : 0), 0)
  const rated = devices.filter((d) => d.joulesPerTh > 0)
  const joulesPerTh = rated.length ? rated.reduce((s, d) => s + d.joulesPerTh, 0) / rated.length : null
  return {
    ok: true,
    kernelConnected: state.lastError === null,
    hashu: {
      reachable: state.hashu.reachable,
      error: state.hashu.error,
      controlUrl: cfg.hashuControlUrl,
      activeRedemptions: state.hashu.status?.active_redemptions ?? 0,
      redemptions: (state.hashu.status?.redemptions || []).slice(-8).reverse(),
      metrics: state.hashu.status?.metrics || null
    },
    pools: { home: cfg.homePool, hashu: cfg.proxyPool, redeem: cfg.redeemPool },
    thermostat: cfg.thermostat,
    prices: { msatPerPih: cfg.msatPerPih, electricitySatsPerKwh: cfg.electricitySatsPerKwh },
    devices,
    fleet: { powerW, hashrateThs, economics: economics({ powerW, hashrateThs, joulesPerTh, msatPerPih: cfg.msatPerPih, electricitySatsPerKwh: cfg.electricitySatsPerKwh }) },
    totals: { ...state.totals, satsEarned: state.totals.tihSold * cfg.msatPerPih / 1024 / 1000 },
    events: state.events.slice(0, 25),
    allowDemoRedeem: !!cfg.allowDemoRedeem
  }
}

function updateSettings (body) {
  const b = body || {}
  const t = state.config.thermostat
  const num = (v, lo, hi) => (typeof v === 'number' && Number.isFinite(v) && v >= lo && v <= hi ? v : undefined)
  if (b.thermostat) {
    const sp = num(b.thermostat.setpointC, 5, 30)
    const max = num(b.thermostat.maxRoomC, 10, 35)
    if (sp !== undefined) t.setpointC = sp
    if (max !== undefined) t.maxRoomC = max
    if (typeof b.thermostat.enabled === 'boolean') t.enabled = b.thermostat.enabled
    if (t.maxRoomC <= t.setpointC) t.maxRoomC = t.setpointC + 1
  }
  if (b.override && typeof b.override === 'object') {
    for (const [id, mode] of Object.entries(b.override)) {
      if (['auto', 'home', 'hashu', 'off'].includes(mode)) {
        state.overrides[id] = mode
        event('info', `override set to ${mode}`, id)
      }
    }
  }
  event('info', `thermostat ${t.enabled ? 'on' : 'off'}, setpoint ${t.setpointC} °C, ceiling ${t.maxRoomC} °C`)
  return view()
}

async function demoRedeem (body) {
  if (!state.config.allowDemoRedeem) throw new Error('ERR_DEMO_REDEEM_DISABLED')
  const b = body || {}
  // Default ≈ 1 THH (3,274 TiH); at most 2^22 TiH (≈ 1,281 THH).
  const amount = Number(b.amountTih || 3274)
  if (!Number.isInteger(amount) || amount < 1 || amount > 2 ** 22) throw new Error('ERR_BAD_AMOUNT')
  const res = await fetch(state.config.hashuControlUrl.replace(/\/$/, '') + '/redemptions', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      amount_tih: amount,
      pool_url: b.poolUrl || state.config.redeemPool.url,
      worker_name: b.workerName || state.config.redeemPool.user
    }),
    signal: AbortSignal.timeout(3000)
  }).catch((err) => { throw new Error(`ERR_HASHU_UNREACHABLE: ${err.cause?.code || err.message}`) })
  const out = await res.json()
  if (!res.ok) throw new Error(`ERR_HASHU_REFUSED: ${out.error}`)
  return out
}

module.exports = { start, stop, tick, view, updateSettings, demoRedeem, state, DEFAULTS }
