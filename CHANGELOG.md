# Changelog

All notable changes to this project are documented in this file. The
format is loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Per-crate releases use the `<crate>-vX.Y.Z` tag scheme; workspace-wide
releases use the bare `vX.Y.Z` shape. Section headers below mirror that:
`## [<crate>-vX.Y.Z]` for per-crate, `## [X.Y.Z]` for workspace.

The release-prepare workflow prepends new sections automatically via
`git-cliff`. Edit `cliff.toml` to tune the generated content.

<!-- new sections inserted above this line by release-prepare -->

## [quorum-rs-v0.7.0] - 2026-06-25

### 🚀 Features

- *(init)* Redesign interactive agent setup
- *(init)* --invite scaffolds both nsed.yaml + agent.yml, wires token from redeemed file

### 🐛 Bug Fixes

- *(tui)* Tolerate under-provisioned local policies on config load
