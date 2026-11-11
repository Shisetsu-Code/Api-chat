import {randomUUID} from 'node:crypto';
import {performance} from 'node:perf_hooks';
import WebSocket from 'ws';

const ORIGIN=process.env.CHAT_TEST_ORIGIN ?? 'http://127.0.0.1:8787';
const ADMIN_KEY=process.env.CHAT_TEST_ADMIN_KEY ?? 'chat-ci-development-admin-secret-not-for-production';
const count=16,interval=125;
const now=()=>performance.now();
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
const started=new Map(),plaintextAt=new Map(),eventAt=new Map();
let listening=false,needsSync=false,syncError=null,websocket,activeSync=false;

async function request(method,path,body=null,token=null,admin=false) {
  const response=await fetch(ORIGIN+'/v1'+path,{
    method,headers:{
      ...(body===null?{}:{'Content-Type':'application/json'}),
      ...(token?{Authorization:'Bearer '+token}:{}),
      ...(admin?{'X-Admin-Key':ADMIN_KEY}:{})
    },
    ...(body===null?{}:{body:JSON.stringify(body)}),
    signal:AbortSignal.timeout(10000)
  });
  const text=await response.text();
  const payload=text?JSON.parse(text):{};
  if(response.status>=400)throw Error('HTTP '+response.status+': '+JSON.stringify(payload));
  return payload;
}
const p95=arr=>[...arr].sort((a,b)=>a-b)[Math.ceil(arr.length*.95)-1];
const chat=randomUUID(),alice=randomUUID(),bob=randomUUID();
async function main(){
  await request('POST','/admin/chats',{chat_id:chat,members:[alice,bob]},null,true);
  const a=(await request('POST','/admin/token',{user_id:alice},null,true)).access_token;
  const b=(await request('POST','/admin/token',{user_id:bob},null,true)).access_token;

  async function sync(){
    if(activeSync)return;
    activeSync=true;
    try{
      while(needsSync){
        needsSync=false;
        const r=await request('GET','/chats/'+chat+'/messages?after=0&limit=200',null,b);
        const seenAt=now();
        for(const msg of r.changes??[]){
          if(msg.type==='message' && started.has(msg.client_message_id)
             && !plaintextAt.has(msg.client_message_id)) {
            plaintextAt.set(msg.client_message_id,seenAt);
          }
        }
      }
    }catch(e){syncError=e;}
    activeSync=false;
  }
  websocket=await new Promise((resolve,reject)=>{
    const ws=new WebSocket(ORIGIN.replace(/^http/,'ws')+'/v1/realtime',{headers:{Authorization:'Bearer '+b},handshakeTimeout:10000});
    ws.on('error',reject);
    ws.on('message',chunk=>{
      let v;try{v=JSON.parse(chunk.toString());}catch{return;}
      if(v.type==='realtime.ready'){listening=true;resolve(ws);}
      if(v.type==='chat.changed'&&v.chat_id===chat){
        if(!eventAt.has(v.seq))eventAt.set(v.seq,now());
        needsSync=true;
        void sync();
      }
    });
  });
  if(!listening)throw Error('WebSocket not ready');
  await sleep(200);
  const t0=now();
  const sends=Array.from({length:count},(_,i)=>(async()=>{
    const scheduled=t0+i*interval,delay=scheduled-now();
    if(delay>0)await sleep(delay);
    const id=randomUUID();
    started.set(id,now());
    const response=await request('POST','/chats/'+chat+'/messages',{client_message_id:id,text:'realtime-latency-'+i},a);
    if(!response.accepted)throw Error('Message not accepted');
  })());
  await Promise.all(sends);
  const streamFinished=now();
  const until=now()+10000;
  while(plaintextAt.size<count && now()<until && !syncError)await sleep(30);
  if(syncError)throw syncError;
  const missing=count-plaintextAt.size;
  const times=[...started].filter(([id])=>plaintextAt.has(id)).map(([id,t])=>plaintextAt.get(id)-t);
  const live=[...plaintextAt.values()].filter(t=>t<streamFinished).length;
  console.log('REALTIME_LATENCY '+JSON.stringify({
    messages:count,received:plaintextAt.size,missing,during_send:live,
    p50_ms:times.length?+([...times].sort((a,b)=>a-b)[Math.floor(times.length/2)]).toFixed(1):null,
    p95_ms:times.length?+p95(times).toFixed(1):null,
    max_ms:times.length?+Math.max(...times).toFixed(1):null,
    goal_p95_under_200ms:times.length===count&&p95(times)<200,
    note:'Local same-host latency, includes sender HTTP and recipient HTTP sync, not remote network RTT'
  }));
  if(missing || live<4)throw Error('Realtime push did not keep pace during sustained traffic');
  websocket.close();
}
main().catch(e=>{websocket?.terminate();console.error('REALTIME_SMOKE_FAIL',e);process.exit(1);});