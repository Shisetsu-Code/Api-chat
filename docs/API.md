# API contract (v1)

All identifiers are UUIDs. The server stores encrypted message text in its Durable Object SQLite database; authenticated sync responses contain plaintext. TLS is required.

## Provision a chat

```sh
curl -X POST "https://YOUR_WORKER/v1/admin/chats" \
  -H "X-Admin-Key: $ADMIN_KEY" -H "Content-Type: application/json" \
  -d '{"chat_id":"11111111-1111-4111-8111-111111111111","members":["22222222-2222-4222-8222-222222222222","33333333-3333-4333-8333-333333333333"]}'
```

## Issue a short-lived user token

```sh
curl -X POST "https://YOUR_WORKER/v1/admin/token" \
  -H "X-Admin-Key: $ADMIN_KEY" -H "Content-Type: application/json" \
  -d '{"user_id":"22222222-2222-4222-8222-222222222222","ttl_seconds":3600}'
```

Tokens use a 32-byte HMAC-SHA256 signing key in Cloudflare secrets. They expire within 24 hours, are scoped to one user, and **are not instantly revocable** in this version.

## Send a message

```sh
curl -X POST "https://YOUR_WORKER/v1/chats/11111111-1111-4111-8111-111111111111/messages" \
  -H "Authorization: Bearer $TOKEN_A" -H "Content-Type: application/json" \
  -d '{"client_message_id":"44444444-4444-4444-8444-444444444444","text":"Hola"}'
```

Response: `{"accepted":true,"chat_id":"...","seq":1,"duplicate":false}`. An identical retry returns `duplicate:true` with the same sequence; a different message body for an existing ID is rejected. `accepted` means durable server storage, **not recipient delivery**.

## Sync

```sh
curl "https://YOUR_WORKER/v1/chats/11111111-1111-4111-8111-111111111111/messages?after=0&limit=100" \
 -H "Authorization: Bearer $TOKEN_B"
```

Response: `{"chat_id":"...","messages":[{"seq":1,"sender_id":"...","text":"Hola",...}],"next":1,"has_more":false}`.

On reconnect: fetch `GET /v1/chats` with bearer, then paginate each chat from its last committed cursor. A WebSocket notification indicates that sync may be needed.

## WebSocket

Native clients: `GET /v1/realtime`, `Upgrade: websocket`, `Authorization: Bearer $TOKEN`.

- Server: `{"type":"realtime.ready"}`
- Server: `{"type":"chat.changed","chat_id":"...","seq":1}`
- Client: text `ping`, server: text `pong`.
- Other client frames are rejected; use HTTPS to send messages.

**Browser warning:** the standard JavaScript WebSocket constructor does not allow arbitrary Authorization headers. A first-party cookie/ticket flow must be added before browser consumption. Do not use bearer tokens in query parameters.

## Error and rate behavior

- 400 invalid request
- 401 missing/invalid credentials
- 403 user is not a member
- 404 unknown route
- 409 duplicate ID with different content / conflicting provisioning
- 429 per-user rate limit (100 messages / 10 seconds)
- 503 transient backend failure (the client should retry the same message ID)

## Delivery semantics

- The chat DO persists the message and recipient outbox rows atomically.
- Alarms deliver pending notifications to user DOs at least once.
- A relay ACK only acknowledges receipt by the user DO; it **does not** prove WebSocket receipt or display.
- Offline devices catch up from the chat DO SQLite source of truth.
- No delivered/read receipts, push notifications or per-conversation user-side ACL cache yet.
