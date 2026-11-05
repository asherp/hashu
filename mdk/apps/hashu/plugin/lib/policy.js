'use strict'

// The decisions, kept free of I/O so they are unit tested (test/policy.test.js).

const HEAT_LEVELS = ['sleep', 'eco', 'normal', 'high']

/**
 * Which pool a heater should mine on.
 *
 * @param {object} a
 * @param {boolean|null} a.redemptionOpen  true/false from the Hashu proxy, null when it is unreachable
 * @param {string} a.override               'auto' | 'home' | 'hashu' | 'off'
 * @returns {{ pool: 'home'|'hashu', reason: string }}
 */
function choosePool ({ redemptionOpen, override }) {
  if (override === 'home' || override === 'off') return { pool: 'home', reason: `manual override: ${override}` }
  if (override === 'hashu') return { pool: 'hashu', reason: 'manual override: hashu' }
  if (redemptionOpen === null) {
    // Fail safe: if Hashu cannot be reached, the heater mines for its owner.
    return { pool: 'home', reason: 'Hashu proxy unreachable, mining for the home pool' }
  }
  return redemptionOpen
    ? { pool: 'hashu', reason: 'THH redemption open, selling hashrate through Hashu' }
    : { pool: 'home', reason: 'no open redemption, mining for the home pool' }
}

/**
 * Which heat level a heater should run at.
 *
 * @param {object} a
 * @param {number|null|undefined} a.roomC   room temperature, null when the sensor has nothing
 * @param {string} a.current                current heat level
 * @param {boolean|null} a.redemptionOpen
 * @param {string} a.override
 * @param {{ enabled: boolean, setpointC: number, hysteresisC: number, maxRoomC: number }} a.thermostat
 * @returns {{ mode: string, reason: string }}
 */
function chooseHeat ({ roomC, current, redemptionOpen, override, thermostat }) {
  const t = thermostat
  if (override === 'off') return { mode: 'sleep', reason: 'manual override: off' }
  const sensorOk = typeof roomC === 'number' && Number.isFinite(roomC)
  if (sensorOk && roomC >= t.maxRoomC) {
    // Hard comfort/safety ceiling, even while hashrate is owed: the
    // redemption pauses and resumes when the room cools.
    return { mode: 'sleep', reason: `room ${roomC.toFixed(1)} °C at the ${t.maxRoomC} °C ceiling` }
  }
  if (sensorOk && current === 'sleep' && roomC > t.maxRoomC - 1) {
    // Ceiling hysteresis: stay off until the room is a degree below it.
    return { mode: 'sleep', reason: `cooling below the ${t.maxRoomC} °C ceiling` }
  }
  if (!t.enabled) return { mode: HEAT_LEVELS.includes(current) && current !== 'sleep' ? current : 'normal', reason: 'thermostat off' }
  if (!sensorOk) {
    // Fail safe: never run flat out blind.
    return { mode: 'eco', reason: 'no room temperature reading, running gently' }
  }
  if (redemptionOpen) {
    return roomC < t.setpointC - t.hysteresisC
      ? { mode: 'high', reason: 'redemption open and the room wants heat' }
      : { mode: 'normal', reason: 'redemption open, keeping the sold hashrate flowing' }
  }
  if (roomC > t.setpointC + t.hysteresisC) return { mode: 'sleep', reason: `room ${roomC.toFixed(1)} °C above ${t.setpointC} °C` }
  if (roomC < t.setpointC - t.hysteresisC) return { mode: 'normal', reason: `room ${roomC.toFixed(1)} °C below ${t.setpointC} °C` }
  return { mode: current === 'high' ? 'normal' : (current || 'normal'), reason: 'room at setpoint' }
}

/**
 * Heat economics for a miner drawing `powerW` at `hashrateThs`.
 * Every joule a miner draws becomes heat, the same as a resistive heater,
 * so the comparison is sats earned per kWh of heat.
 */
function economics ({ powerW, hashrateThs, satsPerThh, electricitySatsPerKwh, joulesPerTh: ratedJPerTh }) {
  const live = powerW > 0 && hashrateThs > 0
  // While the heater sleeps, quote its last measured efficiency.
  const joulesPerTh = live ? powerW / hashrateThs : ratedJPerTh
  if (!(joulesPerTh > 0)) {
    return { joulesPerTh: null, kwhPerThh: null, satsPerKwh: 0, billOffsetPct: 0, satsPerHour: 0 }
  }
  const kwhPerThh = joulesPerTh / 1000 // (J/TH * 3600 s) / 3.6e6 J/kWh
  const satsPerKwh = satsPerThh / kwhPerThh
  return {
    joulesPerTh,
    kwhPerThh,
    satsPerKwh,
    billOffsetPct: electricitySatsPerKwh > 0 ? (100 * satsPerKwh) / electricitySatsPerKwh : 0,
    satsPerHour: live ? hashrateThs * satsPerThh : 0
  }
}

/** `stratum+tcp://h:p` and `h:p` compare equal. */
function samePool (a, b) {
  const norm = (u) => String(u || '').trim().replace(/^stratum\+tcp:\/\//, '').toLowerCase()
  return norm(a && a.url) === norm(b && b.url) && String((a && a.user) || '') === String((b && b.user) || '')
}

module.exports = { HEAT_LEVELS, choosePool, chooseHeat, economics, samePool }
