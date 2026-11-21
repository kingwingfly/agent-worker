import { DurableObject, WorkerEntrypoint } from 'cloudflare:workers'
import { Ledger, monthsAfter } from './ledger.js'
import checkoutHtml from '../public/index.html'
import adminHtml from '../public/admin.html'
import css from '../public/assets/style.css'
import commonJs from '../public/assets/common.js'
import checkoutJs from '../public/assets/checkout.js'
import adminJs from '../public/assets/admin.js'
import mascot from '../public/assets/wallet-cat.webp'
import nunito from '../public/assets/nunito-latin.woff2'
import fontLicense from '../public/assets/nunito-OFL.txt'
import { checkPaymentNetwork, spotRate, transactionId, verify } from './chain.js'
import { applicationMessages, check, HOUR, integer, plan, Problem, userId, uuid } from './policy.js'

const NO_STORE = { 'Cache-Control': 'no-store', 'X-Content-Type-Options': 'nosniff', 'Referrer-Policy': 'no-referrer' }
const SESSION = '__Host-billing'
const INTERNAL = new Set(['/access', '/reserve', '/settle', '/record', '/welcome', '/account', '/apply', '/topup', '/subscribe', '/unsubscribe', '/support', '/stars'])
const TEXT_FILE = /\.(?:txt|md|csv|tsv|json|jsonl|log|ya?ml|toml|xml|py|rs|js|ts|css|sql|sh)$/i
const PREVIEW_BYTES = 32 * 1024
// Serve these small files directly so Cloudflare delivers its authenticated Access
// context to this Worker. The Workers Static Assets router omits that context.
const pages = new Map([
  ['/', ['text/html', checkoutHtml]],
  ['/admin', ['text/html', adminHtml]],
  ['/admin/', ['text/html', adminHtml]],
  ['/assets/style.css', ['text/css', css]],
  ['/assets/common.js', ['text/javascript', commonJs]],
  ['/assets/checkout.js', ['text/javascript', checkoutJs]],
  ['/assets/admin.js', ['text/javascript', adminJs]],
  ['/assets/wallet-cat.webp', ['image/webp', mascot]],
  ['/assets/nunito-latin.woff2', ['font/woff2', nunito]],
  ['/assets/nunito-OFL.txt', ['text/plain', fontLicense]]
])

const json = (body, status = 200, headers = {}) => Response.json(body, { status, headers: { ...NO_STORE, ...headers } })
async function handle(action) {
  try { return await action() }
  catch (error) {
    if (error instanceof Problem) return json({ error: error.message }, error.status)
    console.error('Billing request failed', error?.name, error?.message)
    return json({ error: 'Billing is temporarily unavailable. Please try again.' }, 503)
  }
}
async function payload(request, max = 16_384) {
  check(request.headers.get('content-type')?.split(';')[0] === 'application/json', 'Send a JSON request.', 415)
  const reader = request.body?.getReader()
  check(reader, 'Missing request body.')
  const decoder = new TextDecoder()
  let text = '', size = 0
  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    size += value.byteLength
    if (size > max) {
      await reader.cancel()
      throw new Problem(413, 'Request is too large.')
    }
    text += decoder.decode(value, { stream: true })
  }
  try {
    const value = JSON.parse(text + decoder.decode())
    check(value && typeof value === 'object' && !Array.isArray(value), 'Expected a JSON object.')
    return value
  } catch { throw new Problem(400, 'Invalid JSON object.') }
}
async function digest(text) {
  return await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text))
}
async function tokenHash(value) {
  return Array.from(new Uint8Array(await digest(value)), n => n.toString(16).padStart(2, '0')).join('')
}
function ledger(env) { return env.LEDGER.getByName('service') }

// Only a named service binding can call this entrypoint. The public fetch handler
// below never forwards an internal path or a caller-supplied identity header.
export class GatewayBilling extends WorkerEntrypoint {
  async fetch(request) {
    const path = new URL(request.url).pathname
    if (request.method !== 'POST' || !INTERNAL.has(path)) return json({ error: 'Not found.' }, 404)
    return ledger(this.env).fetch(new Request('https://ledger/internal' + path, request))
  }
}
function asset(method, [type, body]) {
  const headers = new Headers(NO_STORE)
  headers.set('content-type', type.startsWith('text/') ? type + '; charset=utf-8' : type)
  if (!type.startsWith('text/')) headers.set('cache-control', 'public, max-age=86400')
  headers.set('content-security-policy', "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'")
  return new Response(method === 'HEAD' ? null : body, { headers })
}
export default {
  fetch(request, env, ctx) {
    return handle(async () => {
      const url = new URL(request.url), path = url.pathname
      if (path === '/admin' || path.startsWith('/admin/')) {
        check(ctx.access, 'Cloudflare Access sign-in required.', 403)
      }
      const page = pages.get(path)
      if (page && (request.method === 'GET' || request.method === 'HEAD')) return asset(request.method, page)
      check(path.startsWith('/api/') || path.startsWith('/admin/api/'), 'Not found.', 404)
      if (request.method === 'POST') {
        const origin = request.headers.get('origin')
        check(!origin || origin === url.origin, 'Cross-origin request refused.', 403)
      }
      return ledger(env).fetch(request)
    })
  }
}

