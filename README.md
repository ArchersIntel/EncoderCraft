# EncoderCraft

Media encoder for the ArtCraft suite. Renders FilmCraft project sequences and ordinary media files from an Adobe Media Encoder style queue, locally or on headless Linux servers reached over SSH.

- `encoder-core` job model, queue and FFmpeg wrapper
- `encoder-daemon` (`encraft-d`) headless server, REST + WebSocket, bound to localhost
- `encoder-client` SSH provisioning and remote client, plus the `encraft` CLI
- `encoder-import` / `encoder-filmcraft` FilmCraft project reader and headless renderer
- `encoder-app` Tauri desktop app

Build the Linux daemon with `scripts/build-daemon-linux.ps1`; build the app with `cargo build --release -p encoder-app`.
