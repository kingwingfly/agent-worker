import {activeSubscription, api, badge, busy, button, byId, element, empty, money, notice, number, subscription, table, usageExample, usd, when} from "./common.js";

const networks = {ETH: "Ethereum", SOL: "Solana"};
let identityVersion = 0;
let visibleInvoice;
let salesPaused = false;
let refreshTimer;
let accountVersion = 0;
let current;
let review;
let subscribing = false;
const transfers = new Map();
const signing = new Set();

// An empty string means the wallet was opened but a transaction ID is not yet known.
// Keep that state across reloads so returning customers verify their original transfer.
function savedTransfer(key) {
  try {
    return localStorage.getItem(key) ?? transfers.get(key) ?? null;
  } catch {
    return transfers.get(key) ?? null;
  }
}

function saveTransfer(key, tx) {
  if (tx == null) transfers.delete(key);
  else transfers.set(key, tx);
  try {
    if (tx == null) localStorage.removeItem(key);
    else localStorage.setItem(key, tx);
  } catch {
    // Browsers with storage disabled can still recover a transfer in the current page.
  }
}

const transferKey = (user, invoice) => "agent-worker:transfer:" + user + ":" + invoice;

function signedOut() {
  const wasSignedIn = byId("login-screen").hidden;
  identityVersion++;
  clearTimeout(refreshTimer);
  visibleInvoice = null;
  current = null;
  review = null;
  transfers.clear();
  byId("login-screen").hidden = false;
  byId("customer-account").hidden = true;
  byId("logout").hidden = true;
  byId("invoice-detail").hidden = true;
  for (const id of ["user-id", "balance", "wallet-balance", "invoices", "my-subscriptions", "invoice-content", "chain-options"]) {
    byId(id).replaceChildren();
  }
  if (wasSignedIn) publicPricing();
}

async function customerApi(path, body) {
  const version = identityVersion;
  try {
    return await api(path, body);
  } catch (error) {
    if (error.status === 401 && version === identityVersion) signedOut();
    throw error;
  }
}

function rateList(plan) {
  const list = element("dl", null, "rates");
  for (const [name, rate] of [["Input", plan.input_usd], ["Output", plan.output_usd]]) {
    const row = element("div");
    row.append(element("dt", name + " per 1M tokens"), element("dd", money(rate) + " USD"));
    list.append(row);
  }
  return list;
}

async function publicPricing() {
  const offers = byId("public-offers");
  const retry = byId("retry-pricing");
  try {
    const {plan, chains} = await api("/api/pricing");
    retry.hidden = true;
    if (!plan.require_payment) {
      offers.replaceChildren(element("h2", "Free access"), element("p", "Chat is free and new purchases are paused."));
      return;
    }
    const ways = chains.filter(chain => Object.hasOwn(networks, chain));
    const membership = plan.free_weekly_tokens > 0
      ? `Free accounts receive ${number(plan.free_weekly_tokens)} tokens per week. ${usd(plan.monthly_usd)} per month raises that to ${number(plan.paid_weekly_tokens)} (5× free).`
      : "Buy wallet tokens to use the service.";
    const methods = new Intl.ListFormat("en", { type: "disjunction" }).format([...ways, "Telegram Stars"]);
    const payment = ways.length ? "Pay with " + methods + "." : "Buy tokens with Telegram Stars in your private chat using /topup.";
    offers.replaceChildren(element("h2", "Pay as you go"), rateList(plan), element("p", usageExample(plan), "hint"),
      element("p", membership, "offer-plan"), element("p", payment, "hint"));
  } catch {
    offers.replaceChildren(element("h2", "Current prices"), element("p", "Pricing is temporarily unavailable. You can still sign in.", "muted"));
    retry.hidden = false;
  }
}
byId("retry-pricing").addEventListener("click", event => busy(event.currentTarget, publicPricing));

function amountEntered() {
  const input = byId("quote-form").elements.usd;
  return input.validity.valid ? input.valueAsNumber.toFixed(2) : null;
}

