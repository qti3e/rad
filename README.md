# Rust Analyzer Daemon

Still a work in progress. But is pretty much functional and *works*.

## Why?

Multiplexing a language server is more than just multiplexing some bytes without caring what those
bytes are. Doing so can leave the language and the client in a broken state which might not have
some immediate error, but will eventually show itself by some Unrecoverable error.

This implementation tries to keep some state in mind and handle those scenarios where doing just
a simple byte transfer breaks either the client or the server.

