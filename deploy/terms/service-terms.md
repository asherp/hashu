# Hashu Hashrate Service and Supplier Terms

**Draft, not yet in force.** Last updated 2026-11-28. Text in [brackets] is still being decided.

## 1. Who we are and what these terms cover

These terms are an agreement between you and Pembroke Creative, LLC, a Texas limited liability company ("Hashu", "we", "us"). By buying, holding, redeeming, or supplying hashrate vouchers, or by using our sites, you agree to them. If you do not agree, do not use the service.

They cover:

- **The sites:** hashu.io and mint.hashu.io: the checkout, order, and redemption pages, the reliability page, the waitlist, and supplier sign-up.
- **The Hashu mint** at mint.hashu.io, which sells hashrate from miners we operate or rent, and its Nostr profile and inbox.
- **Supplier mints** at names like alice.hashu.io (older links like alice.mint.hashu.io still work), which sell hashrate from independent miners (section 5).
- **The stratum proxy** that miners and redemptions connect to.

**Who the seller is.** Each mint is the seller and issuer of its own vouchers: Pembroke Creative, LLC for the Hashu mint, and the supplier for a supplier mint (section 5). The checkout page names the seller of the mint you are buying from, and these terms apply to that sale. The hashu.me wallet is software, not a seller, and has its own terms of use.

The Hashu software is open source. Running your own copy is governed by its license, not by these terms. Anyone who runs their own mint is the operator of that mint, and we have no role in it.

## 2. What a hashrate voucher is, and what it is not

A Chaumian hashrate voucher ("voucher") is a prepaid claim on computation: SHA-256 hashing work pointed at a mining pool you choose. Vouchers are counted in TiH (tebihash, 2^40 hashes), a unit of work, never in dollars, sats, or any currency, and come in power-of-two pieces from 1 TiH to 2^40 TiH. Wallets may show the same amount as TH-hours, PH-days, or similar, but the voucher itself always counts TiH. They are Cashu tokens issued with blind signatures, so the mint cannot link a voucher to its buyer.

A voucher is only redeemable for hashing work from the mint that issued it. It is **not**:

- **Money or a payment instrument.** It cannot be redeemed for bitcoin, sats, stablecoins, fiat, or any other asset. No mint we operate pays out sats or cash for vouchers, ever.
- **A mining reward.** You can point the hashing at any pool that accepts SHA-256 work, whatever chain or token it mines. That pool alone decides what reward the work earns, if any, and in which coin or token it pays. We do not promise any payout.
- **An investment, security, or lottery.** Vouchers earn no interest or yield, and we make no promise about what anyone else will pay for one.
- **Stable in value.** The bitcoin a TiH can earn changes with every difficulty adjustment and with fees. The price we sell at also changes.

**What is owed is firm; when is best effort.** The amount of work a voucher stands for is a firm obligation of the mint that issued it. When that work is delivered is best effort: it depends on how many miners are online and can be slow. Late delivery is still delivery, and a delay never cancels or shrinks what is owed.

**Vouchers do not expire.** They stay redeemable while the issuing mint operates. Running out of hashrate can delay a redemption, but never cancels a voucher. If a mint closes, section 9 applies.

**Epoch-dated vouchers.** The Hashu mint dates its vouchers to a Bitcoin difficulty epoch (2,016 blocks, about two weeks), and the work is meant to be delivered during that epoch. The checkout sells the current epoch; vouchers for a future epoch are sold separately and can be redeemed once that epoch starts. Supplier mints may do the same. A dated voucher never lapses: once its epoch ends, you can convert it one for one, TiH for TiH, into vouchers for the current epoch, or for the next epoch with hashrate available, at no charge. Change from a stopped redemption comes back dated to the current epoch.

Vouchers are transferable, like a gift card or any Cashu token. You may give, sell, or trade them to anyone, at any price, and whoever holds them can redeem them. A resale is between you and the other person: we are not a party to it, do not set or guarantee resale prices, and never buy vouchers back. We do not run a resale market today. If we launch one, it will come with its own terms, which will apply to trades made through it.

## 3. Buying vouchers

