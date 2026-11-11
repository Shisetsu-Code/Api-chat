# Api-chat

API de mensajería en Rust/WASM para Cloudflare Workers y Durable Objects.

## Diseño
- Un `ChatRoom` (Durable Object SQLite) por conversación: orden, idempotencia, cifrado AES-256-GCM en reposo y outbox transaccional.
- Un `UserHub` (Durable Object) por usuario: un WebSocket por dispositivo, multiplexación de eventos de distintos chats y Hibernation API.
- Envío: cliente -> Worker autenticado -> DO ChatRoom -> DO UserHub. El ACK de envío solamente confirma persistencia en ChatRoom; recibir una notificación no confirma entrega al dispositivo.
- Sincronización: HTTP paginado desde ChatRoom. Notificaciones en tiempo real son indicios, nunca fuente de verdad.
- D1: membresías y sesiones administradas. El Worker comprueba membresía en cada solicitud; la caché de autorización/versionado es trabajo futuro y no se anuncia como implementada.
- **Cifrado del lado servidor, NO E2EE**: el servidor puede descifrar los mensajes. TLS protege transporte. AES-256-GCM protege el contenido persistido en SQLite.

## Puesta en marcha

1. Instalar Rust compatible, `wasm32-unknown-unknown`, `worker-build` y Node.js. `npm install`.
2. Crear D1: `npx wrangler d1 create api-chat-db`, y reemplazar `REPLACE_WITH_D1_DATABASE_ID` en `wrangler.jsonc`.
3. Aplicar `npx wrangler d1 migrations apply DB --remote`.
4. Generar tres secretos independientes: `ADMIN_KEY` (alta entropía), `AUTH_KEY` (32 bytes en base64url sin padding), `MESSAGE_KEY` (32 bytes en base64url sin padding). Configurar con `npx wrangler secret put NOMBRE`.
5. `npm run build`, `npm run deploy`.

No publicar tokens ni secretos. Girar `MESSAGE_KEY` sin un procedimiento de re-cifrado deja ilegibles los mensajes anteriores. No se incluye rotación de claves aún.

## HTTP

- `GET /health` sin credenciales.
- `POST /v1/admin/token` header `X-Admin-Key`, JSON `{"user_id":"uuid","ttl_seconds":3600}` -> bearer HMAC firmado, máximo 24h.
- `POST /v1/admin/chats` header `X-Admin-Key`, JSON `{"chat_id":"uuid","members":["uuid","uuid"]}`. Los miembros deben ser únicos (2-100).
- `POST /v1/chats/{chat_id}/messages` header `Authorization: Bearer <token>`, body `{"client_message_id":"uuid","text":"hola"}`.
- `GET /v1/chats/{chat_id}/messages?after=0&limit=100` mismo token, cursor de secuencia.
- `GET /v1/realtime` con header `Authorization: Bearer <token>` y Upgrade WebSocket. Una sola conexión por dispositivo; los eventos contienen `chat_id` y `seq`. En navegadores, usar una cookie HttpOnly/SameSite o un ticket efímero para WebSocket en vez de exponer bearer tokens en URL (no implementado en esta versión).
  
## Garantías y límites de esta primera versión
- Envíos idempotentes por `(sender_id,client_message_id)`, secuencia monotónica por chat.
- Mensaje y outbox se insertan en la misma transacción SQLite.
- Outbox con reintento por alarma, entrega **al DO de usuario al menos una vez**. Los clientes deben deduplicar eventos.
- El evento WebSocket **no** equivale a recibo de entrega. No hay recibos `DELIVERED`/`READ` ni push móvil en esta versión.
- No se han ejecutado pruebas de compilación ni de despliegue desde este entorno. Revisar y ejecutar validaciones antes de producción.
- No hay límites de cuota completos, rotación de llaves, revocación inmediata de tokens, ni control de grupos masivos. No usar en producción sensible hasta implementarlos y auditarlos.

## Dependencias (verificadas 2026-11-11)
- `worker = 0.8.7`, `worker-build = 0.8.7`.
- `wrangler = 4.160.0`.
- El resto usa versiones fijadas conservadoras compatibles con el workspace Ratchet-Rust; deben resolverse y probarse antes de reclamar compatibilidad total.

Licencia MIT.
