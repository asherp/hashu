'use strict'
const client = require('../client')
module.exports = async () => client.modeOf(await client.info())
