export const WEEK = 7 * 86_400_000
export const HOUR = 3_600_000
export const defaults = {
  access_mode: 'closed', allow_stars_only: false, input_usd: '1.00', output_usd: '2.00',
  free_weekly_tokens: 100_000, monthly_usd: '1.00', stars_per_usd: 50,
  input_cost: 0, output_cost: 0, cache_write_cost: 0, cache_read_cost: 0,
  support: 'Contact the operator who provided your account.'
}

export class Problem extends Error {
  constructor(status, message) { super(message); this.status = status }
}
export function check(condition, message, status = 400) {
  if (!condition) throw new Problem(status, message)
}
export function integer(value, min, max, label) {
  check(Number.isSafeInteger(value) && value >= min && value <= max, 'Invalid ' + label + '.')
  return value
}
export function userId(value) {
  check(typeof value === 'string' && /^[1-9]\d{0,15}$/.test(value) && Number.isSafeInteger(Number(value)), 'Invalid Telegram user ID.')
  return value
}
export function uuid(value) {
  check(typeof value === 'string' && /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value), 'Invalid reference.')
  return value
}
export function units(value, decimals) {
  check(typeof value === 'string' && value.length <= 40 && /^(0|[1-9]\d*)(\.\d+)?$/.test(value), 'Use a positive decimal price.')
  const [whole, fraction = ''] = value.split('.')
  check(fraction.length <= decimals, 'Price has too many decimal places.')
  return BigInt(whole + fraction.padEnd(decimals, '0'))
}
export function dollars(cents) {
  return (BigInt(cents) / 100n).toString() + '.' + (BigInt(cents) % 100n).toString().padStart(2, '0')
}
export function amountCents(usd) {
  const cents = units(usd, 2)
  check(cents >= 1n && cents <= 1_000_000n, 'Enter a USD amount between $0.01 and $10,000.')
  return Number(cents)
}
export function settingsFrom(body) {
  const value = Object.fromEntries(Object.keys(defaults).map(key => [key, body[key]]))
  check(['closed', 'free', 'paid'].includes(value.access_mode), 'Choose closed, free or paid access.')
  check(typeof value.allow_stars_only === 'boolean', 'Choose whether to allow Stars-only billing.')
  integer(value.free_weekly_tokens, 0, 1_000_000_000_000, 'free weekly tokens')
  integer(value.stars_per_usd, 1, 10_000, 'Stars per dollar')
  value.monthly_usd = dollars(amountCents(value.monthly_usd))
  for (const key of ['input_usd', 'output_usd']) {
    const rate = units(value[key], 6)
    check(rate >= 10_000n && rate <= 10_000_000_000n, 'Set the rate per million tokens between $0.01 and $10,000.')
    const [whole, fraction = ''] = value[key].split('.')
    value[key] = whole + '.' + fraction.replace(/0+$/, '').padEnd(2, '0')
  }
  for (const key of ['input_cost', 'output_cost', 'cache_write_cost', 'cache_read_cost']) {
    check(Number.isFinite(value[key]) && value[key] >= 0 && value[key] <= 1_000_000, 'Invalid provider cost.')
  }
  check(typeof value.support === 'string' && value.support.trim().length > 0 && value.support.length <= 500, 'Enter support contact information.')
  return value
}

export const ceilDiv = (n, d) => (n + d - 1n) / d
// The wallet holds whole tokens at the input rate. Round only the final result of each operation.
export function tokensForUsd(cents, inputUsd) {
  const numerator = BigInt(cents) * 10_000n * 1_000_000n
  const rate = units(inputUsd, 6)
  return Number(numerator / rate)
}
export function usageTokens(input, output, rates) {
  return input + Number(ceilDiv(BigInt(output) * units(rates.output_usd, 6), units(rates.input_usd, 6)))
}
export function outputLimit(availableAfterInput, requested, rates) {
  const limit = BigInt(availableAfterInput) * units(rates.input_usd, 6) / units(rates.output_usd, 6)
  return limit < BigInt(requested) ? Number(limit) : requested
}
export function cost(usage, rates) {
  return (usage.input_tokens * rates.input_cost + usage.output_tokens * rates.output_cost
    + usage.cache_creation_input_tokens * rates.cache_write_cost + usage.cache_read_input_tokens * rates.cache_read_cost) / 1_000_000
}
export function plan(settings) {
  const { input_usd, output_usd, free_weekly_tokens, monthly_usd, support, stars_per_usd } = settings
  return { input_usd, output_usd, free_weekly_tokens, monthly_usd, support, stars_per_usd,
    paid_weekly_tokens: free_weekly_tokens * 5, plan_price_tokens: tokensForUsd(amountCents(monthly_usd), input_usd),
    offer: ['monthly', input_usd, output_usd, free_weekly_tokens, monthly_usd].join('/') }
}
export const exhausted = 'There are too few tokens for this request. Send /topup in private to add tokens or /account to see your next weekly refill.'
export const accessClosed = 'Chat access is closed. The administrator must open access before you can chat or make a new purchase. Send /apply in private to request approval.'
export const applicationMessages = {
  none: 'You need approval to use chat. Send /apply in private to request access.',
  pending: 'Your access application is waiting for admin review.',
  approved: 'Your Telegram account is on the whitelist.',
  declined: 'Your access application was declined. Contact the operator via /paysupport for help.',
  revoked: 'Your chat access was revoked. Contact the operator via /paysupport for help.'
}
