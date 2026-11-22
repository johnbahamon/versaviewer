# ADR-0006: Señalización mínima en Node + ws para la Fase 0

- Estado: Aceptado para la Fase 0 (2026-11-22)

## Decisión
Un relay WebSocket de ~250 líneas (`services/signaling`) en TypeScript con la
librería `ws`, en memoria y en una sola instancia. ICE no-trickle (un SDP
completo por lado).

## Motivos
El blueprint pide priorizar la viabilidad del motor y no empezar por el
backend. El contrato de mensajes (`vv-protocol::signal`) sí es definitivo y se
mantiene al migrar.

## Fase 2
Se migra a **NestJS** (blueprint §3) separando autenticación, dispositivos y
sesiones; presencia en Redis para escalar horizontalmente; trickle ICE.
