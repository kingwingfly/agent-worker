// Run after worker-build. Exercise the compiled Rust gateway, SessionDO and real billing binding together.
import test from 'node:test'
import assert from 'node:assert/strict'
import { createHmac } from 'node:crypto'
import { fileURLToPath } from 'node:url'
import { billingModules } from './worker-modules.mjs'
import { Miniflare, convertV4MiniflareOptions } from 'miniflare'

const build = fileURLToPath(new URL('../../crates/gateway/build/', import.meta.url))
const secret = 'session-test-key-for-local-runtime'
const post = (body, headers={}) => ({ method:'POST', headers:{'content-type':'application/json',...headers}, body:JSON.stringify(body) })
const person = 12345, group = -44
const message = (text, chat=person) => ({ message:{message_id:18,chat:{id:chat,type:chat===person?'private':'group'},from:{id:person},text} })
function token(session) {
  const payload = 'v1.' + Buffer.from(session).toString('base64url') + '.' + (Math.floor(Date.now()/1000)+3600)
  return payload+'.'+createHmac('sha256',secret).update(payload).digest('base64url')
}

test('compiled gateway enforces cross-chat quota, pays out Stars receipts and preserves unbilled deployments',async t=>{
  const sent=[], models=[], tools=[]
  const gateway = enabled => ({
    name:enabled?'paid':'free', compatibilityDate:'2026-10-01', modulesRoot:build,
    modules:[{type:'ESModule',path:build+'index.js'},{type:'CompiledWasm',path:build+'index_bg.wasm'}],
    durableObjects:{SESSION:{className:'SessionDO',useSQLite:true}},r2Buckets:{STATE:'gateway-state'},
    serviceBindings:enabled?{BILLING:{name:'merchant',entrypoint:'GatewayBilling'}}:{},outboundService:'upstreams',
    bindings:{BILLING_ENABLED:String(enabled),ALLOWED_TG_USERS:'*',PUBLIC_ORIGIN:'https://agent.test',LLM_BASE_URL:'https://model.test',
      MODEL:'large',SMALL_MODEL:'small',LLM_API_KEY:'test-model-key',TELEGRAM_API_BASE:'https://tg.test',TELEGRAM_BOT_TOKEN:'test-bot',
      TELEGRAM_WEBHOOK_SECRET:'test-webhook',SESSION_TOKEN_SECRET:secret,DEV_RUNNER_URL:'https://runner.test',DEV_MCP_URL:'https://mcp.test'}
  })
  const mf=new Miniflare(convertV4MiniflareOptions({workers:[gateway(true),gateway(false),{
    name:'merchant', ...billingModules, compatibilityDate:'2026-10-01',
    routes:['bill.test/*'], access:{ aud:'local-billing-admin' },
    durableObjects:{LEDGER:{className:'BillingLedger',useSQLite:true}},r2Buckets:{STATE:'gateway-state'}
  },{
    name:'upstreams', compatibilityDate:'2026-10-01',modules:true,
    script:`export default {fetch(req,env) {
      if(new URL(req.url).hostname==='runner.test') {
        if(req.headers.get('upgrade')==='websocket') {
          const [client,server]=Object.values(new WebSocketPair()); server.accept();
          server.addEventListener('message', event => {
            const frame=JSON.parse(event.data)
            if(frame.type==='user_message' && frame.text==='build a report') {
              server.send(JSON.stringify({type:'turn_done',seq:1,turn_id:frame.turn_id,
                result:{is_error:false,subtype:'success',text:'The report is ready.',artifacts:[{path:'report.txt',size:11}]}}))
            }
          })
          return new Response(null,{status:101,webSocket:client})
        }
        if(new URL(req.url).pathname==='/files/out/report.txt') return new Response('test report')
        if(req.method==='GET'&&new URL(req.url).pathname==='/state') return new Response('state',{headers:{'content-length':'5'}})
        return new Response(null,{status:201})
      }
      return env.FIXTURE.fetch(req)
    }}`,
    serviceBindings:{FIXTURE:async req=>{
      const url=new URL(req.url)
      if(url.hostname==='mcp.test') {
        tools.push(url.pathname)
        return Response.json({ok:true})
      }
      if(url.hostname==='tg.test'&&url.pathname.endsWith('/sendDocument')) return Response.json({ok:true,result:{message_id:50}})
      const body=await req.json()
      if(url.hostname==='tg.test'){
        sent.push({method:url.pathname.split('/').at(-1),body})
        return Response.json({ok:true,result:{message_id:sent.length}})
      }
      assert.equal(url.hostname,'model.test')
      models.push({path:url.pathname,query:url.search,body})
      if(url.pathname.endsWith('/count_tokens')) return Response.json({input_tokens:4})
      if(body.stream) return new Response('event: error\ndata: {"type":"error","error":{"type":"overloaded_error"}}\n\n',
        {headers:{'content-type':'text/event-stream'}})
      return Response.json({type:'message',content:[],usage:{input_tokens:4,output_tokens:6}})
    }}
  }]}))
  t.after(()=>mf.dispose())
  const paid=await mf.getWorker('paid'), free=await mf.getWorker('free')
  const webhook=(target,update)=>target.fetch('https://agent.test/tg/webhook',post(update,{'x-telegram-bot-api-secret-token':'test-webhook'}))
  const ask=(target,options={})=>target.fetch('https://agent.test/llm/v1/messages?beta=true',post({model:'large',messages:[{role:'user',content:'hi'}],max_tokens:50,...options},
    {authorization:'Bearer '+token('tg:'+group)}))
  const tool=path=>paid.fetch('https://agent.test'+path,{headers:{authorization:'Bearer '+token('tg:'+group)}})
  const config=await (await mf.dispatchFetch('https://bill.test/admin/api/settings')).json()
  assert.equal(config.mode,'closed')
  await webhook(paid,message('/account'))
  assert.match(sent.at(-1).body.text,/access is closed/)
  await webhook(paid,message('/topup 1'))
  assert.equal(sent.at(-1).method,'sendMessage')
  assert.match(sent.at(-1).body.text,/access is closed/)
  const application=message('/apply Please approve my research account')
  application.message.from.first_name='Ada'
  application.message.from.username='ada_test'
  await webhook(paid,application)
  assert.match(sent.at(-1).body.text,/waiting for admin review/)
  const queue=await (await mf.dispatchFetch('https://bill.test/admin/api/applications')).json()
  assert.deepEqual([queue.applications[0].user,queue.applications[0].username,queue.applications[0].note],[String(person),'ada_test','Please approve my research account'])
  const change=await mf.dispatchFetch('https://bill.test/admin/api/settings',post({access_mode:'paid',allow_stars_only:true,free_weekly_tokens:16}))
  assert.equal(change.status,200)
  const beforeGroup=sent.length
  await webhook(paid,message('hello',group))
  assert.equal(sent.length,beforeGroup)
  await webhook(paid,message('/new'))
  assert.match(sent.at(-1).body.text,/waiting for admin review/)
  await webhook(paid,{callback_query:{id:'cb',from:{id:person},message:message('').message,data:'resume'}})
  assert.equal(sent.at(-1).method,'answerCallbackQuery')
  assert.match(sent.at(-1).body.text,/waiting for admin review/)
  await mf.dispatchFetch('https://bill.test/admin/api/users/'+person+'/access',post({status:'approved',expected:'pending'}))
  assert.equal((await ask(paid)).status,403)
  assert.equal((await webhook(paid,message('/account'))).status,200)
  assert.match(sent.at(-1).body.text,/Available tokens: 16/)
  await webhook(paid,message('/account',group))
  assert.match(sent.at(-1).body.text,/private chat/)
  await webhook(paid,message('hello',group))
  assert.equal((await tool('/mcp')).status,200)
  const readAccount=async ()=>(await mf.dispatchFetch('https://bill.test/admin/api/users/'+person)).json()
  const earlyError=await ask(paid,{stream:true})
  assert.match(await earlyError.text(),/event: error/)
  for(let i=0;i<30 && (await readAccount()).available_tokens!==16;i++) {
    await new Promise(resolve=>setTimeout(resolve,20))
  }
  assert.equal((await readAccount()).available_tokens,16,'a failed stream must release the reservation')
  const beforeAnswer=models.length
  const context={edits:[{type:'clear_tool_uses_20250919'}]}
  const answer=await ask(paid,{context_management:context})
  assert.equal(answer.status,200,await answer.text())
  assert.deepEqual(models.slice(beforeAnswer).map(m=>m.path),['/v1/messages/count_tokens','/v1/messages'])
  for(const request of models.slice(beforeAnswer)) {
    assert.equal(request.query,'?beta=true')
    assert.deepEqual(request.body.context_management,context)
  }
  assert.equal(models.at(-1).body.max_tokens,6)
  for(let i=0;i<25;i++){
    const detail=await (await mf.dispatchFetch('https://bill.test/admin/api/users/'+person)).json()
    if(detail.billable_tokens===10) break
    await new Promise(resolve=>setTimeout(resolve,20))
  }
  const refused=await ask(paid)
  assert.equal(refused.status,402)
  await webhook(paid,message('hello in private'))
  assert.match(sent.at(-1).body.text,/\/topup/)
  await webhook(paid,message('/topup 3.25'))
  const invoice=sent.at(-1)
  assert.equal(invoice.method,'sendInvoice')
  assert.equal(invoice.body.prices[0].amount,Math.ceil(3.25*config.stars_per_usd))
  const purchase={invoice_payload:invoice.body.payload,currency:'XTR',total_amount:invoice.body.prices[0].amount}
  await webhook(paid,{pre_checkout_query:{id:'q1',from:{id:person},...purchase}})
  assert.equal(sent.at(-1).body.ok,true)
  const receipt={message:{...message('').message,successful_payment:{...purchase,telegram_payment_charge_id:'test-stars-charge'}}}
  for(let i=0;i<2;i++) assert.equal((await webhook(paid,receipt)).status,200)
  assert.equal((await readAccount()).wallet_tokens,3_250_000)
  await webhook(paid,message('/subscribe'))
  const confirmation=sent.at(-1).body.text.match(/\/subscribe [a-f0-9-]{36}/)[0]
  assert.equal((await readAccount()).subscriptions.length,0)
  await webhook(paid,message(confirmation))
  await webhook(paid,message(confirmation))
  const detail=await readAccount()
  assert.equal(detail.wallet_tokens,2_250_000)
  assert.equal(detail.subscriptions.length,1)
  assert.equal(detail.available_tokens,2_250_000 + 80 - 16)
  assert.equal(detail.allowance_tokens,80)
  assert.equal((await ask(paid)).status,200)
  await webhook(paid,message('/unsubscribe'))
  assert.equal((await readAccount()).subscriptions[0].auto_renew,false)

  // The compiled SessionDO reports this completed turn and writes the runner's file to the shared R2 bucket.
  assert.equal((await webhook(paid,message('build a report'))).status,200)
  const admin=path=>mf.dispatchFetch('https://bill.test'+path)
  let turn
  for(let attempt=0;attempt<80&&!turn;attempt++){
    const history=await (await admin('/admin/api/users/'+person+'/history')).json()
    turn=history.turns.find(entry=>entry.prompt==='build a report')
    if(!turn) await new Promise(resolve=>setTimeout(resolve,50))
  }
  assert.equal(turn?.answer,'The report is ready.')
  const files=await (await admin('/admin/api/users/'+person+'/files?turn='+turn.id)).json()
  assert.equal(files.files[0].name,'report.txt')
  assert.deepEqual(await (await admin(files.files[0].preview_url)).json(),{text:'test report',truncated:false})
  assert.equal(await (await admin(files.files[0].url)).text(),'test report')

  await mf.dispatchFetch('https://bill.test/admin/api/settings',post({allow_stars_only:false}))
  const startingWallet=(await readAccount()).wallet_tokens
  const unconfigured=models.length
  assert.equal((await ask(paid)).status,403)
  assert.equal(models.length,unconfigured)
  assert.equal((await tool('/file')).status,403)
  const freeAccess=await mf.dispatchFetch('https://bill.test/admin/api/settings',post({access_mode:'free'}))
  assert.equal(freeAccess.status,200)
  assert.equal((await ask(paid)).status,200)
  assert.equal(models.length,unconfigured+1)
  assert.equal(models.at(-1).body.max_tokens,50)
  const unlimited=await readAccount()
  assert.equal(unlimited.available_tokens,null)
  assert.equal(unlimited.wallet_tokens,startingWallet)
  assert.equal((await tool('/file')).status,200)
  await mf.dispatchFetch('https://bill.test/admin/api/users/'+person+'/access',post({status:'revoked'}))
  assert.equal((await ask(paid)).status,403)
  assert.equal(models.length,unconfigured+1)
  assert.equal((await tool('/mcp')).status,403)
  assert.deepEqual(tools,['/mcp','/file'])

  // With no billing binding, the original proxy never issues a pre-count or reduces max_tokens.
  const before=models.length
  assert.equal((await ask(free)).status,200)
  assert.equal(models.length,before+1)
  assert.equal(models.at(-1).body.max_tokens,50)
})
