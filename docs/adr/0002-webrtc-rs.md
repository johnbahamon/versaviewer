# ADR-0002: WebRTC con webrtc-rs 0.21

- Estado: Aceptado para la Fase 0; revisar en la Fase 1 (2026-11-22)

## Contexto
Opciones: `webrtc` (webrtc-rs, Rust puro, reescrito en 0.21 sobre un núcleo
sans-I/O `rtc`), `str0m` (sans-I/O), bindings de libwebrtc de Google (LiveKit).

## Decisión
`webrtc = 0.21` con el runtime Tokio y crypto `ring`.

## Motivos
- Rust puro: cross-compile Mac → Windows sin binarios precompilados de libwebrtc.
- API async completa: ICE/STUN/TURN, DTLS-SRTP, DataChannels, interceptores NACK/RTCP.
- Validado en la Fase 0 con un test de loopback (`crates/vv-transport/tests/loopback.rs`).

## Riesgos y salida
- libwebrtc tiene mejor control de congestión (GCC) y FEC. Si el BWE de
  webrtc-rs no basta en la Fase 1, `vv-transport` es la única capa afectada:
  su API (`Peer`, `PeerEvent`) se diseñó independiente de la implementación.
