# Guía Fase 0 — Ver y controlar un PC Windows desde el Mac

Escenario: el **Mac** ejecuta el servidor de señalización y el **viewer**; el
**PC Windows** (misma red local) ejecuta el **agente**. El vídeo y el input viajan
P2P cifrados (DTLS-SRTP); el servidor solo intercambia el SDP.

```
 Mac (192.168.1.9)                         PC Windows
 ┌──────────────────────┐   WebSocket   ┌──────────────────┐
 │ signaling :8080      │◄──────────────│ vv-agent.exe     │
 │ vv-viewer  ◄─────────┼── WebRTC P2P ─┼─► captura+input  │
 └──────────────────────┘  (UDP, DTLS)  └──────────────────┘
```

## 0. Requisitos (una vez)

En el Mac (ya instalado en este equipo):

```sh
brew install rustup llvm nasm   # nasm: ensamblador SIMD de OpenH264 para x86 (¡imprescindible!)
rustup default stable
rustup target add x86_64-pc-windows-msvc
cargo install --locked cargo-xwin
```

Rust desde Homebrew/rustup queda en `/opt/homebrew/opt/rustup/bin` y
`~/.cargo/bin`; añádelos al `PATH` en `~/.zshrc` si `cargo` no aparece:

```sh
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"
```

## 1. Compilar

```sh
# Agente para Windows (cross-compile desde el Mac)
cargo xwin build --release --target x86_64-pc-windows-msvc -p vv-agent
# -> target/x86_64-pc-windows-msvc/release/vv-agent.exe

# Viewer para el Mac
cargo build --release -p vv-viewer
```

## 2. Arrancar el servidor de señalización (Mac)

```sh
cd services/signaling && npm install && npm run dev
# [signaling] listening on ws://0.0.0.0:8080
```

Averigua la IP del Mac en la red local:

```sh
ipconfig getifaddr en0      # p. ej. 192.168.1.9
```

> macOS puede preguntar si permites conexiones entrantes a `node`: acepta.

## 3. Ejecutar el agente (PC Windows)

Copia `vv-agent.exe` al PC (USB, carpeta compartida, AirDrop a OneDrive…) y en
PowerShell:

```powershell
.\vv-agent.exe --server ws://192.168.1.9:8080
```

Verás el ID y una contraseña de un solo uso:

```
  ┌───────────────────────────────────────┐
  │  ID:          482 913 775             │
  │  Contraseña:  k7mq2xpa                │
  └───────────────────────────────────────┘
```

Windows Defender Firewall preguntará por el acceso a la red: permite **redes
privadas** (WebRTC necesita UDP entrante).

**Modo desatendido** (para no tener que ir al PC a aceptar cada sesión — sigue
exigiendo contraseña y sigue mostrando el indicador de sesión):

```powershell
$env:VV_UNATTENDED_PASSWORD = "una-contraseña-larga"
.\vv-agent.exe --server ws://192.168.1.9:8080 --id 111222333
```

## 4. Conectar desde el Mac

```sh
./target/release/vv-viewer --server ws://192.168.1.9:8080 --id "482 913 775"
# Pide la contraseña; en modo asistido el PC muestra un diálogo de consentimiento.
```

Controles:

- El mouse y el teclado van al PC remoto mientras la ventana tiene el foco.
- **Cmd** se envía como **Ctrl** (Cmd+C / Cmd+V funcionan en Windows). Usa
  `--raw-cmd` para enviarlo como tecla Windows.
- Cerrar la ventana termina la sesión. En el PC, cerrar la consola o pulsar
  Ctrl+C también la termina.

## 5. Probar el fallback por TURN (criterio MVP #8)

```sh
cd infra
VV_PUBLIC_IP=192.168.1.9 VV_TURN_PASS=una-clave docker-compose up -d coturn
```

Como en una prueba LAN los peers están en 192.168.x, comenta en
`infra/coturn/turnserver.conf` cualquier `denied-peer-ip` que cubra tu red (por
defecto no se bloquea 192.168.0.0/16).

Fuerza el relay en ambos lados con `--relay-only`:

```powershell
.\vv-agent.exe --server ws://192.168.1.9:8080 --turn turn:192.168.1.9:3478 --turn-user versaviewer --turn-pass una-clave --relay-only
```

```sh
./target/release/vv-viewer --server ws://192.168.1.9:8080 --id ... \
  --turn turn:192.168.1.9:3478 --turn-user versaviewer --turn-pass una-clave --relay-only
```

Los logs muestran `local ICE candidate types: relay`.

## 6. Prueba de humo automática (sin ventana)

```sh
cargo run --release -p vv-engine --example headless_viewer -- \
  ws://192.168.1.9:8080 111222333 una-contraseña-larga 10
# RESULT frames=... size=1920x1080 avg_fps=...
```

## Vídeo: tubería GPU (Windows)

El agente usa por defecto la GPU para escalar y codificar (ver ADR-0007). En la
consola aparece qué se está usando:

```
video pipeline ready backend="gpu" adapter="..." encoder="Intel® Quick Sync Video H.264 Encoder MFT" ...
stream stats (gpu) fps=... capture_ms=... readback_ms=... encode_ms=...
```

Opciones para diagnosticar:

| Opción | Efecto |
|---|---|
| `--encoder hardware` | Exige encoder por hardware (falla si no hay) |
| `--encoder software` | Escalado en GPU + OpenH264 |
| `--no-gpu` | Todo en CPU, como en la Fase 0 |

## Solución de problemas

| Síntoma | Causa probable |
|---|---|
| `device offline` | El agente no está conectado al mismo servidor / ID mal escrito. |
| Se queda en "Negociando conexión WebRTC" | Firewall de Windows bloqueando UDP; permite el exe en redes privadas. |
| Pantalla negra en apps elevadas / UAC | Limitación conocida de la Fase 0 (ver abajo). |
| El mouse no actúa sobre el Administrador de tareas | UIPI: un agente no elevado no puede inyectar en ventanas elevadas. |
| Logs más detallados | `RUST_LOG=debug` (o `$env:RUST_LOG="debug"` en PowerShell). |

## Limitaciones conocidas de la Fase 0

- Solo el monitor principal. En Windows se captura con DXGI Desktop
  Duplication (GDI como respaldo); las pantallas mayores de 1920×1200 se
  reducen antes de codificar (`--max-width/--max-height`). H.264 por software.
- La captura es **por cambios**: con la pantalla quieta no se envían frames,
  así que los fps del título bajan a 0. Es lo esperado.
- El cursor remoto no se dibuja en el vídeo; se ve el cursor del Mac.
- Los logs `stream stats` (agente) y `receive stats` (viewer) muestran fps y
  milisegundos por etapa (captura, conversión, codificación, decodificación).
- No ve el escritorio seguro (UAC, pantalla de bloqueo) ni puede enviar
  Ctrl+Alt+Supr: requiere el agente como **servicio de Windows** (Fase 2).
- ICE no-trickle: la conexión tarda 1–4 s en establecerse.
- Sin audio, portapapeles ni transferencia de archivos (fases 6–7).
- La señalización usa `ws://` sin TLS: **solo para LAN de pruebas**. El SDP
  incluye las huellas DTLS, así que un atacante en la red podría hacer MITM de
  la señalización. La Fase 2 añade `wss://` y autenticación.
