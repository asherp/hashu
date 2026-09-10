#!/usr/bin/env python3
"""Interactive Plotly Dash front end for the mining-pool simulation."""

from __future__ import annotations

import math

import plotly.graph_objects as go
from dash import Dash, Input, Output, State, dash_table, dcc, html

from pool_sim import Config, DIFF1_WORK, Miner, Simulation


COLORS = {"stream": "#f59e0b", "pps": "#38bdf8", "pplns": "#a78bfa"}
PANEL = {
    "background": "#151b2b", "border": "1px solid #293249",
    "borderRadius": "12px", "padding": "16px",
}


def number_input(label, component_id, value, *, min_value=0, step="any"):
    return html.Label([
        html.Span(label, style={"display": "block", "marginBottom": "5px", "color": "#aab4cc"}),
        dcc.Input(id=component_id, type="number", value=value, min=min_value, step=step,
                  style={"width": "100%", "boxSizing": "border-box", "padding": "8px",
                         "background": "#0d1220", "color": "#edf2ff", "border": "1px solid #35405b",
                         "borderRadius": "6px"}),
    ])


app = Dash(__name__)
app.title = "Pool payout simulator"
app.layout = html.Div([
    html.Div([
        html.H1("Pool payout simulator", style={"marginBottom": "4px"}),
        html.P("One share stream, three payout policies: realized-price streaming, PPS, and PPLNS.",
               style={"color": "#aab4cc", "marginTop": 0}),
    ]),
    html.Div([
        html.Div([
            html.H3("Simulation", style={"marginTop": 0}),
            number_input("Duration (days)", "days", 180, min_value=1, step=1),
            number_input("Random seed", "seed", 7, min_value=0, step=1),
            number_input("Network hashrate (EH/s)", "network-eh", 600, min_value=1),
            number_input("Network difficulty (trillion)", "difficulty-t", 80, min_value=.001),
            number_input("Block reward (BTC)", "reward", 3.25, min_value=0),
            number_input("Maturity (blocks)", "maturity", 100, min_value=0, step=1),
            number_input("Payout tick (minutes)", "tick-minutes", 60, min_value=1, step=1),
        ], style={**PANEL, "display": "grid", "gap": "11px"}),
        html.Div([
            html.H3("Policies", style={"marginTop": 0}),
            number_input("EWMA half-life (days)", "half-life", 14, min_value=.01),
            number_input("PPLNS N (blocks of work)", "pplns-n", 2, min_value=.01),
            html.H3("Miner hashrates (EH/s)"),
            number_input("Alice", "alice-eh", 2, min_value=.001),
            number_input("Bob", "bob-eh", 1, min_value=.001),
            number_input("Carol", "carol-eh", .5, min_value=.001),
            html.Button("Run simulation", id="run", n_clicks=0,
                        style={"padding": "11px", "marginTop": "8px", "background": "#f59e0b",
                               "border": 0, "borderRadius": "7px", "fontWeight": 700,
                               "cursor": "pointer"}),
        ], style={**PANEL, "display": "grid", "gap": "11px"}),
    ], style={"display": "grid", "gridTemplateColumns": "repeat(auto-fit, minmax(250px, 1fr))",
              "gap": "16px"}),
    html.Div(id="status", style={**PANEL, "marginTop": "16px"}),
    dcc.Loading(html.Div([
        html.Div([
            html.Label("Display miner ", style={"color": "#aab4cc"}),
            dcc.Dropdown(id="miner", options=[{"label": x.title(), "value": x} for x in ("alice", "bob", "carol")],
                         value="alice", clearable=False, style={"color": "#111827", "width": "220px"}),
        ], style={**PANEL, "marginTop": "16px"}),
        dcc.Store(id="simulation-data"),
        dcc.Graph(id="payout-chart"),
        dcc.Graph(id="balance-chart"),
        html.Div(dash_table.DataTable(
            id="summary-table",
            style_header={"backgroundColor": "#202941", "fontWeight": "bold", "color": "#edf2ff"},
            style_cell={"backgroundColor": "#151b2b", "color": "#edf2ff", "border": "1px solid #35405b",
                        "padding": "9px", "textAlign": "right"},
            style_cell_conditional=[{"if": {"column_id": c}, "textAlign": "left"} for c in ("policy", "miner")],
        ), style={**PANEL, "marginBottom": "20px"}),
    ]), type="circle"),
], style={"maxWidth": "1200px", "margin": "auto", "padding": "24px",
          "background": "#0a0f1c", "minHeight": "100vh", "color": "#edf2ff",
          "fontFamily": "Inter, system-ui, sans-serif"})


