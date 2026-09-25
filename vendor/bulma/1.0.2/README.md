# Bulma 1.0.2

`bulma.min.css` is `css/bulma.min.css` from the npm package `bulma@1.0.2`, unmodified
(<https://registry.npmjs.org/bulma/-/bulma-1.0.2.tgz>, MIT, see `LICENSE`). It is
the same file the pages loaded from cdn.jsdelivr.net before; serving it from this
site saves a connection to a third party on first visits and lets the CSP allow
styles from this site only.

- sha384: `tl5h4XuWmVzPeVWU0x8bx0j/5iMwCBduLEgZ+2lH4Wjda+4+q3mpCww74dgAB3OX`
  (the integrity value the pages pinned for the CDN copy)
- tarball integrity: `sha512-D7GnDuF6seb6HkcnRMM9E739QpEY9chDzzeFrHMyEns/EXyDJuQ0XA0KxbBl/B2NTsKSoDomW61jFGFaAxhK5A==`

Check it with:

```sh
openssl dgst -sha384 -binary bulma.min.css | base64
```