// Apply the same whole-token floor as the server, including at an exact decimal boundary.
function walletTokens(dollars, inputRate) {
  const [whole, fraction = ""] = inputRate.split(".");
  const cents = BigInt(dollars.replace(".", ""));
  return cents * 10_000n * 10n ** BigInt(fraction.length) / BigInt(whole + fraction);
}

function updateQuoteTotal() {
  if (!current) return;
  const amount = amountEntered();
  const chain = byId("chain-options").querySelector(":checked")?.value;
  const stars = chain === "XTR";
  const total = amount ? walletTokens(amount, current.plan.input_usd) : 0n;
  byId("quote-total").textContent = amount ? `${usd(amount)} buys ${number(total)} wallet tokens.` : "Enter $0.01 to $10,000.00.";
  byId("new-invoice").hidden = stars;
  byId("new-invoice").disabled = !amount || !total || salesPaused;
  const help = byId("stars-help");
  help.hidden = !stars;
  if (stars) {
    help.replaceChildren();
    if (amount && total) {
      const cost = Math.ceil(Number(amount.replace(".", "")) * current.plan.stars_per_usd / 100);
      if (cost > 10_000) help.textContent = "Telegram accepts up to 10,000 Stars per invoice. Enter a smaller amount or choose another payment method.";
      else help.append("Send ", element("code", "/topup " + amount), ` in your private chat with the bot to pay ${number(cost)} Stars.`);
    } else help.textContent = "Enter an amount that buys at least one whole token.";
  }
}

function setChains(chains) {
  const options = byId("chain-options");
  const previous = options.querySelector(":checked")?.value;
  options.replaceChildren();
  const choices = [...chains.filter(chain => Object.hasOwn(networks, chain)), "XTR"];
  for (const chain of choices) {
    const label = element("label", null, "network");
    const choice = element("input");
    choice.type = "radio";
    choice.name = "chain";
    choice.value = chain;
    choice.checked = chain === (choices.includes(previous) ? previous : choices[0]);
    label.append(choice, element("span", chain === "XTR" ? "Telegram Stars" : chain));
    options.append(label);
  }
  updateQuoteTotal();
}

function storageKey(user) {
  return "agent-worker:monthly-membership:" + user;
}

function savedPurchase(user) {
  try {
    const saved = JSON.parse(sessionStorage.getItem(storageKey(user)));
    if (saved?.user === user && saved.attempted && typeof saved.key === "string" && typeof saved.offer === "string") return saved;
  } catch {
    // Browsers with session storage disabled can still retry in this open page.
  }
  return null;
}

function rememberPurchase(intent) {
  try { sessionStorage.setItem(storageKey(intent.user), JSON.stringify(intent)); } catch {}
}

function forgetPurchase(intent) {
  try { sessionStorage.removeItem(storageKey(intent.user)); } catch {}
  if (review?.key === intent.key) review = null;
}

function monthlyMember() {
  return current?.subscriptions.find(row => row.kind === "monthly" && activeSubscription(row));
}

function showPlan() {
  if (!current) return;
  const {plan} = current;
  const active = monthlyMember();
  const member = byId("current-membership");
  member.hidden = !active;
  byId("membership-offer").hidden = Boolean(active);
  if (active) {
    if (review) forgetPurchase(review);
    const card = subscription(active);
    if (active.auto_renew) {
      card.append(button("Cancel at period end", async () => {
        const version = identityVersion;
        await customerApi("/api/cancel", {id: active.id});
        if (version !== identityVersion) return;
        await account();
        if (version === identityVersion) notice("Renewal canceled. Your membership runs through " + when(active.ends_at) + ".");
      }));
    }
    member.replaceChildren(card);
    return;
  }
  const eligible = plan.free_weekly_tokens > 0 && !salesPaused;
  const missing = Math.max(0, plan.plan_price_tokens - current.wallet_tokens);
  byId("plan-price").textContent = eligible ? usd(plan.monthly_usd) + " per month" : "Currently unavailable";
  byId("plan-title").textContent = eligible
    ? `${number(plan.paid_weekly_tokens)} tokens each week (5× free). Renews from your wallet at ${number(plan.plan_price_tokens)} tokens per month.` : "";
  byId("wallet-shortfall").hidden = !eligible || !missing || Boolean(review);
  byId("wallet-shortfall").textContent = `Add ${number(missing)} more wallet tokens to subscribe.`;
  byId("review-plan").hidden = !eligible || Boolean(review) || missing > 0;
  byId("plan-review").hidden = !eligible || !review;
  if (!review || !eligible) return;
  byId("confirm-heading").textContent = review.attempted ? "Pending subscription" : "Confirm monthly subscription";
  byId("confirm-terms").textContent = `Spend ${number(review.price)} wallet tokens (${usd(review.usd)}) now for ${number(review.weekly)} tokens each week. Your wallet renews this membership monthly for the same token amount until you cancel.`;
  byId("pending-purchase").hidden = !review.attempted;
  byId("pending-purchase").textContent = "Retrying checks your last request and completes it once.";
  byId("confirm-plan").textContent = subscribing ? "Processing…" : review.attempted ? "Retry subscription" : "Confirm subscription";
  byId("confirm-plan").disabled = subscribing;
  byId("dismiss-review").hidden = review.attempted;
}

