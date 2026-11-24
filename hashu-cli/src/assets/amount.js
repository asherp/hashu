// Hashu amount picker, shared by the buyer checkout and the operator's
// "Mint test TiH" form. Inlined into both pages at serve time (see
// `crate::amount_js`), so it needs no extra route or request.
//
// The amount box counts hashes (TiH, PiH or EiH), hashrate held for a day
// (TH/s/Day, PH/s/Day, EH/s/Day) or, on the checkout, a sum to spend. The
// magnitude follows what the mint can sell, and the chips are sized to it.
// Orders and vouchers always stay in TiH: this is display only.
'use strict'
window.HashuAmount = (() => {
  const HASHES_PER_TIH = 2 ** 40
  const TIH_PER_PIH = 1024
  // 1 TH/s held for a day (86,400 s) is about 78,580 TiH.
  const TIH_PER_THSD = 86400e12 / HASHES_PER_TIH
  const HASH_UNITS = ['TiH', 'PiH', 'EiH']
  const HASH_NAMES = ['Tebihashes', 'Pebihashes', 'Exbihashes']
  const RATE_UNITS = ['TH/s/Day', 'PH/s/Day', 'EH/s/Day']
  // Hash amounts scale with their size: TiH, then PiH (1,024 TiH), then EiH.
  const hashUnit = (tih) => { let k = 0; while (k < HASH_UNITS.length - 1 && Math.abs(tih) >= 1024 ** (k + 1)) k++; return k }
  const inUnit = (tih, k) => { const v = tih / 1024 ** k; return v.toLocaleString(undefined, { maximumFractionDigits: k === 0 || Math.abs(v) >= 100 ? 0 : Math.abs(v) >= 10 ? 1 : 2 }) }
  const fmtHashes = (tih, k = hashUnit(tih)) => `${inUnit(tih, k)} ${HASH_UNITS[k]}`
  const sig = (x, p = 4) => Number(x.toPrecision(p))
  const fmtUnits = (x, p = 4) => sig(x, p).toLocaleString(undefined, { maximumFractionDigits: 8 })
  const fmtMoney = (x, label) => `${x.toLocaleString(undefined, { maximumFractionDigits: 2 })} ${label === 'sats' && x === 1 ? 'sat' : label}`

  // Wire a picker to its elements.
  //   box, label, chips, units: the amount input, its label, the chip row
  //     and the unit toggle (buttons with data-u = hash | rate | money)
  //   tih: the starting amount
  //   storageKey: localStorage key remembering the chosen unit
  //   limits(): { min, available, max } in TiH, or null before the first
  //     load; available null = uncapped, max = per-order cap
  //   money(): { key, f (TiH per unit), label, minStep, price(tih) } for the
  //     "spend" unit, or null when there is no price
  //   onChange(what): after the amount changed, or the unit ('unit')
  function create (o) {
    const w = { tih: o.tih, unit: 'hash', scaleKey: '' }
    const has = (u) => !!o.units.querySelector(`button[data-u="${u}"]`)
    try { const u = localStorage.getItem(o.storageKey); if (u && u !== 'hash' && has(u)) w.unit = u } catch {}
    const lim = () => o.limits && o.limits()
    // What the mint can sell in one order: the box's scale follows it.
    w.sellCap = () => { const l = lim(); if (!l) return null; const av = l.available; return av > 0 ? Math.min(l.max || av, av) : l.max }
    w.scale = (unit = w.unit) => {
      const ref = w.sellCap() || w.tih
      if (unit === 'money') {
        const m = o.money && o.money()
        if (m) return { unit, key: m.key, f: m.f, label: m.label, name: `Spend (${m.label})`, minStep: m.minStep, price: m.price }
        unit = 'hash'
      }
      if (unit === 'rate') {
        let k = 0; while (k < RATE_UNITS.length - 1 && ref >= TIH_PER_THSD * 1000 ** (k + 1)) k++
        return { unit, key: `rate${k}`, f: TIH_PER_THSD * 1000 ** k, label: RATE_UNITS[k], name: `Hashrate-days (${RATE_UNITS[k]})` }
      }
      const k = hashUnit(ref)
      return { unit, key: `hash${k}`, f: 1024 ** k, label: HASH_UNITS[k], name: `${HASH_NAMES[k]} (${HASH_UNITS[k]})` }
    }
    // An amount in TiH, in the chosen unit family.
    w.fmt = (tih) => {
      const sc = w.scale()
      if (sc.unit === 'money') return fmtMoney(sc.price(tih), sc.label)
      return sc.unit === 'rate' ? `${fmtUnits(tih / sc.f, 3)} ${sc.label}` : fmtHashes(tih)
    }
    // Point the toggle, label and box at the chosen unit. The box is only
    // rewritten when its scale changes, so typing is never interrupted.
    w.renderUnit = (force) => {
      const sc = w.scale()
      for (const b of o.units.querySelectorAll('button')) {
        b.textContent = w.scale(b.dataset.u).label
        b.classList.toggle('on', b.dataset.u === sc.unit)
      }
      o.label.textContent = sc.name
      const l = lim()
      if (l) {
        o.box.min = sig(Math.max(1, l.min) / sc.f, 6)
        const cap = w.sellCap()
        if (cap != null) o.box.max = sig(cap / sc.f, 8)
      }
      if (force || sc.key !== w.scaleKey) {
        o.box.value = sc.unit === 'money' ? sc.price(w.tih) : sig(w.tih / sc.f, sc.unit === 'rate' ? 4 : 6)
      } else if (sc.unit === 'money') {
        // The typed sum holds still while the price moves; the TiH it buys follow.
        w.tih = Math.floor((Number(o.box.value) || 0) * sc.f)
      }
      w.scaleKey = sc.key
    }
    // Chips follow the stock: powers of two from the minimum up to the
    // largest that fits in what is left (or the per-order cap when
    // uncapped), plus one for everything available, labelled "(max)".
    // Rate and money units use round 1-2-5 steps instead. Sold out keeps the last set.
    w.renderChips = () => {
      const l = lim(); if (!l) return
      const min = Math.max(1, l.min)
      const av = l.available
      const cap = av != null ? Math.min(l.max || av, av) : l.max
      if (!(cap >= min)) return
      const sc = w.scale()
      let chips
      if (sc.unit === 'rate' || sc.unit === 'money') {
        // Log-spaced between the minimum (or a thousandth of what is left)
        // and what is left, at most four.
        const hi = cap / sc.f, lo = Math.max(min / sc.f, hi / 1000, sc.minStep || 0)
        const cand = []
        for (let e = Math.floor(Math.log10(lo)); e <= Math.ceil(Math.log10(hi)); e++) {
          for (const m of [1, 2, 5]) { const v = sig(m * 10 ** e, 3); if (v >= lo && v <= hi) cand.push(v) }
        }
        const n = Math.min(4, cand.length)
        const picks = []
        for (let j = 0; j < n; j++) { const v = cand[n === 1 ? cand.length - 1 : Math.round((cand.length - 1) * j / (n - 1))]; if (!picks.includes(v)) picks.push(v) }
        // A sum to spend buys the TiH it covers, rounded down.
        const toTih = (u) => sc.unit === 'money' ? Math.floor(u * sc.f) : Math.round(u * sc.f)
        chips = picks.map((u) => ({ v: Math.min(cap, Math.max(min, toTih(u))), u, label: sc.unit === 'money' ? fmtMoney(u, sc.label) : `${fmtUnits(u)} ${sc.label}` }))
      } else {
        const lo = Math.ceil(Math.log2(min))
        const hi = Math.floor(Math.log2(cap))
        const exps = []
        const n = Math.min(4, hi - lo + 1)
        for (let j = 0; j < n; j++) { const e = n === 1 ? hi : Math.round(lo + (hi - lo) * j / (n - 1)); if (!exps.includes(e)) exps.push(e) }
        chips = exps.map((e) => ({ v: 2 ** e, label: fmtHashes(2 ** e) }))
      }
      const last = chips.length ? chips[chips.length - 1].v : 0
      const all = Math.floor(av)
      if (av != null && av <= (l.max || av) && all > last) chips.push({ v: all, label: `${w.fmt(all)} (max)` })
      const html = chips.map((c) => `<button type="button" data-v="${c.v}"${c.u != null ? ` data-u="${c.u}"` : ''} title="${c.v.toLocaleString()} TiH">${c.label}</button>`).join('')
      if (o.chips.innerHTML !== html) o.chips.innerHTML = html
      w.chipValues = chips.map((c) => c.v)
    }
    // Chips above what is left, or all of them when sold out, are off.
    w.disableChips = (soldOut) => {
      const l = lim(); const av = l && l.available
      for (const b of o.chips.querySelectorAll('button')) b.disabled = soldOut || (av != null && Number(b.dataset.v) > av)
    }
    // Keep the amount inside what can be sold (after a load, the stock may
    // have dropped below it).
    w.clamp = () => {
      const cap = w.sellCap(); const l = lim()
      if (!l || cap == null || !(cap >= l.min)) return
      const t = Math.min(cap, Math.max(l.min, w.tih))
      if (t !== w.tih) { w.tih = t; w.renderUnit(true) }
    }
    o.chips.addEventListener('click', (ev) => {
      const b = ev.target.closest('button'); if (!b) return
      w.tih = Number(b.dataset.v)
      w.renderUnit(true)
      if (b.dataset.u != null && w.unit === 'money') o.box.value = b.dataset.u
      o.onChange && o.onChange()
    })
    o.box.addEventListener('input', () => { w.tih = Math.floor((Number(o.box.value) || 0) * w.scale().f); o.onChange && o.onChange() })
    o.units.addEventListener('click', (ev) => {
      const b = ev.target.closest('button'); if (!b || b.dataset.u === w.unit) return
      w.unit = b.dataset.u
      try { localStorage.setItem(o.storageKey, w.unit) } catch {}
      w.renderUnit(true); w.renderChips()
      o.onChange && o.onChange('unit')
    })
    return w
  }
  return { create, HASHES_PER_TIH, TIH_PER_PIH, TIH_PER_THSD, HASH_UNITS, hashUnit, inUnit, fmtHashes, sig, fmtUnits, fmtMoney }
})()
