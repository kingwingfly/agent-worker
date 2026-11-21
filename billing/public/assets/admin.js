import {activeSubscription, api, badge, busy, button, byId, element, empty, invoicePager, money, notice, number, subscription, table, usageExample, usd, when} from "./common.js";

const base = "/admin/api";
const views = ["overview", "users", "settings", "deposits", "account"];
let currentUser = "";
let viewVersion = 0;
let userSearchVersion = 0;
let accountVersion = 0;
let applicationVersion = 0, applicationOffset = 0;
let historyVersion = 0;
let userRows = [];
let search = "";
let account;
let historyBefore = null;
let historyAfterId = null;
const adjustments = new Map();
const paymentPages = invoicePager(byId("payment-pages"), page => loadAccount(currentUser, page));
const approvalLabel = {none: "Not on whitelist", pending: "Pending review", approved: "Whitelisted", declined: "Declined", revoked: "Revoked"};

function userPath(id, part = "") {
  return `${base}/users/${encodeURIComponent(id)}${part}`;
}

async function overview(version) {
  const stats = await api(base + "/overview");
  if (version !== viewVersion) return;
  byId("sales-total").textContent = usd(stats.sales_usd);
  byId("user-count").textContent = number(stats.users);
  byId("subscription-count").textContent = number(stats.active_subscriptions);
  byId("token-total").textContent = number(stats.billable_tokens);
  byId("cost-total").textContent = money(stats.cost_usd);
  byId("payment-count").textContent = number(stats.payments) + " settled";
  const coins = {ETH: "Ethereum", SOL: "Solana", XTR: "Telegram Stars"};
  byId("revenues").replaceChildren(...Object.entries(coins).map(([symbol, name]) => {
    const row = element("div");
    row.append(element("dt", name), element("dd", `${stats.revenues[symbol] ?? 0} ${symbol}`));
    return row;
  }));
}

async function listUsers(more = false) {
  const request = ++userSearchVersion;
  if (!more) {
    userRows = [];
    byId("user-list").replaceChildren(empty("Loading users…"));
    byId("more-users").hidden = true;
  }
  const params = new URLSearchParams({q: search, offset: userRows.length, approval: byId("user-search").elements.approval.value});
  const {users, more: hasMore} = await api(base + "/users?" + params);
  if (request !== userSearchVersion || byId("users").hidden) return;
  userRows.push(...users);
  const rows = userRows.map(user => {
    const link = element("a", user.id, "mono");
    link.href = "#account/" + encodeURIComponent(user.id);
    return [link, user.blocked ? badge("Suspended", "alert") : badge(approvalLabel[user.approval_status], user.approval_status === "approved" ? "good" : ""), number(user.wallet_tokens), when(user.created_at)];
  });
  byId("user-list").replaceChildren(rows.length
    ? table(["Telegram ID", "Access", "Wallet tokens", "Joined"], rows)
    : empty(search ? "No users match this ID." : "Users appear here when they first access the service."));
  byId("more-users").hidden = !hasMore;
}

async function listApplications(offset = 0) {
  const version = ++applicationVersion;
  const {applications, more} = await api(base + "/applications?offset=" + offset);
  if (version !== applicationVersion || byId("users").hidden) return;
  if (!applications.length && offset) return listApplications(0);
  applicationOffset = offset;
  const rows = applications.map(row => {
    const applicant = element("a", [row.name, row.username && "@" + row.username].filter(Boolean).join(" · ") || row.user);
    applicant.href = "#account/" + row.user;
    const identity = element("div");
    identity.append(applicant);
    if (row.name || row.username) identity.append(element("p", "Telegram ID " + row.user, "muted mono"));
    const actions = element("div", null, "actions");
    for (const [label, status] of [["Approve","approved"],["Decline","declined"]]) {
      actions.append(button(label, async () => {
        await api(userPath(row.user, "/access"), {status, expected:"pending"});
        await Promise.all([listApplications(applicationOffset), listUsers()]);
      }));
    }
    return [identity, row.note || "—", when(row.applied_at), actions];
  });
  byId("applications").replaceChildren(rows.length ? table(["Applicant", "Note", "Applied", "Decision"], rows)
    : empty("No applications are waiting for review."));
  byId("prev-applications").hidden = !offset;
  byId("next-applications").hidden = !more;
}
byId("refresh-applications").addEventListener("click", e => busy(e.currentTarget, () => listApplications(applicationOffset)));
byId("prev-applications").addEventListener("click", e => busy(e.currentTarget, () => listApplications(Math.max(0, applicationOffset-50))));
byId("next-applications").addEventListener("click", e => busy(e.currentTarget, () => listApplications(applicationOffset+50)));

