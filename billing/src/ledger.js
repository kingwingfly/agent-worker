import { accessClosed, amountCents, applicationMessages, check, cost, defaults, dollars, exhausted, HOUR, integer, outputLimit, plan, settingsFrom, tokensForUsd, usageTokens, userId, units, uuid, WEEK } from './policy.js'
import { legacyCharge, migrateWallet } from './migration.js'
import { nativeAmount, reference, validAddress } from './chain.js'

const schema = [
  'CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY, body TEXT NOT NULL)',
  'CREATE TABLE IF NOT EXISTS deposit_addresses (chain TEXT PRIMARY KEY, address TEXT NOT NULL)',
  'CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, created_at INTEGER, wallet_tokens INTEGER NOT NULL, blocked INTEGER DEFAULT 0)',
  `CREATE TABLE IF NOT EXISTS chat_access (user TEXT PRIMARY KEY, status TEXT NOT NULL DEFAULT 'none',
    name TEXT NOT NULL DEFAULT '', username TEXT NOT NULL DEFAULT '', note TEXT NOT NULL DEFAULT '', applied_at INTEGER)`,
  'CREATE INDEX IF NOT EXISTS applications_pending ON chat_access(status,applied_at,user)',
  `CREATE TABLE IF NOT EXISTS subscriptions (id TEXT PRIMARY KEY, user TEXT, source TEXT, starts_at INTEGER,
    ends_at INTEGER, weekly INTEGER, window INTEGER, remaining INTEGER, revoked INTEGER DEFAULT 0)`,
  'CREATE TABLE IF NOT EXISTS weekly_usage (user TEXT PRIMARY KEY, window INTEGER, used INTEGER NOT NULL)',
  `CREATE TABLE IF NOT EXISTS memberships (id TEXT PRIMARY KEY, user TEXT NOT NULL, source TEXT, offer TEXT,
    starts_at INTEGER, ends_at INTEGER, price INTEGER, usd_cents INTEGER, weekly_at_start INTEGER, renew INTEGER DEFAULT 0, revoked INTEGER DEFAULT 0)`,
  'CREATE INDEX IF NOT EXISTS member_user ON memberships(user,ends_at)',
  'CREATE TABLE IF NOT EXISTS membership_charges (membership TEXT, period INTEGER, tokens INTEGER, PRIMARY KEY(membership,period))',
  'CREATE INDEX IF NOT EXISTS member_due ON memberships(renew,ends_at)',
  'CREATE INDEX IF NOT EXISTS subs_user ON subscriptions(user, ends_at)',
  `CREATE TABLE IF NOT EXISTS invoices (id TEXT PRIMARY KEY, user TEXT, chain TEXT, amount TEXT, recipient TEXT, reference TEXT,
    weekly INTEGER, weeks INTEGER, created_at INTEGER, expires_at INTEGER, status TEXT, tx TEXT, precheck TEXT, UNIQUE(chain, tx))`,
  'CREATE INDEX IF NOT EXISTS invoices_user ON invoices(user, created_at)',
  'CREATE TABLE IF NOT EXISTS topups (invoice TEXT PRIMARY KEY, tokens INTEGER, usd_cents INTEGER, rate TEXT)',
  'CREATE TABLE IF NOT EXISTS plan_purchases (id TEXT PRIMARY KEY, user TEXT, offer TEXT, price INTEGER, usd_cents INTEGER)',
  'CREATE TABLE IF NOT EXISTS plan_quotes (id TEXT PRIMARY KEY, user TEXT UNIQUE, offer TEXT, expires_at INTEGER)',
  `CREATE TABLE IF NOT EXISTS claims (invoice TEXT PRIMARY KEY, tx TEXT, submitted INTEGER, attempts INTEGER,
    next_check INTEGER, error TEXT)`,
  'CREATE INDEX IF NOT EXISTS claims_next ON claims(next_check)',
  `CREATE TABLE IF NOT EXISTS calls (id TEXT PRIMARY KEY, user TEXT, created_at INTEGER, status TEXT, held INTEGER,
    allocation TEXT, ratio INTEGER, legacy_conversion INTEGER, rates TEXT, tokens INTEGER DEFAULT 0, cost_usd REAL DEFAULT 0, charged INTEGER DEFAULT 0)`,
  'CREATE INDEX IF NOT EXISTS calls_user ON calls(user, status, created_at)',
  `CREATE TABLE IF NOT EXISTS adjustments (id TEXT PRIMARY KEY, user TEXT, action TEXT, reason TEXT, tokens INTEGER,
    created_at INTEGER, request TEXT)`,
  'CREATE INDEX IF NOT EXISTS adjustments_user ON adjustments(user, created_at)',
  `CREATE TABLE IF NOT EXISTS history (id TEXT PRIMARY KEY, user TEXT, session TEXT, prompt TEXT, answer TEXT,
    created_at INTEGER, is_error INTEGER)`,
  'CREATE INDEX IF NOT EXISTS history_user ON history(user, created_at, id)',
  'CREATE TABLE IF NOT EXISTS logins (token TEXT PRIMARY KEY, user TEXT, kind TEXT, expires_at INTEGER)',
  'CREATE INDEX IF NOT EXISTS logins_expiry ON logins(expires_at)'
]

