import { defineMdkApp, MDK_APP_PANEL_SLOT } from '@tetherto/mdk-react-adapter'
import { HashuHomePanel, OverviewPage } from './pages/overview-page'
import './styles.css'

export default defineMdkApp({
  id: 'io.hashu.heater',
  displayName: 'Hashu Heater',
  routes: [{ path: '', component: OverviewPage }],
  panels: [{ id: 'hashu', slot: MDK_APP_PANEL_SLOT.HOME, title: 'Hashu Heater', component: HashuHomePanel }],
})
