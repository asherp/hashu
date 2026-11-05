'use strict'

const http = require('http')
const net = require('net')
const debug = require('debug')('mdk:worker:heater:mock')

// A home-mining heater simulator. Two halves:
//
// 1. The AxeOS HTTP API subset that src/client.js speaks
//    (GET /api/system/info, PATCH /api/system, POST /api/system/restart),
//    the same surface a Bitaxe serves on the LAN.
// 2. A Stratum V1 client that really connects to the configured pool and
//    submits shares at the rate its hashrate implies at the pool's
//    difficulty. Pointed at the Hashu proxy, those shares are routed to the
//    redeemer's pool and credited to the open redemption, so a redemption
//    fills on mock hardware the same way it does on a real miner. The
//    shares carry no real proof of work: only a pool that accepts every
//    submit (scripts/fake-pool.js) will take them.
//
// It also models the room it heats: the room warms with the heater's power
// draw and loses heat to the outside, so a thermostat has something to act on.

const MODES = {
  sleep: { freq: 0, scale: 0 },
  eco: { freq: 400, scale: 0.75 },
  normal: { freq: 525, scale: 1 },
  high: { freq: 600, scale: 1.12 }
}

function modeForFrequency (f) {
  if (!f) return 'sleep'
  if (f <= 450) return 'eco'
  if (f >= 575) return 'high'
  return 'normal'
}