You buy vouchers by paying a quote at checkout. What you pay is the price of a hashing service, not a deposit: we hold no balance for you and send no bitcoin on your behalf. Each quote states the amount of TiH, the price, and how long it stays valid.

- **Payment methods.** Bitcoin over Lightning (mainnet) to our own node, and, where the checkout offers them, USDT on Ethereum or USDC on Base. A Lightning invoice is valid for one hour, and we absorb any channel or liquidity fees, so you always receive the full TiH quoted. A stablecoin quote names one deposit address and an exact amount, including a few sub-cent digits that identify your quote. It settles once the transfer is 12 blocks deep for USDT or 30 blocks deep for USDC. Send exactly that amount, on that chain, to that address, before the quote expires. A transfer that matches no open quote is not issued vouchers automatically; contact us and we will try to return it by hand, less network fees, but we cannot promise to recover it.
- **Price.** Prices are quoted in millisats per PiH (2^50 hashes). By default the Hashu mint follows the hashrate rental market price on MiningRigRentals plus a markup, and falls back to a fixed price when that market price is unavailable. The price is fixed once you get a quote, and later quotes may differ. The checkout shows the price in the same unit you choose for the amount.
- **Minimum and stock.** You can buy any whole number of TiH from 1 PiH (1,024 TiH) up to what the mint has for sale. Each mint only sells what its miners can produce within its stated horizon, after subtracting everything it already owes: unredeemed vouchers (including expired ones not yet converted), paid quotes not yet issued, and the undelivered rest of open redemptions. Work owed beyond the current epoch is carried into the next epochs' capacity, and vouchers sold for a future epoch count against that epoch. So stock can run out and checkout can pause. Vouchers are sold while supplies last, and delivery timing is best effort (section 2).
- **Delivery of vouchers.** Vouchers are issued as soon as payment settles. If you use your own Cashu wallet (such as hashu.me), they go straight to it. If you use our checkout page instead, our server runs a short-lived wallet for you and hands you a token, so we hold your vouchers briefly; save or claim that token right away. If you paid and did not get vouchers, contact us with your quote ID and proof of payment within [30] days.
- **All sales are final.** Because vouchers are bearer instruments we cannot take back, we do not offer refunds for unused vouchers, except as section 9 says. If we cannot deliver vouchers you paid for, we will, at our choice, issue them later or return your payment by the same method, less network fees.
- **Gifts and tips are not purchases.** A tip sent to a mint's Lightning address or Nostr profile supports Hashu's development and buys no vouchers. Vouchers we send you for free, for example in a Nostr message, are gifts: they work like any other voucher and are not refundable.

We may refuse or cancel any quote before you pay it.

## 4. Redeeming vouchers

To redeem, you give the mint vouchers and the pool address, username (worker), and password you want the hashing sent to, plus up to three backup pools if you like. The mint burns the vouchers and our proxy points miners at your pool until the work is delivered. Redeeming a voucher means you accept the operational rules in force at that time (how redemptions are submitted, minimums, worker names, backup pools, pausing, and how delivery is measured), even if you got the voucher from someone else. Later changes never take away the substantive rights a voucher had when it was issued: the amount of work owed, that it does not expire, that it is redeemable for hashing work only, and the protections in section 9 if its mint closes.

