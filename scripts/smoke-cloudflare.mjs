#!/usr/bin/env node
// Synthetic smoke test. Never uses a real user's identifier or message.
import {randomUUID} from 'node:crypto';
import {performance} from 'node:perf_hooks';
import {check} from './predeploy.mjs';
import WebSocket from 'ws';

const local=process.env.CHAT_SMOKE_ALLOW_HTTP_LOCAL==='1';
const origin=process.env.CHAT_SMOKE_ORIGIN??(local?'http://127.0.0.1:8787':'');
const adminKey=process.env.CHAT_SMOKE_ADMIN_KEY;
const accessId=process.env.CF_ACCESS_CLIENT_ID;
const accessSecret=process.env.CF_ACCESS_CLIENT_SECRET;
if(!adminKey||adminKey.length<32)throw Error('CHAT_SMOKE_ADMIN_KEY is required (not printed)');
if(!local){
  const expected=check('staging').config.vars.PUBLIC_ORIGIN;
  if(origin!==expected||!origin.startsWith('https://'))throw Error('Remote smoke test is staging-only: CHAT_SMOKE_ORIGIN must match staging PUBLIC_ORIGIN');
}else if(origin!=='http://127.0.0.1:8787'){
  throw Error('Local smoke may only target http://127.0.0.1:8787');
}
const a=randomUUID(),b=randomUUID(),chat=randomUUID();
const now=()=>performance.now();
const adminHeaders={
  'X-Admin-Key':adminKey,
  ...(accessId&&accessSecret?{'CF-Access-Client-Id':accessId,'CF-Access-Client-Secret':accessSecret}:{})
};
async function api(method,path,body=null,token=null,admin=false){
  const response=await fetch(origin+'/v1'+path,{method,headers:{
    ...(body===null?{}:{'Content-Type':'application/json'}),
    ...(token?{Authorization:'Bearer '+token}:{}),
    ...(admin?adminHeaders:{})
  },...(body===null?{}:{body:JSON.stringify(body)}),signal:AbortSignal.timeout(15000)});
  const text=await response.text();
  let data;try{data=text?JSON.parse(text):{}}catch{data={invalid_response:text.slice(0,50)};}
  if(response.status<200||response.status>=300)throw Error(method+' '+path+' -> '+response.status+' '+JSON.stringify(data).slice(0,120));
  return data;
}
async function client(token){
  return await new Promise((resolve,reject)=>{
    const ws=new WebSocket(origin.replace(/^http/,'ws')+'/v1/realtime',{
      headers:{Authorization:'Bearer '+token,Origin:origin},
      handshakeTimeout:10000
    });
    const timeout=setTimeout(()=>{ws.terminate();reject(Error('WebSocket ready timeout'));},10000);
    ws.once('error',e=>{clearTimeout(timeout);reject(e);});
    ws.on('message',raw=>{
      let message;try{message=JSON.parse(raw.toString());}catch{return;}
      if(message.type==='realtime.ready'){clearTimeout(timeout);resolve(ws);}
    });
  });
}
async function main(){
  const health=await fetch(origin+'/health',{signal:AbortSignal.timeout(10000)});
  if(health.status!==200)throw Error('Health check failed');
  await api('POST','/admin/chats',{chat_id:chat,members:[a,b]},null,true);
  const tokenA=(await api('POST','/admin/token',{user_id:a,ttl_seconds:900},null,true)).access_token;
  const tokenB=(await api('POST','/admin/token',{user_id:b,ttl_seconds:900},null,true)).access_token;
  const ws=await client(tokenB);
  const hints=new Map();
  ws.on('message',raw=>{
    let event;try{event=JSON.parse(raw.toString());}catch{return;}
    if(event.type==='chat.changed'&&event.chat_id===chat&&!hints.has(event.seq))hints.set(event.seq,now());
  });
  const measurements=[];
  try{
    for(let i=0;i<3;i++){
      const id=randomUUID(),text='staging-smoke-'+i;
      const start=now();
      const ack=await api('POST','/chats/'+chat+'/messages',{client_message_id:id,text},tokenA);
      if(ack.accepted!==true||!Number.isInteger(ack.seq))throw Error('Invalid message ACK');
      measurements.push({id,text,seq:ack.seq,ack_ms:now()-start,start});
    }
    const deadline=Date.now()+12000;
    while(measurements.some(x=>!hints.has(x.seq))&&Date.now()<deadline)await new Promise(r=>setTimeout(r,50));
    if(measurements.some(x=>!hints.has(x.seq)))throw Error('Missing WebSocket notifications');
    const synced=await api('GET','/chats/'+chat+'/messages?after=0&limit=100',null,tokenB);
    for(const row of measurements){
      if(!synced.changes?.some(c=>c.type==='message'&&c.seq===row.seq&&c.text===row.text))
        throw Error('Missing or corrupted decrypted message at seq '+row.seq);
    }
    const again=await api('POST','/chats/'+chat+'/messages',
      {client_message_id:measurements[0].id,text:measurements[0].text},tokenA);
    if(again.duplicate!==true||again.seq!==measurements[0].seq)throw Error('Deduplication failure');
    const last=measurements.at(-1).seq;
    await api('POST','/chats/'+chat+'/receipts',{delivered_seq:last,read_seq:last},tokenB);
    const receipts=await api('GET','/chats/'+chat+'/receipts',null,tokenA);
    if(!receipts.receipts?.some(x=>x.user_id===b&&x.read_seq===last))throw Error('Receipt not persisted');
    await api('POST','/auth/revoke',{},tokenB);
    const forbidden=await fetch(origin+'/v1/chats/'+chat+'/messages?after=0',{headers:{Authorization:'Bearer '+tokenB},signal:AbortSignal.timeout(10000)});
    if(forbidden.status!==401)throw Error('Revoked session is still authorized');
    const latency=measurements.map(x=>+(hints.get(x.seq)-x.start).toFixed(1)).sort((x,y)=>x-y);
    const ackTimes=measurements.map(x=>+x.ack_ms.toFixed(1)).sort((x,y)=>x-y);
    console.log('STAGING_SMOKE_PASS '+JSON.stringify({
      environment:local?'local':'cloudflare-staging',messages:measurements.length,
      websocket_notifications:latency.length,ack_p95_ms:ackTimes.at(-1),
      notification_p95_ms:latency.at(-1),plaintext_sync:true,
      idempotency:true,receipts:true,revocation:true
    }));
  }finally{ws.terminate();}
}
main().catch(e=>{console.error('STAGING_SMOKE_FAILURE '+String(e));process.exitCode=1;});