function displayAccount(data) {
  if (current && current.id !== data.id) {
    review = null;
    visibleInvoice = null;
    byId("invoice-detail").hidden = true;
    transfers.clear();
  }
  current = data;
  const {plan} = data;
  if (!review) review = savedPurchase(data.id);
  if (review && !review.attempted && (review.offer !== plan.offer || data.wallet_tokens < review.price)) {
    review = null;
    notice("Your plan terms or wallet balance changed. Review the current price before purchasing.");
  }
  byId("login-screen").hidden = true;
  byId("customer-account").hidden = false;
  byId("logout").hidden = false;
  byId("wallet-balance").textContent = number(data.wallet_tokens);
  byId("balance").textContent = number(data.available_tokens);
  byId("tier-label").textContent = (data.tier === "subscriber" ? "Subscriber" : "Free") + " weekly tokens";
  byId("weekly-balance").textContent = `${number(data.remaining_weekly_tokens)} of ${number(data.allowance_tokens)} left`;
  byId("next-refill").textContent = data.next_refill ? "Refills " + when(data.next_refill) : "";
  byId("user-id").textContent = "Telegram ID " + data.id;
  byId("support").textContent = plan.support ? "Payment support · " + plan.support : "";
  const active = monthlyMember(), previous = data.subscriptions.filter(row => row.id !== active?.id);
  byId("past-memberships").hidden = !previous.length;
  byId("my-subscriptions").replaceChildren(...previous.map(subscription));
  salesPaused = plan.require_payment === false || data.blocked;
  byId("top-up").hidden = salesPaused;
  byId("membership").hidden = salesPaused && !active;
  byId("buy-options").hidden = salesPaused && !active;
  byId("access-message").hidden = !salesPaused;
  byId("access-message").textContent = data.blocked
    ? "Account access is blocked. Contact payment support for help."
    : salesPaused ? "Chat is free and new purchases are paused." : "";
  byId("wallet-rates").replaceChildren(rateList(plan), element("p", usageExample(plan), "hint"));
  setChains(data.chains);
  showPlan();
  const invoices = data.invoices.map(invoice => {
    if (settled(invoice)) saveTransfer(transferKey(data.id, invoice.id), null);
    const expired = ["open", "expired"].includes(invoice.status) && invoice.expires_at <= Date.now();
    const status = settled(invoice) ? badge("Paid", "good")
      : invoice.status === "confirming" ? badge("Confirming")
      : invoice.note ? badge("Check transfer", "alert") : expired ? badge("Quote expired") : badge(invoice.status);
    const details = button("View invoice", () => showInvoice(invoice));
    const price = invoice.usd == null ? `${invoice.amount} ${invoice.chain}` : usd(invoice.usd);
    const currency = invoice.chain === "XTR" ? "Telegram Stars (XTR)" : invoice.chain;
    return [when(invoice.created_at), price, currency, invoice.product === "tokens" ? number(invoice.tokens) + " tokens" : "Earlier plan", status, details];
  });
  byId("invoices").replaceChildren(invoices.length
    ? table(["Created", "Price", "Currency", "Purchase", "Status", "Invoice"], invoices)
    : empty("Your payment quotes and receipts will appear here after you start a top-up."));
}