- **How delivery is measured.** Work counts when the proxy credits a share your pool's job accepts. A share at difficulty D counts as D/256 TiH. Our share tally is the record of what was delivered. We keep a tamper-evident log of every credited share (section 7) and can give you proof of the shares that filled your redemption. If you think the tally is wrong, tell us within [7] days of the redemption ending.
- **Your pool is your responsibility.** Check its address and credentials before you redeem. Work sent to a wrong pool or account, or rejected by your pool, is not refundable. We do not control pools, their fees, their payout rules, or which coin or token they pay.
- **Worker names.** While our miners work for you, the proxy logs in to your pool as your account, with the selling mint's host name as the worker label (for example `you.mint.hashu.io`). Any label you typed is replaced, except a name containing `@`, which is sent as typed.
- **Backup pools.** If your pool is unreachable, refuses the login, or rejects many shares in a row, the proxy moves the work to your next backup pool for a while. If every pool you gave is down, delivery waits.
- **Order and timing.** Open redemptions are served oldest first. The wallet and order page show an estimated finish time. It is only an estimate: delivery speed depends on how many miners are online and can change.
- **Minimum.** The smallest redemption is 1 PiH (1,024 TiH), because pools count work in shares. Any whole number of TiH above that is fine.
- **Pause, resume, and change pool.** If your wallet set a stop key when you redeemed (hashu.me and the checkout page do), you can pause delivery, resume it, or send it to a different pool at any time. A paused redemption keeps its place and is still owed; nothing is charged or returned while it is paused. We may also pause a redemption, for example while your pool misbehaves. You cannot resume a pause we set.
- **Stopping and change.** You can stop a redemption once at least 1 PiH has been delivered. You are charged for the work delivered, never less than 1 PiH, and undelivered work comes back as new vouchers on the blank outputs your wallet supplied, rounded down to whole TiH. If your wallet supplied no blank outputs, undelivered work cannot be returned. Work is never returned as sats or any other asset.
- **If our miners go down.** If every miner serving a mint has been offline for two minutes or more, the mint takes no new redemptions. If yours is open, you may stop it at any time, even before 1 PiH has been delivered, or while we have it paused. You are still charged at least 1 PiH in that case today. [We plan to return the full floor for undelivered work.]
- **No outcome guarantee.** We deliver SHA-256 hashing work to the pool you chose, not blocks, block-reward shares, or any amount of bitcoin or any other token.

## 5. Suppliers and supplier mints

A supplier is a miner who sells their own hashrate through a supplier mint we host, such as alice.hashu.io. Each supplier mint has its own keys, its own vouchers, and its own price, and only that supplier's verified rigs fill its redemptions. The supplier is the seller and issuer of that mint's vouchers. We provide the software and hosting, not the hashrate.

**If you buy from a supplier:**

- You pay the supplier directly, through the supplier's own Lightning address, at the price (in sats per EiH) the supplier set. The supplier mint issues vouchers once the supplier's wallet reports the invoice paid. We do not receive, hold, or forward your payment.
- Vouchers from one supplier can only be redeemed with that supplier's hashrate. They are not interchangeable with vouchers from the Hashu mint or from other suppliers.
- If a supplier's rigs go offline or the supplier stops operating, delivery slows or stops. That risk is yours and the supplier's. We do not make good a supplier's shortfall. Check a supplier's live hashrate and history before you buy.
- **Supplier default is not our failure, and our failure is not the supplier's.** A supplier is responsible for its own rigs, power, Lightning wallet, prices, and honouring its vouchers. We are responsible for what we run: the hosting, the mint software, the proxy, and the delivery records. If a supplier mint is unreachable, misroutes work, or loses records because of our infrastructure, that is ours to fix, under the same limits as section 10.
- A supplier mint sells only what the supplier's measured hashrate can deliver within the supplier's horizon, topped up each hour. Supplier mints do not yet offer the pause, resume, stop, or change-pool controls in section 4. Messages you send a supplier mint go to the supplier's own Nostr account, not to us.
- **Not all hashes are worth the same.** Every supplier sells while supplies last. Like us, a supplier owes the full amount of work its vouchers stand for, but delivery timing is best effort, and suppliers differ in how reliably they deliver. This is by design: vouchers from a reliable supplier may be worth more than vouchers from an unreliable one, and anyone reselling them is free to price that in. We do not rate suppliers or guarantee any supplier's delivery.
- An order split across several suppliers is several separate purchases, each from its own supplier. Disputes between a buyer and a supplier are theirs to settle. On request, we may share the relevant order and delivery records with both.

**If you are a supplier, you agree to:**

- Point only hardware you own or control at the proxy, and keep it running for the horizon you set.
- Set an asking price, a horizon, and payment details that are accurate and that you can receive.
- Honour every voucher your mint issued, and handle any taxes, licences, and power costs that come with selling your hashrate.
- Keep your rig secret (pw=) private. Anyone with it can mine as you.
- If you stop selling, follow section 9: give notice, stop new sales, and keep your rigs serving open redemptions through the redemption window.

