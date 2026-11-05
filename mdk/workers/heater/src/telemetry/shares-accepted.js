'use strict'
const client = require('../client')
module.exports = async () => Number((await client.info()).sharesAccepted || 0)