def dark_figure(title, y_title):
    fig = go.Figure()
    fig.update_layout(title=title, paper_bgcolor="#0a0f1c", plot_bgcolor="#151b2b",
                      font_color="#dce5f8", legend_title_text="Policy",
                      xaxis_title="Simulation day", yaxis_title=y_title,
                      margin=dict(l=55, r=25, t=60, b=50), hovermode="x unified")
    fig.update_xaxes(gridcolor="#293249")
    fig.update_yaxes(gridcolor="#293249")
    return fig


@app.callback(
    Output("simulation-data", "data"), Output("status", "children"),
    Input("run", "n_clicks"),
    State("days", "value"), State("seed", "value"), State("network-eh", "value"),
    State("difficulty-t", "value"), State("reward", "value"), State("maturity", "value"),
    State("tick-minutes", "value"), State("half-life", "value"), State("pplns-n", "value"),
    State("alice-eh", "value"), State("bob-eh", "value"), State("carol-eh", "value"),
    prevent_initial_call=False,
)
def run_simulation(_, days, seed, network_eh, difficulty_t, reward, maturity,
                   tick_minutes, half_life, pplns_n, alice_eh, bob_eh, carol_eh):
    rates = {"alice": alice_eh, "bob": bob_eh, "carol": carol_eh}
    # About one accepted share per 200 seconds per miner keeps the event model fast.
    miners = [Miner(name, eh * 1e18, max(1.0, eh * 1e18 * 200 / DIFF1_WORK)) for name, eh in rates.items()]
    cfg = Config(days=days, seed=int(seed), network_hashrate=network_eh * 1e18,
                 network_difficulty=difficulty_t * 1e12, block_reward=reward,
                 maturity_blocks=int(maturity), payout_tick=tick_minutes * 60,
                 ewma_half_life_days=half_life, pplns_n=pplns_n, miners=miners)
    sim = Simulation(cfg)
    sim.run()

    # Retain a bounded number of graph points per miner without changing simulation resolution.
    stride = max(1, math.ceil(len(sim.rows) / (3 * 2500)))
    rows = sim.rows[::stride]
    summary = []
    for policy in ("stream", "pps", "pplns"):
        for miner in miners:
            earned = sim.earned[policy][miner.name]
            paid = sim.paid[policy][miner.name]
            summary.append({"policy": policy.upper(), "miner": miner.name.title(),
                            "earned (BTC)": round(earned, 8), "paid (BTC)": round(paid, 8),
                            "unpaid (BTC)": round(earned - paid, 8)})
    expected_seconds = DIFF1_WORK * cfg.network_difficulty / sum(m.hashrate for m in miners)
    status = html.Div([
        html.Strong(f"Found {sim.pool_blocks} pool blocks. "),
        html.Span(f"Expected mean pool-block interval: {expected_seconds / 3600:.2f} hours. "),
        html.Span(f"Plotted every {stride} recorded payout ticks."),
    ])
    return {"rows": rows, "summary": summary}, status


@app.callback(
    Output("payout-chart", "figure"), Output("balance-chart", "figure"),
    Output("summary-table", "data"), Output("summary-table", "columns"),
    Input("simulation-data", "data"), Input("miner", "value"),
)
def update_charts(data, miner):
    payout = dark_figure(f"Cumulative payout — {miner.title()}", "BTC paid")
    balance = dark_figure(f"Streaming state — {miner.title()}", "BTC / EH/s")
    if not data:
        return payout, balance, [], []
    rows = [r for r in data["rows"] if r["miner"] == miner]
    x = [r["time_days"] for r in rows]
    for policy, field in (("stream", "stream_paid"), ("pps", "pps_paid"), ("pplns", "pplns_paid")):
        payout.add_trace(go.Scatter(x=x, y=[r[field] for r in rows], mode="lines",
                                    name=policy.upper(), line={"color": COLORS[policy]}))
    balance.add_trace(go.Scatter(x=x, y=[r["stream_balance"] for r in rows], mode="lines",
                                 name="Unpaid BTC", line={"color": COLORS["stream"]}))
    balance.add_trace(go.Scatter(x=x, y=[r["ewma_hashrate"] / 1e18 for r in rows], mode="lines",
                                 name="EWMA EH/s", yaxis="y2", line={"color": "#34d399"}))
    balance.update_layout(yaxis2={"title": "EWMA EH/s", "overlaying": "y", "side": "right",
                                          "gridcolor": "rgba(0,0,0,0)"})
    columns = [{"name": key, "id": key} for key in data["summary"][0]]
    return payout, balance, data["summary"], columns


if __name__ == "__main__":
    app.run(debug=False)
