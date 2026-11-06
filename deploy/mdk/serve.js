'use strict'

// Serves the built MDK dashboard and forwards its API calls to the MDK
// Gateway in the same container, so Caddy needs one upstream (mdk:3030).
//
//   GET  /assets/*, /app-ui/*, /app-ui.json, ...  -> dist/ (SPA fallback to index.html)
//   /auth /oauth /api /pub /apps /agent           -> Gateway on 127.0.0.1:3847
//
// The Gateway only accepts loopback Host headers by default, so forwarded
// requests carry Host: localhost:3847.

const http = require('http')
const fs = require('fs')
const path = require('path')

const DIST = path.resolve(process.env.MDK_DASHBOARD_DIST || path.join(__dirname, '../../mdk/apps/dashboard/dist'))
const PORT = Number(process.env.PORT || 3030)
const GATEWAY = { host: '127.0.0.1', port: Number(process.env.MDK_GATEWAY_PORT || 3847) }
const API = /^\/(auth|oauth|api|pub|apps|agent)(\/|$|\?)/

const TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript',
  '.mjs': 'text/javascript',
  '.css': 'text/css',
  '.json': 'application/json',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
  '.woff': 'font/woff',
  '.woff2': 'font/woff2',
  '.map': 'application/json'
}

function forward (req, res) {
  const headers = { ...req.headers, host: `localhost:${GATEWAY.port}` }
  const up = http.request({ ...GATEWAY, method: req.method, path: req.url, headers }, (r) => {
    res.writeHead(r.statusCode, r.headers)
    r.pipe(res)
  })
  up.on('error', () => {
    if (!res.headersSent) res.writeHead(502, { 'content-type': 'application/json' })
    res.end(JSON.stringify({ error: 'ERR_GATEWAY_UNREACHABLE' }))
  })
  req.pipe(up)
}

function serveFile (req, res) {
  const urlPath = decodeURIComponent(req.url.split('?')[0])
  let file = path.join(DIST, path.normalize(urlPath))
  if (!file.startsWith(DIST)) { res.writeHead(400); return res.end() }
  fs.stat(file, (err, st) => {
    if (!err && st.isDirectory()) file = path.join(file, 'index.html')
    else if (err) {
      // A client-side route (/home, /app/hashu/, /signin): hand the SPA its shell.
      // Missing assets still 404 so a broken build is visible.
      if (path.extname(urlPath)) { res.writeHead(404); return res.end() }
      file = path.join(DIST, 'index.html')
    }
    fs.readFile(file, (e, body) => {
      if (e) { res.writeHead(404); return res.end() }
      const ext = path.extname(file)
      res.writeHead(200, {
        'content-type': TYPES[ext] || 'application/octet-stream',
        'cache-control': urlPath.startsWith('/assets/') ? 'public, max-age=31536000, immutable' : 'no-cache'
      })
      res.end(body)
    })
  })
}

http.createServer((req, res) => {
  if (API.test(req.url)) return forward(req, res)
  if (req.method !== 'GET' && req.method !== 'HEAD') { res.writeHead(405); return res.end() }
  serveFile(req, res)
}).listen(PORT, '0.0.0.0', () => console.log(`[mdk-serve] dashboard ${DIST} on :${PORT}, API -> ${GATEWAY.host}:${GATEWAY.port}`))