async function account() {
  const identity = identityVersion, version = ++accountVersion;
  const data = await customerApi("/api/account");
  if (identity !== identityVersion || version !== accountVersion) return;
  displayAccount(data);
  clearTimeout(refreshTimer);
  if (data.invoices.some(i => i.status === "confirming")) {
    refreshTimer = setTimeout(() => account().catch(error => notice(error.message, true)), 15000);
  }
  const selected = data.invoices.find(i => i.id === visibleInvoice);
  if (selected && (settled(selected) || selected.note || selected.status === "confirming" || salesPaused)) showInvoice(selected, false);
}

byId("quote-form").elements.usd.addEventListener("input", updateQuoteTotal);
byId("chain-options").addEventListener("change", updateQuoteTotal);
byId("refresh-account").addEventListener("click", event => busy(event.currentTarget, account));
byId("review-plan").addEventListener("click", () => {
  if (salesPaused || !current || subscribing || review) return;
  const {plan} = current;
  if (monthlyMember() || !plan.free_weekly_tokens || current.wallet_tokens < plan.plan_price_tokens) return;
  review = {user: current.id, key: crypto.randomUUID(), offer: plan.offer,
    price: plan.plan_price_tokens, usd: plan.monthly_usd, weekly: plan.paid_weekly_tokens, attempted: false};
  showPlan();
  byId("plan-review").focus();
});
byId("dismiss-review").addEventListener("click", () => {
  if (subscribing || review?.attempted) return;
  review = null;
  showPlan();
  byId("review-plan").focus();
});

async function confirmPlan() {
  if (!review || review.user !== current?.id || subscribing || salesPaused) return;
  const intent = review;
  const identity = identityVersion;
  intent.attempted = true;
  rememberPurchase(intent);
  subscribing = true;
  showPlan();
  notice();
  try {
    const result = await customerApi("/api/subscribe", {key: intent.key, offer: intent.offer});
    if (result.status !== "active") throw new Error("Purchase confirmation is pending.");
  } catch (error) {
    if (identity !== identityVersion) return;
    // A definitive refusal releases the quote. A lost response keeps the key even across a reload.
    if ([400, 402, 403, 409, 422].includes(error.status)) {
      forgetPurchase(intent);
      await account().catch(() => {});
      if (identity === identityVersion) notice(error.message, true);
    } else {
      notice(error.message + " Retry this purchase to resolve its status.", true);
    }
    return;
  } finally {
    subscribing = false;
    if (identity === identityVersion) showPlan();
  }
  forgetPurchase(intent);
  if (identity !== identityVersion) return;
  showPlan();
  try {
    await account();
    if (identity === identityVersion) notice("Your monthly membership is active. Your weekly tokens are available.");
  } catch {
    if (identity === identityVersion) notice("Your monthly membership is active. Refresh to see your updated balance.");
  }
}

byId("confirm-plan").addEventListener("click", confirmPlan);

function settled(invoice) {
  return invoice.status === "paid";
}

function field(list, name, value, mono = false) {
  list.append(element("dt", name), element("dd", value, mono ? "mono" : ""));
}

function wei(decimal) {
  const match = /^(\d+)(?:\.(\d{1,18}))?$/.exec(decimal);
  if (!match) throw new Error("The invoice contains an invalid ETH amount.");
  return BigInt(match[1] + (match[2] || "").padEnd(18, "0"));
}

async function payEth(invoice, beforeSend) {
  const provider = window.ethereum;
  const accounts = await provider.request({method: "eth_requestAccounts"});
  if (!accounts?.[0]) throw new Error("Select an account in your wallet to continue.");
  if (BigInt(await provider.request({method: "eth_chainId"})) !== 1n) {
    throw new Error("Switch your wallet to Ethereum mainnet and try again.");
  }
  if (invoice.expires_at <= Date.now()) throw new Error("This quote expired. Create a new invoice before sending a transfer.");
  if (!/^0x[a-fA-F0-9]{40}$/.test(invoice.recipient) || !/^0x(?:[a-fA-F0-9]{2})+$/.test(invoice.reference)) {
    throw new Error("The invoice contains an invalid Ethereum address or transfer reference.");
  }
  const tx = {from: accounts[0], to: invoice.recipient, value: "0x" + wei(invoice.amount).toString(16), data: invoice.reference, chainId: "0x1"};
  beforeSend();
  return provider.request({method: "eth_sendTransaction", params: [tx]});
}

