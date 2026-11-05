import { toQueryView, useMdkApp, useQuery } from '@tetherto/mdk-react-adapter'
import { useQueryClient } from '@tanstack/react-query'
import { fetchState, postRedeem, postSettings, stateKey } from '../data/hashu'

export const useHashu = () => {
  const { apiUrl } = useMdkApp()
  const url = apiUrl('/api/hashu/state')
  const qc = useQueryClient()
  const query = useQuery({ queryKey: stateKey(url), queryFn: () => fetchState(url), retry: false, refetchInterval: 2000 })
  const view = toQueryView(query, url, { unreachableHint: 'Start the stack with `mdk run`.' })
  const refresh = () => qc.invalidateQueries({ queryKey: stateKey(url) })
  return {
    view,
    setSettings: async (body: unknown) => { await postSettings(apiUrl('/api/hashu/settings'), body); refresh() },
    redeem: async (body: unknown) => { await postRedeem(apiUrl('/api/hashu/redeem'), body); refresh() },
  }
}
