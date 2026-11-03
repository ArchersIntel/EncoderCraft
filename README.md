# EncoderCraft

Media encoder for the ArtCraft suite. Renders FilmCraft project sequences and ordinary media files from an Adobe Media Encoder style queue, locally or on headless Linux servers reached over SSH.

- `encoder-core` job model, queue and FFmpeg wrapper
- `encoder-daemon` (`encraft-d`) headless server, REST + WebSocket, bound to localhost
- `encoder-client` SSH provisioning and remote client, plus the `encraft` CLI
- `encoder-import` / `encoder-filmcraft` FilmCraft project reader and headless renderer
- `encoder-app` Tauri desktop app

Build the Linux daemon with `scripts/build-daemon-linux.ps1`; build the app with `cargo build --release -p encoder-app`.

## License

Licensed under either of the MIT License (`LICENSE-MIT`) or the Apache License, Version 2.0 (`LICENSE-APACHE`), at your option, the same terms as FilmCraft.

The ArtCraft name and logos are trademarks of the ArtCraft Team and are not covered by this license.
