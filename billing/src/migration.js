import { ceilDiv, defaults } from './policy.js'

// The previous ledger stored balances in millionths of a credit. The calling Durable Object
// wraps schema creation and this conversion in the same SQLite transaction.
export function migrateWallet(book) {
  const columns = table => book.all('PRAGMA table_info(' + table + ')').map(row => row.name)
  if (!columns('users').includes('bonus')) return
  const old = JSON.parse(book.one('SELECT body FROM settings WHERE id=1')?.body ?? '{}')
  const ratio = old.tokens_per_credit ?? 1000
  const whole = column => '(' + column + ' * ' + ratio + ')'
  // Split before multiplying to keep even large balances in the SQLite integer range.
  const scaled = column => {
    const magnitude = 'abs(' + column + ')'
    const converted = '(' + magnitude + ' / 1000000 * ' + ratio + ' + (' + magnitude + ' % 1000000 * ' + ratio + ' + 999999) / 1000000)'
    return '(CASE WHEN ' + column + ' < 0 THEN -' + converted + ' ELSE ' + converted + ' END)'
  }
  book.run('ALTER TABLE users RENAME COLUMN bonus TO wallet_tokens')
  book.run('UPDATE users SET wallet_tokens=' + scaled('wallet_tokens'))
  book.run('UPDATE subscriptions SET weekly=' + scaled('weekly') + ',remaining=' + scaled('remaining'))
  book.run('UPDATE invoices SET weekly=' + whole('weekly'))
  if (columns('topups').includes('credits')) {
    book.run('ALTER TABLE topups RENAME COLUMN credits TO tokens')
    book.run('ALTER TABLE topups DROP COLUMN packs')
    book.run('UPDATE topups SET tokens=' + whole('tokens'))
  }
  if (columns('plan_purchases').length) book.run('UPDATE plan_purchases SET price=' + scaled('price'))
  if (columns('plan_quotes').length) book.run('DELETE FROM plan_quotes')
  book.run('ALTER TABLE adjustments RENAME COLUMN credits TO tokens')
  book.run('UPDATE adjustments SET tokens=' + whole('tokens'))
  book.run('ALTER TABLE calls ADD COLUMN legacy_conversion INTEGER')
  book.run('UPDATE calls SET held=' + scaled('held') + ', charged=' + scaled('charged') + ', legacy_conversion=?', ratio)
  // Distribute rounding across the original funding sources so their sum is exactly the migrated hold.
  let cursor = ''
  for (;;) {
    const batch = book.all("SELECT id,allocation FROM calls WHERE id>? AND allocation!='[]' ORDER BY id LIMIT 100", cursor)
    if (!batch.length) break
    for (const call of batch) {
      let sum = 0n, previous = 0
      const allocation = JSON.parse(call.allocation)
      for (const part of allocation) {
        sum += BigInt(part.take)
        const total = Number(ceilDiv(sum * BigInt(ratio), 1_000_000n))
        part.take = total - previous
        previous = total
      }
      book.run('UPDATE calls SET allocation=? WHERE id=?', JSON.stringify(allocation), call.id)
    }
    cursor = batch.at(-1).id
  }
  const settings = { ...defaults, free_weekly_tokens: (old.free_credits ?? 100) * ratio }
  for (const key of ['input_cost','output_cost','cache_write_cost','cache_read_cost','support']) {
    if (old[key] != null) settings[key] = old[key]
  }
  if (old.stars_price && old.pack_usd) settings.stars_per_usd = Math.min(10_000, Math.max(1, Math.round(old.stars_price / Number(old.pack_usd))))
  return settings
}

export function legacyCharge(tokens, call) {
  const oldAmount = ceilDiv(BigInt(tokens) * 1_000_000n, BigInt(call.ratio))
  return Number(ceilDiv(oldAmount * BigInt(call.legacy_conversion), 1_000_000n))
}
