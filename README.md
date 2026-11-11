# Api-chat

Backend de mensajería Rust/WASM sobre **Cloudflare Workers + Durable Objects SQLite + D1**, con cifrado del lado del servidor. Sin backend TypeScript ni servidor externo.

## Arquitectura

```
Emisor HTTPS POST -> Worker (autenticación D1) -> ChatRoom DO (SQL/ACL)
                                              -> Commit message + outbox -> ACK
                                              -> Alarma -> UserHub DO -> WebSocket de usuario
Receptor -> GET /v1/chats/:chat/messages?after=... -> Worker -> ChatRoom DO -> Sync cifrado en reposo
```

- **ChatRoom (uno por conversación)** es autoridad para membresía, secuencias, almacenamiento de mensajes AES-256-GCM, recibos y outbox transaccional.
- **UserHub (uno por usuario)** mantiene WebSockets hibernables y notificaciones ligeras por conversación; no almacena contenido.
- **D1** contiene sesiones revocables y una proyección eventual del índice de membresías para `GET /v1/chats`; no decide permisos por mensaje.
- Notificaciones y proyecciones usan reintentos durables; los clientes deben deduplicar y sincronizar desde SQLite de ChatRoom.
- Cifrado del servidor **no es E2EE**. El proceso que posee las claves puede descifrar mensajes. Metadatos de remitente, timestamps y miembros no están cifrados.

## API

`GET /health`

Administración (exigir Cloudflare Access/WAF además del secreto `X-Admin-Key`):
- `POST /v1/admin/token` `{"user_id":"<uuid>","ttl_seconds":3600}` -> token Bearer ligado a sesión D1.
- `POST /v1/admin/chats` `{"chat_id":"<uuid>","members":["<uuid>","<uuid>"]}`.
- `PUT /v1/admin/chats/:chat/members/:user` y `DELETE ...`: membresía autoritativa en DO.
- `POST /v1/admin/chats/:chat/rotate` `{"target_key_version":2}`: solicita re-cifrado incremental.
- `POST /v1/admin/chats/:chat/prune` `{"before_ms":<unix ms>}`: elimina contenido antiguo y deja tombstones.

Usuario autenticado:
- `GET /v1/chats`: índice eventual.
- `POST /v1/chats/:chat/messages` `{"client_message_id":"<uuid>","text":"hola"}`.
- `GET /v1/chats/:chat/messages?after=0&limit=100`: eventos ordenados, cursor `next`.
- `POST /v1/chats/:chat/receipts` `{"delivered_seq":1,"read_seq":1}`, `GET .../receipts`.
- `POST /v1/realtime/browser-session`: crea cookie `__Host-api-chat-realtime` corta, Secure, HttpOnly, SameSite=Strict.
- `GET /v1/realtime` WebSocket vía cookie (navegador) o Authorization Bearer (cliente nativo).
- `POST /v1/auth/refresh` y `POST /v1/auth/revoke`.

El servidor confirma `accepted` únicamente tras persistir. No equivale a `DELIVERED` o `READ`. La entrega WebSocket es un aviso para realizar sync; los recibos son enviados explícitamente por el cliente.

## Instalación

Requisitos: Rust estable con target `wasm32-unknown-unknown`, `worker-build = 0.8.7`, Node 24+ y Wrangler `4.160.0`.

```bash
rustup target add wasm32-unknown-unknown
cargo install worker-build --version 0.8.7 --locked
npm ci
npx wrangler d1 create api-chat-db
```

Actualizar el `database_id` en `wrangler.jsonc`. Crear secretos **distintos**:
- `ADMIN_KEY`: texto aleatorio de al menos 32 caracteres; administrar fuera del navegador.
- `AUTH_KEY_V1` y `AUTH_KEY_V2`: cada uno 32 bytes aleatorios, codificados base64url sin padding.
- `MESSAGE_KEY_V1`, `MESSAGE_KEY_V2`: 32 bytes, base64url.
- `DEDUP_KEY`: 32 bytes, base64url; mantenerlo estable, independiente de claves de cifrado.

Usar `npx wrangler secret put NOMBRE`. Cambiar `PUBLIC_ORIGIN` por la URL HTTPS real y usar `ACTIVE_AUTH_KEY_VERSION`, `ACTIVE_MESSAGE_KEY_VERSION` inicialmente en `1`.

```bash
npx wrangler d1 migrations apply DB --remote
npm run check
npm run build
npm run predeploy:check
npm run deploy
```

**No despliega automáticamente:** necesitás conectar tu cuenta Cloudflare, definir dominio/origen, crear D1, configurar secretos y políticas de acceso. El script de despliegue bloquea IDs de D1 ficticios y orígenes no HTTPS.

## Rotación de claves

- Crear secretos V2 sin retirar V1.
- `POST .../rotate` por cada conversación: el DO cambia su versión activa inmediatamente y re-cifra mensajes antiguos por lotes con alarma durable.
- Verificar que cada DO terminó la migración antes de retirar la clave anterior. No hay coordinación global automática de retiro de claves entre todas las conversaciones. **No eliminar V1 sin haber comprobado cada sala**.
- `DEDUP_KEY` no se rota junto al contenido; rotarla requiere migrar huellas idempotentes.

## Desarrollo y validación

```bash
cargo fmt --all --check
cargo check --target wasm32-unknown-unknown --locked
cargo clippy --target wasm32-unknown-unknown --locked -- -D warnings
cargo test --lib --locked
npm run build
```

`scripts/integration.mjs` cubre HTTP, WebSocket, D1, Durable Objects, deduplicación, permisos, recibos, rotación, revocación y retención; el workflow CI genera secretos sintéticos y realiza la prueba contra Wrangler local. No usa credenciales reales.

La disponibilidad en Cloudflare real, la seguridad ante adversarios y las propiedades de rendimiento no se infieren de pruebas locales. Ver [API](docs/API.md) y [seguridad/operación](SECURITY.md) para límites y gates de producción.

Licencia MIT.

Protocol requirement: browser/native clients must close their WebSocket and clear local credentials when they receive `session.revoked` (code `4001`); a transport-level close frame is also requested.
