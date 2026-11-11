# API-chat v1 contract

All identifiers are canonical lowercase UUID strings. All errors are JSON `{"error":"CODE"}` with a 4xx/5xx status. The server can decrypt message text; this is server-encrypted storage, not E2EE.

## Authentication and administration

An administrator calls routes with `X-Admin-Key`. Restrict these routes to Cloudflare Access or private WAF rules.

`POST /v1/admin/token` request `{"user_id":"<uuid>","ttl_seconds":3600}`, returns `{"access_token":"...","expires_in":3600,"session_id":"<uuid>","token_type":"Bearer"}`. Session stored in D1; other requests require `Authorization: Bearer ...`.

`POST /v1/admin/chats` request `{"chat_id":"<uuid>","members":["<uuid>","<uuid>"]}`. Idempotently returns existing room only if members match. A newly created room's SQLite is the authority; D1 member index updates asynchronously.

`PUT /v1/admin/chats/:chat/members/:user` adds a member, with no access to previous messages. `DELETE /v1/admin/chats/:chat/members/:user` immediately revokes access at ChatRoom and drops queued message notifications to that user.

`POST /v1/admin/chats/:chat/rotate` request `{"target_key_version":2}` returns HTTP 202; messages are re-encrypted in the background in batches. The room remembers its active version, so new messages use the rotated key. Old master keys must not be retired until all rooms are finished.

`POST /v1/admin/chats/:chat/prune` request `{"before_ms":<unix-ms>}` returns 202; background alarms replace old message events with tombstones and erase ciphertext payloads.

## Messages and sync

`POST /v1/chats/:chat/messages` request `{"client_message_id":"<uuid>","text":"hola"}`. Returns HTTP 200 and `{"chat_id":"<uuid>","accepted":true,"seq":3,"duplicate":false}`. Repeating the same sender, ID and content returns `duplicate:true` and the original sequence; different content with the same ID returns HTTP 409. ACK means durable ChatRoom storage only.

`GET /v1/chats/:chat/messages?after=0&limit=100` returns `{"chat_id":"<uuid>","changes":[...],"next":3,"has_more":false,"membership_revision":0}`. Each change has a `type` and ascending `seq`: `message`, `message.pruned`, `receipt` or `membership.changed`. Page size 1–200. Persist `next`, deduplicate by event sequence and always synchronize after reconnect.

`GET /v1/chats` lists chat identifiers using an eventually consistent D1 projection, not an authorization oracle.

`POST /v1/chats/:chat/receipts` accepts `{"delivered_seq":3,"read_seq":3}`, requires both cursors to refer to existing message events and `read_seq<=delivered_seq`. Cursors are monotonic per user. Returns `{"changed":true,"seq":4,"delivered_seq":3,"read_seq":3}`.

`GET /v1/chats/:chat/receipts` returns the latest committed receipt cursors for participants. Read authorization is enforced in SQLite of the room.

## WebSocket

Native client: `GET /v1/realtime` with WebSocket Upgrade and `Authorization: Bearer <access_token>`.

Browser client: First `POST /v1/realtime/browser-session` with `Authorization: Bearer <access_token>` and `Origin: https://YOUR_ORIGIN`, then open `new WebSocket('wss://YOUR_ORIGIN/v1/realtime')` from the same origin. The issued `__Host-api-chat-realtime` cookie has a 120-second life, HttpOnly, SameSite=Strict and Secure. Do not put tokens in URLs.

Events: `realtime.ready`, `chat.changed`, `receipt.changed`, `membership.changed`, with `chat_id`, `seq`, `event_id`. The client may send string `ping` and receive `pong`; all other client WebSocket frames are rejected. Send messages through HTTP.

`POST /v1/auth/refresh` issues a new access token for the same still-live session; `POST /v1/auth/revoke` revokes the D1 session, closes its WebSockets and denies further API operations.

## Semantics and limits

- Every chat operation uses server-authenticated caller identity; untrusted JSON user IDs are ignored.
- Message content stays encrypted in ChatRoom SQLite. HTTP sync returns plaintext to members (TLS required).
- Delivery is at-least-once to UserHub; the client deduplicates with `event_id` and syncs via HTTP.
- `accepted` (server commit) is distinct from explicit client `DELIVERED` and `READ` receipts.
- 100 participants per room and 100 messages per user every 10 seconds via Cloudflare rate limiter.
- Browser Origin and WAF/admin access policies must match deployed host.

Integration test: `scripts/integration.mjs`. See `SECURITY.md` before deploying.

Revocation: `session.revoked` with code `4001` tells the client to close the WebSocket and clear its session state. The server also attempts a WebSocket close frame, but clients must not rely on that frame alone.
