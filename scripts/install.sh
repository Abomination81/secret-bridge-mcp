#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(dirname -- "$script_dir")
install_dir=${SECRET_BRIDGE_INSTALL_DIR:-"$HOME/.local/bin"}

# Build into the project target directory explicitly, so a CARGO_TARGET_DIR or build.target-dir
# setting cannot leave a stale binary at the path copied below.
cargo build --release --locked --manifest-path "$project_dir/Cargo.toml" --target-dir "$project_dir/target"
mkdir -p "$install_dir"
if [ -L "$install_dir/secret-bridge-mcp" ]; then
    echo "Refusing to replace a symlink at $install_dir/secret-bridge-mcp" >&2
    exit 1
fi
staged=$(mktemp "$install_dir/.secret-bridge-mcp.XXXXXX")
trap 'rm -f "$staged"' EXIT HUP INT TERM
cp "$project_dir/target/release/secret-bridge-mcp" "$staged"
chmod 755 "$staged"
mv -f "$staged" "$install_dir/secret-bridge-mcp"
trap - EXIT HUP INT TERM

echo "Installed SecretBridge at $install_dir/secret-bridge-mcp"
echo "After registering the MCP, enable automatic credential routing: sh \"$script_dir/enable-defaults.sh\" both"
