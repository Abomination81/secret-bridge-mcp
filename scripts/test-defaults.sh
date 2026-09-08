#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
test_root=$(mktemp -d "${TMPDIR:-/tmp}/secretbridge-defaults-tests.XXXXXX")
cleanup() {
    # This is the exact temporary tree created by this test, never a real profile.
    rm -f -- "$test_root/codex/AGENTS.md" "$test_root/codex/AGENTS.override.md" \
        "$test_root/claude/CLAUDE.md" "$test_root/expected" "$test_root/first" \
        "$test_root/external" "$test_root/output"
    rmdir -- "$test_root/codex" "$test_root/claude" "$test_root"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
mkdir "$test_root/codex" "$test_root/claude"

run_setup() {
    sh "$script_dir/enable-defaults.sh" "$@" \
        --codex-dir "$test_root/codex" --claude-dir "$test_root/claude" > "$test_root/output" 2>&1
}
fail() { echo "FAIL: $*" >&2; exit 1; }
must_fail() { if run_setup "$@"; then fail "setup unexpectedly succeeded"; fi; }

printf 'Existing Codex preferences without a final newline' > "$test_root/codex/AGENTS.md"
printf 'Existing Claude preferences\r\n' > "$test_root/claude/CLAUDE.md"
run_setup both
head -n 1 "$test_root/codex/AGENTS.md" | grep -q '^Existing Codex preferences without a final newline$' || fail preservation
head -n 1 "$test_root/claude/CLAUDE.md" > "$test_root/first"
printf 'Existing Claude preferences\r\n' > "$test_root/expected"
cmp "$test_root/expected" "$test_root/first" || fail 'CRLF preservation'
cp "$test_root/codex/AGENTS.md" "$test_root/first"
run_setup both
cmp "$test_root/first" "$test_root/codex/AGENTS.md" || fail idempotence

printf 'Before\r\n<!-- secret-bridge-mcp:begin -->\r\nOld managed block\r\n<!-- secret-bridge-mcp:end -->\r\nAfter without newline' > "$test_root/codex/AGENTS.override.md"
run_setup codex
cmp "$test_root/first" "$test_root/codex/AGENTS.md" || fail 'override must leave AGENTS.md untouched'
tail -c 21 "$test_root/codex/AGENTS.override.md" > "$test_root/first"
printf 'After without newline' > "$test_root/expected"
cmp "$test_root/first" "$test_root/expected" || fail 'suffix without newline preservation'
cp "$test_root/codex/AGENTS.override.md" "$test_root/first"
run_setup codex
cmp "$test_root/first" "$test_root/codex/AGENTS.override.md" || fail 'override idempotence'

printf 'Unchanged\n<!-- secret-bridge-mcp:begin -->\nBroken block\n' > "$test_root/claude/CLAUDE.md"
cp "$test_root/claude/CLAUDE.md" "$test_root/expected"
must_fail both
cmp "$test_root/expected" "$test_root/claude/CLAUDE.md" || fail 'malformed target changed'
cmp "$test_root/first" "$test_root/codex/AGENTS.override.md" || fail 'valid target changed before validation completed'

printf '<!-- secret-bridge-mcp:begin -->\n<!-- secret-bridge-mcp:end -->\n<!-- secret-bridge-mcp:begin -->\n<!-- secret-bridge-mcp:end -->\n' > "$test_root/claude/CLAUDE.md"
must_fail claude
printf '<!-- secret-bridge-mcp:end -->\n' > "$test_root/claude/CLAUDE.md"
must_fail claude
printf 'Do not overwrite this file\n' > "$test_root/external"
cp "$test_root/external" "$test_root/expected"
rm -- "$test_root/claude/CLAUDE.md"
ln -s "$test_root/external" "$test_root/claude/CLAUDE.md"
must_fail claude
cmp "$test_root/external" "$test_root/expected" || fail 'symlink target changed'

echo "PASS: preservation, CRLF, missing final newline, idempotence, override precedence, malformed markers, and symlink refusal."
