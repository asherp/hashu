import { useMdkApp as P, useQuery as S, toQueryView as M, defineMdkApp as O, MDK_APP_PANEL_SLOT as x } from "@tetherto/mdk-react-adapter";
import { jsx as a, jsxs as s } from "react/jsx-runtime";
import { Loader as E, ErrorCard as K, EmptyState as W, SingleStatCard as p, Typography as N } from "@tetherto/mdk-react-devkit";
import { useState as b } from "react";
import { useQueryClient as R } from "@tanstack/react-query";
import { mdkFetch as C } from "@tetherto/mdk-ui-foundation";
const o = (e, h = 1) => e == null || !Number.isFinite(e) ? "—" : e.toFixed(h), H = (e) => (e || "").replace(/^stratum\+tcp:\/\//, ""), A = { sleep: "Off (sleep)", eco: "Eco", normal: "Normal", high: "Boost" };
function _({ d: e, s: h, onOverride: c }) {
  const d = e.onHashu ? "Hashu proxy (selling THH)" : "Home pool";
  return /* @__PURE__ */ s("div", { className: "hashu-card", children: [
    /* @__PURE__ */ s("div", { className: "hashu-card-head", children: [
      /* @__PURE__ */ a("strong", { children: e.deviceId }),
      /* @__PURE__ */ a("span", { className: `hashu-pill ${e.online ? e.onHashu ? "sell" : "home" : "off"}`, children: e.online ? e.onHashu ? "Selling hashrate" : "Mining for you" : "Offline" })
    ] }),
    /* @__PURE__ */ s("div", { className: "hashu-grid", children: [
      /* @__PURE__ */ s("div", { children: [
        /* @__PURE__ */ a("label", { children: "Room" }),
        /* @__PURE__ */ s("b", { children: [
          o(e.roomC),
          " °C"
        ] }),
        /* @__PURE__ */ s("small", { children: [
          "target ",
          h.thermostat.setpointC,
          " °C"
        ] })
      ] }),
      /* @__PURE__ */ s("div", { children: [
        /* @__PURE__ */ a("label", { children: "Heat" }),
        /* @__PURE__ */ s("b", { children: [
          o(e.powerW, 0),
          " W"
        ] }),
        /* @__PURE__ */ a("small", { children: A[e.mode] || e.mode })
      ] }),
      /* @__PURE__ */ s("div", { children: [
        /* @__PURE__ */ a("label", { children: "Hashrate" }),
        /* @__PURE__ */ s("b", { children: [
          o(e.hashrateThs, 1),
          " TH/s"
        ] }),
        /* @__PURE__ */ s("small", { children: [
          e.sharesAccepted,
          " shares"
        ] })
      ] }),
      /* @__PURE__ */ s("div", { children: [
        /* @__PURE__ */ a("label", { children: "Pool" }),
        /* @__PURE__ */ a("b", { children: d }),
        /* @__PURE__ */ s("small", { children: [
          H(e.pool?.url),
          " · ",
          e.pool?.user
        ] })
      ] })
    ] }),
    /* @__PURE__ */ s("p", { className: "hashu-why", children: [
      "Pool: ",
      e.target.poolReason,
      ". Heat: ",
      e.target.modeReason,
      "."
    ] }),
    /* @__PURE__ */ s("div", { className: "hashu-row", children: [
      /* @__PURE__ */ a("span", { children: "Control:" }),
      ["auto", "home", "hashu", "off"].map((n) => /* @__PURE__ */ a("button", { className: e.override === n ? "on" : "", onClick: () => c(n), children: n }, n))
    ] })
  ] });
}
function k({ view: e, setSettings: h, redeem: c, compact: d }) {
  const [n, v] = b(1), [i, f] = b(null);
  if (e.status === "loading") return /* @__PURE__ */ a(E, {});
  if (e.status === "error") return /* @__PURE__ */ a(K, { title: "Could not load Hashu", error: e.message });
  const t = e.data;
  if (!t.kernelConnected && t.devices.length === 0) return /* @__PURE__ */ a(W, { description: "Waiting for the MDK Kernel.", size: "sm" });
  const u = t.fleet.economics, m = (r) => r.then(() => f(null)).catch((l) => f(l?.body?.error || l?.message || String(l))), y = /* @__PURE__ */ s("div", { className: "hashu-summary", children: [
    /* @__PURE__ */ a(p, { name: "Sats per kWh of heat", value: Math.round(u.satsPerKwh) }),
    /* @__PURE__ */ a(p, { name: "Electric heater", value: 0 }),
    /* @__PURE__ */ a(p, { name: "Heating bill paid back", value: `${o(u.billOffsetPct, 0)}%` }),
    /* @__PURE__ */ a(p, { name: "THH sold", value: o(t.totals.thhSold, 2) }),
    /* @__PURE__ */ a(p, { name: "Heat delivered (kWh)", value: o(t.totals.heatKwh, 3) })
  ] });
  return d ? y : /* @__PURE__ */ s("div", { className: "hashu-stack", children: [
    !t.hashu.reachable && /* @__PURE__ */ s("div", { className: "hashu-alert", children: [
      "Hashu proxy unreachable at ",
      t.hashu.controlUrl,
      " (",
      t.hashu.error,
      "). Heaters fall back to the home pool."
    ] }),
    i && /* @__PURE__ */ a("div", { className: "hashu-alert", children: i }),
    y,
    /* @__PURE__ */ s("p", { className: "hashu-note", children: [
      "Every watt a miner draws ends up as heat, exactly like an electric heater. This one also earns ",
      t.prices.satsPerThh,
      " sats per THH it sells: ",
      o(u.joulesPerTh, 1),
      " J/TH means ",
      o(u.kwhPerThh, 4),
      " kWh per THH, so about ",
      Math.round(u.satsPerKwh),
      " sats per kWh, against electricity at ",
      t.prices.electricitySatsPerKwh,
      " sats/kWh."
    ] }),
    t.devices.map((r) => /* @__PURE__ */ a(_, { d: r, s: t, onOverride: (l) => m(h({ override: { [r.deviceId]: l } })) }, r.deviceId)),
    /* @__PURE__ */ s("div", { className: "hashu-card", children: [
      /* @__PURE__ */ s("div", { className: "hashu-card-head", children: [
        /* @__PURE__ */ a("strong", { children: "Thermostat" }),
        /* @__PURE__ */ a("button", { className: t.thermostat.enabled ? "on" : "", onClick: () => m(h({ thermostat: { enabled: !t.thermostat.enabled } })), children: t.thermostat.enabled ? "on" : "off" })
      ] }),
      /* @__PURE__ */ s("div", { className: "hashu-row", children: [
        /* @__PURE__ */ s("span", { children: [
          "Target ",
          t.thermostat.setpointC,
          " °C"
        ] }),
        /* @__PURE__ */ a("button", { onClick: () => m(h({ thermostat: { setpointC: t.thermostat.setpointC - 0.5 } })), children: "−" }),
        /* @__PURE__ */ a("button", { onClick: () => m(h({ thermostat: { setpointC: t.thermostat.setpointC + 0.5 } })), children: "+" }),
        /* @__PURE__ */ s("span", { children: [
          "Never above ",
          t.thermostat.maxRoomC,
          " °C, even while selling."
        ] })
      ] })
    ] }),
    /* @__PURE__ */ s("div", { className: "hashu-card", children: [
      /* @__PURE__ */ s("div", { className: "hashu-card-head", children: [
        /* @__PURE__ */ a("strong", { children: "THH redemptions" }),
        /* @__PURE__ */ s("span", { children: [
          t.hashu.activeRedemptions,
          " open"
        ] })
      ] }),
      t.hashu.redemptions.length === 0 && /* @__PURE__ */ a("p", { className: "hashu-why", children: "None yet." }),
      t.hashu.redemptions.map((r) => /* @__PURE__ */ s("div", { className: "hashu-redemption", children: [
        /* @__PURE__ */ a("span", { children: r.id.slice(0, 8) }),
        /* @__PURE__ */ s("span", { children: [
          r.amount_thh,
          " THH → ",
          r.worker_name,
          " @ ",
          H(r.pool_url)
        ] }),
        /* @__PURE__ */ a("div", { className: "hashu-bar", children: /* @__PURE__ */ a("div", { style: { width: `${Math.round(r.progress * 100)}%` } }) }),
        /* @__PURE__ */ a("span", { children: r.state === "completed" ? "filled" : `${Math.round(r.progress * 100)}%` })
      ] }, r.id)),
      t.allowDemoRedeem && /* @__PURE__ */ s("div", { className: "hashu-row", children: [
        /* @__PURE__ */ a("span", { children: "Demo: redeem" }),
        /* @__PURE__ */ a("input", { type: "number", min: 1, max: 100, value: n, onChange: (r) => v(Number(r.target.value) || 1) }),
        /* @__PURE__ */ s("span", { children: [
          "THH to ",
          t.pools.redeem.user,
          " @ ",
          H(t.pools.redeem.url)
        ] }),
        /* @__PURE__ */ a("button", { onClick: () => m(c({ amountThh: n })), children: "Redeem" })
      ] })
    ] }),
    /* @__PURE__ */ s("div", { className: "hashu-card", children: [
      /* @__PURE__ */ a("div", { className: "hashu-card-head", children: /* @__PURE__ */ a("strong", { children: "What happened" }) }),
      /* @__PURE__ */ a("ul", { className: "hashu-events", children: t.events.map((r, l) => /* @__PURE__ */ s("li", { className: r.kind, children: [
        /* @__PURE__ */ a("time", { children: new Date(r.at).toLocaleTimeString() }),
        " ",
        r.deviceId ? /* @__PURE__ */ a("b", { children: r.deviceId }) : null,
        " ",
        r.text
      ] }, l)) })
    ] })
  ] });
}
const g = (e) => ["hashu", "state", e], D = (e) => C(e), w = (e, h) => C(e, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(h) }), I = (e, h) => w(e, h), L = (e, h) => w(e, h), T = () => {
  const { apiUrl: e } = P(), h = e("/api/hashu/state"), c = R(), d = S({ queryKey: g(h), queryFn: () => D(h), retry: !1, refetchInterval: 2e3 }), n = M(d, h, { unreachableHint: "Start the stack with `mdk run`." }), v = () => c.invalidateQueries({ queryKey: g(h) });
  return {
    view: n,
    setSettings: async (i) => {
      await I(e("/api/hashu/settings"), i), v();
    },
    redeem: async (i) => {
      await L(e("/api/hashu/redeem"), i), v();
    }
  };
}, q = () => {
  const e = T();
  return /* @__PURE__ */ s("section", { className: "hashu-page", children: [
    /* @__PURE__ */ a(N, { variant: "heading1", children: "Hashu Heater" }),
    /* @__PURE__ */ a(N, { variant: "secondary", children: "Your miner heats the room and sells its hashrate as THH ecash. When someone redeems THH, the heater switches to the Hashu proxy until the order is filled, then comes back to your pool." }),
    /* @__PURE__ */ a(k, { ...e })
  ] });
}, j = () => {
  const e = T();
  return /* @__PURE__ */ a(k, { ...e, compact: !0 });
}, V = O({
  id: "io.hashu.heater",
  displayName: "Hashu Heater",
  routes: [{ path: "", component: q }],
  panels: [{ id: "hashu", slot: x.HOME, title: "Hashu Heater", component: j }]
});
export {
  V as default
};
