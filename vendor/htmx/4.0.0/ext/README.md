# htmx 4.0.0 extensions

`hx-sse.min.js` is `dist/ext/hx-sse.min.js` from the npm package `htmx.org@4.0.0`, unmodified
(0BSD). It registers as `sse` and streams server-sent events over fetch. Synth's pages use it.

- sha384: `VZD0TLKqhJ26ayBUgQg3ud6DsOLMJvtcz0ANpNc9WSbgIuQTnlXI2IfsF5jhBjT6`

Check it with:

```sh
openssl dgst -sha384 -binary hx-sse.min.js | base64
```
