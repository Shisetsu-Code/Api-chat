import {readFileSync} from 'node:fs';

const config=JSON.parse(readFileSync(new URL('../wrangler.jsonc',import.meta.url),'utf8'));
const errors=[];
const origin=config.vars?.PUBLIC_ORIGIN;
if(!origin || !/^https:\/\//.test(origin) || /localhost|127\.0\.0\.1|example\.com/.test(origin)){
  errors.push('PUBLIC_ORIGIN must be the real HTTPS origin (not localhost/example.com)');
}
if(!config.d1_databases?.every(db=>/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(db.database_id))){
  errors.push('Set a real D1 database_id created with wrangler');
}
if(!config.durable_objects?.bindings?.some(b=>b.name==='CHAT_ROOM')||
   !config.durable_objects?.bindings?.some(b=>b.name==='USER_HUB')){
  errors.push('Both chat and user Durable Objects are required');
}
if(errors.length){
  console.error('Production deployment blocked:\n'+errors.map(e=>' - '+e).join('\n'));
  process.exit(1);
}
console.log('Configuration validated. Ensure secrets, migrations, Access and WAF are configured before deploy.');
