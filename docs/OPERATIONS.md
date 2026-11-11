# Operación, recuperación y objetivos de servicio

**Aplicación:** Api-chat Rust/WASM, Cloudflare Workers, D1 y Durable Objects SQLite.  
**Estado:** procedimientos preparados, **no habilitados ni probados en cuenta Cloudflare remota**. Cifrado en servidor; no E2EE.

## Responsabilidad de cada componente

| Componente | Autoridad / responsabilidad | Si falla |
|---|---|---|
| Worker HTTP | Autenticación, rate limit, routing, validación | 5xx/timeout; cliente reintenta con el MISMO client_message_id |
| D1 auth_sessions | Sesiones vivas, vencimiento y revocación | Falla cerrada la autorización; no se deben otorgar accesos |
| ChatRoom DO SQLite | Miembros, eventos, mensajes cifrados, outbox, receipts | Recuperación al reactivarse; ACK solo tras persistencia |
| D1 chat_member_index | Proyección eventualmente consistente para listar chats | Puede retrasarse; nunca decide permisos |
| UserHub DO | WebSockets por usuario y notificaciones chat.changed | No confirma entrega a un dispositivo; sync por HTTP |
| Alarm de ChatRoom | Retry durable del outbox / rotaciones / pruning | Puede haber retraso; procesar backlog al reactivar |

El ACK accepted significa commit duradero **en el DO**, no que B haya recibido o leído el contenido. Cliente receptor debe sincronizar con after/limit; receipts explícitos se registran en ChatRoom.

## SLO objetivo, todavía no demostrado

Definir una región/red de prueba concreta y separar tres métricas:

1. **ACK p95**: inicio HTTP POST de A -> ACK del servidor (objetivo preliminar <100 ms).
2. **Notificación p95**: POST de A -> chat.changed en WebSocket B (objetivo preliminar <150 ms).
3. **Contenido recibido p95**: POST de A -> contenido disponible para B tras sync autenticado (objetivo **<200 ms** para usuarios online y misma región).

Otros gates: p99 contenido <500 ms, cero mensajes ACK perdidos, sin autorización posterior a revocación, disponibilidad definida por cliente, 250 usuarios y 1.000 mensajes/s durante un período largo y reproducible. Ninguno de esos números equivale a un compromiso de producción hoy.

