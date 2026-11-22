# ADR-0004: Modelo de seguridad de la Fase 0 (OTP + consentimiento en el agente)

- Estado: Aceptado para la Fase 0; reemplazado en la Fase 2 (2026-11-22)

## Decisión
- El **agente** genera su ID y una contraseña de un solo uso (CSPRNG del SO) y
  es quien la verifica (comparación en tiempo constante, bloqueo exponencial).
- El servidor de señalización solo la reenvía; nunca la guarda ni la registra.
- Cada sesión requiere **consentimiento explícito** del usuario local, salvo el
  **modo desatendido**, que exige una contraseña fija ≥ 8 caracteres
  configurada por el propietario del equipo y mantiene el indicador visible.
- Una sola sesión a la vez; al terminar se sueltan las teclas pulsadas y rota el OTP.

## Limitaciones aceptadas
- `ws://` sin TLS: la señalización puede ser manipulada en la red (MITM del SDP
  y, por tanto, del DTLS). Uso exclusivo en LAN de pruebas.
- Sin autenticación de usuario ni de dispositivo: cualquiera con el ID y el OTP
  puede pedir acceso (de ahí el consentimiento obligatorio por defecto).

## Fase 2
Usuarios autenticados (OIDC), identidad criptográfica por dispositivo,
credenciales de sesión emitidas por el backend, TURN con credenciales REST
temporales, `wss://` y auditoría.