export class BillingLedger extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env)
    this.book = this.atomic(() => new Ledger(ctx.storage.sql, env))
  }
  atomic(action) { return this.ctx.storage.transactionSync(action) }
  fetch(request) {
    return handle(async () => {
      const url = new URL(request.url), path = url.pathname, now = Date.now()
      const post = request.method === 'POST', get = request.method === 'GET'
      const body = post ? await payload(request, path === '/internal/record' ? 1_048_576 : 16_384) : {}
      const book = this.book
      if (post && path.startsWith('/internal/')) {
        const result = await this.internal(path.slice(10), body, now)
        return json(result)
      }
      if (path.startsWith('/admin/api/')) return this.admin(path.slice(11), url.searchParams, body, now, request.method)
      if (get && path === '/api/pricing') return json(this.catalog())
      if (post && path === '/api/login') {
        check(typeof body.code === 'string' && /^[0-9a-f-]{36}$/.test(body.code), 'Invalid or expired sign-in code.', 401)
        const token = crypto.randomUUID(), hash = await tokenHash(token), code = await tokenHash(body.code)
        this.atomic(() => {
          const row = book.one("SELECT user FROM logins WHERE token=? AND kind='code' AND expires_at>?", code, now)
          check(row, 'Invalid or expired sign-in code.', 401)
          book.run('DELETE FROM logins WHERE token=? OR expires_at<=?', code, now)
          book.run("INSERT INTO logins VALUES (?,?,'session',?)", hash, row.user, now + 24 * HOUR)
        })
        return json({ ok: true }, 200, { 'Set-Cookie': this.cookie(token, 86400) })
      }
      const match = (request.headers.get('cookie') || '').match(/(?:^|;\s*)__Host-billing=([0-9a-f-]{36})(?:;|$)/)
      check(match, 'Sign in to your billing account.', 401)
      const hash = await tokenHash(match[1])
      const auth = book.one("SELECT user FROM logins WHERE token=? AND kind='session' AND expires_at>?", hash, now)
      check(auth, 'Your session expired. Sign in again.', 401)
      if (post && path === '/api/logout') {
        book.run('DELETE FROM logins WHERE token=?', hash)
        return json({ ok: true }, 200, { 'Set-Cookie': this.cookie('', 0) })
      }
      const user = auth.user
      if (post && path === '/api/apply') {
        check(Object.keys(body).every(key => key === 'note'), 'Applications use the Telegram identity of your signed-in account.')
        return json(this.atomic(() => book.apply(user, body.note ?? '', now)))
      }
      if (get && path === '/api/account') {
        const settings = book.settings(), options = book.paymentOptions(settings)
        const detail = this.atomic(() => book.account(user, now, options))
        return json({ ...detail, ...this.catalog(settings, options), ...book.payments(user, now, url.searchParams.get('after'), options) })
      }
      if (post && path === '/api/invoice') {
        check(['ETH','SOL'].includes(body.chain), 'Choose ETH or SOL.')
        const issue = (at, rate) => this.atomic(() => book.invoice(user, body.chain, at, body.usd, rate))
        const existing = issue(now, null)
        const [rate] = await Promise.all([existing ? null : spotRate(body.chain), checkPaymentNetwork(body.chain, this.env)])
        // Another checkout can submit or settle this invoice while the RPC is responding.
        const order = existing ? book.order(existing.id, user) : issue(Date.now(), rate)
        const invoice = book.showOrder(order, Date.now())
        if (existing && invoice.status === 'open' && !invoice.tx) {
          book.checkMember(user)
          check(invoice.accepts_payment, 'Payment settings changed. Refresh your account for current payment methods.', 409)
        }
        return json(invoice)
      }
      if (post && path === '/api/subscribe') {
        await this.schedule(monthsAfter(now, 1))
        return json(this.atomic(() => book.subscribe(user, body.key, body.offer, now)))
      }
      if (post && path === '/api/cancel') return json(this.atomic(() => book.cancel(user, body.id)))
      if (post && path === '/api/claim') {
        const result = await this.submitClaim(user, body, now)
        return json(result, result.status === 'confirming' ? 202 : 200)
      }
      throw new Problem(404, 'Not found.')
    })
  }
  catalog(settings = this.book.settings(), options = this.book.paymentOptions(settings)) {
    const charging = options.mode === 'paid'
    return { plan: { ...plan(settings), access_mode: options.mode, require_payment: charging }, chains: charging ? options.chains : [] }
  }
  async schedule(at) {
    if (at == null) return
    const current = await this.ctx.storage.getAlarm()
    if (current == null || current > at) await this.ctx.storage.setAlarm(Math.max(Date.now() + 500, at))
  }
  async submitClaim(user, body, now) {
    const book = this.book, order = book.order(body.id, user)
    check(order.chain !== 'XTR', 'Use the original payment channel.')
    const tx = transactionId(order.chain, body.tx)
    if (order.status === 'paid' && order.tx === tx) return { status: 'paid' }
    if (order.next_check != null && order.submitted_tx === tx) return { status: 'confirming' }
    check(order.status === 'open', 'Invoice is closed.', 409)
    // Arm the object before acknowledging the transaction. If the first RPC call is
    // interrupted, the alarm can pick up the same invoice even after the customer leaves.
    await this.schedule(now + 5000)
    const submitted = this.atomic(() => {
      const current = book.order(order.id, user)
      if (current.status === 'paid' && current.tx === tx) return 'paid'
      check(current.status === 'open', 'Invoice is closed.', 409)
      const old = book.one('SELECT * FROM claims WHERE invoice=?', order.id)
      if (old?.tx === tx && old.next_check != null) return 'confirming'
      check(!old || now >= old.submitted + 10_000, 'Wait a few seconds before submitting another transaction.', 429)
      book.run('INSERT OR REPLACE INTO claims VALUES (?,?,?,0,?,NULL)', order.id, tx, now, now + 5000)
      return 'new'
    })
    if (submitted !== 'new') return { status: submitted }
    return this.verifyClaim(order, tx)
  }
  async verifyClaim(order, tx) {
    const book = this.book
    try {
      await verify(order, tx, this.env)
    } catch (err) {
      const job = book.one('SELECT * FROM claims WHERE invoice=? AND tx=?', order.id, tx)
      if (!job) {
        if (book.order(order.id, order.user).status === 'paid') return { status: 'paid' }
        throw new Problem(409, 'Another transaction is being checked for this invoice.')
      }
      const transient = !(err instanceof Problem) || [409,503].includes(err.status)
      const now = Date.now(), deadline = job.submitted + 24 * HOUR
      if (transient && now < deadline) {
        const elapsed = now - job.submitted
        const cap = order.chain === 'SOL' && elapsed < 60_000 ? 5000 : elapsed < 30 * 60_000 ? 60_000 : HOUR
        const delay = Math.min(cap, 5000 * 2 ** Math.min(job.attempts, 10))
        book.run('UPDATE claims SET attempts=attempts+1,next_check=? WHERE invoice=? AND tx=?', Math.min(deadline, now + delay), order.id, tx)
        return { status: 'confirming' }
      }
      const message = transient ? 'Confirmation timed out. Check the transaction ID and submit it again.' : err.message
      book.run('UPDATE claims SET next_check=NULL,error=? WHERE invoice=? AND tx=?', message, order.id, tx)
      throw new Problem(400, message)
    }
    // Inclusion during the quote window suffices even if finalization or a claim is late.
    try {
      return this.atomic(() => book.pay(order.id, order.user, tx, Date.now()))
    } catch (err) {
      if (err instanceof Problem) {
        book.run('UPDATE claims SET next_check=NULL,error=? WHERE invoice=? AND tx=?', err.message, order.id, tx)
      }
      throw err
    }
  }
  async alarm() {
    const book = this.book
    this.atomic(() => book.renewDue(Date.now()))
    const due = book.all('SELECT i.*,c.tx AS submitted_tx FROM claims c JOIN invoices i ON i.id=c.invoice WHERE c.next_check<=? ORDER BY c.next_check LIMIT 4', Date.now())
    await Promise.all(due.map(async row => {
      try {
        await this.verifyClaim(row, row.submitted_tx)
      } catch (err) {
        if (!(err instanceof Problem)) throw err
      }
    }))
    await this.schedule(book.one(`SELECT min(at) AS at FROM (
      SELECT min(next_check) AS at FROM claims UNION ALL SELECT min(ends_at) AS at FROM memberships WHERE renew=1)`).at)
  }
  cookie(value, age) { return `${SESSION}=${value}; Path=/; Max-Age=${age}; HttpOnly; Secure; SameSite=Strict` }
  async internal(action, body, now) {
    const book = this.book
    if (action === 'account') {
      const code = crypto.randomUUID(), hash = await tokenHash(code)
      return this.atomic(() => {
        book.enroll(body.user, now)
        book.identify(body.user, body.profile)
        const detail = book.account(body.user, now)
        book.run("DELETE FROM logins WHERE (user=? AND kind='code') OR expires_at<=?", body.user, now)
        book.run("INSERT INTO logins VALUES (?,?,'code',?)", hash, body.user, now + 600_000)
        const approved = detail.approval_status === 'approved' && !detail.blocked
        const metered = detail.access_mode === 'paid' && approved
        const usage = detail.access_mode === 'closed' ? 'Chat access is closed.' : !approved ? 'Chat access requires an approved account.' : metered
          ? `Available tokens: ${detail.available_tokens}. Weekly allowance: ${detail.remaining_weekly_tokens} of ${detail.allowance_tokens} remaining. Next refill: ${new Date(detail.next_refill).toISOString().slice(0,16)} UTC.`
          : 'Chat is free with no weekly token limit. New purchases are paused.'
        return { text: usage + ` Wallet: ${detail.wallet_tokens} tokens.`
          + ' ' + applicationMessages[detail.approval_status]
          + (detail.blocked ? ' Your account is suspended.' : '')
          + `\n\nPrivate account sign-in code (10 minutes):\n${code}`
          + (metered && !detail.blocked ? '\n\nSend /topup with a USD amount to buy tokens.' + (detail.allowance_tokens ? ' Send /subscribe to get 5× the free weekly allowance.' : '') : '') }
      })
    }
    // Registration survives an admission denial, including when the starter quota is zero.
    if (action === 'access' || action === 'reserve') book.enroll(body.user, now)
    if (action === 'subscribe' && body.quote) await this.schedule(monthsAfter(now, 1))
    return this.atomic(() => {
      switch (action) {
        case 'welcome': {
          book.enroll(body.user, now)
          book.checkMember(body.user)
          return { ok: true }
        }
        case 'apply': {
          const result = book.apply(body.user, body.note ?? '', now)
          book.identify(body.user, body.profile)
          return result
        }
        case 'access': return book.access(body.user, now)
        case 'reserve': return book.reserve(body, now)
        case 'settle': return book.settle(body, now)
        case 'record': return book.record(body)
        case 'support': return { text: book.settings().support }
        case 'topup': return book.showOrder(book.invoice(body.user, 'XTR', now, body.usd === undefined ? '5.00' : body.usd), now)
        case 'subscribe': return book.telegramPlan(body.user, body.quote, now)
        case 'unsubscribe': {
          const member = book.currentMembership(userId(body.user), now)
          if (!member?.renew) return { text: 'You have no automatically renewing subscription.' }
          book.cancel(body.user, member.id)
          return { text: 'Renewal stopped. Your subscription is active through ' + new Date(member.ends_at).toISOString().slice(0,10) + ' UTC.' }
        }
        case 'stars': return book.stars(body, now)
        default: throw new Problem(404, 'Not found.')
      }
    })
  }
  async admin(route, query, body, now, method) {
    const book = this.book, get = method === 'GET', post = method === 'POST'
    if (get && route === 'overview') return json(book.overview(now))
    if ((get || post) && route === 'settings') {
      const settings = get ? book.settings() : this.atomic(() => book.configure(body))
      return json({ ...settings, ...plan(settings), mode: book.paymentOptions(settings).mode })
    }
    if (get && route === 'deposits') return json(book.paymentOptions())
    if (post && route === 'deposits') return json(this.atomic(() => book.setDeposits(body)))
    if (get && route === 'applications') return json(book.applications(Number(query.get('offset') || 0)))
    if (get && route === 'users') return json(book.users(query.get('q') || '', Number(query.get('offset') || 0), query.get('approval') || ''))
    if (post && route === 'users') {
      const user = userId(body.id)
      this.atomic(() => {
        book.enroll(user, now)
        if (body.approve === true) book.reviewAccess(user, 'approved')
      })
      return json({ id: user })
    }
    const match = route.match(/^users\/(\d+)(?:\/(access|adjust|history|files|download|preview|storage|cleanup))?$/)
    check(match, 'Not found.', 404)
    const user = userId(match[1]), action = match[2]
    book.user(user)
    if (post && action === 'access') return json(this.atomic(() => book.reviewAccess(user, body.status, body.expected)))
    if (get && !action) return json(this.atomic(() => book.detail(user, now, query.get('after'))))
    if (post && action === 'adjust') return json(this.atomic(() => book.adjust(user, body, now)))
    if (get && action === 'history') {
      const before = integer(Number(query.get('before') || Number.MAX_SAFE_INTEGER), 1, Number.MAX_SAFE_INTEGER, 'history cursor')
      return json(book.history(user, before, query.get('after_id') || ''))
    }
    if (get && ['files', 'download', 'preview', 'storage'].includes(action)) {
      let prefix, scope
      if (action === 'storage' || (['preview','download'].includes(action) && query.has('kind'))) {
        const kind = query.get('kind')
        check(['artifacts', 'checkpoints'].includes(kind), 'Invalid storage category.')
        // A private Telegram chat has its user's ID. Shared chats can only be browsed through an attributed turn.
        prefix = (kind === 'checkpoints' ? 'sessions' : 'artifacts') + '/tg:' + user + '/'
        scope = { kind }
      } else {
        const turn = book.one('SELECT id,session FROM history WHERE user=? AND id=?', user, uuid(query.get('turn')))
        check(turn, 'Turn not found.', 404)
        prefix = `artifacts/${turn.session}/${turn.id}/`
        scope = { turn: turn.id }
      }
      if (action === 'download' || action === 'preview') {
        const name = query.get('name')
        check(name && name.length <= 1024 && !name.split('/').some(p => !p || p === '.' || p === '..'), 'Invalid file name.')
        if (action === 'preview') {
          check(TEXT_FILE.test(name), 'Download this file to view it.', 415)
          const key = prefix + name
          const metadata = await this.env.STATE.head(key)
          check(metadata, 'File not found.', 404)
          if (!metadata.size) return json({ text: '', truncated: false })
          const object = await this.env.STATE.get(key, { range: { offset: 0, length: PREVIEW_BYTES }, onlyIf: { etagMatches: metadata.etag } })
          check(object, 'File not found.', 404)
          check(object.body, 'The file changed. Open the preview again.', 409)
          const truncated = object.size > PREVIEW_BYTES
          let text
          try { text = new TextDecoder('utf-8', { fatal: true }).decode(await object.arrayBuffer(), { stream: truncated }) }
          catch { throw new Problem(415, 'Download this file to view its original encoding.') }
          check(!text.includes('\0'), 'Download this binary file to view it.', 415)
          return json({ text, truncated })
        }
        const object = await this.env.STATE.get(prefix + name)
        check(object, 'File not found.', 404)
        const filename = encodeURIComponent(name.split('/').at(-1)).replace(/['()*]/g, c => '%' + c.charCodeAt(0).toString(16))
        return new Response(object.body, { headers: { ...NO_STORE, 'Content-Type': 'application/octet-stream',
          'Content-Security-Policy': 'sandbox', 'Content-Disposition': "attachment; filename*=UTF-8''" + filename } })
      }
      const list = await this.env.STATE.list({ prefix, limit: 100, cursor: query.get('cursor') || undefined })
      return json({ files: list.objects.map(o => {
        const name = o.key.slice(prefix.length)
        const path = '/admin/api/users/' + user, params = new URLSearchParams({ ...scope, name })
        return { name, size: o.size, url: path + '/download?' + params,
          preview_url: TEXT_FILE.test(name) ? path + '/preview?' + params : null }
      }), next: list.truncated ? list.cursor : null })
    }
    if (post && action === 'cleanup') {
      check(body.confirm === user, 'Confirm the Telegram user ID before deleting history.')
      const before = integer(body.before, 1, now, 'cleanup cutoff')
      const turns = book.all('SELECT id,session FROM history WHERE user=? AND created_at<? ORDER BY created_at LIMIT 11', user, before)
      let deleted = 0
      for (const turn of turns.slice(0,10)) {
        const prefix = `artifacts/${turn.session}/${turn.id}/`
        const list = await this.env.STATE.list({ prefix, limit: 100 })
        if (list.objects.length) await this.env.STATE.delete(list.objects.map(o => o.key))
        if (list.truncated) return json({ deleted, more: true })
        book.run('DELETE FROM history WHERE id=? AND user=?', turn.id, user)
        deleted++
      }
      book.run('DELETE FROM logins WHERE expires_at<=?', now)
      return json({ deleted, more: turns.length > 10 })
    }
    throw new Problem(405, 'Method not allowed.')
  }
}
