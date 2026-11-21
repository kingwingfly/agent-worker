# Optional paid service

Deploy this Worker alongside the gateway to sell prepaid access. The checkout lives at the billing Worker's root URL and the operator dashboard at `/admin`. Accounts belong to Telegram users. A person's private chat, group turns and guest turns all draw from the same balance.

## Enable billing

1. In `billing/wrangler.toml`, point `STATE` at the gateway's existing R2 bucket. You can deploy without either deposit address or payment RPC. Chat starts closed. Customers can apply while it is closed.
2. In Cloudflare Zero Trust, create a self-hosted Access application for `<billing-host>/admin` with an Allow policy for your administrators. The path covers the dashboard and its descendants, including `/admin/api/*` and file downloads. Apply the admin policy on each billing hostname you use, including an enabled `workers.dev` hostname. The Worker checks Cloudflare’s authenticated Access context before serving any admin route. The customer checkout at `/` uses its own account sign-in. [Cloudflare Access for Workers](https://developers.cloudflare.com/workers/configuration/cloudflare-access/) · [Application paths](https://developers.cloudflare.com/cloudflare-one/access-controls/policies/app-paths/)

3. Deploy the billing Worker. To accept a chain, also set its corresponding RPC secret. The endpoint must support finalized transaction and block lookups. For local checkout testing, set the same secret in `billing/.dev.vars` and restart Wrangler.
   ```sh
   npx wrangler deploy --config billing/wrangler.toml
   # Optional, for Ethereum mainnet
   npx wrangler secret put ETH_RPC_URL --config billing/wrangler.toml
   # Optional, for Solana mainnet
   npx wrangler secret put SOL_RPC_URL --config billing/wrangler.toml
   ```
4. Open `https://<billing-host>/admin#deposits` through Cloudflare Access. Enter a public receiving address for either or both networks and select **Save deposit addresses**. Choose a wallet you control that accepts native ETH with transaction data or native SOL with a memo. The merchant keeps the signing keys. Each field can also inherit an optional Wrangler `[vars]` value, `ETH_RECIPIENT` or `SOL_RECIPIENT`. Saving a dashboard address overrides that network immediately. Clearing a saved or inherited address and saving disables that network. **Use Wrangler default** followed by **Save deposit addresses** restores inheritance. Editing one network leaves the other network's source alone.
   In **Plan settings**, set the USD rates per million input and output tokens, the free weekly allowance, the monthly subscription price and the number of Telegram Stars per dollar. Add your support contact and provider cost rates there too.
5. In the gateway's `wrangler.toml`, uncomment the `[[services]]` binding for `BILLING`, including the named `GatewayBilling` entrypoint. Set `BILLING_ENABLED = "true"` and deploy the gateway with its usual command. Set `ALLOWED_TG_USERS = "*"` once to let Telegram users reach the application flow and manage all approvals from the dashboard. An explicit static list is an additional restriction applied before the dashboard whitelist. The gateway needs an Anthropic-compatible `/v1/messages/count_tokens` endpoint for metered access.
6. Register the Telegram webhook with `pre_checkout_query` and `message` included. Using the same bot token, public gateway origin and webhook secret as the main deployment, run
   ```sh
   curl "https://api.telegram.org/bot$TELEGRAM_BOT_TOKEN/setWebhook" \
     -d url="$PUBLIC_ORIGIN/tg/webhook" -d secret_token="$TELEGRAM_WEBHOOK_SECRET" \
     -d 'allowed_updates=["message","callback_query","guest_message","stopped_message_generation","pre_checkout_query"]'
   ```
   Publish the **standalone checkout URL** on your service's website. Customers can see the USD price there before signing in. Inside Telegram, `/topup` buys tokens with Stars. Telegram requires Stars for digital purchases inside Telegram apps. [Telegram digital payments](https://core.telegram.org/bots/payments-stars)

## Opening chat and approving users

In **Admin → Plan settings → Chat access**, select a mode and save. Both an open mode and an approved Telegram account are required for chat, model calls and new purchases. An airdrop, an existing wallet balance or a subscription gives no automatic approval. When upgrading a ledger without an explicit access mode, open access and approve the accounts you want to admit.

| Saved setup | What customers get |
| --- | --- |
| **Closed** (default) | Chat and new purchases are blocked for everyone. Applicants can still request access. Saving an address or approving an account leaves the service closed. |
| **Open paid access** | Requires a configured address and mainnet RPC for at least one network, or explicit **Allow Stars-only billing**. Approved users receive the weekly allowance and can top up or subscribe. |
| **Open free access** | Approved users can chat without a weekly billing limit or wallet deduction. New sales pause. Both receiving addresses can be blank. |

Removing the last configured payment method while paid access is open closes chat. Restoring an address alone leaves it closed until an admin opens it again. Closing chat or selecting free access stops automatic renewal on existing memberships. Customers keep the rest of their paid term and wallet balance. They can subscribe again after the term ends if paid access resumes. Calls already in progress settle under their original terms. Free calls still record usage and provider cost. Suspensions and the gateway's optional daily budget continue to apply.

### Apply and review

- A user sends `/apply` or `/apply Please add me to the workspace` in a **private** chat with the bot. Alternatively, they send `/account`, sign in to the checkout and select **Apply for access**. Telegram supplies the applicant's identity. The optional note helps the admin recognize the request.
- In **Admin → Users → Access applications**, the administrator sees the applicant's Telegram name and username when available, note and application time. Choose **Approve** or **Decline** on that row. Filter the user list to **Whitelist** to see approved accounts. For direct grants, use **Add a user → Add to whitelist** or open an existing account and choose **Add to whitelist**. **Remove from whitelist** takes effect on the next request, including for active sessions, and stops subscription renewals for that account.
- `POST /api/apply` accepts a JSON object with an optional `note` of up to 500 characters, such as `{"note":"I’m on the design team"}`. It uses the secure checkout session issued after `/account` sign-in. The response contains `status` and a confirmation message. Repeated submissions return the existing decision. Customers can check `approval_status` in `GET /api/account` or send `/account` to the bot. A declined or revoked account needs a new administrator decision.

Keep the RPC for a retired chain available while outstanding payments settle. Removing or changing a receiving address stops the checkout from inviting new transfers to it. The ledger can still verify transfers to the original invoice address within its deadline, and can record Stars payments already approved. A temporary RPC or price-feed outage prevents the affected quote from being issued. It leaves the saved charging policy in effect.

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

1. Once your account is approved and paid access is open, send `/account` in a private chat with the bot. Open the merchant's standalone checkout in your browser and sign in with the code within ten minutes.
2. Under **Buy tokens**, enter a USD amount, choose **ETH** or **SOL**, then select **Get payment quote**. The invoice gives you the wallet tokens, exact coin amount, mainnet, recipient, reference and UTC deadline. The checkout checks the merchant's mainnet RPC before issuing the quote.
3. Approve one transfer on the selected mainnet. Leave enough in your wallet to cover its network fee separately.
   - For **ETH**, choose **Pay with Ethereum wallet**. A compatible browser wallet fills in the recipient, exact native ETH and hex transaction data. Approving submits the hash for verification. To pay manually, copy all three fields into an Ethereum mainnet wallet that supports transaction data, then paste the hash into **Verify payment**. [Ethereum JSON-RPC](https://ethereum.org/developers/docs/apis/json-rpc/)
   - For **SOL**, choose **Open Solana wallet**. A Solana Pay wallet fills in the recipient, exact native SOL and invoice memo. A manual transfer needs the same three fields on Solana mainnet. After approving, copy the signature from wallet activity into **Verify payment**. [Solana Pay transfer requests](https://github.com/solana-developers/solana-pay/blob/master/SPEC1.1.md)
4. The transfer must be **included in a block by the 15-minute deadline**. You can submit the hash or signature later. When the invoice shows **Paid**, the purchased tokens are available to chat or buy the monthly plan.

### How the dollar quote works

The checkout uses the last ETH–USD or SOL–USD trade from Coinbase Exchange and accepts a price at most five minutes old. It divides your USD amount by that price, rounding up only to the smallest transferable unit (one wei or one lamport). Both the coin amount and the wallet token amount are fixed on the invoice. The network fee is additional and goes through your wallet. [Coinbase Exchange product ticker](https://docs.cdp.coinbase.com/api-reference/exchange-api/rest-api/products/get-product-ticker)

For example, a **$10** invoice at **$2,500 per ETH** would ask for **0.004 ETH**. At **$200 per SOL**, the same purchase would ask for **0.05 SOL**. Either buys **10 million wallet tokens** at the default $1 per million input tokens. These examples use illustrative exchange rates.

### How long settlement takes

| Network | What the merchant verifies | Typical wait after the transaction lands |
| --- | --- | --- |
| Ethereum mainnet | A successful transfer for the exact amount, destination and invoice data in a canonical block at or below the finalized head | About 15 minutes under normal conditions. [Ethereum finality](https://ethereum.org/roadmap/single-slot-finality/) |
| Solana mainnet | A successful finalized transaction with the invoice memo, a signed native transfer and the corresponding increase in the recipient's balance | Roughly 13–20 seconds after confirmation, based on about 32 slots at 400–600 ms. [Solana confirmation guide](https://solana.com/developers/cookbook/transactions/confirmation) |

Wallet approval, block inclusion, finality and the next service check each take time. Finality can come after the quote expires. After you submit a hash or signature, **Confirming** means the service is checking it in the background for up to 24 hours, even if you close the page. Open the original invoice under **Invoices** to see its status. If a check times out, resubmit the **same transaction ID**. If the transfer landed late or used a different amount or reference, contact the merchant via `/paysupport` with the invoice ID and transaction ID before making another payment.

For Telegram Stars, send `/topup 5` in your private chat to buy $5 of tokens. The default is 50 Stars per dollar, rounded up to a whole Star per invoice. The bot delivers the tokens after Telegram confirms payment. Replayed receipts and refunds apply once. A refund takes back the purchased tokens. If the wallet falls below zero, topping up to cover the debt restores spending. Use `/paysupport` for the merchant's contact. [Telegram payment API](https://core.telegram.org/bots/api#refundstarpayment)

The gateway reserves input and output capacity before each billed call, limits output to the available balance, and settles from reported usage. A failed call returns its hold. An unreported call releases its hold after two hours on the next account check. A late report settles once. Operators can configure provider costs independently from selling prices.

To disconnect the billing integration entirely, disable `BILLING_ENABLED` on the gateway after approved Stars purchases finish. Keep the standalone checkout available for outstanding crypto invoices.

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
