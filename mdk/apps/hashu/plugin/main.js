'use strict'

// Background logic, run by the app runtime before it serves (mdk-app.yaml `main`).
const controller = require('./lib/controller')

module.exports = async function main (config, context) {
  const ctx = (context && context.config) ? context : require('@tetherto/mdk-gateway/plugin')
  const cfg = { ...(ctx.config || {}), ...(config || {}) }
  const mdkClient = cfg.mdkClient || (ctx.config && ctx.config.mdkClient)
  delete cfg.mdkClient
  controller.start(cfg, mdkClient, ctx.logger)
}
