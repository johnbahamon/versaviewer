# ADR-0001: Núcleo en Rust, UI en Flutter, Kotlin/Swift solo para adaptadores

- Estado: Aceptado (2026-11-22)

## Contexto
Hace falta un motor de escritorio remoto para 5 sistemas operativos, con baja
latencia y acceso directo a las APIs nativas de captura e input. Se evaluó
Kotlin Multiplatform como alternativa "multiplataforma".

## Decisión
- Motor (`crates/*`) en **Rust + Tokio**.
- UI multiplataforma en **Flutter**, conectada al motor por FFI (`flutter_rust_bridge`).
- **Kotlin** solo para el adaptador Android (MediaProjection, AccessibilityService)
  y **Swift** para iOS/macOS donde haga falta (ReplayKit, permisos).

## Motivos
- Sin GC: latencia predecible en el bucle de captura → codificación → envío.
- FFI de coste cero hacia Win32/DXGI, CoreGraphics/ScreenCaptureKit, PipeWire.
- Ecosistema WebRTC en Rust (webrtc-rs, str0m, bindings de libwebrtc); KMP no
  tiene un WebRTC de escritorio maduro.
- Precedente: RustDesk usa exactamente Rust + Flutter.

## Consecuencias
- El equipo necesita Rust. La UI se puede desarrollar en paralelo sobre la API de `vv-engine`.
- La Fase 0 usa una UI mínima (winit) que se descartará; no contiene lógica.
