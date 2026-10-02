-- throwaway replay probe, bisect 1: flash_arb's latch verbatim, minimal
-- evaluate — emit iff a per-asset trigger exists (age/fresh/price/depth/obi
-- and the round check all OFF). Observable via order counts and reason text.
local POW10 = { [0] = 1 }
for i = 1, 18 do POW10[i] = POW10[i - 1] * 10 end

local function dec_parse(str)
  if type(str) ~= "string" then return nil end
  local t = str:match("^%s*(.-)%s*$")
  if t == "" then return nil end
  local neg = false
  local c = t:sub(1, 1)
  if c == "-" then neg = true; t = t:sub(2)
  elseif c == "+" then t = t:sub(2) end
  local int_part, frac_part = t:match("^(%d*)%.?(%d*)$")
  if not int_part or (int_part == "" and frac_part == "") then return nil end
  local m = tonumber(int_part .. frac_part)
  if m == nil then return nil end
  local s = #frac_part
  if neg and m ~= 0 then m = -m end
  return { m = m, s = s }
end

local function dec_rescale(d, s)
  return d.m * POW10[s - d.s]
end

local function dec_cmp(a, b)
  local s = math.max(a.s, b.s)
  local am, bm = dec_rescale(a, s), dec_rescale(b, s)
  if am < bm then return -1 elseif am > bm then return 1 else return 0 end
end

local function dec_sub(a, b)
  local s = math.max(a.s, b.s)
  return { m = dec_rescale(a, s) - dec_rescale(b, s), s = s }
end

local function dec_mul(a, b)
  return { m = a.m * b.m, s = a.s + b.s }
end

local DEC_100 = { m = 100, s = 0 }

local function int_param(params, name, fallback)
  local raw = params[name]
  if type(raw) ~= "string" then return fallback end
  local n = tonumber(raw)
  if n == nil or n ~= math.floor(n) then return fallback end
  return math.floor(n)
end

local function dec_param(params, name, fallback)
  local raw = params[name]
  if type(raw) ~= "string" then return fallback end
  local d = dec_parse(raw)
  return d or fallback
end

local function read_config()
  local p = bk.params()
  return {
    mom_window_sec = int_param(p, "mom_window_sec", 2),
    mom_threshold_pct = dec_param(p, "mom_threshold_pct", { m = 10, s = 2 }),
  }
end

local spot_closes = {}
local triggers = {}

function bk_on_kline(k)
  if k.interval ~= "sec1" or k.is_closed ~= true then return end
  local close = dec_parse(k.close)
  if not close or close.m <= 0 then return end
  local cfg = read_config()
  local arr = spot_closes[k.symbol]
  if not arr then arr = {}; spot_closes[k.symbol] = arr end
  arr[#arr + 1] = { ts = k.close_time_ms, p = close }
  local cutoff = k.close_time_ms - (cfg.mom_window_sec + 5) * 1000
  local kept, n = {}, 0
  for i = 1, #arr do
    if arr[i].ts >= cutoff then
      n = n + 1
      kept[n] = arr[i]
    end
  end
  spot_closes[k.symbol] = kept

  local win_cutoff = k.close_time_ms - cfg.mom_window_sec * 1000
  local oldest, oldest_ts
  for i = 1, #kept do
    if kept[i].ts >= win_cutoff then
      oldest = kept[i].p
      oldest_ts = kept[i].ts
      break
    end
  end
  if not oldest or oldest.m <= 0 then return end
  if k.close_time_ms - oldest_ts < (cfg.mom_window_sec - 1) * 1000 then return end
  local diff = dec_sub(close, oldest)
  local adiff = diff.m < 0 and { m = -diff.m, s = diff.s } or diff
  local lhs = dec_mul(adiff, DEC_100)
  local rhs = dec_mul(oldest, cfg.mom_threshold_pct)
  if dec_cmp(lhs, rhs) >= 0 then
    triggers[k.symbol] = { dir_up = diff.m > 0, ts = k.close_time_ms }
  end
end

function bk_evaluate()
  local cfg = read_config()
  local entries = {}
  local round = bk.round()
  if round == nil or round.time_left_sec < 0 then
    return { entries = entries, exits = {}, breaks = {} }
  end
  local now_ms = bk.now_ms()
  for _, m in ipairs(bk.markets()) do
    local trig = triggers[m.asset]
    if trig and now_ms - trig.ts <= 1500 then
      local tok = trig.dir_up and m.up_token or m.down_token
      local book = bk.book(tok)
      if book and book.fresh == true then
        local ask = dec_parse(book.best_ask)
        if ask and ask.m > 0 then
          local price = ask
          if price.s <= 2 then price = { m = price.m * (10 ^ (2 - price.s)), s = 2 } end
          if dec_cmp(price, { m = 1, s = 2 }) >= 0 and dec_cmp(price, { m = 55, s = 2 }) <= 0 then
            -- one gate at a time; the other two stay open
            local depth_ok = true
            local obi_ok = true
            local avail_ok = true
            local dbg = ""
            local d = dec_parse(book.bid_depth)
            dbg = dbg .. " depth=" .. tostring(book.bid_depth)
            if d and dec_cmp(d, { m = 1, s = 0 }) < 0 then depth_ok = false end
            local o = dec_parse(book.obi)
            dbg = dbg .. " obi=" .. tostring(book.obi)
            if o then
              local abs = o.m < 0 and { m = -o.m, s = o.s } or o
              if dec_cmp(abs, { m = 1, s = 0 }) > 0 then obi_ok = false end
            end
            local acct = bk.account()
            local a
            if acct and acct.available ~= nil then
              a = dec_parse(acct.available)
              dbg = dbg .. " avail=" .. tostring(acct.available)
            else
              dbg = dbg .. " acct=nil"
            end
            if a and dec_cmp(a, { m = 100, s = 2 }) < 0 then avail_ok = false end
            if avail_ok then
              entries[#entries + 1] = {
                token = tok,
                price = "0.01",
                reason = "bisectD" .. dbg,
              }
            end
          end
        end
      end
    end
  end
  return { entries = entries, exits = {}, breaks = {} }
end
