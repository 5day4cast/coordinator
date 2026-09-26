// Stretches a log-in password (scrypt, ~1-2 s) off the page's thread.
//
// The page sends { module, glue, username, password }: the WASM module it
// has already compiled, and the URL of its JS glue. This worker answers with
// { stretched } (32 bytes, transferred, so no copy stays here) or { error },
// then closes. The Nostr key and the vault key never come here: the page
// turns the stretched bytes into credentials inside its own WASM instance.
self.onmessage = async (event) => {
  const { module, glue, username, password } = event.data;
  try {
    const wasm = await import(glue);
    await wasm.default({ module_or_path: module });
    const stretched = wasm.stretchLoginPassword(username, password);
    self.postMessage({ stretched }, [stretched.buffer]);
  } catch (error) {
    self.postMessage({ error: String(error?.message || error) });
  } finally {
    self.close();
  }
};
