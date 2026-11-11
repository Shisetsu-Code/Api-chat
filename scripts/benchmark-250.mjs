import { randomUUID } from 'node:crypto';
import { performance } from 'node:perf_hooks';
import { mkdirSync, writeFileSync } from 'node:fs';
import WebSocket from 'ws';

const ORIGIN=process.env.CHAT_BENCH_ORIGIN??'http://127.0.0.1:8787';
const ADMIN_KEY=process.env.CHAT_TEST_ADMIN_KEY??'chat-ci-development-admin-secret-not-for-production';
const CLIENTS=Number(process.env.BENCH_CLIENTS??250);
const RATE=Number(process.env.BENCH_RATE??4);
const DURATION=Number(process.env.BENCH_SECONDS??15);
const MAX_PENDING=4000;
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
const now=()=>performance.now();
const reqIP=i=>'198.18.'+Math.floor(i/250)+'.'+(i%250+1);
if(!Number.isInteger(CLIENTS)||CLIENTS<2||CLIENTS%2||CLIENTS>500||!Number.isInteger(RATE)||RATE<1||RATE>20||!Number.isInteger(DURATION)||DURATION<1||DURATION>120)throw Error('Invalid benchmark settings');
const sockets=[],events=new Map(),acks=[],errors=[],rtts=[],scheduleLags=[],active=new Set();
const codes={};
let attempted=0,dropped=0,transportFailures=0,peakPending=0;
function pct(values,f){if(!values.length)return null;const a=[...values].sort((x,y)=>x-y);return +(a[Math.ceil(a.length*f)-1].toFixed(1));}
function stats(a){return {count:a.length,mean_ms:a.length?+(a.reduce((x,y)=>x+y,0)/a.length).toFixed(1):null,p50_ms:pct(a,.5),p95_ms:pct(a,.95),p99_ms:pct(a,.99),max_ms:a.length?+Math.max(...a).toFixed(1):null};}
async function api(method,path,body=null,token=null,ip='198.19.0.1',admin=false){
  const started=now();
  const response=await fetch(ORIGIN+'/v1'+path,{method,headers:{...(body!==null?{'Content-Type':'application/json'}:{}),...(token?{Authorization:'Bearer '+token}:{}),...(admin?{'X-Admin-Key':ADMIN_KEY}:{}),'CF-Connecting-IP':ip},...(body!==null?{body:JSON.stringify(body)}:{}),signal:AbortSignal.timeout(25000)});
  const content=await response.text();let data;
  try{data=content?JSON.parse(content):null;}catch{data={raw:content.slice(0,250)};}
  return {status:response.status,data,rtt:now()-started,at:now()};
}
function required(r,status,label){if(r.status!==status)throw Error(label+': '+r.status+' '+JSON.stringify(r.data));return r.data;}
async function parallelMap(arr,limit,fn){const results=new Array(arr.length);let pos=0;await Promise.all(Array.from({length:Math.min(arr.length,limit)},async()=>{while(pos<arr.length){const i=pos++;results[i]=await fn(arr[i],i);}}));return results;}
async function provision(index){
  const chat=randomUUID(),a=randomUUID(),b=randomUUID();
  required(await api('POST','/admin/chats',{chat_id:chat,members:[a,b]},null,'198.19.0.1',true),201,'chat create');
  const tokenA=required(await api('POST','/admin/token',{user_id:a,ttl_seconds:3600},null,'198.19.0.1',true),201,'token A').access_token;
  const tokenB=required(await api('POST','/admin/token',{user_id:b,ttl_seconds:3600},null,'198.19.0.1',true),201,'token B').access_token;
  return [{chat,id:a,other:b,token:tokenA,index:index*2,ip:reqIP(index*2)},{chat,id:b,other:a,token:tokenB,index:index*2+1,ip:reqIP(index*2+1)}];
}
async function connect(client){return new Promise((resolve,reject)=>{
  const ws=new WebSocket(ORIGIN.replace(/^http/,'ws')+'/v1/realtime',{headers:{Authorization:'Bearer '+client.token,'CF-Connecting-IP':client.ip},handshakeTimeout:12000});
  let ready=false;const timeout=setTimeout(()=>{ws.terminate();reject(Error('WS readiness timeout'))},12000);
  ws.on('message',buffer=>{let data;try{data=JSON.parse(buffer.toString())}catch{return;}
    if(data.type==='realtime.ready'&&!ready){ready=true;clearTimeout(timeout);sockets.push(ws);resolve();}
    if(data.type==='chat.changed'&&data.chat_id===client.chat&&data.seq){const key=data.chat_id+'/'+data.seq+'/'+client.id;if(!events.has(key))events.set(key,now());}
  });
  ws.on('error',e=>{if(!ready){clearTimeout(timeout);reject(e)}else if(errors.length<20)errors.push('WS '+String(e).slice(0,120))});
});}
function offer(client,index,ideal){attempted++;const started=now();scheduleLags.push(started-ideal);
  if(active.size>=MAX_PENDING){dropped++;return;}
  const client_message_id=randomUUID(),text='benchmark/'+client.index+'/'+index+'/'+client_message_id;
  const job=(async()=>{try{
    const r=await api('POST','/chats/'+client.chat+'/messages',{client_message_id,text},client.token,client.ip);
    codes[r.status]=(codes[r.status]??0)+1;rtts.push(r.rtt);
    if(r.status===200&&r.data?.accepted===true&&Number.isInteger(r.data.seq))acks.push({chat:client.chat,seq:r.data.seq,id:client.id,other:client.other,text,started,at:r.at});
    else if(errors.length<20)errors.push('HTTP '+r.status+' '+JSON.stringify(r.data).slice(0,120));
  }catch(e){transportFailures++;if(errors.length<20)errors.push('fetch '+String(e).slice(0,120));}})();
  active.add(job);peakPending=Math.max(peakPending,active.size);job.finally(()=>active.delete(job));
}
async function produce(client,at){const interval=1000/RATE;const phase=client.index*interval/CLIENTS;
  for(let index=0;index<RATE*DURATION;index++){
    const ideal=at+index*interval+phase;const wait=ideal-now();if(wait>0)await sleep(wait);
    offer(client,index,ideal);
  }
}
async function sync(clients){const pairs=clients.filter((_,i)=>i%2===0);
  const recovered=await parallelMap(pairs,12,async c=>{
    let after=0,iterations=0,err=null;const messages=new Map();
    do{const r=await api('GET','/chats/'+c.chat+'/messages?after='+after+'&limit=200',null,c.token,c.ip);
      if(r.status!==200){err='HTTP '+r.status;break;}
      for(const x of r.data.changes??[])if(x.type==='message')messages.set(x.seq,x.text);
      if(!Number.isInteger(r.data.next)||r.data.next<after){err='invalid cursor';break;}
      after=r.data.next;iterations++;if(!r.data.has_more)break;
    }while(iterations<50);
    return {chat:c.chat,messages,err:err??(iterations===50?'pagination exceeded':null)};
  });
  const byChat=new Map(recovered.map(r=>[r.chat,r]));let missing=0,mismatched=0,unexpected=0,syncErrors=0;
  for(const room of recovered)if(room.err)syncErrors++;
  const expected=new Map();for(const ack of acks){if(!expected.has(ack.chat))expected.set(ack.chat,new Map());expected.get(ack.chat).set(ack.seq,ack.text);}
  for(const [chat,room] of byChat){const exp=expected.get(chat)??new Map();for(const [seq,text] of exp){if(!room.messages.has(seq))missing++;else if(room.messages.get(seq)!==text)mismatched++;}for(const seq of room.messages.keys())if(!exp.has(seq))unexpected++;}
  return {recovered:recovered.reduce((x,r)=>x+r.messages.size,0),missing,mismatched,unexpected,syncErrors};
}
async function main(){
  const startedAt=new Date().toISOString(),prepAt=now();
  const clients=(await parallelMap(Array.from({length:CLIENTS/2},(_,i)=>i),12,provision)).flat();
  const setupMs=now()-prepAt,wsAt=now();await parallelMap(clients,20,connect);const wsMs=now()-wsAt;
  await sleep(2000);
  const start=now()+150;
  await Promise.all(clients.map(c=>produce(c,start)));
  await Promise.allSettled([...active]);
  await sleep(8000);
  let seenRecipient=0,seenSender=0;const notifiedLatency=[];
  for(const ack of acks){if(events.has(ack.chat+'/'+ack.seq+'/'+ack.id))seenSender++;const t=events.get(ack.chat+'/'+ack.seq+'/'+ack.other);if(t!==undefined){seenRecipient++;notifiedLatency.push(t-ack.started);}}
  const integrity=await sync(clients);
  const perSecond=Array.from({length:DURATION},(_,sec)=>acks.filter(x=>x.at>=start+sec*1000&&x.at<start+(sec+1)*1000).length);
  const report={date:startedAt,environment:'GitHub Actions / Wrangler local (NOT Cloudflare remote)',topology:{clients:CLIENTS,chats:CLIENTS/2,websockets:sockets.length,ip_model:'distinct synthetic CF-Connecting-IP header per client'},target:{rate_per_client:RATE,messages_per_second:CLIENTS*RATE,duration_s:DURATION,planned_messages:CLIENTS*RATE*DURATION},results:{setup_ms:+setupMs.toFixed(1),ws_setup_ms:+wsMs.toFixed(1),attempted,dropped,accepted:acks.length,accepted_in_window:perSecond.reduce((a,b)=>a+b,0),in_window_rps:perSecond.reduce((a,b)=>a+b,0)/DURATION,http_codes:codes,transport_failures:transportFailures,max_http_inflight:peakPending,ack_latency:stats(rtts),schedule_lag:stats(scheduleLags),recipient_notifications:seenRecipient,sender_notifications:seenSender,recipient_ratio:acks.length?seenRecipient/acks.length:null,notification_latency:stats(notifiedLatency),accepted_per_second:perSecond,integrity,errors:errors.slice(0,20)}};
  mkdirSync('artifacts',{recursive:true});writeFileSync('artifacts/bench-250-clients.json',JSON.stringify(report,null,2)+'\n');
  console.log('BENCHMARK_RESULT '+JSON.stringify({target_rps:CLIENTS*RATE,planned:CLIENTS*RATE*DURATION,accepted:acks.length,achieved_rps:report.results.in_window_rps,statuses:codes,ack:report.results.ack_latency,recipient_delivery_ratio:report.results.recipient_ratio,integrity}));
  // A benchmark can finish running without meeting the requested rate.
  // Mark such runs red even if every acknowledged message remains durable.
  const attemptedPlanned=CLIENTS*RATE*DURATION;
  const meetsRate=attempted===attemptedPlanned && dropped===0 &&
    acks.length>=attemptedPlanned*0.95 && transportFailures===0;
  report.results.persisted_without_ack=Math.max(0,integrity.recovered-acks.length);
  report.results.meets_target=meetsRate;
  writeFileSync('artifacts/bench-250-clients.json',JSON.stringify(report,null,2)+'\n');
  console.log('BENCHMARK_VERDICT '+JSON.stringify({meets_target:meetsRate,
    persisted:integrity.recovered,unacknowledged_persisted:report.results.persisted_without_ack,
    rejected_before_http:dropped,request_timeouts:transportFailures}));
  for(const ws of sockets)ws.terminate();
  if(!meetsRate||integrity.missing||integrity.mismatched||integrity.syncErrors)process.exitCode=2;
}
main().catch(e=>{console.error('BENCH_SETUP_ERROR '+String(e));mkdirSync('artifacts',{recursive:true});writeFileSync('artifacts/bench-250-clients.json',JSON.stringify({failed:true,error:String(e)},null,2));for(const ws of sockets)ws.terminate();process.exitCode=1;});