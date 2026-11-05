'use strict'
const controller = require('../lib/controller')
module.exports = async function redeem (req) { return controller.demoRedeem(req.body) }
