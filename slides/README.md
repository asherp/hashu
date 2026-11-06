# Hashu slides

Seven-slide [Slidev](https://sli.dev) deck introducing Hashu: why it sells computation rather than money, the USD → hashrate → BTC flow, share accounting, the Hashu Heater on Tether MDK, what works today on mainnet, and the live demo at mint.hashu.io. Published at https://hashu.io/slides/ (built into the Caddy image in deploy/).

```bash
cd slides
npm install
npm run dev      # present at http://localhost:3030
npm run build    # static site in dist/
npm run export   # PDF (needs playwright-chromium)
```

Speaker notes live in the `<!-- -->` comments under each slide.
