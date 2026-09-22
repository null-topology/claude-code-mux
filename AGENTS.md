# Fork practices

- Stay close to upstream and avoid style-only divergence.
- Preserve the `claude-code-mux` crate, binary, and release identity.
- Keep Claude aliases on Anthropic by default and explicit GPT models on Codex.
- Keep the Claude route a passive passthrough: never inspect, replace, or add credentials on it.
- Use Codex CLI authentication. Keep automated tests on toy credentials and mock upstreams.
- Preserve unrelated work and run locked format, clippy, and test checks before release.
- Add a `CHANGELOG.md` entry under `## Unreleased` with every user-visible change, in the same pull request.
- Tag `vX.Y.Z` for prebuilt GitHub binaries after renaming `## Unreleased` to `## vX.Y.Z (YYYY-MM-DD)`; the release workflow publishes that section as the release notes and fails without it. crates.io publishing is not part of the release.
