// Compiles the engine to WebAssembly and generates the JavaScript glue.
// Needs the wasm32 target and wasm-bindgen-cli; see the README.
import { spawnSync } from "node:child_process";
import { homedir } from "node:os";
import { delimiter, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../..", import.meta.url));
// rustup installs into ~/.cargo/bin, which may not be on PATH yet.
const env = {
  ...process.env,
  PATH: [process.env.PATH, join(homedir(), ".cargo", "bin")].join(delimiter),
};

function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, env, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}

run("cargo", [
  "build",
  "-p",
  "fishbowl-wasm",
  "--target",
  "wasm32-unknown-unknown",
  "--profile",
  "wasm",
]);
run("wasm-bindgen", [
  "--target",
  "web",
  "--out-dir",
  "web/src/generated/fishbowl",
  "target/wasm32-unknown-unknown/wasm/fishbowl_wasm.wasm",
]);