Resultados existentes: 250 clientes / 1.000 msg/s de carga objetivo en un runner local: solo 34,6–49,8 ACK/s dentro de la ventana, con timeouts, aunque no se perdieron mensajes ACK en sync. El test de un chat con fast fanout del [PR #5](https://github.com/Shisetsu-Code/Api-chat/pull/5) obtuvo p95 41,7 ms en laboratorio, pero el benchmark global no cumplió el objetivo. **No extrapolar**.

## Métricas y alertas

**Disponibles parcialmente:** Workers observability, errores HTTP, registros de CI y benchmarks sintéticos. El código aún **no** publica métricas estructuradas de backlog/outbox ni trazas por etapa. No inventar paneles que no existen.

Instrumentación a implementar antes de producción intensiva:
- Worker: auth D1 p50/p95/p99, rate-limit, queueing, requests y 5xx.
- ChatRoom: duración de autorización SQLite, transacción AES-GCM, storage.sync, latencia ACK, tamaño y edad máxima del outbox, intentos de entrega.
- UserHub: tiempo entre notificación interna y WebSocket, sockets vigentes, cierre por revocación.
- Receptor: tiempo hasta plaintext sincronizado por HTTP, desconexiones, lag por revisión.
- Métricas globales: costo/rendimiento por 1.000 mensajes, estados D1/DO, errores de migración, tiempos de backend bajo picos.

Alertas propuestas: 5xx >1% en 5 minutos, ACK p95 >200 ms, entrega p95 >500 ms, outbox más antiguo >30s, backlog creciente 5 minutos, incremento anormal de 401/403/429, errores de clave o de decrypt >0, D1 fuera de servicio, exceso de consumo.

En Cloudflare Dashboard usar métricas de Workers, Durable Objects y D1, y configurar notificaciones externas. Registrar solo códigos, correlaciones y duraciones; nunca ADMIN_KEY, Authorization, cookies ni plaintext.

## Procedimiento de incidente: degradación / latencia

1. Registrar UTC, versión Worker y mensajes de error. Confirmar que las métricas se refieren a entorno y carga correctos.
2. Revisar D1 auth_sessions (consultas por solicitud), límites por IP/usuario, cola del DO y alarmas. El código actual realiza D1 por cada mensaje, lo cual debe medirse antes de añadir cachés.
3. Ejecutar diagnóstico en staging con generador separado. Comparar p50/p95/p99 de ACK, fanout y contenido. Evitar disparar un benchmark no autorizado contra producción.
4. Si hay timeouts con mensajes posiblemente persistidos, instruir al cliente a reintentar exactamente el mismo client_message_id y payload. Nunca generar un ID nuevo automáticamente.
5. Si el outbox se estanca, verificar alarmas y reintentos; no borrarlo para limpiar la cola.
6. Si se requiere revertir código, consultar la versión anterior, compatibilidad de esquema y usar wrangler rollback. Un rollback de Worker NO revierte SQLite, D1, secretos ni Durable Objects.

## Procedimiento: revocación o filtración de credenciales

- Revocar sesiones afectadas en D1; el UserHub debe cerrar sockets y enviar session.revoked. El cliente debe respetar la notificación y borrar credenciales.
- Rotar ADMIN_KEY y AUTH_KEY solo mediante un procedimiento controlado, sin cortar sesiones legítimas indiscriminadamente si la continuidad es requerida.
- MESSAGE_KEY_V1/V2 cifra el histórico: **no eliminar una versión antigua sin inventario completo de salas migradas**.
- DEDUP_KEY es estable, no intercambiable con MESSAGE_KEY. Rotarla requiere migración de fingerprints históricos.
- Bloquear temporalmente rutas administrativas en Access/WAF durante un incidente. Revisar registros de cambios y preservar evidencia sin plaintext.

## Recuperación y datos

- D1 y DO SQLite son almacenamientos separados; un único backup no basta. Definir políticas de respaldo y recuperación para ambos.
- Cloudflare admite backups/migraciones para D1 y Point-in-Time Recovery en Durable Objects SQLite, con limitaciones de plataforma/retención. Verificar disponibilidad y **probar restauraciones reales** en staging antes de confiar en ellas.
- Backups de claves deben residir en un vault, fuera de Git y fuera del mismo entorno físico que los archivos temporales.
- Las migraciones D1 actuales crean tablas de sesiones y proyecciones; no hay rollback automático de datos. No hacer cambios de esquema incompatibles hacia atrás sin plan de migración reversible.
- El pruning elimina contenido anterior, deja tombstones y parte del log. Definir duración de retención y derechos de supresión antes de recopilar datos reales.
- No se implementan exportación masiva de chats, purga de cuentas ni eliminación legal completa; tratarlos como requisitos pendientes.

## Checklist de cada release

- [ ] Revisión del diff y CI Rust fmt/check/clippy/tests/integración.
- [ ] Validar configuraciones staging y producción con predeploy.
- [ ] Desplegar staging y ejecutar smoke de usuarios sintéticos.
- [ ] Confirmar métricas de latencia, fallos y persistencia en staging; no basarse en Miniflare local.
- [ ] Verificar Cloudflare Access/WAF, rate limits, D1 migrations y secrets.required.
- [ ] Verificar backups/recuperación y compatibilidad de rollback.
- [ ] Registrar versión/revisión, operador, ventana de cambio, monitoreo y plan de reversión.
- [ ] Aprobar despliegue de producción de forma explícita.
- [ ] Smoke no invasivo, métricas y vigilancia tras el despliegue.

## Referencias

- [Deployment](CLOUDFLARE_DEPLOY.md)
- [API contract](API.md)
- [Security](../SECURITY.md)
- [Benchmarks](benchmarks/2026-11-11-250-clients.md)
- https://developers.cloudflare.com/workers/versions-and-deployments/rollbacks/
- https://developers.cloudflare.com/durable-objects/api/alarms/
- https://developers.cloudflare.com/d1/reference/migrations/
