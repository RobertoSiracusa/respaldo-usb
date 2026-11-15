# Respaldo USB

Respaldo incremental de carpetas de un servidor Windows a un disco extraíble. Rust + egui.

## Cómo funciona

- La app se instala en cada servidor. Cada servidor guarda su propia lista de carpetas.
- El disco se reconoce por el archivo `RespaldoUSB\disco.json`, no por la letra de unidad.
- Destino: `X:\RespaldoUSB\<servidor>\C\Ruta\Original\...` — archivos normales, visibles con el Explorador.
- Cambios detectados por hash BLAKE3 contra `manifiesto.json` del disco. Solo se copia lo distinto.
- Nunca borra nada del disco, aunque el archivo desaparezca del servidor.
- Archivos en uso o sin permiso se omiten y quedan en el reporte.
- Copia a temporal + renombrado: un corte no deja archivos a medias.
- "Verificar disco" relee el respaldo; lo dañado se vuelve a copiar en la siguiente corrida.
- Cerrar la ventana la oculta en la bandeja. Salir: menú del icono de bandeja.

Configuración y reportes: `%APPDATA%\RespaldoUSB\`.

## Compilar

En Windows:

```
cargo build --release
```

Desde Linux (requiere `mingw-w64`):

```
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

Resultado: `target/x86_64-pc-windows-gnu/release/respaldo-usb.exe`, un solo archivo sin instalador.

## Pruebas

```
cargo test
```

Para probar la interfaz sin disco real, tratar carpetas como discos:

```
RESPALDO_DISCOS_PRUEBA=/ruta/carpeta cargo run
```

## Límites conocidos

- El respaldo automático necesita la app abierta, es decir, una sesión iniciada. No es un servicio de Windows.
- Un disco ya conectado al abrir la app no dispara respaldo automático; solo los que se conectan después.
- No usa VSS: bases de datos y archivos abiertos en exclusiva se omiten.
- Sin cifrado propio. Para cifrar el disco, usar BitLocker To Go.

## Descargar el ejecutable

- **Versión publicada:** pestaña *Releases* del repositorio en GitHub.
- **Última compilación de `main`:** pestaña *Actions* → última corrida de "Compilar" → artefacto `respaldo-usb-windows-x64`.

Publicar una versión nueva:

```
git tag v0.1.0
git push origin v0.1.0
```
