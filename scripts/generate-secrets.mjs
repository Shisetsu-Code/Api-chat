#!/usr/bin/env node
import {randomBytes} from 'node:crypto';
import {mkdirSync,writeFileSync,constants} from 'node:fs';
import {resolve,dirname} from 'node:path';
import {fileURLToPath} from 'node:url';
const root=resolve(dirname(fileURLToPath(import.meta.url)),'..');
const env=process.argv[2];
if(!['staging','production'].includes(env)){console.error('Usage: node scripts/generate-secrets.mjs staging|production');process.exit(1);}
const output=resolve(root,'.secrets','api-chat.'+env+'.json');
mkdirSync(dirname(output),{recursive:true,mode:0o700});
const random=length=>randomBytes(length).toString('base64url');
const secrets={
  ADMIN_KEY:random(48),
  AUTH_KEY_V1:random(32),
  AUTH_KEY_V2:random(32),
  MESSAGE_KEY_V1:random(32),
  MESSAGE_KEY_V2:random(32),
  DEDUP_KEY:random(32)
};
try{writeFileSync(output,JSON.stringify(secrets,null,2)+'\n',{flag:'wx',mode:0o600});}
catch(e){if(e.code==='EEXIST'){console.error('Refusing to overwrite secrets: '+output);process.exit(1);}throw e;}
console.log('Created '+output+' (mode 0600, gitignored). Secret values were not printed.');
console.log('BACK UP TO A SECURE VAULT before first deployment. Losing MESSAGE_KEY prevents decrypting stored messages.');
