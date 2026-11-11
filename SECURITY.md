# Security and deployment gates

## Implemented
- Cloudflare Worker admission authenticates signed user bearer tokens.
- D1 membership is checked before every send/sync request.
- Conversation messages encrypted at rest with AES-256-GCM; keys derived by HKDF-SHA256 per conversation, purpose-separated from the keyed deduplication HMAC.
- 96-bit cryptographically random nonces from Workers-compatible getrandom; AEAD associated data binds chat and message sequence.
- Idempotency keys prevent duplicate persistent messages and support safe retry.
- SQLite transactionSync commits messages and outbox recipient events together.
- The alarm is armed before commit to recover pending dispatch after a crash; duplicate relays are permissible.
- User WebSockets accepted using Durable Objects Hibernation API and server-controlled identity.
- Per-user Cloudflare rate limiting on sends.
- Distinct secrets: ADMIN_KEY, AUTH_KEY, MESSAGE_KEY.

## Critical limitations before production
1. **Build not yet verified here:** CI is configured but this environment cannot run cargo/worker-build.
2. No immediate revocation of signed bearer tokens (max 24h); production should use server-controlled session versions or short-lived signed access plus revocable refresh sessions.
3. No admin membership revoke/add API nor causal version fencing. Current membership is fixed on creation.
4. No automatic key rotation/key-version format or historical re-encryption tooling.
5. No authenticated browser WebSocket ticket/cookie flow. Native clients can send Authorization header.
6. No CORS origin allowlist. Browser frontends must use same-origin reverse proxy or wait for a configured CORS/token design.
7. No delivery/read device ACKs, Web Push, or storage retention and purge policies.
8. No comprehensive load, chaos, cryptographic vector, failure injection, or cold-start performance tests.
9. Cloudflare/D1 and runtime access controls must be audited and benchmarked before handling sensitive production data.
10. Membership check on each Worker request reads D1; replacing this with caches must include durable revocation fencing before it is safe.
11. Admin provisioning key must be protected and its endpoint restricted using firewall policy/Cloudflare Access.

## Privacy
This is **server-side encryption at rest**, not end-to-end encryption. Anyone with application access to MESSAGE_KEY and chat records can decrypt message content. Transport uses HTTPS/WSS. Sender, recipient IDs, timestamps and chat membership remain in plaintext metadata.

## Future hardening sequence
- Add compile/test gates and obtain successful CI evidence.
- Add session revocation, separate token issuance from administrator provisioning, and WebSocket browser tickets.
- Add membership mutation + consistent revocation fences (including UserHub).
- Add key versions and safe rotation procedure before first production rollout.
- Add explicit per-device delivery acknowledgments, push, retention and failure-injection tests.
- Benchmark cold/warm per-chat DO, D1 lookups and user-DO fanout under sustained loads.
