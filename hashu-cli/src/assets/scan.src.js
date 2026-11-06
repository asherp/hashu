// Source of scan.js, the token scanner the checkout and operator pages load
// when someone presses "Scan token". Rebuild after editing:
//   npm i esbuild buffer jsqr@1.4.0 @ngraveio/bc-ur@1.1.13
//   (from a folder holding scan.src.js and scan-shim.js)
//   npx esbuild scan.src.js --bundle --minify --format=iife --global-name=HashuScan \
//     --platform=browser --target=es2019 --legal-comments=none --outfile=scan.js

import './scan-shim.js'
import { Buffer } from 'buffer'
import jsQR from 'jsqr'
import { URDecoder } from '@ngraveio/bc-ur'
// Decodes a token from QR frames: a plain cashuB QR, or an animated ur:bytes QR.
export function tokenReader () {
  let dec = new URDecoder()
  return {
    // Feeds one frame's QR text; returns { token } when done, else { progress } (0..1) or null.
    feed (text) {
      const t = (text || '').trim()
      const low = t.toLowerCase()
      if (low.startsWith('cashu:')) return { token: t.slice(6) }
      if (low.startsWith('cashua') || low.startsWith('cashub')) return { token: t }
      if (!low.startsWith('ur:')) return null
      try { dec.receivePart(t) } catch { return null }
      if (dec.isComplete()) {
        if (!dec.isSuccess()) { dec = new URDecoder(); return { error: 'animated QR failed its checksum; scanning again' } }
        const ur = dec.resultUR()
        const out = ur.type === 'bytes' ? ur.decodeCBOR().toString() : Buffer.from(ur.cbor).toString()
        dec = new URDecoder()
        const s = out.trim()
        return { token: s.toLowerCase().startsWith('cashu:') ? s.slice(6) : s }
      }
      return { progress: dec.estimatedPercentComplete() }
    }
  }
}
export { jsQR }

// A full-screen camera scanner. Calls onToken(token) once a whole token is
// read (one plain QR, or every part of an animated one), then closes.
export function openScanner ({ onToken }) {
  const reader = tokenReader()
  const el = document.createElement('div')
  el.setAttribute('role', 'dialog')
  el.setAttribute('aria-label', 'Scan a Cashu token')
  el.style.cssText = 'position:fixed;inset:0;z-index:1000;background:rgba(0,0,0,.92);display:flex;flex-direction:column;align-items:center;justify-content:center;gap:12px;padding:16px;color:#fff;font:15px system-ui,sans-serif;text-align:center'
  el.innerHTML = '<video playsinline muted style="width:min(92vw,420px);aspect-ratio:1;object-fit:cover;border-radius:12px;background:#111"></video>' +
    '<div data-msg>Point the camera at the token QR. Animated QR codes are read frame by frame.</div>' +
    '<div style="width:min(92vw,420px);height:6px;border-radius:3px;background:#333;overflow:hidden"><div data-bar style="height:100%;width:0;background:#f7931a;transition:width .2s"></div></div>' +
    '<div style="display:flex;gap:8px;flex-wrap:wrap;justify-content:center"><label style="padding:8px 14px;border:1px solid #666;border-radius:8px;cursor:pointer">Use a photo<input type="file" accept="image/*" hidden></label>' +
    '<button type="button" data-close style="padding:8px 14px;border:1px solid #666;border-radius:8px;background:transparent;color:#fff;font:inherit;cursor:pointer">Cancel</button></div>'
  document.body.appendChild(el)
  const video = el.querySelector('video')
  const msg = el.querySelector('[data-msg]')
  const bar = el.querySelector('[data-bar]')
  const canvas = document.createElement('canvas')
  const ctx = canvas.getContext('2d', { willReadFrequently: true })
  let stream = null
  let raf = 0
  let done = false
  function close () {
    done = true
    cancelAnimationFrame(raf)
    if (stream) stream.getTracks().forEach((t) => t.stop())
    el.remove()
    document.removeEventListener('keydown', onKey)
  }
  function onKey (e) { if (e.key === 'Escape') close() }
  document.addEventListener('keydown', onKey)
  el.querySelector('[data-close]').addEventListener('click', close)
  function handle (text) {
    const r = reader.feed(text)
    if (!r) return
    if (r.token) { close(); onToken(r.token); return }
    if (r.error) msg.textContent = r.error
    if (r.progress != null) {
      bar.style.width = `${Math.round(r.progress * 100)}%`
      msg.textContent = `Reading animated QR: ${Math.round(r.progress * 100)}%`
    }
  }
  function decode (source, w, h) {
    const scale = Math.min(1, 800 / Math.max(w, h))
    canvas.width = Math.round(w * scale)
    canvas.height = Math.round(h * scale)
    ctx.drawImage(source, 0, 0, canvas.width, canvas.height)
    const img = ctx.getImageData(0, 0, canvas.width, canvas.height)
    const q = jsQR(img.data, img.width, img.height, { inversionAttempts: 'dontInvert' })
    return q && q.data
  }
  function tick () {
    if (done) return
    if (video.readyState >= 2 && video.videoWidth) {
      const text = decode(video, video.videoWidth, video.videoHeight)
      if (text) handle(text)
    }
    raf = requestAnimationFrame(tick)
  }
  el.querySelector('input[type=file]').addEventListener('change', async (e) => {
    const file = e.target.files && e.target.files[0]
    if (!file) return
    const img = new Image()
    img.onload = () => {
      const text = decode(img, img.naturalWidth, img.naturalHeight)
      URL.revokeObjectURL(img.src)
      if (text) handle(text)
      else msg.textContent = 'No QR code found in that photo.'
    }
    img.src = URL.createObjectURL(file)
  })
  if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
    msg.textContent = 'This browser cannot open the camera here (it needs HTTPS). Use a photo, or paste the token.'
    return
  }
  navigator.mediaDevices.getUserMedia({ video: { facingMode: 'environment' }, audio: false })
    .then((s) => {
      if (done) { s.getTracks().forEach((t) => t.stop()); return }
      stream = s
      video.srcObject = s
      return video.play().then(tick)
    })
    .catch((e) => { msg.textContent = `Camera unavailable (${e.name || e.message}). Use a photo, or paste the token.` })
}

