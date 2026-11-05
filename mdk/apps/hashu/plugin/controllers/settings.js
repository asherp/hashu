'use strict'
const controller = require('../lib/controller')
module.exports = async function settings (req) { return controller.updateSettings(req.body) }
