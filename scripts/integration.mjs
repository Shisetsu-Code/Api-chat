import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';
import WebSocket from 'ws';

const origin=process.env.CHAT_TEST_ORIGIN ?? 'http://127.0.0.1:8787';
const adminKey=process.env.CHAT_TEST_ADMIN_KEY ?? 'chat-ci-development-admin-secret-not-for-production';
const A=randomUUID(),B=randomUUID(),C=randomUUID(),chat=randomUUID();
const sleep=(ms)=>new Promise(r=>setTimeout(r,ms));

async function api(method,path,body,token,extra={}) {
  const response=await fetch(origin+'/v1'+path,{
    method,
    headers:{
      ...(body ? {'Content-Type':'application/json'} : {}),
      ...(token ? {'Authorization':'Bearer '+token} : {}),
      ...extra
    },
    ...(body ? {body:JSON.stringify(body)} : {})
  });
  const text=await response.text();
  let payload;
  try {payload=text ? JSON.parse(text) : null;}catch{payload={raw_response:text.slice(0,1000),response_status:response.status,path};}
  return {status:response.status,payload,headers:response.headers};
}
function status(r,expected) {
  assert.equal(r.status,expected,'HTTP status mismatch: '+JSON.stringify(r.payload));
  return r.payload;
}
const admin=(method,path,body)=>api(method,'/admin'+path,body,undefined,{'X-Admin-Key':adminKey});
const issue=async id=>status(await admin('POST','/token',{user_id:id,ttl_seconds:3600}),201).access_token;
async function eventually(task,predicate,maxMs=12000) {
  const start=Date.now();
  let last;
  while (Date.now()-start<maxMs) {
    try {last=await task();if(predicate(last))return last;}catch(e){last=e;}
    await sleep(400);
  }
  throw new Error('eventual condition timed out: '+JSON.stringify(last));
}
async function connectCookie(cookie) {
  return new Promise((resolve,reject)=>{
    const ws=new WebSocket(origin.replace(/^http/,'ws')+'/v1/realtime',{
      headers:{Cookie:cookie,Origin:origin},handshakeTimeout:5000
    });
    const timeout=setTimeout(()=>{ws.terminate();reject(new Error('WebSocket timeout'));},7000);
    ws.once('error',e=>{clearTimeout(timeout);reject(e);});
    ws.once('open',()=>{clearTimeout(timeout);resolve(ws);});
  });
}
async function nextEvent(ws,type,maxMs=12000,predicate=()=>true) {
  return new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>{
      ws.off('message',handler);reject(new Error('WS event missing: '+type));
    },maxMs);
    const handler=(raw)=>{
      let event;
      try {event=JSON.parse(raw.toString());}catch{return;}
      if(event.type!==type || !predicate(event))return;
      clearTimeout(timer);ws.off('message',handler);resolve(event);
    };
    ws.on('message',handler);
  });
}

async function run() {
  assert.equal((await fetch(origin+'/health')).status,200);
  const a=await issue(A),b=await issue(B),c=await issue(C);
  status(await admin('POST','/chats/probe',{chat_id:chat,op:'probe'}),200);
  status(await admin('POST','/chats/probe',{chat_id:chat,op:'probe_sql'}),200);
  status(await admin('POST','/chats',{chat_id:chat,members:[A,B]}),201);

  const firstId=randomUUID();
  const first=status(await api('POST','/chats/'+chat+'/messages',
    {client_message_id:firstId,text:'Hola cifrado con AES-GCM'},a),200);
  assert.equal(first.duplicate,false);
  assert.ok(first.seq>0);
  const duplicate=status(await api('POST','/chats/'+chat+'/messages',
    {client_message_id:firstId,text:'Hola cifrado con AES-GCM'},a),200);
  assert.equal(duplicate.duplicate,true);
  assert.equal(duplicate.seq,first.seq);
  status(await api('POST','/chats/'+chat+'/messages',
    {client_message_id:firstId,text:'Diferente'},a),409);
  const got=status(await api('GET','/chats/'+chat+'/messages?after=0&limit=100',null,b),200);
  assert.equal(got.changes.find(e=>e.type==='message')?.text,'Hola cifrado con AES-GCM');

  status(await api('GET','/chats/'+chat+'/messages?after=0',null,c),403);
  status(await api('POST','/chats/'+chat+'/receipts',
    {delivered_seq:first.seq,read_seq:first.seq},b),200);
  const receipts=status(await api('GET','/chats/'+chat+'/receipts',null,a),200);
  assert.ok(receipts.receipts.some(x=>x.user_id===B && x.read_seq===first.seq));

  status(await admin('PUT','/chats/'+chat+'/members/'+C),200);
  const afterJoin=status(await api('GET','/chats/'+chat+'/messages?after=0&limit=200',null,c),200);
  assert.ok(!afterJoin.changes.some(x=>x.type==='message'&&x.seq===first.seq),
    'new user cannot read previous history');

  const ticket=await api('POST','/realtime/browser-session',{},c,{'Origin':origin});
  status(ticket,200);
  const cookie=ticket.headers.get('set-cookie');
  assert.ok(cookie?.includes('__Host-api-chat-realtime='),'secure WS cookie missing');
  const ws=await connectCookie(cookie.split(';')[0]);
  const second=status(await api('POST','/chats/'+chat+'/messages',
    {client_message_id:randomUUID(),text:'Posterior al alta'},a),200);
  const notified=await nextEvent(ws,'chat.changed',12000,e=>e.seq===second.seq);
  assert.equal(notified.chat_id,chat);
  assert.ok(notified.seq>=second.seq);
  const after=status(await api('GET','/chats/'+chat+'/messages?after=0',null,c),200);
  assert.ok(after.changes.some(x=>x.type==='message'&&x.text==='Posterior al alta'));

  status(await admin('POST','/chats/'+chat+'/rotate',{target_key_version:2}),202);
  await eventually(
    async()=>status(await api('GET','/chats/'+chat+'/messages?after=0',null,a),200),
    x=>x.changes.some(e=>e.type==='message'&&e.text==='Posterior al alta')
  );

  status(await admin('DELETE','/chats/'+chat+'/members/'+B),200);
  status(await api('GET','/chats/'+chat+'/messages?after=0',null,b),403);
  status(await api('POST','/chats/'+chat+'/messages',
    {client_message_id:randomUUID(),text:'No autorizado'},b),403);

  const revokeNotice=nextEvent(ws,'session.revoked',12000);
  const revoke=status(await api('POST','/auth/revoke',{},c),200);
  assert.equal(revoke.hub_status,200,'hub relay failed: '+JSON.stringify(revoke));
  assert.ok(revoke.hub_closed>=1,'hub did not request close: '+JSON.stringify(revoke));
  assert.equal((await revokeNotice).code,4001);
  ws.close();
  status(await api('GET','/chats/'+chat+'/messages?after=0',null,c),401);

  await eventually(
    async()=>status(await api('GET','/chats',null,a),200),
    x=>x.chats.some(row=>row.chat_id===chat)
  );
  await sleep(10);
  status(await admin('POST','/chats/'+chat+'/prune',{before_ms:Date.now()-1}),202);
  await eventually(
    async()=>status(await api('GET','/chats/'+chat+'/messages?after=0',null,a),200),
    x=>x.changes.some(e=>e.type==='message.pruned')
  );
  console.log('PASS: encrypted send, dedup, access, membership, receipts, WS, rotation, revoke, projection, pruning');
}
run().catch(e=>{console.error(e);process.exit(1);});
