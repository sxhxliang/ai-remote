# Repository Guidelines

## Project Structure & Module Organization

- `frontend/src/`: React UI in `App.jsx`, transport in `webrtcClient.js`, and bootstrap in `main.jsx`.
- `signaling-server/src/main.rs`: axum WebSocket relay for SDP/ICE.
- `home-agent/src/main.rs`: WebRTC DataChannel bridge to local Ollama.
- `turn-server/src/main.rs`: reference UDP TURN relay; coturn is an alternative.

Each Rust service has its own `Cargo.toml`; there is no root Cargo workspace. No dedicated test or source asset directories exist. Vite generates `frontend/dist/`.

## Build, Test, and Development Commands
In `frontend/`:

- `npm ci`: install locked dependencies.
- `npm run dev`: start Vite.
- `npm run build`: generate `dist/`.
- `npm run preview`: serve the production build locally.

In each Rust service directory, run `cargo build`, `cargo test`, or `cargo run --release` to compile, test, or start that service. Start Ollama with `ollama serve`. Follow `README.md` for configuration and its Rust dependency API compatibility caveats.

## Coding Style & Naming Conventions
Use Rust 2021, four-space indentation, `snake_case` functions/modules, and `PascalCase` types. For Rust changes, run `cargo fmt -- --check` and `cargo clippy` per crate.

Frontend code uses ES modules, two-space indentation, single quotes, and semicolons. Use `PascalCase` components and `camelCase` helpers. No ESLint or Prettier configuration is present.

## Testing Guidelines
There are no automated tests, frontend test runner, or coverage thresholds. For Rust regression tests, use `#[test]` or `#[tokio::test]` within `#[cfg(test)]` modules, with behavior names such as `rejects_disallowed_path`.

For behavior changes, build affected components and verify connection setup, streamed chat, invalid tokens, and denied paths. Exercise TURN fallback when changing networking.

## Commit & Pull Request Guidelines
This copy lacks Git metadata, so historical commit conventions cannot be verified. Prefer scoped, imperative messages such as `fix(frontend): handle connection failures`.

PRs should describe behavior changes, link relevant issues, list validation results and known failures, and include screenshots for UI changes. Update `README.md` when setup or protocols change.

## Security & Configuration Tips
Use strong `SIGNALING_TOKEN` and TURN credentials; never commit secrets. Match `ROOM_ID` and tokens between peers. Align browser ICE settings in `App.jsx` with agent configuration. Set `PUBLIC_IP` for TURN. Preserve `ALLOWED_PATHS` restrictions and Ollama's loopback default.
