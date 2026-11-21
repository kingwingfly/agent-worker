import test from 'node:test'
import assert from 'node:assert/strict'
import { transactionId, verify, reference } from '../src/chain.js'

const id = crypto.randomUUID(), sec = 1_800_000_000
const quote = { id, created_at:sec*1000, expires_at:(sec+3600)*1000 }
const eth = { ...quote, chain:'ETH', recipient:'0x'+'ab'.repeat(20), amount:'0.000000000000000001', reference:reference('ETH',id) }
const tx = '0x'+'cb'.repeat(32)
const signature = '1'.repeat(64), source = 'A'.repeat(32), destination = 'B'.repeat(32)
const sol = { ...quote, chain:'SOL', recipient:destination, amount:'0.05', reference:id }
const system = '11111111111111111111111111111111', memo = 'MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr'
function ethereum(values = {}) {
  const block = {hash:'block', number:'0xe9', timestamp:'0x'+sec.toString(16)}
  const data = {
    eth_chainId:'0x1', eth_getTransactionByHash:{hash:tx, from:'0x'+'de'.repeat(20), to:eth.recipient, input:eth.reference, value:'0x1', blockHash:'block'},
    eth_getTransactionReceipt:{transactionHash:tx, blockHash:'block', blockNumber:'0xe9',status:'0x1'}, canonical:block, final:block, ...values
  }
  return (_, method, args) => data[method === 'eth_getBlockByNumber' ? args[0] === 'finalized' ? 'final' : 'canonical' : method]
}
function solana(change, genesis = '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d', blockTime = null) {
  const result = {transaction:{signatures:[signature], message:{accountKeys:[{pubkey:source,signer:true},{pubkey:destination,signer:false}], instructions:[
    {programId:memo,parsed:id},{programId:system,parsed:{type:'transfer',info:{source,destination,lamports:50_000_000}}}
  ]}},meta:{err:null,preBalances:[100_000_000,100],postBalances:[49_900_000,50_000_100]},blockTime:sec}
  change?.(result)
  return (_,method,params) => {
    if (method === 'getGenesisHash') return genesis
    if (method === 'getBlockTime') {
      assert.deepEqual(params, [123])
      return blockTime
    }
    assert.equal(params[1].commitment, 'finalized')
    assert.equal(params[1].maxSupportedTransactionVersion, 1)
    return result
  }
}
test('ETH requires the quoted native wei, reference, mainnet and finalized canonical receipt', async () => {
  assert.equal(transactionId('ETH',tx.toUpperCase()),tx)
  await verify(eth, tx, {}, ethereum())
  for (const overrides of [{eth_chainId:'0x2'}, {eth_getTransactionReceipt:null}, {canonical:{hash:'orphan'}}, {final:{number:'0x1'}}]) {
    await assert.rejects(verify(eth,tx,{},ethereum(overrides)))
  }
  for (const change of [{amount:'0.1'},{recipient:'0x'+'00'.repeat(20)},{reference:'0x'},{expires_at:(sec-1)*1000}]) {
    await assert.rejects(verify({...eth,...change},tx,{},ethereum()))
  }
})
test('SOL requires a native system transfer, exact memo, real signer, net receipt and mainnet finality', async () => {
  assert.equal(transactionId('SOL',signature),signature)
  await verify(sol,signature,{},solana())
  for (const programId of [memo,'Memo4c2pN8afCj432Lb7RMVKi9PbQnnW7ewFFaV3oAH','Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo']) {
    await verify(sol,signature,{},solana(r=>{
      r.transaction.message.instructions[0].programId=programId
      r.transaction.message.instructions.reverse()
    }))
  }
  const noTimestamp = r=>{r.blockTime=null; r.slot=123}
  await verify(sol,signature,{},solana(noTimestamp,undefined,sec-3))
  await assert.rejects(verify(sol,signature,{},solana(noTimestamp)), {status:409})
  await assert.rejects(verify(sol,signature,{},solana(null,'devnet')))
  for (const change of [
    r=>{r.meta.err={failed:true}}, r=>{r.meta.postBalances[1]=100}, r=>{r.transaction.signatures[0]='another'},
    r=>{r.transaction.message.instructions[0].parsed='another'},
    r=>{r.transaction.message.instructions[1].programId='another'},
    r=>{r.transaction.message.instructions[1].parsed.info.lamports=49_999_999},
    r=>{r.transaction.message.accountKeys[0].signer=false}, r=>{r.blockTime=sec+3601},
    r=>{r.transaction.message.instructions.push({programId:memo,parsed:'another invoice'})},
    r=>{r.transaction.message.instructions.push(r.transaction.message.instructions[1])}
  ]) await assert.rejects(verify(sol,signature,{},solana(change)))
})
