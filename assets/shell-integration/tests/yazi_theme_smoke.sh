#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/kaku-yazi-theme.XXXXXX")"
cleanup() {
	rm -rf "$tmp_dir"
}
trap cleanup EXIT

mkdir -p "$tmp_dir/vendor/zsh-z" \
	"$tmp_dir/vendor/zsh-autosuggestions" \
	"$tmp_dir/vendor/zsh-syntax-highlighting" \
	"$tmp_dir/vendor/zsh-completions"

run_setup() {
	local home_dir="$1"
	HOME="$home_dir" \
	ZDOTDIR="$home_dir" \
	KAKU_INIT_INTERNAL=1 \
	KAKU_SKIP_TOOL_BOOTSTRAP=1 \
	KAKU_SKIP_TERMINFO_BOOTSTRAP=1 \
	KAKU_VENDOR_DIR="$tmp_dir/vendor" \
	bash "$REPO_ROOT/assets/shell-integration/setup_zsh.sh" --update-only >/dev/null
}

expected_flavor="kaku-dark"

expected_auto_flavor() {
	if command -v defaults >/dev/null 2>&1; then
		local appearance
		appearance="$(defaults read -g AppleInterfaceStyle 2>/dev/null || true)"
		if [[ "$appearance" != "Dark" ]]; then
			printf '%s\n' "kaku-light"
			return
		fi
	fi

	printf '%s\n' "kaku-dark"
}

home_new="$tmp_dir/home-new"
mkdir -p "$home_new"
run_setup "$home_new"

theme_new="$home_new/.config/yazi/theme.toml"
dark_new="$home_new/.config/yazi/flavors/kaku-dark.yazi/flavor.toml"
light_new="$home_new/.config/yazi/flavors/kaku-light.yazi/flavor.toml"
wrapper_new="$home_new/.config/kaku/zsh/bin/yazi"

[[ -f "$theme_new" ]]
[[ -f "$dark_new" ]]
[[ -f "$light_new" ]]
[[ -x "$wrapper_new" ]]
grep -Fq '[flavor]' "$theme_new"
grep -Fq "dark = \"$expected_flavor\"" "$theme_new"
grep -Fq "light = \"$expected_flavor\"" "$theme_new"
grep -Fq '{ url = "*/", fg = "#8cc2ff" }' "$dark_new"
grep -Fq '{ url = "*/", fg = "#205ea6" }' "$light_new"

home_legacy="$tmp_dir/home-legacy"
mkdir -p "$home_legacy/.config/yazi"
cat <<'EOF' >"$home_legacy/.config/yazi/theme.toml"
[mgr]
border_symbol = "│"
border_style = { fg = "#555555" }

[indicator]
padding = { open = "", close = "" }
EOF

run_setup "$home_legacy"

theme_legacy="$home_legacy/.config/yazi/theme.toml"
grep -Fq '[flavor]' "$theme_legacy"
! grep -Fq 'border_style = { fg = "#555555" }' "$theme_legacy"

home_static="$tmp_dir/home-static"
mkdir -p "$home_static/.config/yazi"
cat <<'EOF' >"$home_static/.config/yazi/theme.toml"
# Kaku-aligned theme for Yazi 26.x

[app]
overall = { bg = "#15141b" }

[mgr]
cwd = { fg = "#a277ff", bold = true }
border_symbol = "│"
border_style = { fg = "#2b2838" }

[indicator]
current = { fg = "#a277ff", bg = "#2a233f", bold = true }
padding = { open = "", close = "" }

[mode]
normal_main = { fg = "#a277ff", bg = "#2a233f", bold = true }
EOF

run_setup "$home_static"

theme_static="$home_static/.config/yazi/theme.toml"
grep -Fq '[flavor]' "$theme_static"
! grep -Fq '# Kaku-aligned theme for Yazi 26.x' "$theme_static"
! grep -Fq 'overall = { bg = "#15141b" }' "$theme_static"
grep -Fq "dark = \"$expected_flavor\"" "$theme_static"
grep -Fq "light = \"$expected_flavor\"" "$theme_static"

home_auto="$tmp_dir/home-auto"
mkdir -p "$home_auto/.config/kaku"
cat <<'EOF' >"$home_auto/.config/kaku/kaku.lua"
local wezterm = require 'wezterm'
local config = wezterm.config_builder()
config.color_scheme = 'Auto'
return config
EOF

run_setup "$home_auto"

theme_auto="$home_auto/.config/yazi/theme.toml"
auto_flavor="$(expected_auto_flavor)"
grep -Fq "dark = \"$auto_flavor\"" "$theme_auto"
grep -Fq "light = \"$auto_flavor\"" "$theme_auto"

home_light="$tmp_dir/home-light"
mkdir -p "$home_light/.config/kaku"
cat <<'EOF' >"$home_light/.config/kaku/kaku.lua"
local wezterm = require 'wezterm'
local config = wezterm.config_builder()
config.color_scheme = 'Kaku Light'
return config
EOF

run_setup "$home_light"

mkdir -p "$tmp_dir/realbin"
cat <<'EOF' >"$tmp_dir/realbin/yazi"
#!/usr/bin/env bash
echo "real-yazi $*"
EOF
chmod +x "$tmp_dir/realbin/yazi"

wrapper_light="$home_light/.config/kaku/zsh/bin/yazi"
theme_light="$home_light/.config/yazi/theme.toml"
output="$(
  HOME="$home_light" \
  PATH="$home_light/.config/kaku/zsh/bin:$tmp_dir/realbin:$PATH" \
  "$wrapper_light" --version
)"
[[ "$output" == "real-yazi --version" ]]
grep -Fq 'dark = "kaku-light"' "$theme_light"
grep -Fq 'light = "kaku-light"' "$theme_light"

echo "yazi_theme smoke test passed"