const plan = byId("plan");
const settingFields = [...plan.querySelectorAll("[name]")];
const modeText = {
  paid: "Paid access is open to whitelisted accounts. Chat uses the weekly allowance, then the wallet.",
  free: "Free access is open to whitelisted accounts. New purchases are paused.",
  closed: "Access is closed. In Plan settings, select paid or free access and save to open chat."
};
async function settings(version) {
  const data = await api(base + "/settings");
  if (version !== viewVersion) return;
  showSettings(data);
  byId("plan-fields").disabled = false;
}

function showSettings(data) {
  for (const input of settingFields) {
    if (input.type === "checkbox") input.checked = Boolean(data[input.name]);
    else input.value = data[input.name] ?? "";
    input.dataset.saved = input.type === "checkbox" ? String(input.checked) : input.value;
  }
  byId("plan-mode").textContent = modeText[data.mode];
  byId("saved-example").textContent = usageExample(data);
  byId("saved-price").textContent = data.free_weekly_tokens > 0
    ? `The saved subscriber allowance is ${number(data.paid_weekly_tokens)} tokens per week (5× free). Renewal costs ${number(data.plan_price_tokens)} wallet tokens per month (${usd(data.monthly_usd)}).`
    : "Set a positive free weekly allowance to offer monthly memberships.";
}

function showDeposits(data) {
  const form = byId("deposit-form");
  byId("deposit-mode").textContent = modeText[data.mode];
  for (const chain of ["ETH", "SOL"]) {
    const prefix = chain.toLowerCase();
    const field = form.elements[prefix + "_recipient"];
    field.defaultValue = field.value = data[field.name];
    delete field.dataset.inherit;
    field.oninput = () => { delete field.dataset.inherit; };
    const network = data.networks[chain];
    byId(prefix + "-status").replaceChildren(network.ready ? badge("Configured", "good") : badge("Off"));
    const details = byId(prefix + "-setup");
    details.replaceChildren();
    if (network.overridden) {
      details.append(element("p", "Saved in the dashboard.", "hint"), button("Use Wrangler default", () => {
        field.value = network.default_address;
        field.dataset.inherit = "true";
        notice("Save deposit addresses to use the Wrangler default for " + chain + ".");
      }));
    } else if (field.value) details.append(element("p", "Using the Wrangler default.", "hint"));
    if (field.value && network.rpc_configured && !network.ready) details.append(element("p", "Enter a valid receiving address for this network.", "hint"));
    if (!network.rpc_configured) {
      const name = chain + "_RPC_URL";
      details.append(element("p", "Configure the mainnet RPC endpoint as a billing Worker secret.", "hint"),
        element("code", `npx wrangler secret put ${name} -c billing/wrangler.toml`));
      if (["127.0.0.1", "localhost", "[::1]"].includes(location.hostname)) {
        details.append(element("p", `For this local server, set ${name} in billing/.dev.vars and restart Wrangler.`, "hint"));
      }
    } else {
      details.append(element("p", chain + "_RPC_URL is configured.", "hint"));
    }
  }
  byId("deposit-fields").disabled = false;
}
byId("deposit-form").addEventListener("submit", event => {
  event.preventDefault();
  const form = event.currentTarget;
  const version = viewVersion;
  const payload = {};
  for (const field of form.querySelectorAll("input[name]")) {
    if (field.dataset.inherit) payload[field.name] = null;
    else if (field.value.trim() !== field.defaultValue) payload[field.name] = field.value.trim();
  }
  busy(event.submitter, async () => {
    const saved = await api(base + "/deposits", payload);
    if (version !== viewVersion) return;
    showDeposits(saved);
    notice("Deposit addresses saved.");
  });
});

