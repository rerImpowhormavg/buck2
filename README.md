# Buck2 [![CI](https://circleci.com/gh/facebook/buck2.svg?style=svg)](https://app.circleci.com/pipelines/github/facebook/buck2)

**WARNING:** This project is not yet polished. We are continuing to develop it in the open, but don't expect it to be suitable for most people until Mar/Apr/May 2023 (at which point we'll properly announce it). If you try and use it, you will probably have a bad time. If you are willing to work closely with us, please give it a go and [let us know](https://github.com/facebook/buck2/issues) what is blocking you.

This repo contains the code for the Buck2 build system - the successor to the original [Buck build system](https://buck.build). To understand why it might be interesting, see [this explainer](https://buck2.build/docs/why/). For the moment, we only test it on Linux.

## Getting started

To clone, build, and install `buck2`:
```sh
git clone https://github.com/facebook/buck2.git
cd buck2/
cargo install --path=cli
```

To build and install the latest `buck2` executable:
```sh
rustup install nightly
cargo +nightly install --git https://github.com/facebook/buck2.git cli
```

Build uses prebuilt `protoc` binary from
[protoc-bin-vendored](https://crates.io/crates/protoc-bin-vendored) crate.
If these binaries to do not work on your machine (for example, when building for NixOS),
path to `protoc` binary and protobuf include path can be specified via
`BUCK2_BUILD_PROTOC` and `BUCK2_BUILD_PROTOC_INCLUDE` environment variables.

To build a project with `buck2`, go to the [getting started guide](https://buck2.build/docs/getting_started/).

## Terminology conventions

Frequently used terms and their definitions can be found in the [glossary page](https://buck2.build/docs/concepts/glossary/).

## Coding conventions

Beyond the obvious (well-tested, easy to read) we prefer guidelines that are automatically enforced, e.g. through `rust fmt`, Clippy or the custom linter we have written. Some rules:

* Use the utilities from Gazebo where they are useful, in particular, `dupe`.
* Prefer `to_owned` to convert `&str` to `String`.
* Qualify `anyhow::Result` rather than `use anyhow::Result`.
* Most errors should be returned as `anyhow::Result`. Inspecting errors outside tests and the top-level error handler is strongly discouraged.
* Most errors should be constructed with `thiserror` deriving `enum` values, not raw `anyhow!`.
* We use the `derivative` library to derive the `PartialEq` and `Hash` traits when some fields should be ignored.
* Prefer `use crate::foo::bar` over `use super::bar` or `use crate::foo::*`, apart from test modules which often have `use super::*` at the top.
* Modules should either have submodules or types/functions/constants, but not both.
* Prefer `anyhow::Error` for checking internal invariants that are maintained between multiple files, while `panic!`/`unreachable!` are reasonable if the invariant is file-local.

### Error messages

* Names (of variables, targets, files, etc) should be quoted with backticks,
  e.g. ``Variable `x` not defined``.
* Lists should use square brackets, e.g. ``Available targets: [`aa`, `bb`]``.
* Error messages should start with an upper case letter.
  Error messages should not end with a period.

## License

Buck2 is both MIT and Apache License, Version 2.0 licensed, as found in the [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE) files.
