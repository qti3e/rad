# Rust Analyzer Daemon

Still a work in progress. But is pretty much functional and *works*.

## Why?

Multiplexing a language server is more than just multiplexing some bytes without caring what those
bytes are. Doing so can leave the language and the client in a broken state which might not have
some immediate error, but will eventually show itself by some Unrecoverable error.

This implementation tries to keep some state in mind and handle those scenarios where doing just
a simple byte transfer breaks either the client or the server.

## Installation

Not on cargo.toml just yet, so you have to clone.

```
git clone https://github.com/qti3e/rad.git
cd rad
cargo install --path .
```

## Running it

Keep the following command running in one of the terminals:

```
rad daemon
```

Configure your editor use `rad` as the rust-analyzer binary, for example in neovim:

```
  lspconfig.rust_analyzer.setup {
    cmd = { "rad" },
    settings = {
      ['rust-analyzer'] = {},
    },
  }
```

## Supported Systems

I personally don't care about Windows and use Unix Domain Sockets right now, but if someone
really wants to have windows support, make a pull request.

## Future work

1. `workDoneProgress` is explicitly ignored right now.
2. There is no garbage collection right now. The server keeps running.
3. Multiple readers on the same document is not supported yet.
4. Better logs and possibly a nice terminal UI.