plan.addEventListener("submit", event => {
  event.preventDefault();
  const payload = {};
  for (const input of settingFields) {
    if ((input.type === "checkbox" ? String(input.checked) : input.value) === input.dataset.saved) continue;
    if (input.name === "access_mode") payload.expected_access_mode = input.dataset.saved;
    if (input.type === "checkbox") payload[input.name] = input.checked;
    else if (input.name === "monthly_usd") payload[input.name] = input.valueAsNumber.toFixed(2);
    else if (["input_usd", "output_usd"].includes(input.name)) payload[input.name] = input.valueAsNumber.toFixed(6);
    else if (input.type === "number") payload[input.name] = input.valueAsNumber;
    else payload[input.name] = input.value.trim();
  }
  const version = viewVersion;
  busy(event.submitter, async () => {
    const saved = await api(base + "/settings", payload);
    if (version === viewVersion) {
      showSettings(saved);
      notice("Plan settings saved.");
    }
  });
});

function revokeForm(row) {
  const disclosure = element("details");
  disclosure.append(element("summary", "Revoke membership"));
  const form = element("form", null, "inline-form");
  const label = element("label", "Reason");
  const input = element("input");
  input.required = true;
  input.maxLength = 500;
  label.append(input);
  const submit = element("button", "Revoke", "danger");
  form.append(label, submit);
  form.addEventListener("submit", event => {
    event.preventDefault();
    busy(submit, () => adjust({action: "revoke", subscription: row.id, reason: input.value.trim()}, "Membership revoked."));
  });
  disclosure.append(form);
  return disclosure;
}

async function loadAccount(id, page = paymentPages.cursors) {
  const version = ++accountVersion;
  const after = page.at(-1);
  const data = await api(userPath(id) + (after ? "?after=" + encodeURIComponent(after) : ""));
  if (currentUser !== id || version !== accountVersion) return;
  paymentPages.show(page, data.payments_next);
  account = data;
  byId("account-title").textContent = "User " + data.id;
  byId("account-state").replaceChildren(data.blocked ? badge("Suspended", "alert") : badge(approvalLabel[data.approval_status], data.approval_status === "approved" ? "good" : ""));
  const metered = data.access_mode === "paid", closed = data.access_mode === "closed";
  byId("available").textContent = data.blocked ? "Blocked" : closed ? "Closed" : data.approval_status !== "approved" ? "Approval required" : metered ? number(data.available_tokens) : "Unlimited";
  byId("wallet-tokens").textContent = number(data.wallet_tokens);
  byId("weekly-left").textContent = closed ? "Paused" : metered ? number(data.remaining_weekly_tokens) : "Unlimited";
  const tier = data.tier === "subscriber" ? "Subscriber" : "Free";
  const next = data.next_refill ? " · Next refill " + when(data.next_refill) : "";
  byId("user-allowance").textContent = closed ? modeText.closed : metered ? `${tier} · ${number(data.allowance_tokens)} tokens per week${next}` : "Free chat. Wallet tokens are saved.";
  byId("account-tokens").textContent = number(data.billable_tokens);
  byId("account-cost").textContent = money(data.cost_usd);
  byId("joined").textContent = "Joined " + when(data.created_at);
  byId("toggle-user").textContent = data.blocked ? "Restore access" : "Block access";
  byId("whitelist-status").textContent = approvalLabel[data.approval_status];
  byId("toggle-whitelist").textContent = data.approval_status === "approved" ? "Remove from whitelist" : "Add to whitelist";
  const subs = data.subscriptions.map(row => {
    const item = subscription(row);
    item.append(element("p", `${row.source} · ${row.id}`, "muted mono"));
    if (activeSubscription(row) || (!row.revoked && row.starts_at > Date.now())) item.append(revokeForm(row));
    return item;
  });
  byId("subscriptions").replaceChildren(...(subs.length ? subs : [empty("This user has no memberships.")]));
  const payments = data.payments.map(row => {
    const item = element("details");
    item.append(element("summary", "Details"), element("p", "Invoice " + row.id, "mono"));
    const tokens = row.product === "tokens" ? number(row.tokens) : "—";
    const terms = row.product === "tokens" ? "Token top-up" : `Earlier plan · ${number(row.weekly_tokens)} tokens each week for ${number(row.weeks)} weeks.`;
    item.append(element("p", terms));
    if (row.tx) item.append(element("p", row.tx, "mono"));
    return [when(row.created_at), row.usd == null ? "—" : usd(row.usd), tokens, `${row.amount} ${row.chain}`, row.status, item];
  });
  byId("payments").replaceChildren(payments.length
    ? table(["Created", "Price (USD)", "Wallet tokens", "Payment amount", "Status", "Reference"], payments) : empty("This user has no payments."));
  const changes = data.adjustments.map(row => [when(row.created_at), row.action || "—", row.reason || "—", row.tokens ? number(row.tokens) : "—"]);
  byId("adjustments").replaceChildren(changes.length
    ? table(["Date", "Action", "Reason", "Tokens"], changes) : empty("No account adjustments have been recorded."));
  byId("account-body").hidden = false;
}

