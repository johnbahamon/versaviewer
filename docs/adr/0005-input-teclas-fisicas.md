# ADR-0005: Input con teclas físicas y coordenadas normalizadas

- Estado: Aceptado (2026-11-22)

## Decisión
- El teclado se transmite como **W3C `KeyboardEvent.code`** (posición física) y
  el agente lo traduce a scancodes Set-1 (Windows) o `kVK_*` (macOS).
- El mouse usa coordenadas **normalizadas [0,1]** relativas a la pantalla compartida.
- En Windows se usa `SendInput` directamente con `KEYEVENTF_SCANCODE` (el
  manejo de teclas extendidas E0 de `enigo` no era fiable para flechas, Supr, etc.).
- En un viewer macOS, Cmd se envía como Ctrl por defecto (`--raw-cmd` lo desactiva).

## Motivos
Es el comportamiento esperado en soporte remoto: escribir como si se estuviera
frente al equipo remoto, con su distribución de teclado, y sin que el viewer
conozca la resolución ni el escalado DPI remotos.

## Consecuencias
- Teclados con distribuciones distintas en ambos lados producen el carácter de
  la distribución remota (igual que TeamViewer/RustDesk en "modo mapa").
- Un futuro "modo texto" (envío de caracteres Unicode) cubrirá IMEs y móviles.
