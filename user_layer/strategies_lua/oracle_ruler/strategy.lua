-- oracle_ruler — the calibration probe (§6.5 contract).
--
-- ONE JOB: rest a maker bid at `entry_price` on the round's UP token,
-- unconditionally, once per round. Everything else (fills, exits, settlement,
-- fees) is the kernel's — which is exactly what makes this a RULER: any booked
-- PnL that disagrees with the closed-form expectation built into the synthetic
-- corpus (scripts/lib/oracle-ruler.mjs) is a measurement-layer defect, and the
-- probe has no gates to blame.
--
-- Fill mechanics (asserted in oracle-ruler-check.mjs against sim.rs):
--   * submit when bid 0.30 / ask 0.31 → no cross at submit (safe under the
--     strict post-only reading);
--   * at t=4s the ask drops to 0.30 → the resting bid crosses and fills AT
--     ITS OWN PRICE (sim.rs crosses(): BUY fills when best ask <= limit),
--     ~3s after submit — inside the 5000ms MakerThenTaker escalation window,
--     so the fill is a MAKER fill at 0.30 and the entry fee is 0.
-- Exit machinery: none from this side — the seal. The kernel ladder owns
-- every exit; the check script asserts the booked exit_reason per arm.

-- ── exact decimal helpers (shared semantics with mad_dog) ───────────────────

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
  local keep = 18 - #int_part
  if #frac_part > keep then frac_part = frac_part:sub(1, keep) end
  local m = tonumber(int_part .. frac_part)
  if m == nil then return nil end
  local s = #frac_part
  if neg and m ~= 0 then m = -m end
  return { m = m, s = s }
end

local function dec_fmt(d)
  local m, s = d.m, d.s
  local neg = m < 0
  if neg then m = -m end
  local digits = tostring(m)
  if s > 0 then
    if #digits <= s then digits = string.rep("0", s - #digits + 1) .. digits end
    return (neg and "-" or "") .. digits:sub(1, #digits - s) .. "." .. digits:sub(#digits - s + 1)
  end
  return (neg and "-" or "") .. digits
end

-- ── config (one tunable: the resting bid price) ─────────────────────────────

local function read_config()
  local p = bk.params()
  local d = nil
  if type(p.entry_price) == "string" then d = dec_parse(p.entry_price) end
  if not d or d.m <= 0 then d = { m = 30, s = 2 } end
  return { entry_price = d }
end

-- ── per-round single-flight: one bid per token per round ────────────────────

local tracker = { slot = nil, sent = {} }

local function reset_if_new_round(slot)
  if slot ~= tracker.slot then
    tracker.slot = slot
    tracker.sent = {}
  end
end

function bk_on_round(round)
  reset_if_new_round(round.slot)
end

--- Rest one maker bid at `entry_price` on each market's UP token (the corpus
--- declares exactly one round; the UP token is the probe's subject).
function bk_evaluate()
  local round = bk.round()
  if round == nil then
    return { entries = {}, exits = {}, breaks = {} }
  end
  reset_if_new_round(round.slot)
  local cfg = read_config()
  local entries = {}
  for _, mk in ipairs(bk.markets()) do
    local tok = mk.up_token
    if not tracker.sent[tok] then
      local book = bk.book(tok)
      if book and book.fresh == true then
        local ask = dec_parse(book.best_ask)
        if ask and ask.m > 0 then
          tracker.sent[tok] = true
          entries[#entries + 1] = {
            token = tok,
            price = dec_fmt(cfg.entry_price),
            reason = string.format("oracle ruler probe bid @ %s (ask %s)", dec_fmt(cfg.entry_price), dec_fmt(ask)),
          }
        end
      end
    end
  end
  return { entries = entries, exits = {}, breaks = {} }
end
