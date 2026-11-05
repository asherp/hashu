'use strict'
const client = require('../client')
module.exports = async () => Math.round(Number((await client.info()).hashRate || 0) / 10) / 100 // GH/s -> TH/s
