'use strict'

const { opts, env } = require('@tetherto/mdk-worker/device')

// AxeOS HTTP API (Bitaxe firmware; the mock speaks the same subset):
//   GET   /api/system/info     hashRate (GH/s), power (W), temp, stratumURL, stratumPort, stratumUser, frequency, ...
//   PATCH /api/system          { stratumURL, stratumPort, stratumUser, stratumPassword, frequency }
//   POST  /api/system/restart
// A closed-firmware heater (Heatbit) has no such API: point it at the Hashu
// proxy once in its app and let the proxy do the routing.

const base = `http://${opts.host || '127.0.0.1'}:${opts.port || 80}`
const auth = env.DEVICE_TOKEN ? { authorization: `Bearer ${env.DEVICE_TOKEN}` } : {}

// Heat levels as ASIC frequency (MHz). 0 = sleep, only the mock honours it.
const FREQUENCY = { sleep: 0, eco: 400, normal: 525, high: 600 }

const call = async (path, init) => {
  let res, text
  try {
    res = await fetch(base + path, { ...init, headers: { ...auth, ...(init && init.headers) } })
    text = await res.text()
  } catch (err) {
    throw new Error(`ERR_DEVICE_CALL_FAILED: ${(err.cause && err.cause.code) || err.message}`)
  }
  if (!res.ok) throw new Error(`ERR_DEVICE_CALL_FAILED: ${res.status}`)
  try { return text ? JSON.parse(text) : {} } catch { return {} }
}

const patch = (body) => call('/api/system', {
  method: 'PATCH',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify(body)
})

const restart = () => call('/api/system/restart', { method: 'POST' })

function modeOf (info) {
  if (info.powerMode) return info.powerMode
  const f = Number(info.frequency)
  if (!f) return 'sleep'
  if (f <= 450) return 'eco'
  if (f >= 575) return 'high'
  return 'normal'
}

function parsePool (url) {
  const m = /^(?:stratum\+tcp:\/\/)?([^:/\s]+):(\d+)$/.exec(String(url || '').trim())
  return m ? { host: m[1], port: Number(m[2]) } : null
}

module.exports = {
  FREQUENCY,
  modeOf,
  parsePool,
  info: () => call('/api/system/info'),
  async setPool ({ url, user, pass }) {
    const p = parsePool(url)
    if (!p || !user) throw new Error('ERR_BAD_POOL')
    await patch({ stratumURL: p.host, stratumPort: p.port, stratumUser: user, stratumPassword: pass || 'x' })
    await restart()
    return { ok: true, pool: { url: `stratum+tcp://${p.host}:${p.port}`, user } }
  },
  async setMode (mode) {
    if (!(mode in FREQUENCY)) throw new Error(`ERR_BAD_POWER_MODE: ${mode}`)
    await patch({ frequency: FREQUENCY[mode], powerMode: mode })
    return { ok: true, power_mode: mode }
  },
  restart
}
