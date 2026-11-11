-- PiFin playback overlay for mpv: title, clock, progress bar, timecodes,
-- "Ends at", seek feedback, pause icon, and audio/subtitle menus driven by a
-- TV remote (UP = audio, DOWN = subtitles, ENTER = pick, BACK = close).
--
-- Performance: one ASS overlay in real screen pixels. While hidden the
-- redraw timer is stopped and the overlay is empty (zero cost). While shown
-- only the small bar/timecode layer redraws, 2x per second. No observers on per-frame properties.
-- Script name is "pifin_osd" (mpv turns '-' into '_'): input.conf binds
-- "script-binding pifin_osd/<name>". Title comes from --script-opts
-- pifin-osd-series/season/episode/name/year, else from media-title.

local mp = require "mp"
local options = require "mp.options"

local o = { series = "", season = "", episode = "", name = "", year = "" }
options.read_options(o, "pifin-osd")

local FONT = "DejaVu Sans"
local C = { accent = "DCA400", text = "FFFFFF", dim = "C8C8C8", black = "000000" } -- BBGGRR
local HIDE_AFTER, MENU_TIMEOUT = 4, 15

-- two layers: 'ov' (scrims, title, clock, pause, menu) changes rarely, so mpv
-- keeps its rendered bitmaps; 'dyn' (bar, timecodes) is small and redraws 2x/s
local ov = mp.create_osd_overlay("ass-events")
local dyn = mp.create_osd_overlay("ass-events")
dyn.z = 1
local W, H, u = 1280, 720, 1          -- screen px, scale (1 = 720p)
local visible, menu = false, nil      -- menu = {kind, rows, cursor}
local timer, hide_t, menu_t = nil, nil, nil
local bubble, bubble_until = nil, 0
local last, last_dyn = nil, nil
local meta = { l1 = "", l2 = "" }

-- ----------------------------------------------------------------- helpers --
local function esc(s) return (tostring(s or ""):gsub("\\", "\\\226\129\160"):gsub("{", "\\{"):gsub("}", "\\}")) end

local function fmt(sec)
    if not sec or sec < 0 then sec = 0 end
    sec = math.floor(sec)
    local h, m, s = math.floor(sec / 3600), math.floor((sec % 3600) / 60), sec % 60
    if h > 0 then return string.format("%d:%02d:%02d", h, m, s) end
    return string.format("%d:%02d", m, s)
end

local function text(an, x, y, size, color, str, bold, alpha)
    return string.format("{\\r\\fn%s\\fs%d\\b%d\\bord%.1f\\shad0\\3c&H000000&\\1c&H%s&\\1a&H%02X&\\q2\\an%d\\pos(%d,%d)}%s",
        FONT, math.floor(size * u), bold and 1 or 0, 1.5 * u, color, alpha or 0, an, x, y, str)
end

local function shape(color, alpha, path)
    return string.format("{\\r\\an7\\pos(0,0)\\bord0\\shad0\\1c&H%s&\\1a&H%02X&\\p1}%s{\\p0}", color, alpha, path)
end

local function rrect(x0, y0, x1, y1, r)   -- rounded rectangle path
    r = math.min(r, (x1 - x0) / 2, (y1 - y0) / 2)
    local k = r * 0.45
    return string.format(
        "m %d %d l %d %d b %d %d %d %d %d %d l %d %d b %d %d %d %d %d %d l %d %d b %d %d %d %d %d %d l %d %d b %d %d %d %d %d %d ",
        x0 + r, y0, x1 - r, y0, x1 - k, y0, x1, y0 + k, x1, y0 + r,
        x1, y1 - r, x1, y1 - k, x1 - k, y1, x1 - r, y1,
        x0 + r, y1, x0 + k, y1, x0, y1 - k, x0, y1 - r,
        x0, y0 + r, x0, y0 + k, x0 + k, y0, x0 + r, y0)
end