// Keep the same idempotency key when a response gets lost and the operator retries.
async function adjust(payload, confirmation) {
  const id = currentUser;
  const request = JSON.stringify([id, payload]);
  if (!adjustments.has(request)) adjustments.set(request, crypto.randomUUID());
  await api(userPath(id, "/adjust"), {...payload, key: adjustments.get(request)});
  adjustments.delete(request);
  if (currentUser !== id) return;
  if (payload.action === "airdrop" || payload.action === "grant") byId(payload.action).reset();
  await loadAccount(id);
  if (currentUser === id) notice(confirmation);
}

for (const action of ["airdrop", "grant"]) {
  byId(action).addEventListener("submit", event => {
    event.preventDefault();
    const form = event.currentTarget;
    const data = {action, reason: form.elements.reason.value.trim()};
    for (const input of form.querySelectorAll('input[type="number"]')) data[input.name] = input.valueAsNumber;
    busy(event.submitter, () => adjust(data, action === "airdrop" ? "Tokens granted." : "Membership granted."));
  });
}

byId("toggle-user").addEventListener("click", event => {
  const blocked = !account.blocked;
  busy(event.currentTarget, () => adjust({action: "block", blocked, reason: blocked ? "Blocked access" : "Restored access"}, blocked ? "User access blocked." : "User access restored."));
});
byId("toggle-whitelist").addEventListener("click", event => {
  const id = currentUser, expected = account.approval_status;
  busy(event.currentTarget, async () => {
    await api(userPath(id, "/access"), {status: expected === "approved" ? "revoked" : "approved", expected});
    if (currentUser === id) await loadAccount(id);
  });
});

function fileUrl(path, id) {
  if (!path || !URL.canParse(path, location.origin)) return null;
  const url = new URL(path, location.origin);
  return url.origin === location.origin && url.pathname.startsWith(userPath(id, "/")) ? url.href : null;
}

function fileEntry(file, id) {
  const target = fileUrl(file.url, id);
  if (!target) return null;
  const entry = element("li");
  const link = element("a", file.name);
  link.href = target;
  link.download = "";
  link.setAttribute("aria-label", "Download " + file.name);
  entry.append(link, element("span", number(file.size) + " bytes", "muted"));
  const previewUrl = fileUrl(file.preview_url, id);
  if (previewUrl) {
    const preview = element("div", null, "file-preview");
    preview.hidden = true;
    const toggle = button("Preview", async () => {
      if (preview.hidden && !preview.childElementCount) {
        const result = await api(previewUrl);
        if (currentUser !== id || !entry.isConnected) return;
        preview.append(element("pre", result.text, "transcript"));
        if (result.truncated) preview.append(element("p", "Preview truncated. Download the file for the full contents.", "hint"));
      }
      preview.hidden = !preview.hidden;
      toggle.textContent = preview.hidden ? "Preview" : "Hide preview";
      toggle.setAttribute("aria-expanded", String(!preview.hidden));
    });
    toggle.setAttribute("aria-expanded", "false");
    toggle.setAttribute("aria-label", "Toggle preview of " + file.name);
    entry.append(toggle, preview);
  }
  return entry;
}

