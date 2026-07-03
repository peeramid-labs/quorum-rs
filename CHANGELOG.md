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


## [0.7.1] - 2026-07-03

### 🐛 Bug Fixes

- *(validate)* Accept unified quorum.yml in `quorum validate`

### 📚 Documentation

- *(readme)* Revamp — quickstart, badges, 0.7 versions, current doc links
- *(readme)* Punch up tone — hook line, mermaid deliberation diagram, drop diataxis
- *(readme)* Simplify the verdict node in the deliberation diagram
- *(readme)* Drop hard-coded crate versions (table column + install pins + status)

## [quorum-rs-v0.7.0] - 2026-06-28

### 🚀 Features

- *(smoke)* Show the real backend (provider/model/base_url/engine) being tested
- *(smoke)* NSED stage runs N deliberations × R rounds (propose+evaluate) with full per-round details
- *(telemetry)* DeliberationContextAssembled event — prior-context + scratchpad signals per propose/evaluate
- *(smoke)* Surface every failure with full breakdown, 400 reason, and progress bars

### 🐛 Bug Fixes

- *(release)* Bound changelog at latest stable tag, not latest rc

### 📚 Documentation

- *(telemetry)* Document deliberation_context_assembled event
- *(init)* Document provider engine field (vllm) in the fleet boilerplate
