import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';
import {performance} from 'node:perf_hooks';

const origin=process.env.CHAT_TEST_ORIGIN ?? 'http://127.0.0.1:8787';
const adminKey=process.env.CHAT_TEST_ADMIN_KEY ?? 'chat-ci-development-admin-secret-not-for-production';
const rooms=12, messagesPerRoom=10;
async function call(method,path,body,token,admin=false){
  const response=await fetch(origin+'/v1'+path,{
    method,
    headers:{
      ...(body?{'Content-Type':'application/json'}:{}),
      ...(token?{'Authorization':'Bearer '+token}:{}),
      ...(admin?{'X-Admin-Key':adminKey}:{})
    },
    ...(body?{body:JSON.stringify(body)}:{})
  });
  const text=await response.text();
  let data;
  try{data=text?JSON.parse(text):null;}catch{data={raw:text.slice(0,256)};}
  return {status:response.status,data};
}
function check(result,want){
  assert.equal(result.status,want,'HTTP '+result.status+': '+JSON.stringify(result.data));
  return result.data;
}
async function provision(){
  const chat=randomUUID(),a=randomUUID(),b=randomUUID();
  check(await call('POST','/admin/chats',{chat_id:chat,members:[a,b]},null,true),201);
  const tokenA=check(await call('POST','/admin/token',{user_id:a,ttl_seconds:3600},null,true),201).access_token;
  const tokenB=check(await call('POST','/admin/token',{user_id:b,ttl_seconds:3600},null,true),201).access_token;
  return {chat,a,b,tokenA,tokenB};
}
async function main(){
  const metadata=await Promise.all(Array.from({length:rooms},()=>provision()));
  const workloads=metadata.flatMap(r=>Array.from({length:messagesPerRoom},(_,i)=>({
    room:r,client_message_id:randomUUID(),text:'concurrency:'+r.chat+':'+i
  })));
  const started=performance.now();
  const committed=await Promise.all(workloads.map(async job=>({
    ...job,
    ack:check(await call('POST','/chats/'+job.room.chat+'/messages',{
      client_message_id:job.client_message_id,text:job.text
    },job.room.tokenA),200)
  })));
  const elapsed=(performance.now()-started)/1000;
  assert.equal(committed.length,rooms*messagesPerRoom);
  for(const r of metadata){
    const accepted=committed.filter(m=>m.room.chat===r.chat);
    const ids=accepted.map(v=>v.ack.seq);
    assert.equal(new Set(ids).size,messagesPerRoom,'duplicate sequence in chat');
    assert.ok(accepted.every(m=>m.ack.accepted===true && m.ack.duplicate===false));
    const synced=check(await call('GET','/chats/'+r.chat+'/messages?after=0&limit=200',null,r.tokenB),200);
    const messages=synced.changes.filter(x=>x.type==='message');
    assert.equal(messages.length,messagesPerRoom,'lost, duplicated or delayed message');
    const recovered=new Set(messages.map(m=>m.text));
    assert.equal(recovered.size,messagesPerRoom,'repeated ciphertext after sync');
    for(const message of accepted)assert.ok(recovered.has(message.text));
  }
  const candidate=committed[0];
  const retries=await Promise.all(Array.from({length:10},()=>call('POST','/chats/'+candidate.room.chat+'/messages',{
    client_message_id:candidate.client_message_id,text:candidate.text
  },candidate.room.tokenA)));
  assert.ok(retries.every(r=>r.status===200 && r.data.duplicate===true && r.data.seq===candidate.ack.seq),
    'concurrent retries must return original sequence');
  const newId=randomUUID();
  const simultaneous=await Promise.all(Array.from({length:10},()=>call('POST','/chats/'+candidate.room.chat+'/messages',{
    client_message_id:newId,text:'simultaneous-deduplication'
  },candidate.room.tokenA)));
  assert.ok(simultaneous.every(r=>r.status===200),'concurrent same-ID first sends must succeed');
  assert.equal(new Set(simultaneous.map(r=>r.data.seq)).size,1,'same-ID sends must share a seq');
  assert.equal(simultaneous.filter(r=>r.data.duplicate===false).length,1,'exactly one new message');
  check(await call('POST','/chats/'+candidate.room.chat+'/messages',{
    client_message_id:newId,text:'different payload'
  },candidate.room.tokenA),409);
  const synced=check(await call('GET','/chats/'+candidate.room.chat+'/messages?after=0&limit=200',null,candidate.room.tokenB),200);
  assert.equal(synced.changes.filter(x=>x.type==='message').length,messagesPerRoom+1);
  console.log('PASS concurrency: '+committed.length+' messages, '+rooms+' rooms, unique ordering, recovery, simultaneous retries and conflicts');
  console.log('Local observed time '+elapsed.toFixed(2)+'s ('+(committed.length/elapsed).toFixed(1)+' messages/s); not a remote performance benchmark');
}
main().catch(error=>{console.error(error);process.exitCode=1;});