function createServer ({
  host,
  port,
  serial,
  hashrateThs = 40, // Heatbit-class heater
  powerW = 1300,
  stratumURL = '127.0.0.1',
  stratumPort = 3333,
  stratumUser = 'home.heater',
  stratum = true,
  room = {}
} = {}) {
  const state = {
    serial: serial || 'HEAT-0',
    mode: 'normal',
    stratumURL,
    stratumPort: Number(stratumPort),
    stratumUser,
    stratumPassword: 'x',
    bootTime: Date.now(),
    sharesAccepted: 0,
    sharesRejected: 0,
    difficulty: 0,
    connected: false
  }

  // Room: dT/dt = (P - k (T - T_out)) / C. Defaults give a 20 C room that
  // settles near 33 C at full power, with a ~2 minute time constant so the
  // thermostat visibly acts during a demo.
  const roomModel = {
    temp: room.start ?? 19,
    outside: room.outside ?? 8,
    k: room.lossWPerK ?? 52, // W per K of heat loss
    c: room.capacityJPerK ?? 15000 // J per K, demo-fast (minutes, not hours)
  }

  const scale = () => MODES[state.mode].scale
  const hashrateThsNow = () => hashrateThs * scale()
  const powerNow = () => (state.mode === 'sleep' ? 3 : powerW * scale())

  let last = Date.now()
  const roomTimer = setInterval(() => {
    const now = Date.now()
    const dt = (now - last) / 1000
    last = now
    const q = powerNow() - roomModel.k * (roomModel.temp - roomModel.outside)
    roomModel.temp += (q * dt) / roomModel.c
  }, 500)
  roomTimer.unref()

  // ---- Stratum client ---------------------------------------------------
  let sock = null
  let submitTimer = null
  let retryTimer = null
  let job = null
  let nonce = 0
  let stopped = false

  function stopStratum () {
    clearTimeout(submitTimer)
    clearTimeout(retryTimer)
    if (sock) { sock.removeAllListeners(); sock.destroy(); sock = null }
    state.connected = false
    job = null
  }

  function scheduleSubmit () {
    clearTimeout(submitTimer)
    if (!sock || !job || !state.difficulty || state.mode === 'sleep') {
      submitTimer = setTimeout(scheduleSubmit, 1000)
      return
    }
    // Expected seconds per share = diff * 2^32 / hashrate.
    const mean = (state.difficulty * 2 ** 32) / (hashrateThsNow() * 1e12)
    const wait = Math.max(50, -Math.log(1 - Math.random()) * mean * 1000)
    submitTimer = setTimeout(() => {
      if (!sock || !job) return scheduleSubmit()
      nonce = (nonce + 1) >>> 0
      send({
        id: 1000 + nonce,
        method: 'mining.submit',
        params: [state.stratumUser, job.id, '00000000', job.ntime, nonce.toString(16).padStart(8, '0')]
      })
      scheduleSubmit()
    }, Math.min(wait, 60000))
  }

  function send (msg) {
    if (sock) sock.write(JSON.stringify(msg) + '\n')
  }

  function startStratum () {
    stopStratum()
    if (!stratum || stopped) return
    const s = net.connect(state.stratumPort, state.stratumURL)
    sock = s
    let buf = ''
    s.setEncoding('utf8')
    s.on('connect', () => {
      state.connected = true
      debug('%s connected to %s:%d', state.serial, state.stratumURL, state.stratumPort)
      send({ id: 1, method: 'mining.subscribe', params: ['hashu-heater-mock/0.1'] })
      send({ id: 2, method: 'mining.authorize', params: [state.stratumUser, state.stratumPassword] })
      scheduleSubmit()
    })
    s.on('data', (chunk) => {
      buf += chunk
      let i
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i).trim()
        buf = buf.slice(i + 1)
        if (!line) continue
        let m
        try { m = JSON.parse(line) } catch { continue }
        if (m.method === 'mining.set_difficulty') state.difficulty = Number(m.params[0])
        else if (m.method === 'mining.notify') job = { id: m.params[0], ntime: m.params[7] }
        else if (typeof m.id === 'number' && m.id >= 1000) {
          if (m.result === true) state.sharesAccepted++
          else state.sharesRejected++
        }
      }
    })
    const retry = () => {
      if (sock !== s) return
      state.connected = false
      sock = null
      job = null
      clearTimeout(submitTimer)
      // Every Stratum miner reconnects on its own; the Hashu proxy relies on
      // this to move a miner between routes.
      retryTimer = setTimeout(startStratum, 1000)
    }
    s.on('error', retry)
    s.on('close', retry)
  }

  // ---- AxeOS HTTP API ---------------------------------------------------
  const info = () => ({
    hostname: state.serial.toLowerCase(),
    ASICModel: hashrateThs >= 10 ? 'HEATER_40T' : 'BM1370',
    serial: state.serial,
    version: 'hashu-mock-0.1',
    uptimeSeconds: Math.floor((Date.now() - state.bootTime) / 1000),
    hashRate: Math.round(hashrateThsNow() * 1000 * (0.99 + Math.random() * 0.02)), // GH/s
    power: Math.round(powerNow() * (0.995 + Math.random() * 0.01) * 10) / 10,
    temp: Math.round((state.mode === 'sleep' ? roomModel.temp : 55 + 10 * scale()) * 10) / 10,
    roomTemp: Math.round(roomModel.temp * 10) / 10,
    frequency: MODES[state.mode].freq,
    powerMode: state.mode,
    stratumURL: state.stratumURL,
    stratumPort: state.stratumPort,
    stratumUser: state.stratumUser,
    stratumConnected: state.connected,
    stratumDifficulty: state.difficulty,
    sharesAccepted: state.sharesAccepted,
    sharesRejected: state.sharesRejected
  })

  const server = http.createServer((req, res) => {
    const reply = (code, body) => {
      res.writeHead(code, { 'content-type': 'application/json' })
      res.end(JSON.stringify(body))
    }
    const readBody = (fn) => {
      let b = ''
      req.on('data', (c) => { b += c })
      req.on('end', () => {
        try { fn(JSON.parse(b || '{}')) } catch (err) { reply(400, { error: err.message }) }
      })
    }

    if (req.method === 'GET' && req.url === '/api/system/info') return reply(200, info())
    if (req.method === 'PATCH' && req.url === '/api/system') {
      return readBody((b) => {
        if (b.stratumURL !== undefined) state.stratumURL = String(b.stratumURL)
        if (b.stratumPort !== undefined) state.stratumPort = Number(b.stratumPort)
        if (b.stratumUser !== undefined) state.stratumUser = String(b.stratumUser)
        if (b.stratumPassword !== undefined) state.stratumPassword = String(b.stratumPassword)
        if (b.powerMode !== undefined) {
          if (!MODES[b.powerMode]) return reply(400, { error: 'ERR_BAD_POWER_MODE' })
          state.mode = b.powerMode
        } else if (b.frequency !== undefined) {
          state.mode = modeForFrequency(Number(b.frequency))
        }
        reply(200, {})
      })
    }
    if (req.method === 'POST' && req.url === '/api/system/restart') {
      state.bootTime = Date.now()
      startStratum()
      return reply(200, { message: 'System will restart shortly.' })
    }
    reply(404, { error: 'ERR_NOT_FOUND' })
  })

  server.listen(port, host || '127.0.0.1', () => {
    debug('heater mock %s on %s:%d', state.serial, host, server.address().port)
    startStratum()
  })

  return {
    server,
    state,
    room: roomModel,
    exit () {
      stopped = true
      stopStratum()
      clearInterval(roomTimer)
      server.close()
    }
  }
}

module.exports = { createServer }
