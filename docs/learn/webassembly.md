# Rust in the browser with WebAssembly

WebAssembly (wasm) is a compact binary instruction format that every modern
browser can run at close to native speed. Rust can compile to it, so the same
engine code that runs in the terminal also runs inside a web page, with no
server.

How Fishbowl's browser build works:

1. `cargo build --target wasm32-unknown-unknown` compiles the `fishbowl-wasm`
   crate to a `.wasm` file.
2. `wasm-bindgen` generates a small JavaScript wrapper, so TypeScript can write
   `new Fishbowl()` and `db.run(sql)` without touching raw memory.
3. Vite bundles the wrapper and the `.wasm` file into the static site.

## Crossing the boundary

JavaScript and wasm share only numbers and raw memory. Passing a string means
copying bytes into wasm memory and back out, which `wasm-bindgen` handles.
Fishbowl keeps the boundary narrow: `run(sql)` returns one JSON string holding
every statement's result, the plan trees and the table list. TypeScript parses
it into types defined in `web/src/engine.ts`. A Rust test pins the JSON shape so
the two sides cannot drift apart silently.

Integers travel as text inside that JSON. JavaScript numbers lose precision
above 2^53, and SQL integers go up to 2^63.

## Size

The download is about 700 KB compressed. Most of it is the SQL parser. A
separate Cargo profile builds the browser version optimized for size
(`opt-level = "s"`) instead of speed.

Code: `crates/wasm/src/lib.rs`, `web/scripts/build-wasm.mjs`,
`web/src/engine.ts`.
