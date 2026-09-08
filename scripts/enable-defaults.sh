#!/bin/sh
# Install persistent client guidance; this does not register the MCP server.
set -eu

client=both
codex_dir=${CODEX_HOME:-"$HOME/.codex"}
claude_dir=${CLAUDE_CONFIG_DIR:-"$HOME/.claude"}
stage_codex=
stage_claude=

usage() {
    echo "Usage: $0 [codex|claude|both] [--codex-dir PATH] [--claude-dir PATH]"
}

if [ "$#" -gt 0 ]; then
    case "$1" in
        codex|claude|both) client=$1; shift ;;
    esac
fi
while [ "$#" -gt 0 ]; do
    case "$1" in
        --codex-dir|--claude-dir)
            option=$1
            if [ "$#" -lt 2 ] || [ -z "$2" ]; then
                echo "Missing path for $option" >&2
                exit 1
            fi
            case "$option" in
                --codex-dir) codex_dir=$2 ;;
                --claude-dir) claude_dir=$2 ;;
            esac
            shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; exit 1 ;;
    esac
done

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
template="$script_dir/../docs/client-instructions.md"
begin_marker='<!-- secret-bridge-mcp:begin -->'
end_marker='<!-- secret-bridge-mcp:end -->'

cleanup() {
    if [ -n "$stage_codex" ]; then rm -f -- "$stage_codex"; fi
    if [ -n "$stage_claude" ]; then rm -f -- "$stage_claude"; fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

check_file() {
    if [ -L "$1" ]; then
        echo "Refusing to replace a symlink: $1" >&2
        return 1
    fi
    if [ -e "$1" ] && [ ! -f "$1" ]; then
        echo "Expected a regular instruction file: $1" >&2
        return 1
    fi
}

check_markers() {
    # Reject duplicates, nesting, partial blocks, or altered marker lines.
    awk -v begin="$begin_marker" -v end="$end_marker" '
        { line = $0; sub(/\r$/, "", line) }
        index(line, "secret-bridge-mcp:begin") || index(line, "secret-bridge-mcp:end") {
            if (line == begin && state == 0 && blocks == 0) { state = 1; next }
            if (line == end && state == 1) { state = 0; blocks++; next }
            bad = 1
        }
        END { if (bad || state) exit 1 }
    ' "$1" || {
        echo "Malformed SecretBridge markers; leaving the file unchanged: $1" >&2
        return 1
    }
}

check_directory() {
    if [ -L "$1" ] || { [ -e "$1" ] && [ ! -d "$1" ]; }; then
        echo "Expected a non-symlink client configuration directory: $1" >&2
        return 1
    fi
}

prepare() {
    # The caller has already validated both clients before any file is replaced.
    target=$1
    staged=$2
    if [ -f "$target" ]; then
        cp -p -- "$target" "$staged"
        source_file=$target
    else
        chmod 600 "$staged"
        source_file=/dev/null
    fi
    source_lines=$(awk 'END { print NR }' "$source_file")
    last_byte=$(tail -c 1 -- "$source_file" | od -An -tu1 | tr -d ' \n')
    source_crlf=$(awk '/\r$/ { print "yes"; exit }' "$source_file")
    awk -v begin="$begin_marker" -v end="$end_marker" -v template="$template" \
        -v total_lines="$source_lines" -v last_byte="$last_byte" -v crlf="$source_crlf" '
        BEGIN { newline = crlf == "yes" ? "\r\n" : "\n" }
        function insert_block( entry ) {
            while ((getline entry < template) > 0) {
                sub(/\r$/, "", entry)
                printf "%s%s", entry, newline
            }
            close(template)
        }
        {
            line = $0; sub(/\r$/, "", line)
            if (line == begin) { insert_block(); inside = 1; replaced = 1; next }
            if (line == end) { inside = 0; next }
            if (!inside) {
                printf "%s", $0
                if (NR < total_lines || last_byte == "10") printf "\n"
            }
        }
        END {
            if (!replaced) {
                if (NR > 0) {
                    if (last_byte != "10") printf "%s", newline
                    printf "%s", newline
                }
                insert_block()
            }
        }
    ' "$source_file" > "$staged"
}

check_file "$template"
check_markers "$template"
if ! [ -s "$template" ]; then
    echo "Missing client instruction template: $template" >&2
    exit 1
fi

case "$client" in
    codex|both)
        check_directory "$codex_dir"
        check_file "$codex_dir/AGENTS.override.md"
        if [ -s "$codex_dir/AGENTS.override.md" ]; then
            codex_target="$codex_dir/AGENTS.override.md"
        else
            codex_target="$codex_dir/AGENTS.md"
        fi
        check_file "$codex_target"
        if [ -f "$codex_target" ]; then check_markers "$codex_target"; fi ;;
esac
case "$client" in
    claude|both)
        check_directory "$claude_dir"
        claude_target="$claude_dir/CLAUDE.md"
        check_file "$claude_target"
        if [ -f "$claude_target" ]; then check_markers "$claude_target"; fi ;;
esac

case "$client" in
    codex|both)
        mkdir -p -- "$codex_dir"
        stage_codex=$(mktemp "$codex_dir/.secret-bridge-instructions.XXXXXX")
        prepare "$codex_target" "$stage_codex" ;;
esac
case "$client" in
    claude|both)
        mkdir -p -- "$claude_dir"
        stage_claude=$(mktemp "$claude_dir/.secret-bridge-instructions.XXXXXX")
        prepare "$claude_target" "$stage_claude" ;;
esac

case "$client" in
    codex|both)
        check_file "$codex_target"
        mv -f -- "$stage_codex" "$codex_target"
        stage_codex=
        echo "Installed SecretBridge guidance in $codex_target" ;;
esac
case "$client" in
    claude|both)
        check_file "$claude_target"
        mv -f -- "$stage_claude" "$claude_target"
        stage_claude=
        echo "Installed SecretBridge guidance in $claude_target" ;;
esac

echo "Prerequisite: register and enable the SecretBridge MCP server in each client."
echo "Start new Codex / Claude Code sessions to load the guidance. Tool approval settings are unchanged."