local function scrim(top, y0, y1)      -- fake vertical gradient, 4 bands (big alpha areas cost CPU on the Pi)
    local out, n = {}, 4
    for i = 0, n - 1 do
        local a = math.floor(0x50 + (0xF8 - 0x50) * i / (n - 1)) -- dark at the edge
        local b0 = top and (y0 + (y1 - y0) * i / n) or (y1 - (y1 - y0) * (i + 1) / n)
        local b1 = top and (y0 + (y1 - y0) * (i + 1) / n) or (y1 - (y1 - y0) * i / n)
        out[#out + 1] = shape(C.black, a, string.format("m 0 %d l %d %d l %d %d l 0 %d", b0, W, b0, W, b1, b1))
    end
    return table.concat(out, "\n")
end

-- ----------------------------------------------------------------- metadata --
local function load_meta()
    local series, name = o.series, o.name
    local season, episode = tonumber(o.season), tonumber(o.episode)
    if series == "" and name == "" then
        local t = mp.get_property("media-title", "")
        local a, b, c, d = t:match("^(.-)%s+%-%s+S(%d+)E(%d+)%s+%-%s+(.*)$")
        if a then series, season, episode, name = a, tonumber(b), tonumber(c), d else name = t end
    end
    if series ~= "" then
        meta.l1 = series
        meta.l2 = (season and episode) and string.format("S%d:E%d  ·  %s", season, episode, name) or name
    else
        meta.l1 = name .. ((o.year ~= "") and ("  (" .. o.year .. ")") or "")
        meta.l2 = ""
    end
end

-- ----------------------------------------------------------------- tracks --
local function track_label(t)
    local parts = {}
    if t.lang then parts[#parts + 1] = t.lang:upper() end
    if t.title then parts[#parts + 1] = t.title end
    if t.codec then parts[#parts + 1] = (t.codec:gsub("subrip", "srt"):gsub("hdmv_pgs_subtitle", "pgs")) end
    if t.external then parts[#parts + 1] = "external" end
    if t.type == "audio" and t["demux-channel-count"] then parts[#parts + 1] = t["demux-channel-count"] .. "ch" end
    return #parts > 0 and table.concat(parts, "  ·  ") or ("Track " .. t.id)
end

local function build_rows(kind)
    local rows = {}
    local any_sel = false
    for _, t in ipairs(mp.get_property_native("track-list") or {}) do
        if t.type == kind then
            rows[#rows + 1] = { id = t.id, label = esc(track_label(t)), selected = t.selected }
            any_sel = any_sel or t.selected
        end
    end
    if kind == "sub" then table.insert(rows, 1, { id = "no", label = "Off", selected = not any_sel }) end
    return rows
end

-- ----------------------------------------------------------------- render --
local function build()
    if not visible and not menu then return "", "" end
    local a, b = {}, {}
    local M = math.floor(64 * u)
    a[#a + 1] = scrim(true, 0, 130 * u)
    a[#a + 1] = scrim(false, H - 150 * u, H)

    -- title (top left) and clock (top right)
    a[#a + 1] = text(7, M, 40 * u, 40, C.text, esc(meta.l1), true)
    if meta.l2 ~= "" then a[#a + 1] = text(7, M, 92 * u, 26, C.dim, esc(meta.l2)) end
    a[#a + 1] = text(9, W - M, 40 * u, 34, C.text, os.date("%H:%M"))

    -- progress bar
    local pos = mp.get_property_number("time-pos", 0)
    local dur = mp.get_property_number("duration", 0)
    local cache = mp.get_property_number("demuxer-cache-time", 0)
    local by, bh = H - 62 * u, math.max(4, 6 * u)
    local x0, x1 = M, W - M
    local frac = dur > 0 and math.min(1, pos / dur) or 0
    local cfrac = dur > 0 and math.min(1, math.max(frac, cache / dur)) or 0
    b[#b + 1] = shape(C.text, 0xC0, rrect(x0, by, x1, by + bh, bh / 2))
    if cfrac > frac then b[#b + 1] = shape(C.text, 0x90, rrect(x0, by, x0 + (x1 - x0) * cfrac, by + bh, bh / 2)) end
    if frac > 0 then
        local px = x0 + (x1 - x0) * frac
        b[#b + 1] = shape(C.accent, 0, rrect(x0, by, math.max(px, x0 + bh), by + bh, bh / 2))
        local r = bh * 1.4                                      -- playhead knob
        b[#b + 1] = shape(C.text, 0, rrect(px - r, by + bh / 2 - r, px + r, by + bh / 2 + r, r))
    end

    -- timecodes under the bar, "Ends at" / seek bubble above it
    local ty = by + bh + 8 * u
    b[#b + 1] = text(7, x0, ty, 24, C.text, fmt(pos))
    if dur > 0 then
        b[#b + 1] = text(9, x1, ty, 24, C.dim, "−" .. fmt(dur - pos))
        b[#b + 1] = text(3, x1, by - 14 * u, 22, C.dim, "Ends at " .. os.date("%H:%M", os.time() + math.floor(dur - pos)))
    end
    if bubble and mp.get_time() < bubble_until then
        b[#b + 1] = text(1, x0, by - 14 * u, 30, C.accent, esc(bubble), true)
    end

    -- pause icon
    if mp.get_property_bool("pause") and not menu then
        local cx, cy, ph, pw, gap = W / 2, H / 2, 110 * u, 26 * u, 22 * u
        a[#a + 1] = shape(C.black, 0x80, rrect(cx - 90 * u, cy - 90 * u, cx + 90 * u, cy + 90 * u, 90 * u))
        a[#a + 1] = shape(C.text, 0, rrect(cx - gap / 2 - pw, cy - ph / 2, cx - gap / 2, cy + ph / 2, 6 * u))
        a[#a + 1] = shape(C.text, 0, rrect(cx + gap / 2, cy - ph / 2, cx + gap / 2 + pw, cy + ph / 2, 6 * u))
    end

    -- track menu
    if menu then
        local rh, pad = 54 * u, 22 * u
        local mw = math.min(680 * u, W - 2 * M)
        local mh = pad * 2 + 46 * u + #menu.rows * rh
        local mx, my = (W - mw) / 2, math.max(20 * u, (H - mh) / 2 - 30 * u)
        a[#a + 1] = shape("141414", 0x20, rrect(mx, my, mx + mw, my + mh, 22 * u))
        a[#a + 1] = text(7, mx + pad + 8 * u, my + pad, 20, C.dim, menu.kind == "audio" and "AUDIO" or "SUBTITLES", true)
        for i, r in ipairs(menu.rows) do
            local ry = my + pad + 46 * u + (i - 1) * rh
            if i == menu.cursor then
                a[#a + 1] = shape(C.text, 0xD8, rrect(mx + pad - 6 * u, ry, mx + mw - pad + 6 * u, ry + rh - 6 * u, 14 * u))
            end
            if r.selected then
                local cx, cy, rr = mx + mw - pad - 18 * u, ry + (rh - 6 * u) / 2, 7 * u
                a[#a + 1] = shape(C.accent, 0, rrect(cx - rr, cy - rr, cx + rr, cy + rr, rr))
            end
            a[#a + 1] = text(4, mx + pad + 12 * u, ry + (rh - 6 * u) / 2, 28,
                r.selected and C.accent or (i == menu.cursor and C.text or C.dim), r.label, i == menu.cursor)
        end
    end
    return table.concat(a, "\n"), table.concat(b, "\n")
end

local function put(o, s)
    o.res_x, o.res_y = W, H
    o.data = s
    if s == "" then o:remove() else o:update() end
end

local function draw()
    local s, d = build()
    if s ~= last then last = s; put(ov, s) end
    if d ~= last_dyn then last_dyn = d; put(dyn, d) end
end

-- -------------------------------------------------------------- show/hide --
local try_hide
local function arm_hide()
    if hide_t then hide_t:kill() end
    hide_t = mp.add_timeout(HIDE_AFTER, try_hide)
end

try_hide = function()
    if menu or mp.get_property_bool("pause") then arm_hide() return end
    visible = false
    if timer then timer:stop() end
    mp.set_property_number("sub-pos", 100)
    draw()
end

local function show()
    if not visible then mp.set_property_number("sub-pos", 82) end  -- keep subs above the bar
    visible = true
    if not timer then timer = mp.add_periodic_timer(0.5, draw) else timer:resume() end
    arm_hide()
    draw()
end

local function flash(msg, secs)
    bubble, bubble_until = msg, mp.get_time() + (secs or 1.2)
    show()
end

-- ------------------------------------------------------------------ menus --
local MENU_KEYS = { "UP", "DOWN", "ENTER", "KP_ENTER", "SPACE", "LEFT", "RIGHT", "BS", "ESC", "GO_BACK" }
local close_menu

local function menu_key(k)
    if menu_t then menu_t:kill() end
    menu_t = mp.add_timeout(MENU_TIMEOUT, function() close_menu() end)
    if k == "UP" then menu.cursor = (menu.cursor - 2) % #menu.rows + 1
    elseif k == "DOWN" then menu.cursor = menu.cursor % #menu.rows + 1
    elseif k == "ENTER" or k == "KP_ENTER" or k == "SPACE" then
        local r = menu.rows[menu.cursor]
        if r then mp.set_property(menu.kind == "audio" and "aid" or "sid", tostring(r.id)) end
    else close_menu() return end
    show()
end

local function open_menu(kind)
    if menu then
        local same = menu.kind == kind
        close_menu()
        if same then return end
    end
    menu = { kind = kind, rows = build_rows(kind), cursor = 1 }
    for i, r in ipairs(menu.rows) do if r.selected then menu.cursor = i end end
    for _, k in ipairs(MENU_KEYS) do
        mp.add_forced_key_binding(k, "menu-" .. k, function() menu_key(k) end, { repeatable = true })
    end
    menu_t = mp.add_timeout(MENU_TIMEOUT, function() close_menu() end)
    show()
end

close_menu = function()
    if not menu then return end
    for _, k in ipairs(MENU_KEYS) do mp.remove_key_binding("menu-" .. k) end
    if menu_t then menu_t:kill() end
    menu = nil
    show()
end

-- --------------------------------------------------------- named bindings --
local function seek(sec, label)
    mp.commandv("seek", tostring(sec))
    flash(label)
end
mp.add_key_binding(nil, "toggle-pause", function() mp.command("cycle pause"); show() end)
mp.add_key_binding(nil, "seek-back", function() seek(-10, "−10s") end, { repeatable = true })
mp.add_key_binding(nil, "seek-fwd", function() seek(30, "+30s") end, { repeatable = true })
mp.add_key_binding(nil, "seek-big-back", function() seek(-300, "−5:00") end)
mp.add_key_binding(nil, "seek-big-fwd", function() seek(300, "+5:00") end)
mp.add_key_binding(nil, "open-audio", function() open_menu("audio") end)
mp.add_key_binding(nil, "open-sub", function() open_menu("sub") end)
mp.add_key_binding(nil, "show-overlay", show)
mp.add_key_binding(nil, "back", function() if menu then close_menu() else mp.command("quit") end end)

-- ------------------------------------------- observers (phone, IPC, etc.) --
local ready = false   -- skip the initial callbacks fired on registration
local function on_dims()
    local d = mp.get_property_native("osd-dimensions")
    if d and d.w > 0 and d.h > 0 then W, H, u = d.w, d.h, d.h / 720; last, last_dyn = nil, nil; if visible or menu then draw() end end
end
mp.observe_property("osd-dimensions", "native", on_dims)
mp.observe_property("pause", "bool", function() if ready then show() end end)
mp.observe_property("seeking", "bool", function(_, v) if ready and v then show() end end)
mp.observe_property("volume", "number", function(_, v) if ready and v then flash(string.format("Volume %d", v)) end end)
mp.observe_property("mute", "bool", function(_, v) if ready then flash(v and "Muted" or "Sound on") end end)
mp.observe_property("current-tracks/sub", "native", function(_, t)
    if not ready then return end
    flash(t and ("Subtitles: " .. track_label(t)) or "Subtitles off", 2)
end)
mp.observe_property("current-tracks/audio", "native", function(_, t)
    if ready and t then flash("Audio: " .. track_label(t), 2) end
end)
mp.observe_property("track-list", "native", function()
    if menu then menu.rows = build_rows(menu.kind); menu.cursor = math.min(menu.cursor, #menu.rows) end
end)
mp.register_event("file-loaded", function()
    load_meta()
    mp.add_timeout(1, function() ready = true end)
    show()
end)
mp.register_event("end-file", function() close_menu(); visible = false; last, last_dyn = nil, nil; ov:remove(); dyn:remove() end)
