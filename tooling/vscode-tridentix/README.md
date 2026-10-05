# Tridentix Language Support — VS Code Extension (local install)

Syntax highlighting for `.trix` files: keywords, types, strings, comments,
numbers, function names, actor/supervisor names, and operators.

## Install locally (no marketplace publishing done — that's a real,
## separate step requiring a publisher account, see note below)

1. Copy this whole `vscode-tridentix/` folder to your VS Code extensions
   directory:
   - Linux/Mac: `~/.vscode/extensions/tridentix-language-0.1.0/`
   - Windows: `%USERPROFILE%\.vscode\extensions\tridentix-language-0.1.0\`
2. Restart VS Code.
3. Open any `.trix` file — it should now have syntax highlighting.

## What's real vs. what's still needed for a "real" extension
- ✅ **Real**: TextMate grammar (`syntaxes/tridentix.tmLanguage.json`) covers
  the actual current Tridentix keyword set (checked against `lexer.rs`),
  and both JSON files are validated as well-formed.
- ❌ **Not done**: publishing to the VS Code Marketplace (needs a
  Microsoft publisher account + `vsce publish`), an icon, a language
  server (LSP) for real diagnostics/autocomplete/go-to-definition, and
  a formatter. Syntax highlighting alone is "the easy 10%" of real IDE
  support — the LSP (which needs to embed or shell out to the actual
  `tridentix` compiler for live error-checking) is the harder, bigger
  remaining piece.
