# ADR-0007: Tubería de vídeo en GPU (Windows)

- Estado: Aceptado, pendiente de validar en hardware real (2026-11-22)

## Contexto
Primera prueba real: PC con pantalla 4K. Con todo en CPU, cada frame
implicaba leer 33 MB, reducirlos y codificarlos por software: ~35 ms por
frame repartidos entre captura, conversión y OpenH264, compitiendo por los
mismos núcleos (≈15–19 fps con movimiento).

## Decisión
Nuevo crate `vv-gpu` con una tubería que no saca los píxeles de la GPU:

1. **Captura** DXGI Desktop Duplication (textura BGRA en GPU).
2. **Escalado + conversión** con el *video processor* de D3D11
   (BGRA 4K → NV12 1080p, BT.601 rango limitado).
3. **Codificación** con el encoder H.264 por hardware vía Media Foundation
   (Quick Sync / NVENC / AMF), Baseline, sin B-frames, CBR, baja latencia.
   Si no hay encoder hardware usable: lectura NV12 (~3 MB) + OpenH264.

Cada etapa tiene respaldo y el motor vuelve a la tubería CPU si la GPU falla
al abrir o 3 veces seguidas durante la sesión. `--no-gpu` y
`--encoder auto|hardware|software` permiten forzar cada camino.

## Consecuencias
- Coste por frame esperado: unos pocos ms y CPU casi libre.
- Código COM/D3D11/MF no verificable en CI sin Windows: cada cambio se valida
  con las métricas `stream stats (gpu)` en un equipo real.
- Las ediciones "N" de Windows sin Media Feature Pack no tienen `mfplat.dll`:
  el agente no arranca en ellas (pendiente: carga diferida).
- El viewer sigue decodificando con OpenH264 por software; la decodificación
  por hardware (VideoToolbox en macOS) es el siguiente paso del lado viewer.