We may delist a supplier, or stop routing to their rigs, if they abuse the service or leave buyers undelivered. Except in an emergency (such as stolen hashrate or a legal order), we will delist only as section 9 describes, so holders get a redemption window. Miners who mine through the proxy without registering are sent to their own home pool, and their hashrate is never sold.

## 6. Vouchers are bearer items: custody

Whoever holds a voucher can redeem it. We have no accounts and keep no balances, so we cannot see, freeze, recover, or replace a voucher that is lost, stolen, spent by someone else, or sent to the wrong person. The one time we hold vouchers for you is the short-lived checkout wallet in section 3, between payment and when you save or claim the token.

Where you keep vouchers is up to you. The hashu.me wallet, or any other Cashu wallet, is covered by its own terms, not these.

## 7. Privacy and the records we keep

Blind signatures mean the mint cannot tell which purchase paid for a voucher, or who holds one. A redemption is different: it names a pool account, and we keep a permanent record of the work sent to it. We have no user accounts, do not sell data, and do not use analytics or ad trackers. Today the records below are kept indefinitely, except where the table says otherwise.

| Record | What it holds | Who can see it |
| --- | --- | --- |
| Payment quotes | Amount of TiH, price, Lightning invoice or stablecoin request, status, and times. Stablecoin transfers are also public on their blockchain. | Us |
| Spent-voucher list | A fingerprint of each spent voucher, so it cannot be spent twice. It does not identify the buyer. | Us |
| Redemption ledger | Each redemption's ID, size, pool address, worker name, backup pools, a hash of your stop key, pauses and pool changes, and TiH delivered to each pool. Pool passwords are kept only while the redemption is open and are deleted after it ends. | Us |
| Share log | Every share credited to a redemption: its 80-byte block header, target, and time, tied to the redemption's ID (and so, through the ledger, to its pool and worker). | Us; on request, proofs of your own shares |
| Checkout orders | The private order link and the redemption and quote it belongs to. Change keys stay in your browser. | Us, and anyone with the order link |
| Public status | Each open redemption's ID, size, and progress. No pool or worker names. | Anyone |
| Hourly reliability record | For each mint and each hour: the 16 best shares and any blocks found, total work, redemption counts, uptime, and hashrate. Signed, posted to public Nostr relays, and timestamped in Bitcoin with OpenTimestamps. It names no redeemer, pool, or worker. | Anyone, permanently; it cannot be deleted once published |
| Messages to the Hashu mint | Nostr direct messages you send the mint, decrypted, with your Nostr public key. Messages to a supplier mint go to the supplier, not us. | Us |
| Waitlist and supplier sign-up | What you type: contact (email or Nostr), mint name, hashrate, horizon, payout preferences, a note; for suppliers, also price, Lightning address, Nostr key, and a hash of the rig secret. No IP address is stored. | Us; a supplier's name, price, and live hashrate are public |
| Server logs and rig telemetry | IP addresses of miners connecting to the proxy, and when they connect; per-rig work and sessions. Telemetry is deleted after 30 days; logs [ROTATION]. | Us |

Pools you redeem to see our proxy's IP address, not yours, and the worker name in section 4. Payment providers (our Lightning node's channel partner, blockchains, RPC providers) see what their role requires. We may share records when the law requires it, or with a buyer and supplier in a dispute (section 5). A separate privacy notice is [to come].

## 8. Eligibility and acceptable use

You must be at least [18] and able to form a binding contract. You may not use the service if you are in, or a resident of, [New York State], a country or region under comprehensive US sanctions, or anywhere using it would be illegal for you. You may not use it if you are on a US or UN sanctions list.

You agree not to:

- Point stolen, hijacked, or botnet hashrate at the proxy, or hardware you lack permission to use.
- Redeem hashrate toward anything other than a mining pool or your own stratum endpoint, or use it to attack a pool or the network.
- Overload, probe, or disrupt the mint, proxy, or sites, or replay, forge, or double-spend vouchers.
- Claim, when reselling vouchers, that Hashu backs the resale, buys vouchers back, or guarantees their value.
- Use the service to launder money, evade sanctions, or fund illegal activity.

We may refuse, pause, or cancel any purchase, redemption, or supplier listing that we reasonably believe breaks these terms or the law.

## 9. If a mint closes

