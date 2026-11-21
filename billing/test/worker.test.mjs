import test from 'node:test'
import assert from 'node:assert/strict'
import { billingModules } from './worker-modules.mjs'
import { Miniflare, convertV4MiniflareOptions } from 'miniflare'
import { units } from '../src/policy.js'

const user = '12345', recipient = '0x' + 'ab'.repeat(20), tx = '0x' + 'cc'.repeat(32)
const solAddress = '1'.repeat(31) + '2', signature = '1'.repeat(64)
const post = (body, headers = {}) => ({
  method:'POST', headers:{'content-type':'application/json', ...headers}, body:JSON.stringify(body)
})
async function data(response, expected = 200) {
  const value = await response.json()
  assert.equal(response.status, expected, JSON.stringify(value))
  return value
}
test('Worker authority boundaries, verified checkout, concurrent claims, and scoped R2 cleanup', async t => {
  let quote, solQuote, finalized = false, feedAge = 0, ethChain = '0x1', solSlot = 42
  let pauseNextHead
  const mf = new Miniflare(convertV4MiniflareOptions({ workers: [{
    name:'merchant', ...billingModules, compatibilityDate:'2026-10-01',
    routes:['merchant.test/*'], access:{ aud:'local-billing-admin' },
    durableObjects:{ LEDGER:{className:'BillingLedger',useSQLite:true} }, r2Buckets:['STATE'],
    bindings:{ETH_RPC_URL:'https://rpc.test', SOL_RPC_URL:'https://sol.test'},
    outboundService:async request => {
      const url = new URL(request.url)
      if (url.hostname === 'api.exchange.coinbase.com') {
        assert.match(url.pathname,/^\/products\/(ETH|SOL)-USD\/ticker$/)
        return Response.json({price:url.pathname.includes('ETH') ? '3000' : '200', time:new Date(Date.now()-feedAge).toISOString()})
      }
      const {method,params} = await request.json()
      if (url.hostname === 'sol.test') {
        if (method === 'getGenesisHash') return Response.json({result:'5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d'})
        if (method === 'getSlot') {
          assert.equal(params[0].commitment, 'finalized')
          return Response.json({result:solSlot})
        }
        assert.equal(method,'getTransaction')
        assert.equal(params[1].maxSupportedTransactionVersion, 1)
        const lamports = Number(units(solQuote.amount,9)), source = '1'.repeat(31)+'3'
        return Response.json({result:{blockTime:Math.floor(solQuote.created_at/1000),
          meta:{err:null,preBalances:[lamports+5000,0],postBalances:[0,lamports]},
          transaction:{signatures:[signature],message:{accountKeys:[{pubkey:source,signer:true},{pubkey:solAddress}],instructions:[
            {programId:'MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr',parsed:solQuote.reference},
            {programId:'11111111111111111111111111111111',parsed:{type:'transfer',info:{source,destination:solAddress,lamports}}}
          ]}}}})
      }
      if (method === 'eth_chainId') return Response.json({result:ethChain})
      if (method === 'eth_getBlockByNumber' && params[0] === 'finalized') {
        const pause = pauseNextHead
        pauseNextHead = null
        if (pause) await pause()
        return Response.json({result:{hash:'head',number:finalized ? '0x11' : '0x09'}})
      }
      const block = {hash:'block',number:'0x10',timestamp:'0x'+Math.floor(quote.created_at/1000).toString(16)}
      const replies = {
        eth_getTransactionByHash:{hash:tx,to:recipient,from:'0x'+'de'.repeat(20),blockHash:'block',
          value:'0x'+units(quote.amount,18).toString(16),input:quote.reference},
        eth_getTransactionReceipt:{status:'0x1',transactionHash:tx,blockNumber:'0x10',blockHash:'block'},
        eth_getBlockByNumber:block
      }
      return Response.json({jsonrpc:'2.0',id:1,result:replies[method]})
    }
  },{
    name:'private-client', modules:true, compatibilityDate:'2026-10-01',
    script:'export default { fetch(request, env) { return env.BILLING.fetch(request) } }',
    serviceBindings:{BILLING:{name:'merchant',entrypoint:'GatewayBilling'}}
  }] }))
  t.after(() => mf.dispose())
  const web = await mf.getWorker('merchant'), internal = await mf.getWorker('private-client')
  const get = (path, headers = {}) => web.fetch('https://merchant.test'+path, {headers})
  const send = (path, body, headers = {}) => web.fetch('https://merchant.test'+path, post(body, headers))
  // The ingress simulates an Access-authenticated request. Direct service calls
  // have no Access context, including when they carry a customer session cookie.
  const adminGet = path => mf.dispatchFetch('https://merchant.test'+path)
  const adminPost = (path, body, headers = {}) => mf.dispatchFetch('https://merchant.test'+path, post(body, headers))
  const gateway = (path, body) => internal.fetch('https://billing.test'+path, post(body))

  assert.equal((await send('/internal/access',{user})).status,404)
  assert.equal((await get('/admin')).status,403)
  assert.equal((await get('/admin/api/users')).status,403)
  assert.equal((await adminGet('/admin')).status,200)
  assert.match(await (await get('/')).text(), /Sign-in code/)
  assert.match((await get('/assets/admin.js')).headers.get('content-type'), /javascript/)
  for (const [file, type, signature] of [['wallet-cat.webp','image/webp','RIFF'],['nunito-latin.woff2','font/woff2','wOF2']]) {
    const asset = await get('/assets/'+file)
    assert.equal(asset.headers.get('content-type'), type)
    assert.equal(Buffer.from(await asset.arrayBuffer()).toString('ascii',0,4), signature)
  }
  assert.equal((await get('/api/account')).status,401)
  const publicPrices = await data(await get('/api/pricing'))
  assert.equal(publicPrices.plan.input_usd,'1.00')
  assert.deepEqual(publicPrices.chains,[])
  assert.equal(publicPrices.wallet_tokens,undefined)
  assert.equal((await adminPost('/admin/api/users',{id:user},{origin:'https://unrelated.test'})).status,403)
  assert.equal((await gateway('/admin/api/settings',{})).status,404)

  const {text} = await data(await gateway('/account',{user}))
  const code = text.match(/[0-9a-f-]{36}/)[0]
  const login = await send('/api/login',{code})
  const cookieHeader = login.headers.get('set-cookie')
  assert.match(cookieHeader,/HttpOnly; Secure; SameSite=Strict/)
  const customer = { cookie:cookieHeader.split(';')[0] }
  assert.equal((await send('/api/login',{code})).status,401)
  assert.equal((await data(await get('/api/account',customer))).id,user)
  const blocked = await get('/admin/api/users/'+user,customer)
  assert.equal(blocked.status,403)

  assert.deepEqual((await data(await get('/api/account',customer))).chains,[])
  assert.equal((await send('/api/invoice',{chain:'ETH',usd:'10'},customer)).status,503)
  const deposit = {eth_recipient:recipient,sol_recipient:solAddress}
  assert.equal((await adminPost('/admin/api/deposits',{...deposit,sol_recipient:'a private key'})).status,400)
  assert.equal((await data(await adminGet('/admin/api/deposits'))).eth_recipient,'')
  assert.equal((await data(await adminPost('/admin/api/deposits',deposit))).networks.SOL.ready,true)
  ethChain = '0xaa36a7'
  await data(await send('/api/invoice',{chain:'ETH',usd:'30'},customer),503)
  ethChain = '0x1'
  solSlot = null
  await data(await send('/api/invoice',{chain:'SOL',usd:'10'},customer),503)
  solSlot = 42
  assert.deepEqual((await data(await get('/api/account',customer))).invoices,[])
  feedAge = 600_000
  assert.equal((await send('/api/invoice',{chain:'ETH',usd:'30'},customer)).status,503)
  feedAge = 0
  quote = await data(await send('/api/invoice',{chain:'ETH',usd:'30'},customer))
  assert.equal(quote.usd,'30.00')
  assert.equal(quote.amount,'0.01')
  assert.equal(quote.rate_usd,'3000')
  assert.equal(quote.tokens,30_000_000)
  assert.equal(quote.product,'tokens')
  assert.equal(quote.expires_at-quote.created_at,900_000)
  const started = Promise.withResolvers(), resume = Promise.withResolvers()
  pauseNextHead = () => { started.resolve(); return resume.promise }
  const reused = send('/api/invoice',{chain:'ETH',usd:'30'},customer)
  await started.promise
  try {
    await data(await adminPost('/admin/api/deposits',{...deposit,eth_recipient:''}))
    const claims = await Promise.all([1,2].map(() => send('/api/claim',{id:quote.id,tx},customer).then(r => data(r,202))))
    assert.deepEqual(claims.map(r => r.status),['confirming','confirming'])
  } finally {
    resume.resolve()
  }
  const refreshed = await data(await reused)
  assert.equal(refreshed.id,quote.id)
  assert.equal(refreshed.status,'confirming')
  assert.equal(refreshed.tx,tx)
  assert.equal((await data(await get('/api/account',customer))).invoices[0].status,'confirming')
  finalized = true
  // No further claims are submitted. The persisted alarm settles after finality on its own.
  let account
  for (let attempt = 0; attempt < 40; attempt++) {
    account = await data(await get('/api/account',customer))
    if (account.wallet_tokens === 30_000_000) break
    await new Promise(resolve => setTimeout(resolve,250))
  }
  assert.deepEqual(await data(await send('/api/claim',{id:quote.id,tx},customer)),{status:'paid'})
  assert.equal(account.subscriptions.length,0)
  assert.equal(account.available_tokens,30_100_000)
  assert.equal((await data(await adminGet('/admin/api/overview'))).payments,1)
  const order = {offer:account.plan.offer,key:crypto.randomUUID()}
  const confirmations = await Promise.all([1,2].map(() => send('/api/subscribe',order,customer).then(r=>data(r))))
  assert.deepEqual(confirmations,[{status:'active'},{status:'active'}])
  assert.equal((await data(await get('/api/account',customer))).wallet_tokens,29_000_000)
  solQuote = await data(await send('/api/invoice',{chain:'SOL',usd:'10.00'},customer))
  assert.equal(solQuote.amount,'0.05')
  assert.deepEqual(await data(await send('/api/claim',{id:solQuote.id,tx:signature},customer)),{status:'paid'})
  assert.equal((await data(await get('/api/account',customer))).wallet_tokens,39_000_000)
  assert.equal((await data(await adminGet('/admin/api/overview'))).sales_usd,'40.00')

  const paid = await data(await get('/api/account',customer))
  const older = await data(await get('/api/account?after='+solQuote.id,customer))
  assert.deepEqual(older.invoices.map(i=>i.id),[quote.id])
  assert.equal(older.invoices_next,null)
  const olderForAdmin = await data(await adminGet('/admin/api/users/'+user+'?after='+solQuote.id))
  assert.deepEqual(olderForAdmin.payments,older.invoices)
  assert.equal(olderForAdmin.payments_next,null)
  assert.equal(paid.tier,'subscriber')
  assert.equal(paid.allowance_tokens,500_000)
  assert.equal(paid.subscriptions[0].auto_renew,true)
  assert.equal((await send('/api/subscribe',{...order,key:crypto.randomUUID()},customer)).status,409)
  assert.equal((await data(await send('/api/cancel',{id:paid.subscriptions[0].id},customer))).status,'cancelled')
  const cancelled = await data(await get('/api/account',customer))
  assert.equal(cancelled.tier,'subscriber')
  assert.equal(cancelled.subscriptions[0].auto_renew,false)
  assert.equal(cancelled.wallet_tokens,paid.wallet_tokens)

  const turn = crypto.randomUUID(), session = 'tg:-23', at = Date.now()-1000
  const bucket = await mf.getR2Bucket('STATE','merchant')
  await data(await gateway('/record',{user,id:turn,session,prompt:'request',answer:'report',is_error:false,created_at:at}))
  const saved = 'artifacts/'+session+'/'+turn+'/report.txt'
  await bucket.put(saved, 'generated content')
  await bucket.put('sessions/tg:'+user+'/state.tar.gz','older private checkpoint')
  await bucket.put('sessions/tg:'+user+'9/state.tar.gz','another user')
  const path = '/admin/api/users/'+user
  const files = await data(await adminGet(path+'/files?turn='+turn))
  assert.equal(await (await adminGet(files.files[0].url)).text(),'generated content')
  assert.deepEqual(await data(await adminGet(files.files[0].preview_url)),{text:'generated content',truncated:false})
  assert.equal((await get(files.files[0].preview_url,customer)).status,403)
  await bucket.put('artifacts/tg:'+user+'/long.txt','X'.repeat(40000))
  const preview = await data(await adminGet(path+'/preview?kind=artifacts&name=long.txt'))
  assert.equal(preview.text.length,32768)
  assert.equal(preview.truncated,true)
  await bucket.put('artifacts/tg:'+user+'/empty.txt','')
  assert.deepEqual(await data(await adminGet(path+'/preview?kind=artifacts&name=empty.txt')),{text:'',truncated:false})
  assert.equal((await adminGet(path+'/preview?kind=artifacts&name=../secret.txt')).status,400)
  assert.equal((await get(files.files[0].url,customer)).status,403)
  const archives = await data(await adminGet(path+'/storage?kind=checkpoints'))
  assert.equal(archives.files.length,1)
  assert.equal(await (await adminGet(archives.files[0].url)).text(),'older private checkpoint')
  assert.equal((await adminPost(path+'/cleanup',{before:at+1,confirm:'another user'})).status,400)
  assert.deepEqual(await data(await adminPost(path+'/cleanup',{before:at+1,confirm:user})),{deleted:1,more:false})
  assert.equal(await bucket.get(saved),null)
  assert.equal((await data(await adminGet(path+'/history'))).turns.length,0)
  assert.equal((await data(await adminGet(path))).subscriptions.length,1)
  assert.equal((await data(await adminGet(path+'/storage?kind=checkpoints'))).files.length,1)
  await data(await send('/api/logout',{},customer))
  assert.equal((await get('/api/account',customer)).status,401)
})