// Calendar months renew on the original UTC day, clamped in shorter months.
export function monthsAfter(at, months, day = new Date(at).getUTCDate()) {
  const date = new Date(at)
  date.setUTCDate(1)
  date.setUTCMonth(date.getUTCMonth() + months + 1, 0)
  date.setUTCDate(Math.min(day, date.getUTCDate()))
  return date.getTime()
}

const orders = `SELECT i.*,t.tokens,t.usd_cents,t.rate,c.tx AS submitted_tx,c.next_check,c.error AS claim_error
  FROM invoices i LEFT JOIN topups t ON t.invoice=i.id LEFT JOIN claims c ON c.invoice=i.id`

export class Ledger {
  constructor(sql, env = {}) {
    this.sql = sql
    this.env = env
    const migratedSettings = migrateWallet(this)
    for (const statement of schema) sql.exec(statement)
    if (migratedSettings) this.configure(migratedSettings)
    else this.reconcileAccess()
  }
  run(sql, ...params) { return this.sql.exec(sql, ...params) }
  all(sql, ...params) { return this.run(sql, ...params).toArray() }
  one(sql, ...params) { return this.all(sql, ...params)[0] }

  settings() {
    const row = this.one('SELECT body FROM settings WHERE id=1')
    const stored = row ? JSON.parse(row.body) : {}
    return Object.fromEntries(Object.entries(defaults).map(([key, value]) => [key, stored[key] ?? value]))
  }
  configure(body) {
    const previous = this.settings()
    const value = settingsFrom({ ...previous, ...body })
    check(!Object.hasOwn(body, 'expected_access_mode') || previous.access_mode === body.expected_access_mode || previous.access_mode === value.access_mode,
      'Another administrator changed chat access. Refresh Plan settings before changing it again.', 409)
    const options = this.paymentOptions(value)
    // Opening is an operator action. Saving an address or a price never opens a closed service.
    check(value.access_mode !== 'paid' || previous.access_mode === 'paid' || options.mode === 'paid',
      'Configure a deposit address and its mainnet RPC, or enable Stars-only billing, before opening paid access.', 409)
    this.run('INSERT OR REPLACE INTO settings VALUES (1,?)', JSON.stringify(value))
    this.reconcileAccess(value, options)
    return { ...value, access_mode: options.mode }
  }
  paymentOptions(settings = this.settings()) {
    const data = { networks: {}, chains: [] }
    const saved = new Map(this.all('SELECT chain,address FROM deposit_addresses').map(row => [row.chain, row.address]))
    for (const chain of ['ETH', 'SOL']) {
      const configured = this.env[chain + '_RECIPIENT']
      const fallback = typeof configured === 'string' ? configured.trim() : ''
      const address = saved.get(chain) ?? fallback
      const endpoint = this.env[chain + '_RPC_URL']
      const rpc = typeof endpoint === 'string' && /^https?:\/\//i.test(endpoint.trim()) && URL.canParse(endpoint)
      const ready = rpc && validAddress(chain, address)
      data[chain.toLowerCase() + '_recipient'] = address
      data.networks[chain] = { ready, rpc_configured: rpc, overridden: saved.has(chain), default_address: fallback }
      if (ready) data.chains.push(chain)
    }
    data.mode = 'closed'
    if (settings.access_mode === 'free') data.mode = 'free'
    if (settings.access_mode === 'paid' && (data.chains.length || settings.allow_stars_only)) data.mode = 'paid'
    return data
  }
  reconcileAccess(settings = this.settings(), options = this.paymentOptions(settings)) {
    // Losing the last payment method closes the service until the operator explicitly reopens it.
    if (options.mode !== settings.access_mode) {
      this.run('UPDATE settings SET body=? WHERE id=1', JSON.stringify({ ...settings, access_mode: options.mode }))
    }
    if (options.mode !== 'paid') this.run('UPDATE memberships SET renew=0 WHERE renew=1')
    return options
  }
  setDeposits(body) {
    for (const [chain, field] of [['ETH','eth_recipient'], ['SOL','sol_recipient']]) {
      if (!Object.hasOwn(body, field)) continue
      if (body[field] === null) this.run('DELETE FROM deposit_addresses WHERE chain=?', chain)
      else {
        check(typeof body[field] === 'string', 'Enter a receiving address or leave the network blank.')
        const address = body[field].trim()
        check(!address || validAddress(chain, address), 'Enter a valid public ' + chain + ' receiving address.')
        this.run('INSERT OR REPLACE INTO deposit_addresses VALUES (?,?)', chain, address)
      }
    }
    return this.reconcileAccess()
  }
  user(id) {
    const user = this.one('SELECT * FROM users WHERE id=?', userId(id))
    check(user, 'Account not found.', 404)
    return user
  }
  enroll(id, now) {
    const existing = this.one('SELECT * FROM users WHERE id=?', userId(id))
    if (existing) return existing
    this.run('INSERT INTO users (id,created_at,wallet_tokens) VALUES (?,?,0)', id, now)
    return this.user(id)
  }
  approval(id) {
    return this.one('SELECT status FROM chat_access WHERE user=?', id)?.status ?? 'none'
  }
  identify(id, profile) {
    if (!profile) return
    const name = String(profile.name ?? '').trim().slice(0,128), username = String(profile.username ?? '').trim().slice(0,64)
    this.run(`INSERT INTO chat_access(user,name,username) VALUES (?,?,?) ON CONFLICT(user)
      DO UPDATE SET name=excluded.name,username=excluded.username`, id, name, username)
  }
  apply(id, note, now) {
    check(!this.enroll(id, now).blocked, 'This account is suspended.', 403)
    check(typeof note === 'string' && note.length <= 500, 'Keep the application note within 500 characters.')
    this.run(`INSERT INTO chat_access(user,status,note,applied_at) VALUES (?,'pending',?,?)
      ON CONFLICT(user) DO UPDATE SET status='pending',note=excluded.note,applied_at=excluded.applied_at
      WHERE chat_access.status='none'`, id, note.trim(), now)
    const status = this.approval(id)
    return { status, text: applicationMessages[status] }
  }
  applications(offset) {
    integer(offset, 0, 10_000_000, 'application offset')
    const rows = this.all("SELECT user,name,username,note,applied_at FROM chat_access WHERE status='pending' ORDER BY applied_at,user LIMIT 51 OFFSET ?", offset)
    return { applications: rows.slice(0,50), more: rows.length > 50 }
  }
  reviewAccess(id, status, expected) {
    check(['approved','declined','revoked'].includes(status), 'Choose a whitelist decision.')
    const current = this.approval(id)
    if (current === status) return { status }
    check(!expected || expected === current, 'Another administrator updated this account. Refresh the list.', 409)
    this.run('INSERT INTO chat_access(user,status) VALUES (?,?) ON CONFLICT(user) DO UPDATE SET status=excluded.status', id, status)
    if (status !== 'approved') this.run('UPDATE memberships SET renew=0 WHERE user=? AND renew=1', id)
    return { status }
  }
  checkMember(id, options = this.paymentOptions()) {
    check(options.mode !== 'closed', accessClosed, 403)
    check(!this.user(id).blocked, 'This account is suspended. Send /paysupport in private for help.', 403)
    const status = this.approval(id)
    check(status === 'approved', applicationMessages[status], 403)
  }
  // Older, separately purchased passes finish with their original refill calendar.
  legacyPasses(user, now) {
    this.run(`UPDATE subscriptions SET window=CAST((?-starts_at)/? AS INTEGER), remaining=weekly
      WHERE user=? AND revoked=0 AND ends_at>? AND window < CAST((?-starts_at)/? AS INTEGER)`, now, WEEK, user, now, now, WEEK)
    return this.all(`SELECT * FROM subscriptions WHERE user=? AND revoked=0 AND starts_at<=? AND ends_at>?
      ORDER BY starts_at+(window+1)*?, id`, user, now, now, WEEK)
  }
  renew(member, now) {
    this.releaseStale(member.user, now)
    const settings = this.settings(), account = this.user(member.user)
    const next = monthsAfter(member.ends_at, 1, new Date(member.starts_at).getUTCDate())
    if (this.paymentOptions(settings).mode !== 'paid' || this.approval(member.user) !== 'approved' || !settings.free_weekly_tokens || account.blocked || account.wallet_tokens < member.price || next <= now) {
      this.run('UPDATE memberships SET renew=0 WHERE id=?', member.id)
      return
    }
    this.run('UPDATE users SET wallet_tokens=wallet_tokens-? WHERE id=?', member.price, member.user)
    this.run('INSERT INTO membership_charges VALUES (?,?,?)', member.id, member.ends_at, member.price)
    this.run('UPDATE memberships SET ends_at=? WHERE id=?', next, member.id)
  }
  currentMembership(user, now) {
    const latest = this.one('SELECT * FROM memberships WHERE user=? AND revoked=0 ORDER BY ends_at DESC LIMIT 1', user)
    if (!latest || latest.starts_at > now) return null
    if (latest.ends_at > now) return latest
    if (latest.renew) {
      this.renew(latest, now)
      return this.one('SELECT * FROM memberships WHERE id=? AND ends_at>?', latest.id, now) ?? null
    }
    return null
  }
  renewDue(now) {
    for (const member of this.all('SELECT * FROM memberships WHERE renew=1 AND ends_at<=? ORDER BY ends_at LIMIT 50', now)) this.renew(member, now)
  }
  weekly(user, now) {
    const member = this.currentMembership(user, now), account = this.user(user)
    const window = Math.max(0, Math.floor((now - account.created_at) / WEEK))
    const limit = this.settings().free_weekly_tokens * (member ? 5 : 1)
    const used = this.one('SELECT used FROM weekly_usage WHERE user=? AND window=?', user, window)?.used ?? 0
    return { member, window, limit, remaining: Math.max(0, limit - used), next: account.created_at + (window + 1) * WEEK }
  }
  balance(user, now) {
    const week = this.weekly(user, now), wallet = this.user(user).wallet_tokens
    if (wallet < 0) return 0
    const legacy = this.legacyPasses(user, now).reduce((sum, pass) => sum + pass.remaining, 0)
    return Math.min(Number.MAX_SAFE_INTEGER, wallet + week.remaining + legacy)
  }
  insufficient(user, now, needed) {
    const { pending } = this.one("SELECT coalesce(sum(held),0) AS pending FROM calls WHERE user=? AND status IN ('reserved','unreported') AND created_at>=?", user, now - 2 * HOUR)
    check(pending < needed, 'Tokens are reserved for another model call. Please try again shortly.', 429)
    check(false, exhausted, 402)
  }
  access(user, now, options = this.paymentOptions()) {
    this.checkMember(user, options)
    this.releaseStale(user, now)
    if (options.mode === 'paid' && this.balance(user, now) <= 0) this.insufficient(user, now, 1)
    return { ok: true }
  }
  grantLegacy(id, user, weekly, weeks, now, source) {
    this.run(`INSERT INTO subscriptions (id,user,source,starts_at,ends_at,weekly,window,remaining)
      VALUES (?,?,?,?,?,?,0,?)`, id, user, source, now, now + weeks * WEEK, weekly, weekly)
  }
  deduct(user, amount, now) {
    const allocation = []
    const week = this.weekly(user, now)
    const take = Math.min(week.remaining, amount)
    if (take > 0) {
      this.run(`INSERT INTO weekly_usage VALUES (?,?,?) ON CONFLICT(user) DO UPDATE SET window=excluded.window,
        used=CASE WHEN weekly_usage.window=excluded.window THEN weekly_usage.used+excluded.used ELSE excluded.used END`, user, week.window, take)
      allocation.push({ week: week.window, take })
      amount -= take
    }
    if (amount > 0) {
      for (const pass of this.legacyPasses(user, now)) {
        const fromPass = Math.min(amount, pass.remaining)
        if (fromPass > 0) {
          this.run('UPDATE subscriptions SET remaining=remaining-? WHERE id=?', fromPass, pass.id)
          allocation.push({ id: pass.id, window: pass.window, take: fromPass })
          amount -= fromPass
        }
        if (!amount) break
      }
    }
    if (amount > 0) {
      check(this.user(user).wallet_tokens >= amount, exhausted, 402)
      this.addWallet(user, -amount)
      allocation.push({ take: amount })
    }
    return allocation
  }
  refund(user, allocation, amount) {
    for (const part of allocation.toReversed()) {
      if (amount <= 0) break
      const give = Math.min(amount, part.take)
      if (part.week != null) {
        this.run('UPDATE weekly_usage SET used=max(0,used-?) WHERE user=? AND window=?', give, user, part.week)
      } else if (part.id) {
        this.run('UPDATE subscriptions SET remaining=remaining+? WHERE id=? AND window=?', give, part.id, part.window)
      } else {
        this.addWallet(user, give)
      }
      amount -= give
    }
  }
  releaseStale(user, now) {
    const stale = this.all(`SELECT * FROM calls WHERE user=? AND status IN ('reserved','unreported') AND created_at<? LIMIT 100`,
      user, now - 2 * HOUR)
    for (const call of stale) {
      this.refund(user, JSON.parse(call.allocation), call.held)
      this.run("UPDATE calls SET status='released',allocation='[]' WHERE id=?", call.id)
    }
  }
  reserve({ user, id, input, output, min_output = 1 }, now) {
    uuid(id)
    const settings = this.settings()
    const options = this.paymentOptions(settings)
    this.access(user, now, options)
    let held = 0, limit = null, allocation = []
    if (options.mode === 'paid') {
      if (input == null) return { count_required: true }
      integer(input, 0, 100_000_000, 'input token count')
      integer(output, 1, 100_000_000, 'output token limit')
      integer(min_output, 1, output, 'minimum output')
      const available = this.balance(user, now)
      const minimum = usageTokens(input, min_output, settings)
      if (available < minimum) this.insufficient(user, now, minimum - available)
      limit = outputLimit(available - input, output, settings)
      held = usageTokens(input, limit, settings)
      allocation = this.deduct(user, held, now)
    }
    this.run(`INSERT INTO calls (id,user,created_at,status,held,allocation,rates) VALUES (?,?,?,'reserved',?,?,?)`,
      id, user, now, held, JSON.stringify(allocation), JSON.stringify(settings))
    return { max_output: limit }
  }
  settle({ id, usage, failed }, now) {
    const call = this.one('SELECT * FROM calls WHERE id=?', uuid(id))
    check(call, 'Unknown reservation.', 404)
    if (!['reserved', 'unreported', 'released'].includes(call.status)) return { ok: true }
    let tokens = 0, charge = 0, actualCost = 0
    const rates = JSON.parse(call.rates)
    if (!failed) {
      for (const key of ['input_tokens', 'output_tokens', 'cache_creation_input_tokens', 'cache_read_input_tokens']) {
        integer(usage?.[key], 0, 100_000_000, 'reported token count')
      }
      let input = usage.input_tokens + usage.cache_creation_input_tokens
      if (!call.legacy_conversion) input += usage.cache_read_input_tokens
      tokens = input + usage.output_tokens
      actualCost = cost(usage, rates)
      if (call.held) charge = call.legacy_conversion ? legacyCharge(tokens, call) : usageTokens(input, usage.output_tokens, rates)
      if (!tokens && !usage.cache_read_input_tokens) {
        if (call.status === 'reserved') this.run("UPDATE calls SET status='unreported' WHERE id=?", id)
        return { ok: true }
      }
    }
    const held = call.status === 'released' ? 0 : call.held
    // Close this hold within the settlement transaction before a due renewal can release stale holds.
    this.run('UPDATE calls SET status=?,tokens=?,cost_usd=?,charged=?,allocation=? WHERE id=?',
      failed ? 'failed' : 'settled', tokens, actualCost, charge, '[]', id)
    if (charge <= held) this.refund(call.user, JSON.parse(call.allocation), held - charge)
    else {
      // Counts are estimates. Record the full provider cost while limiting the debit to available funds.
      const extra = Math.min(charge - held, this.balance(call.user, now))
      this.deduct(call.user, extra, now)
      if (held + extra < charge) this.run('UPDATE calls SET charged=? WHERE id=?', held + extra, id)
    }
    return { ok: true }
  }
  purchaseSettings(user, now, settings = this.settings(), options = this.paymentOptions(settings)) {
    this.enroll(user, now)
    this.checkMember(user, options)
    check(options.mode === 'paid', 'Chat is currently free. New purchases are paused.', 409)
    return settings
  }
  invoice(user, chain, now, usd, rate = null) {
    const settings = this.settings(), options = this.paymentOptions(settings)
    this.purchaseSettings(user, now, settings, options)
    check(['ETH', 'SOL', 'XTR'].includes(chain), 'Choose a supported currency.')
    const cents = amountCents(usd)
    const tokens = tokensForUsd(cents, settings.input_usd)
    const stars = Math.ceil(cents * settings.stars_per_usd / 100)
    let address = ''
    if (chain === 'XTR') check(stars <= 10_000, 'Choose a smaller dollar amount for a single Stars payment.')
    else {
      check(options.networks[chain].ready, 'The merchant has not enabled this payment network.', 503)
      address = options[chain.toLowerCase() + '_recipient']
    }
    check(tokens >= 1 && Number.isSafeInteger(this.user(user).wallet_tokens + tokens), 'This top-up exceeds the supported wallet balance.')
    // A reused crypto quote must leave time for the wallet prompt and block inclusion.
    const reuseAfter = chain === 'XTR' ? now : now + 2 * 60_000
    const pending = this.one(`${orders} WHERE i.user=? AND i.chain=? AND i.status='open' AND i.precheck IS NULL
      AND i.expires_at>? AND i.recipient=? AND c.invoice IS NULL AND t.tokens=? AND t.usd_cents=?
      AND (i.chain!='XTR' OR i.amount=?) ORDER BY i.created_at DESC LIMIT 1`, user, chain, reuseAfter, address, tokens, cents, String(stars))
    if (pending) return pending
    if (chain !== 'XTR' && rate === null) return null
    const id = crypto.randomUUID()
    const amount = chain === 'XTR' ? String(stars) : nativeAmount(chain, cents, rate)
    this.run(`INSERT INTO invoices (id,user,chain,amount,recipient,reference,weekly,weeks,created_at,expires_at,status)
      VALUES (?,?,?,?,?,?,0,0,?,?,'open')`, id, user, chain, amount, address, reference(chain, id),
      now, now + (chain === 'XTR' ? HOUR : 15 * 60_000))
    this.run('INSERT INTO topups (invoice,tokens,usd_cents,rate) VALUES (?,?,?,?)', id, tokens, cents, rate)
    return this.order(id, user)
  }
  subscribe(user, key, offer, now) {
    uuid(key)
    const receipt = this.one('SELECT user,offer FROM memberships WHERE id=? UNION ALL SELECT user,offer FROM plan_purchases WHERE id=?', key, key)
    if (receipt) {
      check(receipt.user === user && receipt.offer === offer, 'This purchase reference was already used.', 409)
      return { status: 'active' }
    }
    const terms = plan(this.purchaseSettings(user, now))
    check(offer === terms.offer, 'Plan pricing changed. Review the current offer before subscribing.', 409)
    check(terms.free_weekly_tokens > 0, 'Subscriptions are unavailable while the weekly allowance is zero.', 409)
    this.releaseStale(user, now)
    check(!this.currentMembership(user, now), 'You already have an active subscription.', 409)
    check(this.user(user).wallet_tokens >= terms.plan_price_tokens, 'The plan costs ' + terms.plan_price_tokens + ' wallet tokens. Top up first.', 402)
    this.addWallet(user, -terms.plan_price_tokens)
    this.run(`INSERT INTO memberships (id,user,source,offer,starts_at,ends_at,price,usd_cents,weekly_at_start,renew)
      VALUES (?,?,'Wallet',?,?,?,?,?,?,1)`, key, user, offer, now, monthsAfter(now, 1), terms.plan_price_tokens, amountCents(terms.monthly_usd), terms.paid_weekly_tokens)
    this.run('INSERT INTO membership_charges VALUES (?,?,?)', key, now, terms.plan_price_tokens)
    return { status: 'active' }
  }
  cancel(user, id) {
    const row = this.one('SELECT ends_at FROM memberships WHERE id=? AND user=?', uuid(id), userId(user))
    check(row, 'Subscription not found.', 404)
    this.run('UPDATE memberships SET renew=0 WHERE id=?', id)
    return { status: 'cancelled', ends_at: row.ends_at }
  }
  telegramPlan(user, key, now) {
    if (!key) {
      const terms = plan(this.purchaseSettings(user, now))
      const active = this.currentMembership(user, now)
      if (active) return { text: 'Your subscription is active through ' + new Date(active.ends_at).toISOString().slice(0,10) + '.' + (active.renew ? ' Send /unsubscribe to stop renewal.' : '') }
      check(terms.free_weekly_tokens > 0, 'Subscriptions are unavailable while the weekly allowance is zero.', 409)
      const id = crypto.randomUUID()
      this.run('INSERT OR REPLACE INTO plan_quotes VALUES (?,?,?,?)', id, user, terms.offer, now + 600_000)
      return { text: `Subscribe for ${terms.plan_price_tokens} wallet tokens ($${terms.monthly_usd}) every month. Your weekly allowance becomes ${terms.paid_weekly_tokens} tokens (5× free). Renewal uses your wallet. Cancel with /unsubscribe.\nTo confirm within 10 minutes, send /subscribe ${id}` }
    }
    uuid(key)
    const quote = this.one('SELECT offer FROM memberships WHERE id=? AND user=?', key, user)
      ?? this.one('SELECT offer FROM plan_quotes WHERE id=? AND user=? AND expires_at>?', key, user, now)
    check(quote, 'This offer expired. Send /subscribe for a current quote.', 409)
    this.subscribe(user, key, quote.offer, now)
    this.run('DELETE FROM plan_quotes WHERE id=?', key)
    return { text: 'Subscribed. Send /account to see your weekly allowance, or /unsubscribe to stop renewal.' }
  }
  order(id, user) {
    const row = this.one(`${orders} WHERE i.id=? AND i.user=?`, uuid(id), userId(user))
    check(row, 'Invoice not found.', 404)
    return row
  }
  pay(id, user, tx, now) {
    const order = this.order(id, user)
    if (order.tx === tx && order.status === 'paid') return { status: 'paid' }
    check(order.status === 'open' && !this.one('SELECT id FROM invoices WHERE chain=? AND tx=?', order.chain, tx),
      'The invoice or transfer has already been used.', 409)
    this.run("UPDATE invoices SET status='paid',tx=? WHERE id=?", tx, id)
    this.currentMembership(user, now)
    if (order.tokens == null) this.grantLegacy(id, user, order.weekly, order.weeks, now, order.chain)
    else this.addWallet(user, order.tokens)
    this.run('DELETE FROM claims WHERE invoice=?', id)
    return { status: 'paid' }
  }
  addWallet(user, quantity) {
    check(Number.isSafeInteger(this.user(user).wallet_tokens + quantity), 'Token balance exceeds the supported range.')
    this.run('UPDATE users SET wallet_tokens=wallet_tokens+? WHERE id=?', quantity, user)
  }
  stars(body, now) {
    const { kind, user, invoice, amount, currency, charge, query } = body
    const order = this.order(invoice, user)
    check(order.chain === 'XTR' && currency === 'XTR' && amount === Number(order.amount), 'This invoice does not match the purchase.')
    if (kind === 'precheck') {
      this.purchaseSettings(user, now)
      check(order.expires_at >= now && order.status === 'open'
        && (!order.precheck || order.precheck === query), 'The invoice is closed. Send /topup for a new one.')
      this.run('UPDATE invoices SET precheck=? WHERE id=?', query, invoice)
      return { ok: true }
    }
    check(typeof charge === 'string' && charge.length > 0 && charge.length <= 512, 'Invalid Telegram charge ID.')
    if (kind === 'refund') {
      check(order.tx === charge && ['paid','refunded'].includes(order.status), 'The refund does not match a paid invoice.', 409)
      if (order.status === 'refunded') return { status: 'refunded' }
      this.run("UPDATE invoices SET status='refunded' WHERE id=?", invoice)
      if (order.tokens == null) this.run('UPDATE subscriptions SET revoked=1 WHERE id=?', invoice)
      else this.addWallet(user, -order.tokens)
      return { status: 'refunded' }
    }
    check(kind === 'paid', 'Unknown payment event.')
    if (order.tx === charge && order.status === 'refunded') return { status: 'refunded' }
    return this.pay(invoice, user, charge, now)
  }
  adjust(id, body, now) {
    this.user(id)
    const key = uuid(body.key), action = body.action
    const receipt = this.one('SELECT user,request FROM adjustments WHERE id=?', key)
    const request = JSON.stringify(body)
    if (receipt) {
      check(receipt.user === id && receipt.request === request, 'This action ID was already used.', 409)
      return { ok: true }
    }
    let amount = 0
    const reason = body.reason ?? ''
    check(typeof reason === 'string' && reason.length <= 500 && (action === 'block' || reason.trim()), 'Enter a reason for this change.')
    if (action === 'airdrop') {
      this.currentMembership(id, now)
      amount = integer(body.tokens, 1, 1_000_000_000_000, 'airdrop tokens')
      this.addWallet(id, amount)
    } else if (action === 'grant') {
      const months = integer(body.months, 1, 12, 'subscription months')
      check(this.settings().free_weekly_tokens > 0, 'Set a free weekly allowance before granting the plan.', 409)
      check(!this.currentMembership(id, now), 'This user already has an active subscription.', 409)
      this.run(`INSERT INTO memberships (id,user,source,starts_at,ends_at,price,usd_cents,weekly_at_start)
        VALUES (?,?,'Admin',?,?,0,0,?)`, key, id, now, monthsAfter(now, months), this.settings().free_weekly_tokens * 5)
    } else if (action === 'revoke') {
      const sub = uuid(body.subscription)
      const member = this.one('SELECT id FROM memberships WHERE id=? AND user=?', sub, id)
      if (member) this.run('UPDATE memberships SET revoked=1,renew=0 WHERE id=?', sub)
      else {
        check(this.one('SELECT id FROM subscriptions WHERE id=? AND user=?', sub, id), 'Subscription not found.', 404)
        this.run('UPDATE subscriptions SET revoked=1 WHERE id=?', sub)
      }
    } else if (action === 'block') {
      check(typeof body.blocked === 'boolean', 'Choose an account status.')
      this.run('UPDATE users SET blocked=? WHERE id=?', Number(body.blocked), id)
    } else check(false, 'Unknown account action.')
    this.run('INSERT INTO adjustments VALUES (?,?,?,?,?,?,?)', key, id, action, reason, amount, now, request)
    return { ok: true }
  }
  subscriptions(id, now, week) {
    const legacy = this.all(`SELECT s.*,p.price,p.usd_cents FROM subscriptions s LEFT JOIN plan_purchases p ON p.id=s.id
      WHERE s.user=?`, id).map(p => ({
      kind: 'legacy', id: p.id, source: p.source, weekly_tokens: p.weekly, auto_renew: false,
      price_tokens: p.price, usd: p.usd_cents == null ? null : dollars(p.usd_cents),
      remaining_tokens: !p.revoked && p.ends_at > now ? p.remaining : 0,
      starts_at: p.starts_at, ends_at: p.ends_at, revoked: Boolean(p.revoked),
      next_refill: !p.revoked && p.starts_at + (p.window + 1) * WEEK < p.ends_at && p.ends_at > now ? p.starts_at + (p.window + 1) * WEEK : null
    }))
    const monthly = this.all(`SELECT m.*, (SELECT count(*) FROM membership_charges c WHERE c.membership=m.id) AS charges
      FROM memberships m WHERE m.user=?`, id).map(p => {
      const active = week.member?.id === p.id
      return { kind: 'monthly', id: p.id, source: p.source, starts_at: p.starts_at, ends_at: p.ends_at,
        price_tokens: p.price, usd: dollars(p.usd_cents), periods_paid: p.charges, auto_renew: Boolean(p.renew), revoked: Boolean(p.revoked),
        weekly_tokens: active ? week.limit : p.weekly_at_start, remaining_tokens: active ? week.remaining : 0,
        next_refill: active && week.next < p.ends_at ? week.next : null }
    })
    return [...monthly, ...legacy].sort((a,b) => b.starts_at - a.starts_at || b.id.localeCompare(a.id))
  }
  account(id, now, options = this.paymentOptions()) {
    this.releaseStale(id, now)
    const week = this.weekly(id, now), u = this.user(id), approval = this.approval(id)
    const legacy = this.legacyPasses(id, now).reduce((sum,p) => sum + p.remaining, 0)
    let available = 0
    if (approval === 'approved' && !u.blocked) {
      if (options.mode === 'free') available = null
      if (options.mode === 'paid' && u.wallet_tokens >= 0) available = Math.min(Number.MAX_SAFE_INTEGER, u.wallet_tokens + week.remaining + legacy)
    }
    return {
      id, access_mode: options.mode, approval_status: approval, blocked: Boolean(u.blocked), created_at: u.created_at, wallet_tokens: u.wallet_tokens,
      tier: week.member ? 'subscriber' : 'free', allowance_tokens: week.limit, remaining_weekly_tokens: week.remaining,
      next_refill: week.next, available_tokens: available,
      subscriptions: this.subscriptions(id, now, week)
    }
  }
  payments(id, now, after, options = this.paymentOptions()) {
    const cursor = after ? this.order(after, id) : null
    const rows = this.all(`${orders} WHERE i.user=? AND (i.created_at,i.id)<(?,?)
      ORDER BY i.created_at DESC,i.id DESC LIMIT 51`, id, cursor?.created_at ?? Number.MAX_SAFE_INTEGER, cursor?.id ?? '')
    return { invoices: rows.slice(0,50).map(row => this.showOrder(row, now, options)), invoices_next: rows.length > 50 ? rows[49].id : null }
  }
  detail(id, now, after) {
    const page = this.payments(id, now, after)
    return {
      ...this.account(id, now),
      ...this.one('SELECT coalesce(sum(tokens),0) AS billable_tokens,coalesce(sum(cost_usd),0) AS cost_usd FROM calls WHERE user=?', id),
      payments: page.invoices, payments_next: page.invoices_next,
      adjustments: this.all('SELECT action,reason,tokens,created_at FROM adjustments WHERE user=? ORDER BY created_at DESC LIMIT 50', id)
    }
  }
  showOrder(row, now, options = this.paymentOptions()) {
    const { id, chain, amount, recipient, reference, weekly, weeks, created_at, expires_at, status } = row
    const current = options[chain.toLowerCase() + '_recipient']
    const receiving = options.chains.includes(chain) && (chain === 'ETH' ? recipient.toLowerCase() === current.toLowerCase() : recipient === current)
    return { id, chain, amount, recipient, reference, weekly_tokens: weekly, weeks, created_at, expires_at,
      accepts_payment: options.mode === 'paid' && (chain === 'XTR' || receiving),
      product: row.tokens == null ? 'legacy_plan' : 'tokens', usd: row.usd_cents == null ? null : dollars(row.usd_cents),
      tokens: row.tokens, rate_usd: row.rate,
      tx: row.tx ?? row.submitted_tx ?? null, note: row.claim_error ?? null,
      status: row.next_check != null ? 'confirming' : status === 'open' && expires_at < now ? 'expired' : status }
  }
  users(q, offset, approval = '') {
    check(typeof q === 'string' && /^\d{0,16}$/.test(q), 'Search by Telegram user ID.')
    integer(offset, 0, 10_000_000, 'page offset')
    check(approval === '' || approval === 'approved', 'Choose a valid account filter.')
    const rows = this.all(`SELECT u.*,coalesce(a.status,'none') AS approval_status FROM users u LEFT JOIN chat_access a ON a.user=u.id
      WHERE u.id LIKE ? AND (?='' OR a.status=?) ORDER BY u.created_at DESC,u.id LIMIT 51 OFFSET ?`, q + '%', approval, approval, offset)
    return { more: rows.length > 50, users: rows.slice(0,50).map(row => ({
      id: row.id, created_at: row.created_at, blocked: Boolean(row.blocked), approval_status: row.approval_status, wallet_tokens: row.wallet_tokens
    })) }
  }
  overview(now) {
    const totals = this.one(`SELECT (SELECT count(*) FROM users) AS users,
      (SELECT count(*) FROM subscriptions WHERE ends_at>? AND revoked=0) +
      (SELECT count(*) FROM memberships WHERE ends_at>? AND revoked=0) AS active_subscriptions,
      (SELECT count(*) FROM invoices WHERE status='paid') AS payments,
      (SELECT coalesce(sum(t.usd_cents),0) FROM topups t JOIN invoices i ON i.id=t.invoice WHERE i.status='paid') AS sales_cents,
      coalesce(sum(tokens),0) AS billable_tokens,coalesce(sum(cost_usd),0) AS cost_usd FROM calls`, now, now)
    const totalsByChain = { ETH: 0n, SOL: 0n, XTR: 0n }, decimals = { ETH: 18, SOL: 9, XTR: 0 }
    for (const row of this.all("SELECT chain,amount,count(*) AS n FROM invoices WHERE status='paid' GROUP BY chain,amount")) {
      totalsByChain[row.chain] += units(row.amount, decimals[row.chain]) * BigInt(row.n)
    }
    const revenues = {}
    for (const [chain, amount] of Object.entries(totalsByChain)) {
      const scale = 10n ** BigInt(decimals[chain])
      const fraction = (amount % scale).toString().padStart(decimals[chain], '0').replace(/0+$/, '')
      revenues[chain] = String(amount / scale) + (fraction ? '.' + fraction : '')
    }
    const { sales_cents, ...stats } = totals
    return { ...stats, sales_usd: dollars(sales_cents), revenues }
  }
  record({ user, id, session, prompt, answer, is_error, created_at }) {
    this.user(user)
    uuid(id)
    check(/^tg:(-?\d+|guest:-?\d+:\d+)$/.test(session), 'Invalid session.')
    integer(created_at, 1, Number.MAX_SAFE_INTEGER, 'turn time')
    this.run('INSERT OR IGNORE INTO history VALUES (?,?,?,?,?,?,?)', id, user, session, prompt.slice(0, 64000), answer.slice(0, 64000), created_at, Number(is_error))
    return { ok: true }
  }
  history(user, before, afterId) {
    // Both parts of the cursor matter when concurrent sessions finish in the same millisecond.
    const rows = this.all(`SELECT * FROM history WHERE user=? AND (created_at<? OR (created_at=? AND id<?))
      ORDER BY created_at DESC,id DESC LIMIT 26`, user, before, before, afterId)
    const turns = rows.slice(0, 25), last = turns.at(-1)
    return { turns, next: rows.length > 25 ? last.created_at : null, next_id: rows.length > 25 ? last.id : null }
  }
}
