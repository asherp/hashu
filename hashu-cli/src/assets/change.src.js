// Source of change.js: the checkout's client-side change wallet. Rebuild
// after editing:
//   npm i esbuild@0.24.2 @noble/secp256k1@2.2.3 @noble/hashes@1.7.1 qrcode-generator@1.4.4
//   (from a folder holding change.src.js)
//   npx esbuild change.src.js --bundle --minify --format=iife --global-name=HashuChange \
//     --platform=browser --target=es2020 --legal-comments=none --outfile=change.js
//
// When the checkout redeems a token, the browser (not the server) makes the
// melt's blank outputs (NUT-08). Their secrets and blinding factors come
// from a random key that lives only in the order link's #fragment, which
// browsers never send to a server. If the redemption settles early, the
// mint signs change on those blanks; the order page restores the
// signatures (NUT-09), unblinds them here and builds a cashuB token. The
// server never sees anything that can spend the change.

import { ProjectivePoint as Point, CURVE } from '@noble/secp256k1'
import { sha256 } from '@noble/hashes/sha256'
import { hmac } from '@noble/hashes/hmac'
import { bytesToHex, hexToBytes, utf8ToBytes, concatBytes } from '@noble/hashes/utils'
import qrcode from 'qrcode-generator'

const DST = utf8ToBytes('Secp256k1_HashToCurve_Cashu_')

// NUT-00 hash_to_curve.
export function hashToCurve (message) {
  const msgHash = sha256(concatBytes(DST, message))
  for (let counter = 0; counter < 65536; counter++) {
    const c = new Uint8Array(4)
    new DataView(c.buffer).setUint32(0, counter, true)
    const h = sha256(concatBytes(msgHash, c))
    try { return Point.fromHex('02' + bytesToHex(h)) } catch (_) {}
  }
  throw new Error('no valid point')
}

const toBig = (b) => BigInt('0x' + bytesToHex(b))

// The i-th blank's secret (a hex string, as Cashu wallets use) and
// blinding factor, from the order key.
function derive (keyHex, i) {
  const k = hexToBytes(keyHex)
  const secret = bytesToHex(hmac(sha256, k, utf8ToBytes(`hashu-change-secret-${i}`)))
  let r = toBig(hmac(sha256, k, utf8ToBytes(`hashu-change-r-${i}`))) % CURVE.n
  if (r === 0n) r = 1n
  return { secret, r }
}

function blind (keyHex, i) {
  const { secret, r } = derive(keyHex, i)
  const Y = hashToCurve(utf8ToBytes(secret))
  const B = Y.add(Point.BASE.multiply(r))
  return { secret, r, Y, B_: B.toHex(true) }
}

/** A fresh order key: 32 random bytes, hex. */
export function newKey () {
  const b = new Uint8Array(32)
  crypto.getRandomValues(b)
  return bytesToHex(b)
}

/**
 * The stop secret: whoever shows it to the order's stop endpoint may stop
 * the redemption early. The server only ever learns its hash at redeem
 * time, and the secret itself only when it is used.
 */
export function stopSecret (keyHex) {
  return bytesToHex(hmac(sha256, hexToBytes(keyHex), utf8ToBytes('hashu-change-stop')))
}

/** Hex SHA-256 of the stop secret, sent with the redemption. */
export function stopHash (keyHex) {
  return bytesToHex(sha256(hexToBytes(stopSecret(keyHex))))
}

/** How many blanks cover change below `amount`: its bit length, at least 1. */
export function blankCount (amount) {
  return Math.max(1, Math.ceil(Math.log2(Math.max(1, amount) + 1)))
}

/** Blank outputs (NUT-08) for the melt request. */
export function blanks (keyHex, n, keysetId) {
  const out = []
  for (let i = 0; i < n; i++) out.push({ amount: 0, id: keysetId, B_: blind(keyHex, i).B_ })
  return out
}

/**
 * Turn the mint's NUT-09 restore response into proofs. `restore` is
 * `{ outputs, signatures }` for our blanks; `keysFor(id)` returns the
 * keyset's `{ amount: pubkeyHex }` map.
 */
export async function unblind (keyHex, n, restore, keysFor) {
  const byB = new Map()
  for (let i = 0; i < n; i++) { const b = blind(keyHex, i); byB.set(b.B_, b) }
  const proofs = []
  const sigs = restore.signatures || []
  for (let j = 0; j < sigs.length; j++) {
    const out = restore.outputs[j], sig = sigs[j]
    const b = byB.get(out.B_)
    if (!b) continue
    const keys = await keysFor(sig.id)
    const K = Point.fromHex(keys[String(sig.amount)])
    const C = Point.fromHex(sig.C_).add(K.multiply(b.r).negate())
    proofs.push({ amount: Number(sig.amount), id: sig.id, secret: b.secret, C: C.toHex(true), Y: b.Y.toHex(true) })
  }
  return proofs
}

// ---- cashuB (NUT-00 token V4): CBOR, then base64url ----------------------
function cbor (v) {
  const parts = []
  const head = (major, n) => {
    if (n < 24) parts.push(Uint8Array.of((major << 5) | n))
    else if (n < 256) parts.push(Uint8Array.of((major << 5) | 24, n))
    else if (n < 65536) parts.push(Uint8Array.of((major << 5) | 25, n >> 8, n & 255))
    else { const b = new Uint8Array(5); b[0] = (major << 5) | 26; new DataView(b.buffer).setUint32(1, n); parts.push(b) }
  }
  const enc = (x) => {
    if (typeof x === 'number') return head(0, x)
    if (typeof x === 'string') { const b = utf8ToBytes(x); head(3, b.length); parts.push(b); return }
    if (x instanceof Uint8Array) { head(2, x.length); parts.push(x); return }
    if (Array.isArray(x)) { head(4, x.length); x.forEach(enc); return }
    const ks = Object.keys(x); head(5, ks.length); for (const k of ks) { enc(k); enc(x[k]) }
  }
  enc(v)
  return concatBytes(...parts)
}

function b64url (bytes) {
  let s = ''
  for (const b of bytes) s += String.fromCharCode(b)
  return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
}

/** A cashuB token for `proofs` (grouped by keyset). */
export function encodeToken (mintUrl, unit, proofs, memo) {
  const byId = new Map()
  for (const p of proofs) {
    if (!byId.has(p.id)) byId.set(p.id, [])
    byId.get(p.id).push({ a: p.amount, s: p.secret, c: hexToBytes(p.C) })
  }
  const t = { m: mintUrl, u: unit }
  if (memo) t.d = memo
  t.t = [...byId].map(([id, p]) => ({ i: hexToBytes(id), p }))
  return 'cashuB' + b64url(cbor(t))
}

/** An SVG QR code of `text`. */
export function qrSvg (text) {
  const q = qrcode(0, 'L')
  q.addData(text)
  q.make()
  return q.createSvgTag({ cellSize: 4, margin: 2, scalable: true })
}
