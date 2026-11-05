'use strict'
const client = require('../client')
module.exports = async () => {
  const i = await client.info()
  return { url: `stratum+tcp://${i.stratumURL}:${i.stratumPort}`, user: i.stratumUser }
}
