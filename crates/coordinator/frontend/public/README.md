a WASM package used to validate the transactions created by the market maker

### How to build
- requires wasm-bindgen and wasm-pack
`cargo install wasm-bindgen-cli`
`cargo install wasm-pack`

- run, this will add the wasm to the frontend UI and make it available to the browser code:
`wasm-pack build --target web --out-dir ../server/ui/dist`
