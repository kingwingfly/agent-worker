# Optional paid service

Deploy this Worker alongside the gateway to sell prepaid access. The checkout lives at the billing Worker's root URL and the operator dashboard at `/admin`. Accounts belong to Telegram users. A person's private chat, group turns and guest turns all draw from the same balance.

## Enable billing

1. In `billing/wrangler.toml`, point `STATE` at the gateway's existing R2 bucket. The billing Worker uses the merchant's public receiving addresses and a mainnet RPC endpoint for each network. The merchant keeps the signing keys.
2. In Cloudflare Zero Trust, create a self-hosted Access application for `<billing-host>/admin` with an Allow policy for your administrators. The path covers the dashboard and its descendants, including `/admin/api/*` and file downloads. Apply the admin policy on each billing hostname you use, including an enabled `workers.dev` hostname. The Worker checks Cloudflare’s authenticated Access context before serving any admin route. The customer checkout at `/` uses its own account sign-in. [Cloudflare Access for Workers](https://developers.cloudflare.com/workers/configuration/cloudflare-access/) · [Application paths](https://developers.cloudflare.com/cloudflare-one/access-controls/policies/app-paths/)

3. From the repository root, set the billing RPC secrets and deploy the billing Worker. Each RPC must support the finalized transaction and block lookups used by its chain.
   ```sh
   npx wrangler secret put ETH_RPC_URL --config billing/wrangler.toml
   npx wrangler secret put SOL_RPC_URL --config billing/wrangler.toml
   npx wrangler deploy --config billing/wrangler.toml
   ```
   For local payment testing, put your RPC URLs in `billing/.dev.vars` as `ETH_RPC_URL` and `SOL_RPC_URL`.
4. Open `https://<billing-host>/admin#deposits` through Cloudflare Access. Enter the Ethereum mainnet address where you receive native ETH with transaction data and the Solana mainnet address where you receive SOL. Save the form. Each network becomes available to customers when its address and RPC are configured. Leaving an address blank disables new invoices for that network. If you previously used `ETH_RECIPIENT` or `SOL_RECIPIENT` in Wrangler, the dashboard shows those values until you save new settings. Each invoice keeps its original receiving address and terms.
   In **Plan settings**, set the USD rates per million input and output tokens, the free weekly allowance, the monthly subscription price and the number of Telegram Stars per dollar. Add your support contact and provider cost rates there too.
5. In the gateway's `wrangler.toml`, uncomment the `[[services]]` binding for `BILLING`, including the named `GatewayBilling` entrypoint. Set `BILLING_ENABLED = "true"` and deploy the gateway with its usual command. Choose `ALLOWED_TG_USERS = "*"` for public signups or keep an allow-list. The gateway needs an Anthropic-compatible `/v1/messages/count_tokens` endpoint for enforced paid access.
6. Register the Telegram webhook with `pre_checkout_query` and `message` included. Using the same bot token, public gateway origin and webhook secret as the main deployment, run
   ```sh
   curl "https://api.telegram.org/bot$TELEGRAM_BOT_TOKEN/setWebhook" \
     -d url="$PUBLIC_ORIGIN/tg/webhook" -d secret_token="$TELEGRAM_WEBHOOK_SECRET" \
     -d 'allowed_updates=["message","callback_query","guest_message","stopped_message_generation","pre_checkout_query"]'
   ```
   Publish the **standalone checkout URL** on your service's website. Customers can see the USD price there before signing in. Inside Telegram, `/topup` buys tokens with Stars. Telegram requires Stars for digital purchases inside Telegram apps. [Telegram digital payments](https://core.telegram.org/bots/payments-stars)

## Pricing and subscriptions

| Default | Amount |
| --- | --- |
| Input | $1.00 per million tokens |
| Output | $2.00 per million tokens |
| Free allowance | 100,000 tokens per week |
| Monthly subscription | $1.00, paid with 1,000,000 wallet tokens |
| Subscriber allowance | 500,000 tokens per week (5× free) |

A customer enters a dollar amount and receives whole tokens at the input price. At the default rates, $1 buys enough for 1,000,000 input tokens or 500,000 output tokens. A call with 100,000 input and 10,000 output uses 120,000 tokens. Both the plan and a top-up of the same dollar amount use the same whole-token conversion. Model calls use the prices in effect when they start. Input includes fresh, cache-write and cache-read tokens. The ledger rounds the output portion up once per call.

Each account's allowance refills every seven days from enrollment. Chat spends the weekly allowance first and then the purchased wallet balance. Wallet tokens persist. An upgrade raises the current week's total to 5× the free allowance and carries forward usage already spent. There is no five-hour window. Changing the free allowance updates both tiers. Setting it to zero leaves direct wallet usage available and pauses subscription sales and renewals.

The checkout and the bot's private `/subscribe` command show the monthly token charge before confirmation. Only the wallet pays for subscriptions. A subscription renews at the confirmed token price each calendar month. A January 31 subscription renews on February's last day, then on March 31. Cancel in the checkout or with `/unsubscribe` to keep the current paid term and stop its next renewal. Insufficient wallet funds at renewal return the account to the free tier. [Cloudflare Durable Object alarms](https://developers.cloudflare.com/durable-objects/api/alarms/)

## Buying tokens

1. Send `/account` in a private chat with the bot. Open the standalone checkout in your browser and sign in with the code within ten minutes.
2. Under **Buy tokens**, enter a USD amount, choose **ETH** or **SOL**, then select **Get payment quote**. The invoice shows the tokens you will receive, the exact amount to send in that currency, the receiving address, the reference and the deadline. The price is locked for 15 minutes from issue. The checkout checks the merchant's mainnet RPC before giving you a quote. [Coinbase Exchange ticker](https://docs.cdp.coinbase.com/api-reference/exchange-api/rest-api/products/get-product-ticker)
3. Approve one transfer on the selected mainnet. Leave enough in your wallet to cover its network fee separately.
   - **ETH:** Choose **Pay with Ethereum wallet** in a browser with a compatible wallet. It fills in the recipient, amount and hex transaction data, and submits the hash when you approve. To pay manually, use a wallet that supports transaction data, copy all three invoice fields, then paste the hash into **Verify payment**. [Ethereum JSON-RPC](https://ethereum.org/developers/docs/apis/json-rpc/)
   - **SOL:** Choose **Open Solana wallet** to open a Solana Pay-compatible wallet with the address, amount and memo filled in. You can also enter these fields in a wallet that supports an on-chain memo. After approving, copy the transaction signature from wallet activity and paste it into **Verify payment**. [Solana Pay transfer requests](https://github.com/solana-developers/solana-pay/blob/master/SPEC1.1.md)
4. The transfer must land by the displayed deadline. You can submit its ID after that. Once submitted, the service checks for finality in the background for up to 24 hours, even if you close the page. If you return after an interruption, open the original entry under **Invoices** and verify that transfer. When it shows **Paid**, the purchased tokens are in your wallet.

For Telegram Stars, send `/topup 5` in your private chat to buy $5 of tokens. The default is 50 Stars per dollar, rounded up to a whole Star per invoice. The bot delivers the tokens after Telegram confirms payment. Replayed receipts and refunds apply once. A refund takes back the purchased tokens. If the wallet falls below zero, topping up to cover the debt restores spending. Use `/paysupport` for the merchant's contact. [Telegram payment API](https://core.telegram.org/bots/api#refundstarpayment)

The gateway reserves input and output capacity before each billed call, limits output to the available balance, and settles from reported usage. A failed call returns its hold. An unreported call releases its hold after two hours on the next account check. A late report settles once. Operators can configure provider costs independently from selling prices.

Disable usage charging in **Plan settings** for unlimited free access while retaining cost reports. New sales pause, and subscriptions reaching their renewal date during the pause expire. Invoices already paid in their quote window can still settle. To disconnect billing entirely, disable `BILLING_ENABLED` on the gateway after approved Stars purchases finish. The gateway also supports its independent optional per-chat daily budget, which defaults to unlimited.

On upgrade, existing wallet balances, passes, outstanding invoices and usage reservations convert once at the deployment's last configured tokens-per-credit rate. Older prepaid passes keep their original allowances and end dates. Recurring wallet charges require a new monthly subscription confirmation.

## Operator dashboard

- **Overview** shows USD top-up sales, actual native amounts received, active subscriptions, model tokens and estimated cost. Refunds are excluded. Spending tokens already in a wallet adds no new sale.
- **Users** supports Telegram ID search and pre-enrollment. A user page shows the wallet, weekly usage and next refill, subscription and payment history, and account adjustments. Admins can airdrop tokens, grant a fixed term of the 5× plan, revoke a subscription or suspend an account.
- **Chat history and files** opens automatically under **Users → select a Telegram ID**. The latest captured turn opens with its prompt, final reply and generated files. Use **Preview** on text files or **Download** for the original. History includes attributed group and guest turns completed while the gateway billing integration was enabled. Each text field retains up to 64,000 characters. The text viewer reads the first 32 KiB and downloads stream through authenticated admin routes. **Private chat storage** gives access to private output files and checkpoint archives already in the gateway bucket by Telegram ID, including older checkpoints. An archive contains the original transcript and workspace. If an older private account has never enrolled in billing, use **Users → Add a user** with its Telegram ID to browse its existing storage.
- **Cleanup** takes a user ID and UTC cutoff. It removes captured turns older than that cutoff and the R2 output files belonging to them, in resumable batches. The gateway manages its recovery checkpoints under its existing retention policy. Accounts, payment identifiers, grants and usage totals survive cleanup.

The ledger uses one SQLite-backed Durable Object per billing deployment. Only the gateway's named service binding can submit usage or issue sign-in codes. The public endpoints authenticate browser sessions or the administrator. Use a separate billing deployment and receiving addresses for each independent merchant.

## Test locally

Run `npx wrangler dev --local --config billing/wrangler.toml --ip 127.0.0.1 --port 8787` and open `http://127.0.0.1:8787/admin`. The `[access.dev]` setting simulates an authenticated operator for local development. Production uses the Access policy configured in Zero Trust. [Local Access simulation](https://developers.cloudflare.com/workers/configuration/cloudflare-access/#test-ctxaccess-locally)

With Node 22.13 or newer and the repository's npm dependencies installed, run `npm run test:billing`. The tests use in-memory SQLite, an isolated Workers runtime and simulated payment-network responses. After building the gateway with `(cd crates/gateway && worker-build --release)`, run `node --test billing/test/gateway.mjs` to exercise Telegram purchases and model admission through the compiled gateway in both deployment modes. For a local gateway plus billing Worker, use Wrangler multi-worker development with the gateway config first and the billing config second. Use development secrets, the existing external runner and mock the Bot API as described in the main README.
