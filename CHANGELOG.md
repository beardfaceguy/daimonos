# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Public-facing repository maintenance files (`LICENSE`, `CONTRIBUTING.md`,
  `SECURITY.md`, `CODEOWNERS`)

### Changed

- Expanded `.gitignore` to cover common local artifacts

### Fixed

- Agent turns now recover from a mid-stream transport break (HTTP/2 `CANCEL`,
  connection reset, or abrupt idle disconnect) instead of dead-stopping: the
  provider adapters classify such faults as retryable so the turn resumes on the
  bounded retry path (vikunja #1418). Note: a recovered turn currently re-streams
  the partial text it had already shown, so a repeated snippet mid-turn is
  expected until the separate re-stream de-duplication follow-up lands.