function fileList(id, endpoint, filters, label, noFiles, disclosure) {
  const group = element("div");
  const list = element("ul", null, "file-list");
  let cursor = null;
  const status = element("p", "", "hint");
  status.setAttribute("role", "status");
  const load = element("button", label, "secondary");
  load.type = "button";
  async function fetchPage() {
    if (load.disabled) return;
    load.disabled = true;
    status.textContent = "Loading files…";
    try {
      const params = new URLSearchParams(filters);
      if (cursor) params.set("cursor", cursor);
      const page = await api(userPath(id, endpoint) + "?" + params);
      if (currentUser !== id || !group.isConnected) return;
      for (const file of page.files) {
        const entry = fileEntry(file, id);
        if (entry) list.append(entry);
      }
      cursor = page.next;
      load.textContent = "Load more files";
      load.hidden = !cursor;
      status.textContent = list.childElementCount || cursor ? "" : noFiles;
    } catch (error) {
      status.textContent = error.message || "The files could not be listed.";
      load.textContent = "Retry file list";
    } finally {
      load.disabled = false;
    }
  }
  load.addEventListener("click", fetchPage);
  group.append(list, status, load);
  if (disclosure) {
    // Load metadata for just the turns the operator opens. File contents need a separate click.
    function loadOnOpen() {
      if (!disclosure.open) return;
      disclosure.removeEventListener("toggle", loadOnOpen);
      fetchPage();
    }
    disclosure.addEventListener("toggle", loadOnOpen);
  }
  return group;
}

async function loadHistory(more = false) {
  const version = ++historyVersion;
  const id = currentUser;
  const params = new URLSearchParams();
  if (more && historyBefore != null) {
    params.set("before", historyBefore);
    if (historyAfterId != null) params.set("after_id", historyAfterId);
  }
  const suffix = params.size ? "?" + params : "";
  const page = await api(userPath(id, "/history" + suffix));
  if (id !== currentUser || version !== historyVersion) return;
  const history = byId("history");
  if (!more) history.replaceChildren();
  for (const turn of page.turns) {
    const item = element("details", null, "entry");
    const heading = element("summary");
    const snippet = turn.prompt?.replace(/\s+/g, " ").trim() || "Chat turn";
    heading.append(element("span", snippet.length > 120 ? snippet.slice(0, 120) + "…" : snippet),
      element("small", when(turn.created_at) + (turn.is_error ? " · Failed" : ""), "muted"));
    item.append(heading);
    item.append(element("p", turn.session, "mono muted"), element("h3", "User"), element("p", turn.prompt || "No text recorded.", "transcript"));
    item.append(element("h3", "Agent"), element("p", turn.answer || "No answer recorded.", "transcript"), fileList(id, "/files", {turn: turn.id}, "View files", "This turn has no saved output files.", item));
    item.open = !more && !history.childElementCount;
    history.append(item);
  }
  if (!history.childElementCount) history.append(empty("No chat turns have been recorded for this user. Completed conversations appear here while billing is enabled. Older private chat saves are available under Private chat storage."));
  historyBefore = page.next;
  historyAfterId = page.next_id;
  byId("more-history").hidden = historyBefore == null;
  byId("open-history").textContent = "Refresh history";
}

const cleanupDate = byId("cleanup").elements.date;
function setCleanupTime() {
  cleanupDate.max = new Date().toISOString().slice(0, 19);
  if (!cleanupDate.value) cleanupDate.value = cleanupDate.max;
}
byId("account").querySelector(".cleanup").addEventListener("toggle", event => {
  if (event.currentTarget.open) setCleanupTime();
});
cleanupDate.addEventListener("focus", setCleanupTime);

