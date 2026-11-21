export const byId = (id) => document.getElementById(id);

export function element(tag, text, className) {
  const node = document.createElement(tag);
  if (text != null) node.textContent = text;
  if (className) node.className = className;
  return node;
}

export function notice(message = "", error = false) {
  const box = byId("notice");
  box.textContent = message;
  box.classList.toggle("error", error);
  box.hidden = !message;
}

export async function api(path, body) {
  const options = {credentials: "same-origin", cache: "no-store", headers: {Accept: "application/json"}};
  if (body !== undefined) {
    options.method = "POST";
    options.headers["Content-Type"] = "application/json";
    options.body = JSON.stringify(body);
  }
  const reply = await fetch(path, options);
  const text = await reply.text();
  let value;
  try {
    value = JSON.parse(text);
  } catch {
    value = null;
  }
  if (!reply.ok) {
    const detail = value?.error?.message || value?.message || value?.error;
    const message = typeof detail === "string" ? detail : (!value && !text.startsWith("<") && text.length < 300 ? text : "");
    const error = new Error(message || `The request failed (HTTP ${reply.status}).`);
    error.status = reply.status;
    throw error;
  }
  if (!value && text) throw new Error("The service returned an unreadable response.");
  return value;
}

// Disable only the submitting control. Long-running requests keep the rest of the page usable.
export async function busy(control, task) {
  if (control?.disabled) return;
  if (control) control.disabled = true;
  notice();
  try {
    await task();
  } catch (error) {
    notice(error.message || "The service could not be reached. Try again.", true);
  } finally {
    if (control) control.disabled = false;
  }
}

const quantity = new Intl.NumberFormat(undefined, {maximumFractionDigits: 6});
const dollars = new Intl.NumberFormat(undefined, {style: "currency", currency: "USD", maximumFractionDigits: 6});
const prices = new Intl.NumberFormat(undefined, {style: "currency", currency: "USD"});
export const usd = (value) => prices.format(value);
const utc = new Intl.DateTimeFormat(undefined, {
  dateStyle: "medium", timeStyle: "short", timeZone: "UTC"
});
export const number = (value) => quantity.format(value ?? 0);
export const money = (value) => dollars.format(value ?? 0);
export const when = (value) => value ? utc.format(new Date(value)) + " UTC" : "—";

export function empty(text) {
  return element("p", text, "empty");
}

export function badge(text, tone = "") {
  return element("span", text, "badge " + tone);
}

export function button(text, onClick, className = "secondary") {
  const node = element("button", text, className);
  node.type = "button";
  node.addEventListener("click", () => busy(node, onClick));
  return node;
}

// Commit a cursor only after its page loads. Refreshes then keep the operator or customer on that page.
export function invoicePager(nav, load) {
  let cursors = [], next = null;
  const newer = button("Newer invoices", () => load(cursors.slice(0, -1)));
  const older = button("Older invoices", () => load([...cursors, next]));
  nav.append(newer, older);
  return {
    get cursors() { return cursors; },
    show(page, following) {
      cursors = page;
      next = following;
      newer.hidden = !page.length;
      older.hidden = !following;
      nav.hidden = newer.hidden && older.hidden;
    }
  };
}

export function table(headers, rows) {
  const wrap = element("div", null, "table-wrap");
  const grid = element("table");
  const tr = grid.createTHead().insertRow();
  for (const heading of headers) {
    const th = element("th", heading);
    th.scope = "col";
    tr.append(th);
  }
  const tbody = grid.createTBody();
  for (const row of rows) {
    const line = tbody.insertRow();
    for (const value of row) {
      const cell = line.insertCell();
      if (value instanceof Node) cell.append(value);
      else cell.textContent = value ?? "—";
    }
  }
  wrap.append(grid);
  return wrap;
}

export function usageExample(plan) {
  const inputRate = Math.round(Number(plan.input_usd) * 1_000_000);
  const outputRate = Math.round(Number(plan.output_usd) * 1_000_000);
  const billed = 100_000 + Math.ceil(10_000 * outputRate / inputRate);
  return `One call with 100,000 input and 10,000 output tokens uses ${number(billed)} tokens from your balance (${money(billed * inputRate / 1e12)}).`;
}

export function activeSubscription(row) {
  const now = Date.now();
  return !row.revoked && row.starts_at <= now && row.ends_at > now;
}

export function subscription(row) {
  const section = element("article", null, "entry");
  const active = activeSubscription(row);
  const status = row.revoked ? "Revoked" : active ? "Active" : row.starts_at > Date.now() ? "Scheduled" : "Ended";
  const header = element("div", null, "actions");
  header.append(element("h3", number(row.weekly_tokens) + " tokens per week"), badge(status, active ? "good" : ""));
  section.append(header, element("p", `${when(row.starts_at)} – ${when(row.ends_at)}`, "muted"));
  if (row.price_tokens != null) {
    const price = number(row.price_tokens) + " wallet tokens" + (row.usd == null ? "" : " (" + usd(row.usd) + ")");
    section.append(element("p", active && row.auto_renew ? `Renews for ${price} on ${when(row.ends_at)}.` : `Paid ${price}.`));
  }
  if (active) {
    const refill = row.next_refill && row.next_refill < row.ends_at ? " · Refills " + when(row.next_refill) : "";
    section.append(element("p", number(row.remaining_tokens) + " tokens left this week" + refill));
    if (row.kind === "monthly" && !row.auto_renew) section.append(element("p", "Renewal is off.", "hint"));
  }
  return section;
}
