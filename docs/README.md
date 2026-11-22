# Documentación de VersaViewer

- [Guía Fase 0: ver un PC Windows desde el Mac](guia-fase0.md)
- [Arquitectura, protocolo, seguridad y hoja de ruta](arquitectura.md)
- Blueprint original: [../readme.md](../readme.md)

## ADR

| # | Decisión |
|---|---|
| [0001](adr/0001-rust-core-flutter-ui.md) | Núcleo en Rust, UI Flutter, Kotlin/Swift solo para adaptadores |
| [0002](adr/0002-webrtc-rs.md) | WebRTC con webrtc-rs 0.21 |
| [0003](adr/0003-h264-openh264.md) | H.264 con OpenH264 en la Fase 0 |
| [0004](adr/0004-seguridad-fase0.md) | Seguridad Fase 0: OTP y consentimiento en el agente |
| [0005](adr/0005-input-teclas-fisicas.md) | Input con teclas físicas y coordenadas normalizadas |
| [0006](adr/0006-senalizacion-fase0.md) | Señalización mínima Node + ws |

## Desarrollo

```sh
cargo test --workspace                     # Rust (incluye loopback WebRTC)
cargo clippy --workspace --all-targets
(cd services/signaling && npm test)        # señalización
cargo xwin build --release --target x86_64-pc-windows-msvc -p vv-agent
```
