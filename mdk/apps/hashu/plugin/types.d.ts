// Shared by the controllers (JSDoc) and the UI (`import type`).

export type AppRequest = {
  params: Record<string, string>
  query: Record<string, string | string[] | undefined>
  body?: unknown
  headers: Record<string, string | string[] | undefined>
}

export type Pool = { url: string; user: string; pass?: string }

export type Economics = {
  joulesPerTh: number | null
  kwhPerThh: number | null
  satsPerKwh: number
  billOffsetPct: number
  satsPerHour: number
}

export type Heater = {
  deviceId: string
  joulesPerTh: number | null
  online: boolean
  error: string | null
  hashrateThs: number
  powerW: number
  chipC: number | null
  roomC: number | null
  mode: string
  pool: Pool | null
  onHashu: boolean
  sharesAccepted: number
  override: 'auto' | 'home' | 'hashu' | 'off'
  target: { pool: 'home' | 'hashu'; poolReason: string; mode: string; modeReason: string }
  economics: Economics
}

export type Redemption = {
  id: string
  pool_url: string
  worker_name: string
  amount_thh: number
  filled_thh: number
  progress: number
  state: 'pending' | 'active' | 'completed'
}

export type HashuState = {
  ok: boolean
  kernelConnected: boolean
  hashu: {
    reachable: boolean | null
    error: string | null
    controlUrl: string
    activeRedemptions: number
    redemptions: Redemption[]
    metrics: Record<string, number> | null
  }
  pools: { home: Pool; hashu: Pool; redeem: Pool }
  thermostat: { enabled: boolean; setpointC: number; hysteresisC: number; maxRoomC: number }
  prices: { satsPerThh: number; electricitySatsPerKwh: number }
  devices: Heater[]
  fleet: { powerW: number; hashrateThs: number; economics: Economics }
  totals: { heatKwh: number; thhProduced: number; thhSold: number; satsEarned: number; since: number }
  events: Array<{ at: number; kind: string; text: string; deviceId?: string }>
  allowDemoRedeem: boolean
}
