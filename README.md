# Respaldo USB

Respaldo incremental de carpetas de un servidor Windows a un disco extraíble. Rust + egui.

## Cómo funciona

- La app se instala en cada servidor. La configuración es de la máquina: todos los administradores ven la misma.
- Una sola copia de la app por servidor, aunque haya varias sesiones de usuario abiertas.
- El disco se reconoce por el archivo `RespaldoUSB\disco.json`, no por la letra de unidad.
- Destino: `X:\RespaldoUSB\<servidor>\C\Ruta\Original\...` — archivos normales, visibles con el Explorador.
- Dos servidores con el mismo nombre no se mezclan: el segundo usa `<servidor>-<id>`.
- Cambios detectados por hash BLAKE3 contra `manifiesto.json` del disco. Solo se copia lo distinto.
- Nunca borra nada del disco, aunque el archivo desaparezca del servidor. Si un archivo pasa a ser carpeta (o al revés), la copia anterior se renombra a `nombre.~anterior-<fecha>`.
- Copia a temporal + renombrado: un corte no deja archivos a medias.
- Cada archivo copiado se relee del disco sin caché de Windows para confirmar la escritura.
- "Verificar disco" relee todo el respaldo sin caché; lo dañado se vuelve a copiar en la siguiente corrida.
- Si el manifiesto se daña, se reconstruye comparando cada archivo con su copia, sin recopiar lo que ya está bien.
- Se conservan fechas y atributos (solo lectura, oculto, sistema). Opcional: permisos NTFS, si el disco es NTFS.
- Archivos en uso se omiten y quedan en el reporte. Enlaces simbólicos y uniones se omiten y se cuentan aparte.
- Ejecutada como administrador, lee archivos aunque sus permisos no lo permitan (privilegio de respaldo).
- Cerrar la ventana la oculta en la bandeja. Salir: menú del icono de bandeja.

Configuración, estado, reportes y `errores.log`: `%ProgramData%\RespaldoUSB\`.

Restaurar: copiar los archivos de vuelta con el Explorador. Para recuperar también permisos NTFS:

```
robocopy "X:\RespaldoUSB\SERVIDOR\C\Datos" "C:\Datos" /E /COPYALL
```

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
- No copia flujos de datos alternativos (ADS) de NTFS.
- Un archivo modificado necesita espacio libre para la copia nueva antes de reemplazar la anterior.
- Sin cifrado propio. Para cifrar el disco, usar BitLocker To Go.
- El ejecutable no está firmado: Windows SmartScreen puede avisar al abrirlo. Cada versión publica su suma SHA-256.

## Descargar el ejecutable

- **Versión publicada:** pestaña *Releases* del repositorio en GitHub.
- **Última compilación de `main`:** pestaña *Actions* → última corrida de "Compilar" → artefacto `respaldo-usb-windows-x64`.

Publicar una versión nueva:

```
git tag v0.1.0
git push origin v0.1.0
```
