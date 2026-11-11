# Api-chat

API de chat implementada principalmente en Rust/WASM sobre **Cloudflare Workers + D1 + Durable Objects SQLite**. Diseño con DO por conversación y DO por usuario, almacenamiento de mensajes cifrado con AES-256-GCM del lado servidor, idempotencia y outbox durable.

**Estado:** CI funcional local aprobado. **Despliegue remoto y validación de capacidad/p95 en Cloudflare pendientes.** El cifrado del servidor **no es E2EE**.

## Arquitectura

    Cliente A -- HTTPS POST --> Worker (token/sesión D1 + cuotas)
                                  |
                                  v
                       ChatRoom DO (por conversación)
                        ACL local, seq, SQLite, AES-GCM
                        commit mensaje + outbox -> ACK
                                  |
                             fanout/retry
                                  v
                        UserHub DO (por usuario)
                         WSS para dispositivos
                                  |
                                  v
                             Cliente B
                  (chat.changed -> sync HTTP)

- Los mensajes quedan autorizados, secuenciados y persistidos en SQLite del ChatRoom.
- El ACK accepted significa persistencia del servidor, no llegada al receptor.
- El UserHub mantiene WebSockets hibernables y distribuye notificaciones ligeras; **el contenido se recupera por HTTPS autenticado**.
- D1 controla sesiones revocables; una proyección eventual permite descubrir chats, nunca concede permisos.
- Cada cambio de membresía se valida dentro del DO de conversación. Los reintentos emplean el mismo client_message_id.
- Se retienen secretos en Cloudflare Secrets y se derivan claves por conversación con HKDF-SHA256.

**Rendimiento:** el objetivo es recepción p95 inferior a 200 ms para usuarios online en una red/ámbito medidos. Este objetivo **no está demostrado** bajo 250 clientes y 1.000 mensajes/s. El trabajo de fanout inmediato está en [PR #5](https://github.com/Shisetsu-Code/Api-chat/pull/5), todavía experimental y **no integrado a main**.

## Inicio rápido (sin Cloudflare remoto)

Requisitos: Node 24+, Rust estable, target wasm32-unknown-unknown, worker-build 0.8.7 y dependencias bloqueadas.

    npm ci
    rustup target add wasm32-unknown-unknown
    cargo install worker-build --version 0.8.7 --locked
    npm run build
    npx wrangler d1 migrations apply DB --local
    npm run dev

El archivo wrangler.jsonc es **solo para desarrollo local**: utiliza un database_id placeholder y PUBLIC_ORIGIN HTTP. No desplegar ese archivo. Para pruebas automatizadas, CI genera .dev.vars con claves sintéticas.

## Subir a Cloudflare (staging y producción)

El despliegue real usa dos configuraciones independientes que se crean a partir de plantillas, ambas excluidas de Git.

    npm run config:init:staging
    npm run config:init:production
    npm run secrets:new:staging
    npm run secrets:new:production

Crear las bases D1 reales por separado:

    npx wrangler d1 create api-chat-staging-db
    npx wrangler d1 create api-chat-production-db

Rellenar los UUID de D1 y orígenes HTTPS en wrangler.staging.jsonc y wrangler.production.jsonc. Nunca reutilizar D1 ni claves entre entornos. Respaldar las claves generadas en un vault.

    npm run test:deploy-config
    npm run deploy:check:staging
    npm run deploy:check:production

Cuando staging esté configurado y se disponga de acceso a Cloudflare:

    npm run deploy:staging -- --secrets-file .secrets/api-chat.staging.json

El comando compila, aplica migraciones D1 remotas y despliega usando la configuración de staging. Se puede verificar con npm run smoke:staging después de cargar CHAT_SMOKE_ORIGIN y CHAT_SMOKE_ADMIN_KEY en variables de entorno. Para producción, primero completar el checklist de seguridad y rendimiento remoto; luego:

    npm run deploy:production -- --secrets-file .secrets/api-chat.production.json

**No hay despliegue automático, Cloudflare Access, WAF ni credenciales configuradas desde GitHub.** No se crean o modifican recursos remotos hasta ejecutar explícitamente los comandos --apply en una terminal autenticada. Un preflight bloquea dominios/UUID ficticios y requiere la confirmación de producción.

Manual completo: **[docs/CLOUDFLARE_DEPLOY.md](docs/CLOUDFLARE_DEPLOY.md)**.

## API resumida

- GET /health
- POST /v1/admin/token, POST /v1/admin/chats
- PUT/DELETE /v1/admin/chats/:chat/members/:user
- POST /v1/chats/:chat/messages, GET /v1/chats/:chat/messages?after=0&limit=100
- POST/GET /v1/chats/:chat/receipts
- GET /v1/chats, GET /v1/realtime (WSS)
- POST /v1/realtime/browser-session
- POST /v1/auth/refresh y /v1/auth/revoke
- POST /v1/admin/chats/:chat/rotate y /prune

El cliente debe cerrar su WebSocket cuando recibe session.revoked (code 4001). No introducir bearer tokens en querystrings de WebSocket.

Detalles: **[docs/API.md](docs/API.md)**.

## Validación y límites

    npm run check
    cargo test --lib --locked
    npm run test:deploy-config
    npm run build

CI ejecuta compilación, Clippy, pruebas unitarias, migraciones D1 locales, integración HTTP/WS, carga de concurrencia, validación de templates y dry-run. El smoke de staging usa datos sintéticos. La aceptación de producción exige pruebas remotas con errores de red, timeouts, concurrencia, rate limiting, backups y mediciones de latencia y disponibilidad.

**Documentación:** [Deploy](docs/CLOUDFLARE_DEPLOY.md) · [Operación](docs/OPERATIONS.md) · [Seguridad](SECURITY.md) · [Benchmark](docs/benchmarks/2026-11-11-250-clients.md).

Licencia MIT.
