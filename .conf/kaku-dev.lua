local wezterm = require("wezterm")

local function resolve_bundled_config()
	-- local home = os.getenv("HOME") or ""
	-- local local_override = home .. '/.config/kaku/bundled/kaku.lua'
	-- local f = io.open(local_override, 'r')
	-- if f then
	--   f:close()
	--   return local_override
	-- end

	local resource_dir = wezterm.executable_dir:gsub("MacOS/?$", "Resources")
	local bundled = resource_dir .. "/kaku.lua"
	f = io.open(bundled, "r")
	if f then
		f:close()
		return bundled
	end

	local dev_bundled = wezterm.executable_dir .. "/../../assets/macos/Kaku.app/Contents/Resources/kaku.lua"
	f = io.open(dev_bundled, "r")
	if f then
		f:close()
		return dev_bundled
	end

	local app_bundled = "/Applications/Kaku.app/Contents/Resources/kaku.lua"
	f = io.open(app_bundled, "r")
	if f then
		f:close()
		return app_bundled
	end

	local home_bundled = home .. "/Applications/Kaku.app/Contents/Resources/kaku.lua"
	f = io.open(home_bundled, "r")
	if f then
		f:close()
		return home_bundled
	end

	return nil
end

local config = {}
local bundled = resolve_bundled_config()

if bundled then
	local ok, loaded = pcall(dofile, bundled)
	if ok and type(loaded) == "table" then
		config = loaded
	else
		wezterm.log_error("Kaku: failed to load bundled defaults from " .. bundled)
	end
else
	wezterm.log_error("Kaku: bundled defaults not found")
end

-- User overrides:
-- Kaku intentionally keeps WezTerm-compatible Lua API names
-- for maximum compatibility, so `wezterm.*` here is expected.
--
-- 1) Font family and size
-- config.font = wezterm.font('JetBrains Mono')
-- config.font_size = 16.0
--
-- 2) Color scheme
-- config.color_scheme = 'Builtin Solarized Dark'
--
-- 3) Window size and padding
config.initial_cols = 130
config.initial_rows = 30
config.window_background_opacity = 1
-- config.macos_window_background_blur = 20

-- Will remove the OS X traffic light buttons
-- config.window_decorations = "RESIZE"

--           F O N T
-- config.color_scheme = 'AdventureTime'
config.font_size = 14
config.line_height = 1
-- config.line_height = 0.9
wezterm.font("JetBrainsMono Nerd Font", { weight = "Medium", italic = false })
--

-- 4) Default shell/program
-- config.default_prog = { '/bin/zsh', '-l' }
--
-- 5) Cursor and scrollback
-- config.default_cursor_style = 'BlinkingBar'
-- config.scrollback_lines = 20000
--
-- 6) Add or override a key binding
-- table.insert(config.keys, {
--   key = 'Enter',
--   mods = 'CMD|SHIFT',
--   action = wezterm.action.TogglePaneZoomState,
-- })

config.tab_bar_at_bottom = true
config.active_pane_indicator = "Gutter"
config.active_pane_indicator_size = 12
return config
