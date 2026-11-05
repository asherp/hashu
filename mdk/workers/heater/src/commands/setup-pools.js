'use strict'
const client = require('../client')
module.exports = async (params) => {
  const pools = Array.isArray(params.pools) ? params.pools : [params.pools]
  if (!pools[0]) throw new Error('ERR_BAD_POOL')
  return client.setPool(pools[0])
}
