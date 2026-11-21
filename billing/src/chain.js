import { check, Problem, units } from './policy.js'

const SYSTEM = '11111111111111111111111111111111'
const MEMO_PROGRAMS = new Set([
  'Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo',
  'MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr',
  'Memo4c2pN8afCj432Lb7RMVKi9PbQnnW7ewFFaV3oAH'
])
const BASE58 = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'

function base58Size(text) {
  if (typeof text !== 'string' || !text.length || text.length > 90) return 0
  let value = 0n
  for (const char of text) {
    const digit = BASE58.indexOf(char)
    if (digit < 0) return 0
    value = value * 58n + BigInt(digit)
  }
  const leading = text.match(/^1*/)[0].length
  return leading + (value ? Math.ceil(value.toString(16).length / 2) : 0)
}

export function validAddress(chain, address) {
  if (chain === 'ETH') return /^0x[\da-f]{40}$/i.test(address) && !/^0x0{40}$/i.test(address)
  return chain === 'SOL' && base58Size(address) === 32 && address !== SYSTEM
}

export async function spotRate(chain) {
  try {
    const response = await fetch('https://api.exchange.coinbase.com/products/' + chain + '-USD/ticker', {
      headers: { Accept: 'application/json' }, signal: AbortSignal.timeout(7000)
    })
    if (!response.ok) throw new Error()
    const { price, time } = await response.json()
    const age = Date.now() - Date.parse(time)
    if (!Number.isFinite(age) || age < -60_000 || age > 300_000 || units(price, 12) <= 0n) throw new Error()
    return price
  } catch {
    throw new Problem(503, 'A fresh USD exchange rate is unavailable. Try again shortly.')
  }
}

export function nativeAmount(chain, cents, rate) {
  const places = chain === 'ETH' ? 18 : 9
  const divisor = 100n * units(rate, 12)
  check(divisor > 0n, 'Invalid exchange rate.')
  const amount = (BigInt(cents) * 10n ** BigInt(places + 12) + divisor - 1n) / divisor
  if (chain === 'SOL') check(amount <= BigInt(Number.MAX_SAFE_INTEGER), 'This invoice is too large for Solana. Enter a smaller dollar amount.')
  const digits = amount.toString().padStart(places + 1, '0')
  const decimals = digits.slice(-places).replace(/0+$/, '')
  return digits.slice(0, -places) + (decimals ? '.' + decimals : '')
}

export function reference(chain, id) {
  if (chain !== 'ETH') return id
  return '0x' + Array.from(new TextEncoder().encode(id), b => b.toString(16).padStart(2, '0')).join('')
}

async function rpc(url, method, params) {
  try {
    const response = await fetch(url, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }), signal: AbortSignal.timeout(12_000)
    })
    if (!response.ok) throw new Error()
    const reply = await response.json()
    if (reply.error || !Object.hasOwn(reply, 'result')) throw new Error()
    return reply.result
  } catch {
    throw new Problem(503, 'The payment network is unavailable. Keep your transaction ID and check again.')
  }
}

export function transactionId(chain, tx) {
  check(chain === 'ETH' ? typeof tx === 'string' && /^0x[\da-f]{64}$/i.test(tx) : chain === 'SOL' && base58Size(tx) === 64, 'Enter the full transaction hash or signature.')
  return chain === 'ETH' ? tx.toLowerCase() : tx
}

async function ethereumHead(ask) {
  const [chain, head] = await Promise.all([ask('eth_chainId'), ask('eth_getBlockByNumber', ['finalized', false])])
  check(chain === '0x1', 'The payment RPC is not on Ethereum mainnet.', 503)
  check(head?.number && head.hash, 'The Ethereum RPC cannot read finalized blocks.', 503)
  return head
}

async function checkSolanaMainnet(ask) {
  const genesis = await ask('getGenesisHash')
  check(genesis === '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d', 'The payment RPC is not on Solana mainnet.', 503)
}

