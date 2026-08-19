# Contributing to Vyse

Thanks for your interest in Vyse. This repository is the open-source codebase and the basis for the hosted edge at `vyse.chipling.xyz`.

## How to contribute

1. Fork the repository on GitHub.
2. Create a branch for your change.
3. Make your changes and add tests where they cover real behavior.
4. Run the test suite (see below).
5. Open a pull request with a clear description of what changed and why.

Please keep PRs focused. Larger changes are easier to review when split into smaller, logical pieces.

## Development setup

Vyse is a Rust workspace. Use **stable Rust** (see `rust-toolchain.toml` if present, otherwise the latest stable toolchain).

```bash
git clone https://github.com/meet447/vyse
cd vyse
cargo test --workspace
```

To run the edge locally, see [docs/self-host.md](docs/self-host.md).

## Production operations

The `/deploy/` directory holds private VPS and hosting notes for the production edge. It is **gitignored** and not part of the public tree. If you want to run Vyse in production, follow [docs/self-host.md](docs/self-host.md) and operate your own edge.

## License

By contributing, you agree that your contributions may be released under Vyse’s dual license: [MIT](LICENSE-MIT) **OR** [Apache-2.0](LICENSE-APACHE), at the recipient’s choice.

## Code of conduct

Be respectful and constructive. Disagreement is fine; personal attacks are not. Maintainers may close or lock threads that become unproductive.
