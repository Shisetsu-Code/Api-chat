# Despliegue de Api-chat en Cloudflare

Guía operativa para noviembre de 2026. Backend Rust/WASM, Cloudflare Workers, un Durable Object SQLite por conversación y otro por usuario, D1 para sesiones e índices. **Cifrado AES-256-GCM en servidor; no es E2EE**.

**Estado:** el código dispone de pruebas CI locales, pero todavía **no hay evidencia de despliegue ni pruebas de carga en Cloudflare remoto**. Estos comandos preparan la infraestructura: ninguna cuenta ni recurso Cloudflare se crea desde el chat.

## 1. Estructura y aislamiento

| Entorno | Worker | Endpoint | D1 | Durable Objects |
|---|---|---|---|---|
| Local | api-chat | http://127.0.0.1:8787 | Simulada | Locales |
| Staging | api-chat-staging | https://api-chat-staging.SUBDOMINIO.workers.dev | api-chat-staging-db | ChatRoom/UserHub exclusivos |
| Producción | api-chat-production | https://chat-api.TUDOMINIO | api-chat-production-db | ChatRoom/UserHub exclusivos |

Los ejemplos versionados son wrangler.staging.example.jsonc y wrangler.production.example.jsonc. El contenido real de wrangler.staging.jsonc y wrangler.production.jsonc se mantiene fuera de Git. Cada configuración declara por separado D1, bindings, Durable Object SQLite exports y nombres de secretos requeridos. Los nuevos Workers usan la sintaxis exports de Cloudflare; el archivo local wrangler.jsonc conserva el esquema legado verificado en los CI históricos. No mezclar namespaces ni reutilizar claves entre entornos.

## 2. Requisitos