test('the Durable Object alarm renews an idle subscriber once without a customer request', async t => {
  // Shorten just the first term in isolated storage. The Worker handles scheduling, charging
  // and renewal through the deployed implementation; the observer only reads its receipts.
  const harness = {type:'ESModule',path:billingModules.modulesRoot+'src/test-clock.js',contents:`
    import worker, { BillingLedger as Ledger, GatewayBilling } from './worker.js'
    export default worker
    export { GatewayBilling }
    export class BillingLedger extends Ledger {
      async fetch(req) {
        const path = new URL(req.url).pathname
        if (path === '/test/soon') {
          const first = await this.ctx.storage.getAlarm()
          const original = this.book.one('SELECT ends_at FROM memberships').ends_at
          const due = Date.now() + 150
          this.book.run('UPDATE memberships SET ends_at=?', due)
          await this.ctx.storage.setAlarm(due)
          return Response.json({first, original})
        }
        if (path === '/test/receipts') {
          return Response.json({count:this.book.one('SELECT count(*) AS n FROM membership_charges').n,
            wallet:this.book.user('12345').wallet_tokens, alarm:await this.ctx.storage.getAlarm()})
        }
        return super.fetch(req)
      }
    }`}
  const mf = new Miniflare(convertV4MiniflareOptions({ workers:[{
    name:'renewal',modulesRoot:billingModules.modulesRoot,modules:[harness,...billingModules.modules],
    compatibilityDate:'2026-10-01',durableObjects:{LEDGER:{className:'BillingLedger',useSQLite:true}}
  }] }))
  t.after(() => mf.dispose())
  const namespace = await mf.getDurableObjectNamespace('LEDGER','renewal')
  const stub = namespace.get(namespace.idFromName('service'))
  const send = (path,body,headers) => stub.fetch('https://ledger'+path,post(body,headers))
  const {text} = await data(await send('/internal/account',{user}))
  const login = await send('/api/login',{code:text.match(/[a-f0-9-]{36}/)[0]})
  const headers = {cookie:login.headers.get('set-cookie').split(';')[0]}
  await data(await send('/admin/api/users/'+user+'/adjust',{action:'airdrop',tokens:2_000_000,reason:'Fixture',key:crypto.randomUUID()}))
  const {plan} = await data(await stub.fetch('https://ledger/api/pricing'))
  await data(await send('/api/subscribe',{offer:plan.offer,key:crypto.randomUUID()},headers))
  const {first,original} = await data(await stub.fetch('https://ledger/test/soon'))
  assert.equal(first,original)
  let state
  for (let attempt=0;attempt<80;attempt++) {
    state=await data(await stub.fetch('https://ledger/test/receipts'))
    if (state.count===2 && state.alarm>Date.now()) break
    await new Promise(resolve=>setTimeout(resolve,50))
  }
  assert.equal(state.count,2)
  assert.equal(state.wallet,0)
  assert.ok(state.alarm>Date.now()+20*86_400_000)
})