function showInvoice(invoice, focus = true) {
  const content = byId("invoice-content");
  const panel = byId("invoice-detail");
  visibleInvoice = invoice.id;
  content.replaceChildren();
  panel.hidden = false;
  if (focus) panel.focus();
  const crypto = Object.hasOwn(networks, invoice.chain);
  const isEth = invoice.chain === "ETH";
  const key = transferKey(current.id, invoice.id);
  const attempt = savedTransfer(key);
  const topUp = invoice.product === "tokens";
  const price = invoice.usd == null ? `${invoice.amount} ${invoice.chain}` : usd(invoice.usd) + " USD";
  const title = element("h3", price);
  const terms = topUp ? number(invoice.tokens) + " wallet tokens"
    : `Earlier plan · ${number(invoice.weekly_tokens)} tokens per week for ${number(invoice.weeks)} ${invoice.weeks === 1 ? "week" : "weeks"}.`;
  content.append(title, element("p", terms, "muted"));
  const details = element("dl", null, "invoice-data");
  field(details, "Exact payment amount", `${invoice.amount} ${invoice.chain}`, true);
  if (invoice.rate_usd != null) field(details, "Quoted exchange rate", `${money(invoice.rate_usd)} per ${invoice.chain}`);
  field(details, "Invoice ID", invoice.id, true);
  if (settled(invoice) || !crypto) {
    field(details, "Status", invoice.status);
    if (invoice.tx) field(details, "Transaction", invoice.tx, true);
    content.append(details);
    if (!crypto && invoice.status === "open") content.append(element("p", "Complete this Telegram Stars payment in your private chat with the bot."));
    return;
  }

  const expired = invoice.expires_at <= Date.now();
  field(details, "Network", networks[invoice.chain] + " mainnet");
  field(details, "Recipient address", invoice.recipient, true);
  field(details, isEth ? "Transaction data (hex)" : "Transfer memo", invoice.reference, true);
  field(details, "Quote expires", when(invoice.expires_at));
  content.append(details);
  const message = invoice.note || (invoice.status === "confirming"
    ? "Transaction ID submitted. Verification continues after you close checkout."
    : salesPaused
    ? "New sales are paused. You can still verify a transfer sent within this invoice’s quote window."
    : expired
    ? "The transfer window has closed. You can still verify a transfer made within the quote window."
    : attempt !== null || invoice.tx
    ? "Check your wallet activity and verify the transfer you already sent using its transaction ID below."
    : `Send the exact native ${invoice.chain} amount to this address on ${networks[invoice.chain]} mainnet before the quote expires. Include the ${isEth ? "hex transaction data" : "memo"} exactly as shown and pay the network fee separately.`);
  content.append(element("p", message));
  const actions = element("div", null, "transfer-actions");
  content.append(actions);
  const form = element("form", null, "claim stack");
  const label = element("label", isEth ? "Transaction hash" : "Transaction signature");
  const input = element("input");
  input.type = "text";
  input.required = true;
  input.autocomplete = "off";
  input.spellcheck = false;
  input.setAttribute("autocapitalize", "none");
  input.value = attempt || invoice.tx || "";
  label.append(input);
  const verify = element("button", "Verify payment");
  const help = isEth
    ? "The Ethereum wallet button submits the hash for you. For a manual transfer, paste its hash here."
    : "After approving the transfer in your Solana wallet, paste its signature here.";
  form.append(label, element("p", help, "hint"), verify);
  content.append(form);

  const canPay = !salesPaused && !expired && !invoice.tx && invoice.status === "open";
  if (canPay && attempt === "" && !signing.has(key)) {
    actions.append(button("I haven’t sent a transfer", () => {
      saveTransfer(key, null);
      showInvoice(invoice, false);
    }));
  }
  if (canPay && attempt === null && isEth && typeof window.ethereum?.request === "function") {
    const wallet = button("Pay with Ethereum wallet", async () => {
      const version = identityVersion;
      let hash;
      try {
        hash = await payEth(invoice, () => {
          if (version !== identityVersion || visibleInvoice !== invoice.id) throw new Error("Reopen this invoice to pay.");
          if (savedTransfer(key) !== null) throw new Error("Check your wallet activity for the existing transfer.");
          signing.add(key);
          saveTransfer(key, "");
        });
        saveTransfer(key, hash);
      } catch (error) {
        if (error.code === 4001 && signing.has(key)) saveTransfer(key, null);
        throw error;
      } finally {
        signing.delete(key);
        if (version === identityVersion && visibleInvoice === invoice.id) showInvoice(invoice, false);
      }
      if (version === identityVersion) await claimPayment(invoice, hash, version);
    }, "button");
    actions.append(wallet);
  }
  if (canPay && attempt === null && !isEth && /^[1-9A-HJ-NP-Za-km-z]{32,44}$/.test(invoice.recipient)) {
    const pay = element("a", "Open Solana wallet", "button");
    const params = new URLSearchParams({amount: invoice.amount, memo: invoice.reference});
    pay.href = `solana:${invoice.recipient}?${params}`;
    pay.addEventListener("click", event => {
      if (invoice.expires_at <= Date.now()) {
        event.preventDefault();
        notice("This quote expired. Create a new invoice before sending a transfer.", true);
        return;
      }
      if (savedTransfer(key) !== null) event.preventDefault();
      else saveTransfer(key, "");
      // Let the browser hand the link to the wallet before updating this panel.
      setTimeout(() => { if (visibleInvoice === invoice.id) showInvoice(invoice, false); }, 0);
    });
    actions.append(pay);
  }
  form.addEventListener("submit", event => {
    event.preventDefault();
    const tx = input.value.trim();
    const version = identityVersion;
    saveTransfer(key, tx);
    busy(verify, () => claimPayment(invoice, tx, version));
  });
}

