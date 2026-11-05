'use strict'
const client = require('../client')
module.exports = async (params) => client.setMode(params.mode === 'low' ? 'eco' : params.mode)
