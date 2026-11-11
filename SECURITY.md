# Security and operational acceptance gates

Status: development branch. Passing CI is necessary but insufficient for production authorization.

## Trust boundaries

- The public Worker authenticates a signed HMAC access token, checks the D1 session against revocation/expiry on every HTTP request, and passes server-derived identity to the conversation DO. Caller-supplied user IDs never confer permissions.
- The room DO's SQLite membership table is authoritative: membership checks and message writes occur in one transaction. This avoids stale D1 membership caches granting access.
- The D1 `chat_member_index` is a projection for discovery, not for message security. `member_index_outbox` retries projection.
- The UserHub DO receives notifications from the room DO (not plaintext), and revalidates D1 sessions for every pushed WebSocket event. After self-revocation, the hub closes matching sockets.
- Admin routes require `X-Admin-Key`. Put them behind **Cloudflare Access** or dedicated WAF allow rules, segregate admin credentials and rotate if suspected compromised. The Admin key gives authority to create users, chats, alter membership and delete old content.
- TLS is required in deployment. The browser session sets a short-lived, HttpOnly, Secure, SameSite=Strict cookie, limited to `/v1/realtime` by audience, with exact Origin checking.

## Cryptography

- Cloudflare Secrets hold two possible versions of the 32-byte server encryption key. Derive AES-256-GCM keys separately for each conversation with HKDF-SHA256.
- Each encrypted message has a random 96-bit nonce; associated data authenticates `chat_id`, sequence and key version.
- Never log plaintext message bodies or keys. The content is decrypted only in the authorized sync response.
- A separate HMAC key `DEDUP_KEY` computes deterministic, keyed fingerprints for idempotency checks.
- Tokens have an HMAC signature, key-version prefix, audience, UUID subject and session ID, and an expiry no greater than 3600 seconds for access or 120 seconds for WebSocket cookies.
- Server-side encryption at rest is **not end-to-end encryption**; anyone with the running Worker secret and database access can decrypt data. For E2EE you need a client-side authenticated key agreement and ratchet protocol.

## Correctness

- Message + recipient outbox inserts occur in one SQLite `transactionSync`; successful ACK follows `storage.sync()`.
- The outbox dispatches hints to a user DO at least once with persistent retry and exponential backoff, bounded to 60 seconds between attempts.
- A user DO returning HTTP 200 acknowledges relay processing, **not** WebSocket receipt. Explicit `DELIVERED` and `READ` cursors are committed in the room DO.
- Clients must deduplicate by `event_id`, use `seq` for cursor ordering and recover missed data through paged HTTP sync.
- A revoked member cannot send, sync or issue receipts. On adding a user, the first visible message is after `joined_seq`; no previous plaintext history is revealed to that newly added member.
- Outbox alarms are arranged before message writes, and alarm cleanup is gated against concurrent sends. A sustained downstream outage uses durable, uncapped rearm cycles.
- Pruning replaces old message events with tombstones; the event/receipt log remains. Long-term high-volume history compaction, backups and data export require an explicit retention policy.

## Deployment and validation requirements

Before carrying sensitive production traffic:

1. CI must pass `cargo check`, Clippy, native unit tests, Workers build and HTTP/WebSocket integration tests.
2. Run remote Cloudflare staging load and fault-injection tests: 1k/10k messages, >100 simultaneous sockets, high latency, abrupt DO eviction, D1 outage, duplicate packets, revoked member during fanout, simulated lost ACK.
3. Verify D1 migrations and Cloudflare account bindings for the exact deployment environment. Do not deploy with placeholder IDs.
4. Provision all secrets via Wrangler, configure real HTTPS origin, set Cloudflare Access/WAF on admin paths, enable operational alerting and logs without sensitive payloads.
5. Confirm that **all** chats have completed server-side key rotation and backup/recovery before destroying an old encryption key. Rotating `DEDUP_KEY` also requires a migration.
6. Establish deletion/retention and disaster recovery policies with automated testing, and a process for revoking credentials during incidents.
7. Audit the encrypted storage and identity threat model with a qualified security reviewer.

## Known limitations

- Text messages only; no attachments, E2EE, federation, mobile push or mandatory one-WebSocket-per-device enforcement.
- No global admin inventory endpoint to prove full fleet key-rotation completion; old key retirement must be audited externally.
- No global event-log compaction, automated data backups, distributed tracing or SLO monitoring configured in this repository.
- Up to 100 members per conversation, with per-user rate limiting. One conversation DO has finite throughput. Large public channels need a different fanout design.
- GitHub Actions local success does not prove Cloudflare production performance or resistance to malicious traffic.

Production readiness requires evidence for the deployment gates, not simply compiling this code.

Revoked WebSocket sessions receive a `session.revoked` event and an attempted close frame. The client must close the socket when this event arrives. Every subsequent notification is gated by a live D1 session check even if local WebSocket transport delays the close frame.
