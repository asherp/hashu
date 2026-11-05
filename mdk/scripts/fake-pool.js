'use strict'

// A Stratum V1 pool that accepts every share, for the no-hardware demo.
// The mock heater's shares carry no real proof of work, so a real pool
// would reject them; this one counts them per worker instead.
//
//   node scripts/fake-pool.js --port 3340 --name home --difficulty 10000
//
// GET http://127.0.0.1:<port+1000>/stats returns { name, workers: { user: shares } }.

const net = require('net')
const http = require('http')

const arg = (k, d) => {
  const i = process.argv.indexOf(`--${k}`)
  return i > 0 ? process.argv[i + 1] : d
}
const port = Number(arg('port', 3340))
const name = arg('name', 'pool')
const difficulty = Number(arg('difficulty', 10000))
const HASH32 = '11'.repeat(32)
const workers = {}

net.createServer((sock) => {
  let buf = ''
  const send = (m) => sock.write(JSON.stringify(m) + '\n')
  sock.setEncoding('utf8')
  sock.on('error', () => {})
  sock.on('data', (chunk) => {
    buf += chunk
    let i
    while ((i = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, i).trim()
      buf = buf.slice(i + 1)
      if (!line) continue
      let m
      try { m = JSON.parse(line) } catch { continue }
      if (m.method === 'mining.subscribe') {
        send({ id: m.id, result: [[], 'abcdef01', 4], error: null })
      } else if (m.method === 'mining.authorize') {
        send({ id: m.id, result: true, error: null })
        send({ id: null, method: 'mining.set_difficulty', params: [difficulty] })
        send({ id: null, method: 'mining.notify', params: ['job1', HASH32, 'aabbccdd', '11223344', [], '20000000', '1d00ffff', '60000000', true] })
        console.log(`[${name}] authorize ${m.params[0]}`)
      } else if (m.method === 'mining.submit') {
        const w = m.params[0]
        workers[w] = (workers[w] || 0) + 1
        send({ id: m.id, result: true, error: null })
      } else if (m.id !== undefined && m.id !== null) {
        send({ id: m.id, result: true, error: null })
      }
    }
  })
}).listen(port, '127.0.0.1', () => console.log(`[${name}] fake pool on 127.0.0.1:${port} (difficulty ${difficulty})`))

http.createServer((req, res) => {
  res.writeHead(200, { 'content-type': 'application/json' })
  res.end(JSON.stringify({ name, difficulty, workers }))
}).listen(port + 1000, '127.0.0.1')
