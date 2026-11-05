'use strict'

const { once } = require('node:events')
const { createServer } = require('./mock/server')

module.exports = {
  async setup () {
    const mock = createServer({ host: '127.0.0.1', port: 0, serial: 'SMOKE-0', stratum: false })
    await once(mock.server, 'listening')
    return {
      config: { host: '127.0.0.1', port: mock.server.address().port },
      commands: {
        setPowerMode: { mode: 'normal' },
        setupPools: { pools: [{ url: 'stratum+tcp://127.0.0.1:3333', user: 'smoke.1', pass: 'x' }] }
      },
      teardown: () => mock.exit()
    }
  }
}