- Cuenta Cloudflare con acceso a Workers, Durable Objects y D1.
- Node.js 24, Rust estable, wasm32-unknown-unknown y worker-build 0.8.7.
- Wrangler 4.160.0 **fijado por package-lock.json**. Actualizar dependencias solo en un PR probado.
- Un dominio administrado por Cloudflare para producción. Se recomienda Custom Domain y deshabilitar workers.dev en producción.
- Cloudflare Access/WAF para /v1/admin/*, separado del acceso público a los clientes.
- Un gestor seguro de secretos/backup, monitoreo y presupuesto de consumo.

Instalación (terminal en raíz del repo):

    npm ci
    rustup target add wasm32-unknown-unknown
    cargo install worker-build --version 0.8.7 --locked
    npx wrangler login
    npx wrangler whoami

## 3. Crear entornos

    npm run config:init:staging
    npm run config:init:production

Se crearán los archivos de configuración no versionados. Estos comandos se niegan a sobrescribir configuraciones existentes.

Crear D1 remotas independientes:

    npx wrangler d1 create api-chat-staging-db
    npx wrangler d1 create api-chat-production-db

Copiar cada database_id UUID devuelto por Cloudflare a la configuración correspondiente. No reutilizar IDs. En staging, reemplazar REPLACE_CLOUDFLARE_SUBDOMAIN por el subdominio workers.dev de la cuenta. En producción, configurar un hostname real en **ambos** campos routes[0].pattern y vars.PUBLIC_ORIGIN (mismo hostname, con https:// en PUBLIC_ORIGIN). El dominio debe estar en una zona Cloudflare apta para Custom Domains.

El preflight impide producción con workers_dev=true y rechaza hosts locales, dominios de ejemplo, configuraciones mezcladas y D1 no configuradas.

## 4. Crear y custodiar secretos

Cada entorno requiere:
- ADMIN_KEY: material aleatorio usado exclusivamente en operaciones administrativas.
- AUTH_KEY_V1, AUTH_KEY_V2: claves de 32 bytes codificadas base64url sin padding para firmar tokens.
- MESSAGE_KEY_V1, MESSAGE_KEY_V2: claves de 32 bytes para derivar claves AES-GCM por conversación.
- DEDUP_KEY: clave independiente de 32 bytes para huellas HMAC de idempotencia.

Generar **una sola vez**:

    npm run secrets:new:staging
    npm run secrets:new:production

Escribe .secrets/api-chat.staging.json y .secrets/api-chat.production.json con permisos 0600 (en Unix), ambos en .gitignore, y sin mostrar valores por pantalla. En Windows, proteger con ACL/BitLocker y un gestor de secretos; 0600 no garantiza restricciones NTFS.

**Antes del despliegue**, almacenar estas claves en un gestor externo y verificar recuperación. Perder MESSAGE_KEY_V1/V2 puede volver ilegibles conversaciones. No regenerar claves en un despliegue posterior. El script se niega a sobrescribirlas.

Cloudflare permite aportar los secretos de primer despliegue mediante wrangler deploy --secrets-file (archivo local JSON); la configuración secrets.required obliga a que estén definidos. Si ya están cargados, el archivo puede omitirse. El usuario es responsable de destruir copias temporales de forma segura.

## 5. Validación sin cambios remotos

    npm run test:deploy-config
    npm run deploy:check:staging
    npm run deploy:check:production

Estos comandos revisan orígenes HTTPS, identificadores, bindings, variables, secretos requeridos, aislamiento y construcción del Worker. Ejecutan wrangler deploy --dry-run. **No aplican migraciones ni hacen deploy real**.

Las configuraciones de ejemplo no superan estas validaciones hasta reemplazar los marcadores. Eso es deliberado.

## 6. Primer despliegue en staging

Después de validar:

    npm run deploy:staging -- --secrets-file .secrets/api-chat.staging.json

El script exige que la configuración sea válida, compila Rust/WASM, aplica las migraciones de D1 remota y despliega el Worker con los secretos indicados. Las migraciones hacen cambios reales y Wrangler puede solicitar confirmación. En primeras versiones, los Durable Objects con SQLite se provisionan desde exports.

Para comprobar la URL desplegada sin usar datos de clientes, configurar CHAT_SMOKE_ORIGIN y CHAT_SMOKE_ADMIN_KEY en el terminal usando el gestor de secretos, y ejecutar:

    npm run smoke:staging

El smoke test se niega a ejecutarse sobre producción. Genera usuarios/chat sintéticos en staging, prueba POST/GET, WebSocket, deduplicación, recibos y revocación. Deja registros sintéticos; nunca ejecutarlo con una base real de clientes. Si Cloudflare Access protege el área administrativa, configurar CF_ACCESS_CLIENT_ID y CF_ACCESS_CLIENT_SECRET como variables temporales de entorno (no en argumentos ni logs).

La URL pública de staging **debe tener restricciones de acceso y WAF también**. Un endpoint workers.dev puede ser descubierto; el X-Admin-Key por sí solo no sustituye Zero Trust.

## 7. Preparación y despliegue en producción

Puertas obligatorias, antes de aceptar tráfico real:

1. Staging funcionando en infraestructura Cloudflare real y con sus propias claves.
2. Ensayos remotos de 250 clientes × 4 mensajes/s, 1.000 msg/s sostenidos, latencia p95 de recepción inferior a 200 ms dentro del alcance de red definido; registrar ACK p95 y p99 por separado.
3. Fallos inyectados: revocación, reconexión, duplicados, caída de D1, reinicio de Durable Object, reintentos de outbox, pérdidas de ACK.
4. Configurar dominio HTTPS de producción, Cloudflare Access/WAF para /v1/admin/*, observabilidad y alertas, política de backups/retención, pruebas de restauración y límites de costos.
5. Validar que todas las claves están en el gestor de secretos y que el procedimiento de rotación es recuperable.
6. Revisar riesgos de gestión de tokens y cumplimiento de privacidad. El servicio no implementa E2EE ni un login público autogestionado.

Con las puertas aprobadas:

    npm run deploy:check:production
    npm run deploy:production -- --secrets-file .secrets/api-chat.production.json

El comando de producción contiene una confirmación explícita. Se aplican migraciones remotas a D1 de producción y luego deploy. Mantener habilitada la validación secrets.required.

Para despliegues posteriores con secretos ya existentes:

    npm run deploy:staging
    npm run deploy:production

El comando genérico npm run deploy **no despliega**: necesita especificar el entorno.

## 8. Verificación tras desplegar

- GET /health devuelve 200 y status=ok, pero solo demuestra vida del Worker, no que D1 y los DO funcionen.
- Comprobar D1 migrado, ChatRoom, UserHub, cookies Secure/HttpOnly, sesiones, permisos y WebSockets con pruebas sintéticas en staging.
- Observar errores 5xx/429, tiempos de confirmación, p95/p99 de entrega, backlog y edad de outbox, utilización/costos D1/DO/Worker.
- Mantener el acceso al dominio workers.dev de producción desactivado; usar el Custom Domain configurado.
- No activar tráfico de usuarios reales antes de completar el checklist de la sección 7.

## 9. Rollback y recuperación

El historial de despliegues Workers permite seleccionar y revertir una versión:

    npx wrangler deployments list --config wrangler.production.jsonc
    npx wrangler rollback --config wrangler.production.jsonc

**Rollback de código no revierte D1 ni SQLite de los DO.** No existe un down-migration automático. Verificar compatibilidad hacia atrás de esquemas antes de liberar versiones; ante un cambio irreversible, restaurar backup/PITR conforme al procedimiento aprobado. No borrar D1, namespaces DO o claves durante una reversión. Cloudflare indica que cambios de lifecycle de DO pueden impedir rollback.

La rotación de claves actual es por conversación, no tiene inventario global de finalización. No retirar claves antiguas hasta auditar todas las salas. DEDUP_KEY debe permanecer estable, salvo que se ejecute una migración específica de fingerprints. Para incidentes, revocar sesiones, aislar la API, preservar evidencia sin plaintext y seguir docs/OPERATIONS.md.

## 10. Lo que sigue pendiente

Este repositorio prepara el despliegue, **no** configura automáticamente Cloudflare Access/WAF, DNS real, cuenta, D1, secretos, backup, monitoreo remoto ni pruebas de carga remotas. La arquitectura base (main) confirma mensajes durables y notifica por outbox/alarma. El despacho inmediato está evaluándose en el [PR #5](https://github.com/Shisetsu-Code/Api-chat/pull/5); no se debe afirmar p95 <200 ms bajo 1.000 msg/s por las pruebas locales actuales.

Referencias Cloudflare:
- https://developers.cloudflare.com/workers/wrangler/configuration/
- https://developers.cloudflare.com/workers/wrangler/commands/d1/
- https://developers.cloudflare.com/durable-objects/reference/durable-objects-migrations/
- https://developers.cloudflare.com/workers/versions-and-deployments/rollbacks/
- https://developers.cloudflare.com/workers/configuration/secrets/