export async function checkPaymentNetwork(chain, env) {
  const ask = (method, params = []) => rpc(env[chain + '_RPC_URL'], method, params)
  if (chain === 'ETH') return ethereumHead(ask)
  const [, slot] = await Promise.all([checkSolanaMainnet(ask), ask('getSlot', [{commitment:'finalized'}])])
  check(Number.isSafeInteger(slot) && slot > 0, 'The Solana RPC cannot read finalized slots.', 503)
}

// Verify a finalized native transfer and its invoice reference using the merchant's RPC.
// The caller re-reads the invoice and records the unique transaction inside one SQLite transaction.
export async function verify(invoice, tx, env, call = rpc) {
  const mismatch = 'This transfer does not match the invoice amount, destination or reference.'
  const pending = 'The transfer is not finalized yet. Check the network and try again.'
  let time
  if (invoice.chain === 'ETH') {
    const ask = (method, params = []) => call(env.ETH_RPC_URL, method, params)
    const [finalized, transfer, receipt] = await Promise.all([
      ethereumHead(ask), ask('eth_getTransactionByHash', [tx]), ask('eth_getTransactionReceipt', [tx])
    ])
    check(receipt?.blockNumber && receipt.blockHash && transfer?.blockHash === receipt.blockHash, pending, 409)
    check(BigInt(receipt.blockNumber) <= BigInt(finalized.number), pending, 409)
    const block = await ask('eth_getBlockByNumber', [receipt.blockNumber, false])
    check(block?.hash && block.hash === receipt.blockHash, pending, 409)
    // A receipt can change before finality. Only a canonical, finalized result is definitive.
    check(receipt.status === '0x1' && receipt.transactionHash?.toLowerCase() === tx && transfer.hash?.toLowerCase() === tx
      && transfer.to?.toLowerCase() === invoice.recipient.toLowerCase() && transfer.from?.toLowerCase() !== invoice.recipient.toLowerCase()
      && transfer.input?.toLowerCase() === invoice.reference && BigInt(transfer.value) === units(invoice.amount, 18), mismatch)
    time = Number(BigInt(block.timestamp))
  } else {
    const ask = (method, params = []) => call(env.SOL_RPC_URL, method, params)
    const [, result] = await Promise.all([checkSolanaMainnet(ask), ask('getTransaction', [tx, {
      commitment: 'finalized', encoding: 'jsonParsed', maxSupportedTransactionVersion: 1
    }])])
    check(result, pending, 409)
    const message = result.transaction?.message
    check(result.meta?.err === null && result.transaction.signatures?.[0] === tx && Array.isArray(message?.instructions), mismatch)
    const keys = message.accountKeys
    const destination = keys.findIndex(key => key.pubkey === invoice.recipient)
    const amount = units(invoice.amount, 9)
    // Solana Pay puts the memo before the transfer. Other wallets append it.
    // Require one invoice memo and one direct payment to this merchant in the same signed transaction.
    const memos = message.instructions.filter(ix => MEMO_PROGRAMS.has(ix.programId))
    const transfers = message.instructions.filter(ix =>
      ix.programId === SYSTEM && ix.parsed?.type === 'transfer' && ix.parsed.info?.destination === invoice.recipient)
    check(memos.length === 1 && memos[0].parsed === invoice.reference && transfers.length === 1, mismatch)
    const info = transfers[0].parsed.info
    const paid = info.source !== invoice.recipient && keys.some(key => key.pubkey === info.source && key.signer)
      && Number.isSafeInteger(info.lamports) && BigInt(info.lamports) === amount
    const before = result.meta.preBalances?.[destination], after = result.meta.postBalances?.[destination]
    check(paid && Number.isSafeInteger(before) && Number.isSafeInteger(after) && BigInt(after) - BigInt(before) >= amount, mismatch)
    time = result.blockTime
    if (time == null && Number.isSafeInteger(result.slot)) time = await ask('getBlockTime', [result.slot])
  }
  check(Number.isSafeInteger(time), 'The block timestamp is not available yet.', 409)
  // The unique random invoice reference proves the transfer was prepared after issuance.
  // Chain timestamps can lag the merchant's clock at issuance. The quote's deadline still applies.
  check(time > 0 && time <= Math.floor(invoice.expires_at / 1000),
    'The transfer landed outside the invoice window. Contact payment support with your transaction ID.')
}
