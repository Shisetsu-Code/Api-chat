import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {randomBytes} from 'node:crypto';
import {validate,validateSecrets} from '../scripts/predeploy.mjs';

const tempFile=env=>JSON.parse(readFileSync(new URL('../wrangler.'+env+'.example.jsonc',import.meta.url),'utf8'));
const staging=tempFile('staging');
const production=tempFile('production');
assert.ok(validate(staging,'staging').some(x=>x.includes('D1 database_id')),'template must fail if not configured');
assert.ok(validate(production,'production').length>0,'production must fail while placeholders remain');
staging.vars.PUBLIC_ORIGIN='https://api-chat-staging.acme-workers.workers.dev';
production.vars.PUBLIC_ORIGIN='https://chat-api.acme.net';
production.routes[0].pattern='chat-api.acme.net';
staging.d1_databases[0].database_id='123e4567-e89b-42d3-a456-426614174000';
production.d1_databases[0].database_id='123e4567-e89b-42d3-a456-426614174001';
assert.deepEqual(validate(staging,'staging',production),[]);
assert.deepEqual(validate(production,'production',staging),[]);
const bad={...production,workers_dev:true};
assert.ok(validate(bad,'production').some(x=>x.includes('workers_dev')));
const same=structuredClone(production);
same.d1_databases[0].database_id=staging.d1_databases[0].database_id;
assert.ok(validate(same,'production',staging).some(x=>x.includes('different D1')));
const leak=structuredClone(production);
leak.vars.MESSAGE_KEY_V1='secret-test-value';
assert.ok(validate(leak,'production').some(x=>x.includes('must not be in vars')));
const secrets={
  ADMIN_KEY:randomBytes(48).toString('base64url'),
  AUTH_KEY_V1:randomBytes(32).toString('base64url'),
  AUTH_KEY_V2:randomBytes(32).toString('base64url'),
  MESSAGE_KEY_V1:randomBytes(32).toString('base64url'),
  MESSAGE_KEY_V2:randomBytes(32).toString('base64url'),
  DEDUP_KEY:randomBytes(32).toString('base64url')
};
assert.deepEqual(validateSecrets(secrets),[]);
const compromised={...secrets,MESSAGE_KEY_V2:secrets.MESSAGE_KEY_V1};
assert.ok(validateSecrets(compromised).length>0,'reused keys must fail');
console.log('PASS: deployment guard, environment isolation, custom domain and secret checks');
