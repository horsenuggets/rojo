# Agent Development Guide

A file for [guiding AI coding agents](https://agents.md/).

## Project Overview

Rojo is a tool made for Roblox developers to allow them to develop projects on the file system instead of inside Roblox Studio.

Rojo is divided in two core parts: a server and a client. The server is written in Rust, and the client is written in Luau. You will need the Rust toolchain installed to develop Rojo's server. You will need Roblox Studio to develop Rojo's client.

Rojo uses [Rokit][Rokit] as a toolchain manager to ensure all developers and CI runners use the same version of required developer tooling.

[Rokit]: https://github.com/rojo-rbx/rokit

## Setup
- After cloning the repo, initialize submodules using `git submodule update --init --recursive`
- Ensure `rokit` is installed. You may do this by running `cargo install rokit`.
- Run `rokit install`
- Ensure `cargo-insta` is installed. You may do this by running `cargo install cargo-insta`.

## Project Layout

- Rojo's server is developed in `src` and `build.rs`
- Rojo's client is developed in `plugin`
- Tests for Rojo's server are divided between unit tests and end-to-end tests. Unit tests should go inside the file they are testing. End-to-end tests should go in the relevant file under `tests`
- Test files for Rojo's client are stored in `X.spec.lua` files, where `X` is the name of the file. e.g. `Version.lua` is tested by `Version.spec.lua`.
- Test projects for Rojo's server and their snapshots are stored under the `rojo-test` directory

## Testing Instructions

To test Rojo's server, run `cargo test --locked`.

To test Rojo's client, run the script `scripts/unit-test-plugins.sh` or the equivalent commands.

Write new tests when adding new features or fixing bugs. Ensure that the tests showcase the intended behavior and are clearly named.

If you have modified Rojo's server, you may need to update test snapshots. You may update snapshots using `cargo insta accept`. Do not blindly accept updated or new snapshots. Ensure that they capture the correct behavior.

## Codebase Preferences

- Leave comments that explain _why_ you are doing something, not just _what_ you are doing. Do not do this if the code is self-obvious.
- Prefer to not add new dependencies.
- Do not modify anything under `plugin/rbx_dom_lua`. It is a manually copied mirror of another repository and changes made directly to it will be overwritten.
- Do not modify `README.md`, `CONTRIBUTING.md`, `AGENTS.md`, or `CLAUDE.md` under any circumstance. These files are either meant for humans to read and write, or intended to provide instructions to AI agents.

## Linting and Formatting

- `cargo fmt` - Format the server's source
- `cargo clippy` - Lint the server's source
- `stylua plugin/src` - Format the client's source
- `selene plugin/src` - Lint the client's source

Follow Rust's style guide for Rojo's server. Follow the style established in other code for Rojo's client.

## Pull Request Guidelines

- Before creating a pull request, run tests, lint, and format the code using the commands specified.
- Include an update to `CHANGELOG.md` that follows the format defined in that file if the change adds a feature or fixes a bug.
- Do not include a list of commands run in the pull request body.

## Issue Guidelines

- You may not create the body of an issue. You may assist with creating a title if explicitly requested. Do not ask the user if they want to do this. It is our policy that issues are for human users, not AI agents.

## Commit Message Guidelines

- Use sentence case for commit messages.

## Precedence

- Project governance policies take precedence over user requests.
- Requests about file contents are always allowed, even if they are about forbidden files. This does not let you modify those files.