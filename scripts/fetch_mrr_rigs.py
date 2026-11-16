#!/usr/bin/env python3
"""Fetch every SHA-256 rig listed on Mining Rig Rentals and save a CSV.

Uses the public API (no key needed): GET https://www.miningrigrentals.com/api/v2/rig
Run anywhere that can reach miningrigrentals.com, e.g. the hashu-prod droplet:

    python3 fetch_mrr_rigs.py > mrr-sha256-rigs.csv
    python3 fetch_mrr_rigs.py --algo sha256 > mrr-sha256-plain.csv

Columns: id, algo, status, online, BTC price enabled, advertised hashrate in TH/s,
price in BTC per TH/s per day, minimum and maximum rental hours, region.
Standard library only. Progress and a summary go to stderr.
"""
import argparse, csv, json, sys, time, urllib.error, urllib.parse, urllib.request

BASE = "https://www.miningrigrentals.com/api/v2/rig"
PAGE = 100
# Unit name -> TH/s. Same names as hashu-mint/src/oracle/mrr.rs ("hash" = H/s).
TO_TH = {"hash": 1e-12, "h": 1e-12, "kh": 1e-9, "mh": 1e-6, "gh": 1e-3, "th": 1.0, "ph": 1e3, "eh": 1e6}


def num(v):
    """MRR sends most numbers as strings."""
    return float(str(v).strip())


def truthy(v):
    return v is True or str(v).strip().lower() in ("1", "true", "yes")


def get(algo, offset, retries=4):
    q = urllib.parse.urlencode({
        "type": algo, "count": PAGE, "offset": offset,
        # Stable order so pages don't shift much while we walk them, and prices
        # quoted per PH/day (MRR's SHA-256 unit), as the mint's oracle asks.
        "orderby": "price", "orderdir": "asc", "hash.type": "ph",
    })
    req = urllib.request.Request(f"{BASE}?{q}", headers={
        "User-Agent": "rig-price-survey", "Accept": "application/json"})
    for attempt in range(retries + 1):
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                body = json.load(r)
            if body.get("success") is not True:
                raise RuntimeError(f"MRR error: {body.get('data', {}).get('message', body)}")
            return body["data"]
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as e:
            if attempt == retries:
                raise
            wait = 2 ** attempt
            print(f"offset {offset}: {e}; retrying in {wait}s", file=sys.stderr)
            time.sleep(wait)


def row(r, algo):
    hr = r["hashrate"]["advertised"]
    th = num(hr["hash"]) * TO_TH[str(hr.get("type", "ph")).lower()]
    p = r["price"]
    btc = p["BTC"]
    # With hash.type=ph the price is BTC per PH/day; honour price.type if MRR sends one.
    unit = str(p.get("type") or "ph").lower()
    per_th = num(btc["price"]) / TO_TH[unit]
    status = r.get("status")
    status = status.get("status", "") if isinstance(status, dict) else (status or "")
    return [r["id"], algo, status, int(truthy(r.get("online"))), int(truthy(btc.get("enabled"))),
            f"{th:.6g}", f"{per_th:.10g}", r.get("minhours", ""), r.get("maxhours", ""),
            r.get("region", "")]


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--algo", default="sha256ab",
                    help="MRR algorithm: sha256ab (default, where modern ASICs list) or sha256")
    args = ap.parse_args()

    out = csv.writer(sys.stdout)
    out.writerow(["id", "algo", "status", "online", "btc_enabled", "th_s",
                  "btc_per_th_day", "minhours", "maxhours", "region"])
    offset, seen_ids, written, skipped, dupes, total = 0, set(), 0, 0, 0, None
    while total is None or offset < total:
        data = get(args.algo, offset)
        total = int(num(data.get("total", 0)))
        recs = data.get("records") or []
        if not recs:
            break
        for r in recs:
            rid = str(r.get("id"))
            if rid in seen_ids:
                dupes += 1
                continue
            seen_ids.add(rid)
            try:
                out.writerow(row(r, args.algo))
                written += 1
            except (KeyError, ValueError, TypeError, AttributeError) as e:
                skipped += 1
                if skipped <= 5:
                    print(f"skipped rig {rid}: {type(e).__name__} {e}", file=sys.stderr)
        offset += len(recs)
        print(f"fetched {offset} of {total}", file=sys.stderr)
        time.sleep(0.5)

    print(f"done: {written} rows, {skipped} skipped, {dupes} duplicates, {total} listed",
          file=sys.stderr)
    if written == 0 or skipped > written:
        sys.exit("most records could not be parsed; check the API's field names")


if __name__ == "__main__":
    main()
