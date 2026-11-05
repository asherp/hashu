'use strict'
const client = require('../client')
module.exports = async () => {
  const v = (await client.info()).roomTemp
  return v === undefined ? null : Number(v)
}
