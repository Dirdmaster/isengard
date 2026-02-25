# Contributing to Isengard

Thanks for your interest in contributing.

## Getting started

1. Fork the repository and clone it locally
2. Make sure you have Go 1.25+ and Node.js installed
3. Run `npm install` to set up git hooks via lefthook
4. Run `go build ./...` to verify the build

## Making changes

- Keep commits small and focused on a single change
- Use [Conventional Commits](https://www.conventionalcommits.org/) format: `feat:`, `fix:`, `chore:`, `refactor:`, `docs:`, `test:`
- Lefthook runs `go fmt`, `go vet`, `golangci-lint`, and `go build` on pre-commit automatically
- Tests run on pre-push with race detection
- Add or update tests if your change affects behavior

## Code style

- Follow standard Go conventions (`go fmt`, `go vet`)
- Use `slog` for structured logging
- Write godoc comments on all exported symbols
- Keep functions short and focused

## Reporting issues

Open an issue with a clear description of the problem, steps to reproduce, and your environment (OS, Docker version, Isengard version).