Vouchers do not expire, so a mint that stops operating owes its holders a fair way out. This section applies when we close the Hashu mint or this service, and when a supplier closes their mint or we delist one.

1. **Notice.** We post notice on hashu.io, on the mint's own page, and on its Nostr profile at least [30] days before redemption ends. A supplier who closes must give the same notice, and we post it for them if they do not.
2. **Sales stop first.** From the notice, the mint sells no new vouchers.
3. **Redemption window.** Redemption stays open for at least [90] days after the notice, and the miners behind the mint keep serving open redemptions. A redemption opened before the window ends is served until it is delivered or stopped.
4. **Remaining work.** Work still owed when we wind down is delivered, not refunded. We may serve it with rented hashrate (for example from a rig rental market) as well as our own, or arrange for another mint to exchange the vouchers for its own, TiH for TiH. Because vouchers are bearer items issued with blind signatures, a voucher does not record who bought it or what they paid, so we do not refund purchase prices, except where the law requires it.
5. **Supplier mints.** A supplier, not Hashu, received the payment for its vouchers, so any refund of a supplier's vouchers is the supplier's to make. We will keep hosting a closing supplier's mint for redemption through the window, and pass on contact details we have for the supplier.

These steps do not apply where we must stop at once because of a court or regulator's order, a security breach, or events outside our control. Then we will do as much of them as we lawfully and practically can.

## 10. Demo status, disclaimers, and the legal small print

**Experimental service.** Hashu is a working demo of new software. Payments are real (Lightning on Bitcoin mainnet), and hashing can go to mainnet or test-network pools. It runs on a single server, can go down, and may lose data. Use only amounts you can afford to lose.

**No warranty.** The service and software are provided "as is" and "as available", without warranties of any kind, including merchantability, fitness for a purpose, uptime, hashrate delivered, or mining results.

**Limitation of liability.** To the extent the law allows, we are not liable for indirect, incidental, consequential, or special damages, or for lost profits, lost mining rewards, lost vouchers, or equipment losses. This includes losses from pool, stratum, worker, or payout settings you supplied, from pools, suppliers, or other third parties, and from downtime, bugs, or operational errors. Our total liability for any claim is limited to the greater of what you paid us in the 12 months before the claim or [US$100].

**Indemnity.** You will cover our losses from claims caused by your breach of these terms or your misuse of the service.

**Taxes.** You are responsible for any taxes on what you buy, sell, or mine.

**Third parties.** Pools, Lightning nodes, blockchains, rig rental markets, and other mints are outside our control. We are not responsible for them.

**Changes.** We may update these terms by posting a new version with a new date. Changes apply to purchases and redemptions started after that date, and never take away the substantive rights in section 4 that a voucher had when it was issued. Every version is kept in the Hashu repository's history.

**Suspension.** We may suspend or end your access for a breach of these terms. Vouchers you already hold stay redeemable unless they came from a breach, such as stolen hashrate or a double spend.

**Regulatory position.** The service is built so that we sell hashing services and nothing else. We do not hold or transmit bitcoin or stablecoins for users, keep stored-value accounts, or pay out mining rewards, and we offer no custodial wallet beyond the short-lived checkout wallet described in section 3. Rules for bitcoin-related services vary by place and can change. You are responsible for whether your use is lawful where you are, and for your own taxes and reporting.

**Feedback and intellectual property.** If you send us feedback or ideas, we may use them freely. The Hashu name and site content belong to us. The software is licensed under its open-source license.

**Entire agreement and severability.** These terms and any policies they link to are the whole agreement between you and us. If a court finds part of them unenforceable, the rest still applies.

**Governing law and disputes.** These terms are governed by the laws of the State of Texas, without regard to its conflict-of-law rules. Before filing anything, contact us and we will try to resolve it within 30 days. Any dispute not resolved that way goes to binding individual arbitration under the American Arbitration Association's Consumer Arbitration Rules, held online or by phone where possible, or else in [COUNTY] County, Texas. You and we each waive the right to a jury trial and to bring or join a class or representative action. Either side may instead bring an individual claim in small-claims court. You may opt out of arbitration by emailing us within 30 days of first accepting these terms.

**Contact.** [EMAIL or Nostr npub].