byId("cleanup").addEventListener("submit", event => {
  event.preventDefault();
  const id = currentUser;
  const form = event.currentTarget;
  const before = Date.parse(form.elements.date.value + "Z");
  const confirm = form.elements.confirm.value.trim();
  if (confirm !== id) return notice("Enter this user's Telegram ID to confirm deletion.", true);
  busy(event.submitter, async () => {
    let count = 0;
    let more = true;
    while (more) {
      const result = await api(userPath(id, "/cleanup"), {before, confirm});
      count += result.deleted;
      more = result.more;
      if (id === currentUser && more) notice(`Cleanup in progress. Removed ${number(count)} records.`);
    }
    if (currentUser !== id) return;
    form.reset();
    await loadHistory();
    if (currentUser === id) notice(`Cleanup complete. Removed ${number(count)} records.`);
  });
});

async function navigate() {
  const version = ++viewVersion;
  const [name, id] = location.hash.slice(1).split("/");
  const selected = name === "account" && /^\d+$/.test(id || "") ? "account" : ["users", "settings", "deposits"].includes(name) ? name : "overview";
  currentUser = selected === "account" ? id : "";
  if (version > 1) {
    window.scrollTo(0, 0);
    byId("main").focus({preventScroll: true});
  }
  for (const view of views) byId(view).hidden = view !== selected;
  for (const link of document.querySelectorAll(".tabs a")) {
    if (link.hash === "#" + (selected === "account" ? "users" : selected)) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  }
  notice();
  try {
    if (selected === "overview") await overview(version);
    if (selected === "users") await Promise.all([listUsers(), listApplications()]);
    if (selected === "settings") {
      byId("plan-fields").disabled = true;
      await settings(version);
    }
    if (selected === "deposits") {
      byId("deposit-fields").disabled = true;
      const data = await api(base + "/deposits");
      if (version === viewVersion) showDeposits(data);
    }
    if (selected === "account") {
      paymentPages.show([], null);
      byId("account-title").textContent = "User " + id;
      byId("account-body").hidden = true;
      byId("account-state").replaceChildren();
      byId("history").replaceChildren(empty("Loading chat history…"));
      byId("more-history").hidden = true;
      byId("open-history").textContent = "Refresh history";
      historyBefore = null;
      historyAfterId = null;
      byId("private-archives").replaceChildren(fileList(id, "/storage", {kind: "checkpoints"}, "List archives", "No private chat archives are stored."));
      byId("private-files").replaceChildren(fileList(id, "/storage", {kind: "artifacts"}, "List output files", "No private output files are stored."));
      for (const form of byId("account").querySelectorAll("form")) form.reset();
      byId("account").querySelector(".cleanup").open = false;
      await Promise.all([loadAccount(id), loadHistory().catch(error => {
        if (version === viewVersion) byId("history").replaceChildren(empty(error.message + " Use Refresh history to try again."));
      })]);
    }
  } catch (error) {
    if (version === viewVersion) notice(error.message, true);
  }
}

byId("refresh").addEventListener("click", event => busy(event.currentTarget, () => overview(viewVersion)));
byId("add-user").addEventListener("submit", event => {
  event.preventDefault();
  const form = event.currentTarget;
  const version = viewVersion;
  busy(event.submitter, async () => {
    const user = await api(base + "/users", {id: form.elements.id.value.trim(), approve: form.elements.approve.checked});
    form.reset();
    if (version === viewVersion) location.hash = "#account/" + encodeURIComponent(user.id);
  });
});
byId("more-users").addEventListener("click", event => busy(event.currentTarget, () => listUsers(true)));
byId("user-filter").addEventListener("change", () => listUsers().catch(err => notice(err.message, true)));
byId("user-search").addEventListener("submit", event => {
  event.preventDefault();
  search = event.currentTarget.elements.q.value.trim();
  busy(event.submitter, () => listUsers());
});
byId("open-history").addEventListener("click", event => busy(event.currentTarget, () => loadHistory()));
byId("more-history").addEventListener("click", event => busy(event.currentTarget, () => loadHistory(true)));
document.querySelector(".skip").addEventListener("click", event => {
  event.preventDefault();
  byId("main").focus();
});
window.addEventListener("hashchange", navigate);
navigate();
