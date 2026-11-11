#!/usr/bin/env node
// Offline validation only. Never sends credentials or changes Cloudflare.
import { readFileSync, existsSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT=resolve(dirname(fileURLToPath(import.meta.url)),'..');
const REQUIRED=['ADMIN_KEY','AUTH_KEY_V1','AUTH_KEY_V2','MESSAGE_KEY_V1','MESSAGE_KEY_V2','DEDUP_KEY'];
const UUID=/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

export function validate(config, environment, other=null){
  const errors=[];
  const add=x=>errors.push(x);
  if(!['staging','production'].includes(environment))return ['Environment must be staging or production'];
  if(config.name!=='api-chat-'+environment)add('Worker name must be api-chat-'+environment);
  if(config.main!=='build/index.js')add('main must point to build/index.js');
  if(!/^\d{4}-\d{2}-\d{2}$/.test(config.compatibility_date??''))add('Valid compatibility_date required');
  let parsed;
  try{parsed=new URL(config.vars?.PUBLIC_ORIGIN);}catch{add('PUBLIC_ORIGIN must be a valid HTTPS URL');}
  if(parsed){
    if(parsed.protocol!=='https:'||parsed.username||parsed.password||parsed.port||parsed.pathname!=='/'||parsed.search||parsed.hash)
      add('PUBLIC_ORIGIN must be an HTTPS origin (no path, port, credentials or query)');
    if(/placeholder|replace|your_|example\.com|localhost|127\.0\.0\.1|\.test$/i.test(parsed.hostname))
      add('PUBLIC_ORIGIN contains a placeholder or development host');
  }
  if(environment==='staging'){
    if(config.workers_dev!==true)add('staging requires workers_dev=true');
    if(parsed && (!parsed.hostname.startsWith('api-chat-staging.')||!parsed.hostname.endsWith('.workers.dev')))
      add('staging origin must be api-chat-staging.<subdomain>.workers.dev');
    if(config.routes?.length)add('staging must not configure production routes');
  }else{
    if(config.workers_dev!==false)add('production requires workers_dev=false');
    const routes=config.routes;
    if(!Array.isArray(routes)||routes.length!==1||routes[0].custom_domain!==true)
      add('production requires exactly one Cloudflare Custom Domain');
    else if(parsed && routes[0].pattern!==parsed.hostname)
      add('production Custom Domain must equal PUBLIC_ORIGIN host');
  }
  const db=config.d1_databases;
  if(!Array.isArray(db)||db.length!==1||db[0].binding!=='DB')add('Exactly one D1 DB binding is required');
  else{
    if(db[0].database_name!=='api-chat-'+environment+'-db')
      add('D1 database_name must identify the selected environment');
    if(!UUID.test(db[0].database_id??'')||/^0+$/.test((db[0].database_id??'').replaceAll('-','')))
      add('Replace D1 database_id with a real Cloudflare D1 UUID');
    if(db[0].migrations_dir!=='migrations')add('D1 migrations_dir must be migrations');
    if(other?.d1_databases?.[0]?.database_id===db[0].database_id)
      add('staging and production must have different D1 database IDs');
  }
  const bindings=config.durable_objects?.bindings??[];
  for(const [name,cl] of [['CHAT_ROOM','ChatRoom'],['USER_HUB','UserHub']]){
    if(!bindings.some(x=>x.name===name&&x.class_name===cl))add('Missing DO binding '+name+' -> '+cl);
    if(config.exports?.[cl]?.type!=='durable-object'||config.exports?.[cl]?.storage!=='sqlite')
      add('Missing SQLite Durable Object export '+cl);
  }
  if(config.migrations)add('Use exports instead of legacy migrations for these new Workers');
  for(const name of REQUIRED){
    if(!config.secrets?.required?.includes(name))add('Missing required secret declaration: '+name);
    if(Object.hasOwn(config.vars??{},name))add('Secret '+name+' must not be in vars');
  }
  for(const name of ['ACTIVE_AUTH_KEY_VERSION','ACTIVE_MESSAGE_KEY_VERSION']){
    if(!['1','2'].includes(config.vars?.[name]))add(name+' must be 1 or 2');
  }
  for(const name of ['MESSAGE_RATE_LIMITER','API_RATE_LIMITER','WS_RATE_LIMITER']){
    if(!config.ratelimits?.some(x=>x.name===name&&Number(x.simple?.limit)>0&&Number(x.simple?.period)>0))
      add('Missing or invalid rate limiter '+name);
  }
  const rateIds=(config.ratelimits??[]).map(x=>x.namespace_id);
  if(new Set(rateIds).size!==rateIds.length)add('Rate limiter namespace IDs must be distinct');
  if(other && other.name===config.name)add('staging and production Worker names must differ');
  return errors;
}
export function validateSecrets(value){
  const errors=[];
  const keys=Object.keys(value??{});
  for(const key of REQUIRED){
    if(typeof value?.[key]!=='string'){errors.push('Missing secret '+key);continue;}
    if(key==='ADMIN_KEY'){
      if(value[key].length<40)errors.push('ADMIN_KEY must have at least 40 random characters');
    }else{
      try{
        const bytes=Buffer.from(value[key],'base64url');
        if(bytes.length!==32||bytes.toString('base64url')!==value[key])
          errors.push(key+' must be 32 random bytes as canonical base64url');
      }catch{errors.push('Invalid encoding for '+key);}
    }
  }
  if(keys.some(k=>!REQUIRED.includes(k)))errors.push('Unexpected key in secrets file');
  const values=REQUIRED.filter(k=>typeof value?.[k]==='string').map(k=>value[k]);
  if(new Set(values).size!==values.length)errors.push('Every secret must have an independent value');
  return errors;
}
export function loadConfig(environment){
  const path=resolve(ROOT,'wrangler.'+environment+'.jsonc');
  if(!existsSync(path))throw Error('Missing '+path+'. Run: node scripts/init-cloudflare.mjs '+environment);
  return {config:JSON.parse(readFileSync(path,'utf8')),path};
}
export function check(environment){
  const {config,path}=loadConfig(environment);
  const otherEnv=environment==='staging'?'production':'staging';
  let other=null;try{other=loadConfig(otherEnv).config;}catch{}
  const issues=validate(config,environment,other);
  if(issues.length)throw Error('Deployment blocked:\n'+issues.map(s=>' - '+s).join('\n'));
  return {config,path};
}
if(process.argv[1]&&resolve(process.argv[1])===fileURLToPath(import.meta.url)){
  try{
    const env=process.argv[2];
    if(!['staging','production'].includes(env))throw Error('Usage: node scripts/predeploy.mjs staging|production');
    const result=check(env);
    console.log('PASS: '+env+' config validated ('+result.config.name+'); no Cloudflare changes.');
    console.log('Does NOT verify that remote D1, secrets, DNS, Access or WAF exist.');
  }catch(e){console.error(e.message);process.exitCode=1;}
}
