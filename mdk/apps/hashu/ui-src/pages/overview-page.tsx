import { Typography } from '@tetherto/mdk-react-devkit'
import { HashuView } from '../components/heater-view'
import { useHashu } from '../hooks/use-hashu'

export const OverviewPage = () => {
  const h = useHashu()
  return (
    <section className="hashu-page">
      <Typography variant="heading1">Hashu Heater</Typography>
      <Typography variant="secondary">
        Your miner heats the room and sells its hashrate as THH vouchers. When someone redeems THH, the heater switches to the Hashu
        proxy until the order is filled, then comes back to your pool.
      </Typography>
      <HashuView {...h} />
    </section>
  )
}

export const HashuHomePanel = () => {
  const h = useHashu()
  return <HashuView {...h} compact />
}
