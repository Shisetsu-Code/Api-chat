#!/usr/bin/env node
import {copyFileSync,constants,existsSync} from 'node:fs';
import {resolve,dirname} from 'node:path';
import {fileURLToPath} from 'node:url';
const root=resolve(dirname(fileURLToPath(import.meta.url)),'..');
const env=process.argv[2];
if(!['staging','production'].includes(env)){console.error('Usage: node scripts/init-cloudflare.mjs staging|production');process.exit(1);}
const template=resolve(root,'wrangler.'+env+'.example.jsonc');
const actual=resolve(root,'wrangler.'+env+'.jsonc');
if(existsSync(actual)){console.error('Refusing to overwrite existing configuration: '+actual);process.exit(1);}
copyFileSync(template,actual,constants.COPYFILE_EXCL);
console.log('Created '+actual+' (gitignored). Fill D1 UUID and exact HTTPS PUBLIC_ORIGIN before deploying.');
console.log('Config and secrets are separate. Read docs/CLOUDFLARE_DEPLOY.md.');
