-- Renames the server for a day of the year, and gives the name back afterwards.
-- Needs the capabilities read_guild, manage_guild, storage and schedule.
local DAY = 86400
local REASON = 'temporary guild profile'

local function days_from_civil(y, m, d)
  if m <= 2 then y = y - 1 end
  local era = y // 400
  local yoe = y - era * 400
  local doy = (153 * (m + (m > 2 and -3 or 9)) + 2) // 5 + d - 1
  local doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
  return era * 146097 + doe - 719468
end

local function civil_year(days)
  local z = days + 719468
  local era = z // 146097
  local doe = z - era * 146097
  local yoe = (doe - doe // 1460 + doe // 36524 - doe // 146096) // 365
  local doy = doe - (365 * yoe + yoe // 4 - yoe // 100)
  local mp = (5 * doy + 2) // 153
  local y = yoe + era * 400
  if mp >= 10 then y = y + 1 end
  return y
end

local function minutes(text)
  local h, m = string.match(text or '', '^(%d+):(%d+)$')
  if not h then return nil end
  return tonumber(h) * 60 + tonumber(m)
end

local function rename(guild_id, name)
  return pcall(closure.execute, {
    type = 'edit_guild', guild_id = guild_id, patch = { name = name }, reason = REASON })
end

local function reconcile(guild_id)
  local cfg = closure.config()
  if type(cfg) ~= 'table' or type(cfg.name) ~= 'string' or cfg.name == '' then return end
  local from, to = minutes(cfg.from), minutes(cfg.to)
  local month, day = cfg.month, cfg.day
  if not (from and to and to > from and math.type(month) == 'integer' and math.type(day) == 'integer') then
    return
  end
  local offset = math.type(cfg.utc_offset_minutes) == 'integer' and cfg.utc_offset_minutes or 0

  local function bounds(year)
    local midnight = days_from_civil(year, month, day) * DAY - offset * 60
    return midnight + from * 60, midnight + to * 60
  end

  local now = closure.now()
  local year = civil_year((now + offset * 60) // DAY)
  local first, last = bounds(year)
  local active, next_at
  if now < first then
    active, next_at = false, first
  elseif now < last then
    active, next_at = true, last
  else
    active, next_at = false, (bounds(year + 1))
  end

  local original = closure.store_get('original')
  local applied = type(original) == 'string'
  local done = true
  if active and not applied then
    local outcome = closure.execute({ type = 'get_guild', guild_id = guild_id })
    local current = outcome.data and outcome.data.name
    if type(current) == 'string' then
      closure.store_set('original', current)
      if not rename(guild_id, cfg.name) then
        closure.store_delete('original')
        done = false
      end
    else
      done = false
    end
  elseif not active and applied then
    if rename(guild_id, original) then
      closure.store_delete('original')
    else
      done = false
    end
  end
  closure.schedule({ name = 'reconcile', at = done and next_at or now + 300, key = 'reconcile' })
end

function on_event(event)
  if event.type == 'config_changed' or (event.type == 'scheduled' and event.data.name == 'reconcile') then
    reconcile(event.data.guild_id)
  end
  return {}
end
