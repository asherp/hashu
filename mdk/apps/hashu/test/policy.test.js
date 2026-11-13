'use strict'

const test = require('node:test')
const assert = require('node:assert')
const { choosePool, chooseHeat, economics, samePool } = require('../plugin/lib/policy')

const thermostat = { enabled: true, setpointC: 21, hysteresisC: 0.5, maxRoomC: 26 }

test('pool follows redemptions', () => {
  assert.equal(choosePool({ redemptionOpen: true, override: 'auto' }).pool, 'hashu')
  assert.equal(choosePool({ redemptionOpen: false, override: 'auto' }).pool, 'home')
})

test('pool fails safe to home when Hashu is unreachable', () => {
  const c = choosePool({ redemptionOpen: null, override: 'auto' })
  assert.equal(c.pool, 'home')
  assert.match(c.reason, /unreachable/)
})

test('manual pool overrides win', () => {
  assert.equal(choosePool({ redemptionOpen: true, override: 'home' }).pool, 'home')
  assert.equal(choosePool({ redemptionOpen: false, override: 'hashu' }).pool, 'hashu')
  assert.equal(choosePool({ redemptionOpen: true, override: 'off' }).pool, 'home')
})

test('thermostat with no redemption', () => {
  const h = (roomC, current = 'normal') => chooseHeat({ roomC, current, redemptionOpen: false, override: 'auto', thermostat }).mode
  assert.equal(h(22), 'sleep')
  assert.equal(h(20), 'normal')
  assert.equal(h(21.2, 'sleep'), 'sleep') // inside the band: hold
  assert.equal(h(21.2, 'normal'), 'normal')
})

test('redemption keeps hashrate flowing, boosts when the room is cold', () => {
  const h = (roomC) => chooseHeat({ roomC, current: 'sleep', redemptionOpen: true, override: 'auto', thermostat }).mode
  assert.equal(h(23), 'normal')
  assert.equal(h(19), 'high')
})

test('ceiling stops the heater even during a redemption, with hysteresis', () => {
  const h = (roomC, current) => chooseHeat({ roomC, current, redemptionOpen: true, override: 'auto', thermostat }).mode
  assert.equal(h(26, 'normal'), 'sleep')
  assert.equal(h(25.5, 'sleep'), 'sleep')
  assert.equal(h(24.9, 'sleep'), 'normal')
})

test('missing room sensor runs gently', () => {
  for (const roomC of [null, undefined, NaN]) {
    assert.equal(chooseHeat({ roomC, current: 'high', redemptionOpen: false, override: 'auto', thermostat }).mode, 'eco')
  }
})

test('off override sleeps', () => {
  assert.equal(chooseHeat({ roomC: 15, current: 'normal', redemptionOpen: true, override: 'off', thermostat }).mode, 'sleep')
})

test('economics: sats per kWh of heat', () => {
  // 625 msat per PiH ≈ 2 sats per THH.
  const e = economics({ powerW: 1300, hashrateThs: 40, msatPerPih: 625, electricitySatsPerKwh: 150 })
  assert.equal(e.joulesPerTh, 32.5)
  // A TiH is 2^40 hashes ≈ 1.0995 TH: 32.5 J/TH × 1.0995 TH / 3.6e6 J/kWh.
  assert.ok(Math.abs(e.kwhPerTih - 9.926e-6) < 1e-9)
  assert.ok(Math.abs(e.satsPerKwh - 61.489) < 0.01)
  assert.ok(Math.abs(e.billOffsetPct - 40.99) < 0.01)
  assert.ok(Math.abs(e.satsPerHour - 79.94) < 0.01)
  // asleep: quote the last measured efficiency, earn nothing now
  const z = economics({ powerW: 3, hashrateThs: 0, joulesPerTh: 32.5, msatPerPih: 625, electricitySatsPerKwh: 150 })
  assert.ok(Math.abs(z.satsPerKwh - 61.489) < 0.01)
  assert.equal(z.satsPerHour, 0)
})

test('samePool ignores the stratum scheme', () => {
  assert.ok(samePool({ url: 'stratum+tcp://1.2.3.4:3333', user: 'a' }, { url: '1.2.3.4:3333', user: 'a' }))
  assert.ok(!samePool({ url: '1.2.3.4:3333', user: 'a' }, { url: '1.2.3.4:3333', user: 'b' }))
})
