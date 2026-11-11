#!/usr/bin/env node
// Explicit, guarded remote deploy. With no --apply it NEVER changes Cloudflare.
import { readFileSync, statSync } from 'node:fs';
import { resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { check, validateSecrets } from './predeploy.mjs';

const env=process.argv[2];
const flags=process.argv.slice(3);
const applied=flags.includes('--apply');
const approved=flags.includes('--confirm-production');
const secretIndex=flags.indexOf('--secrets-file');
if(!['staging','production'].includes(env)||flags.some(x=>!['--apply','--confirm-production','--secrets-file'].includes(x)&&
  (secretIndex<0||flags[secretIndex+1]!==x))||(secretIndex>=0&&!flags[secretIndex+1])){
  console.error('Usage: node scripts/deploy-cloudflare.mjs staging|production [--apply] [--secrets-file PATH] [--confirm-production]');
  process.exit(2);
}
try{
  const {config,path}=check(env);
  if(applied&&env==='production'&&!approved)
    throw Error('Production deployment requires --confirm-production after staging validation.');
  if(approved&&env!=='production')throw Error('--confirm-production is only valid for production');
  const secretsPath=secretIndex<0?null:resolve(flags[secretIndex+1]);
  if(secretsPath){
    const json=JSON.parse(readFileSync(secretsPath,'utf8'));
    const errs=validateSecrets(json);
    if(errs.length)throw Error('Unsafe secrets file:\n'+errs.map(e=>' - '+e).join('\n'));
    const metadata=statSync(secretsPath);
    if(process.platform!=='win32'&&(metadata.mode&0o077)!==0)
      throw Error('Secrets file must have mode 0600. Run chmod 600 on it.');
  }
  function exec(name,args){
    const win=process.platform==='win32';
    const program=win?name+'.cmd':name;
    const r=spawnSync(program,args,{stdio:'inherit',shell:win,env:process.env});
    if(r.error)throw r.error;
    if(r.status!==0)throw Error('Command '+name+' failed (exit '+r.status+')');
  }
  console.log('Valid configuration for '+env+' ('+config.name+'). Build target Rust/WASM.');
  exec('npm',['run','build']);
  if(!applied){
    console.log('Dry run: no remote mutations, secrets, migrations or uploads.');
    exec('npx',['--no-install','wrangler','deploy','--dry-run','--config',path,'--outdir',resolve('build','predeploy-'+env)]);
    console.log('Dry-run succeeded. For remote deployment, add --apply.');
  }else {
    console.log('Applying remote D1 migrations for '+env+'. This may change stored data.');
    exec('npx',['--no-install','wrangler','d1','migrations','apply','DB','--remote','--config',path]);
    const args=['--no-install','wrangler','deploy','--config',path];
    if(secretsPath)args.push('--secrets-file',secretsPath);
    console.log('Deploying '+config.name+' to Cloudflare. Validate Access/WAF rules separately.');
    exec('npx',args);
    console.log('Deployment command completed. Run staging smoke test and inspect live telemetry.');
  }
}catch(e){console.error(e.message);process.exitCode=1;}