async function claimPayment(invoice, tx, version) {
  const {id} = invoice;
  const topUp = invoice.product === "tokens";
  const reply = await customerApi("/api/claim", {id, tx});
  if (version !== identityVersion) return;
  if (reply.status !== "paid") {
    await account();
    if (version === identityVersion) notice("Transaction ID submitted. We’ll keep checking for network confirmation, even if you close checkout.");
    return;
  }
  if (visibleInvoice === id) {
    visibleInvoice = null;
    byId("invoice-detail").hidden = true;
  }
  saveTransfer(transferKey(current.id, id), null);
  await account();
  if (version === identityVersion) notice(topUp ? `Payment confirmed. ${number(invoice.tokens)} tokens have been added to your wallet.` : "Payment confirmed. Your plan is active.");
}

byId("login").addEventListener("submit", event => {
  event.preventDefault();
  const form = event.currentTarget;
  busy(event.submitter, async () => {
    await api("/api/login", {code: form.elements.code.value.trim()});
    identityVersion++;
    form.reset();
    await account();
  });
});
byId("logout").addEventListener("click", event => busy(event.currentTarget, async () => {
  await customerApi("/api/logout", {});
  signedOut();
  byId("login").elements.code.focus();
}));
byId("quote-form").addEventListener("submit", event => {
  event.preventDefault();
  const amount = amountEntered();
  busy(event.submitter, async () => {
    const chain = byId("chain-options").querySelector(":checked")?.value;
    if (!Object.hasOwn(networks, chain) || !current || salesPaused || !amount || !walletTokens(amount, current.plan.input_usd)) return;
    const version = identityVersion;
    const invoice = await customerApi("/api/invoice", {chain, usd: amount});
    if (version !== identityVersion) return;
    showInvoice(invoice);
    await account();
  });
});
byId("close-invoice").addEventListener("click", () => {
  byId("invoice-detail").hidden = true;
  visibleInvoice = null;
  if (byId("new-invoice").hidden || salesPaused) byId("refresh-account").focus();
  else byId("new-invoice").focus();
});
publicPricing();
account().catch(error => {
  if (error.status !== 401) notice(error.message, true);
});
