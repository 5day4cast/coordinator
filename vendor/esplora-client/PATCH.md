# Esplora HTTP compatibility patch

Source: published `esplora-client` 0.11.0, upstream revision `8f49c84e6cc7c981725c214f4042f527d263b6c3`.
The upstream MIT license is retained.

The Ark wallet and BDK adapter require the 0.11 API.
This local patch uses reqwest 0.12, removing their legacy Hyper 0.14 and h2 0.3 dependency path.
The coordinator lockfile selects patched h2 0.4.16 or later.
The asynchronous client uses the same reqwest methods and propagates transport errors.
The blocking client also propagates malformed hexadecimal responses instead of panicking.

The application source otherwise matches the published archive.
Remove this patch when the Ark wallet and its BDK adapter both support an upstream client with current HTTP dependencies.
