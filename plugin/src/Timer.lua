local Settings = require(script.Parent.Settings)

local clock = os.clock

local Timer = {
	-- Hydration can yield, so another coroutine must not consume its active scopes.
	_entries = setmetatable({}, { __mode = "k" }),
	_profiler = {
		Begin = debug.profilebegin,
		End = debug.profileend,
	},
}

-- CPU scopes must not yield, including through nested wall-only spans. Wall spans
-- retain logical nesting and optional elapsed-time logs without opening an annotation.
function Timer._start(label, mode)
	if type(label) ~= "string" then
		error("[Rojo+ Timer] Timer.start requires a label.", 2)
	end

	if mode ~= nil and mode ~= "cpu" and mode ~= "wall" then
		error('[Rojo+ Timer] Timer.start requires mode "cpu" or "wall".', 2)
	end

	local enabled = Settings:get("timingLogsEnabled")
	local profiled = mode ~= "wall"
	if profiled then
		Timer._profiler.Begin("Rojo." .. label)
	end

	local thread = coroutine.running()
	local entries = Timer._entries[thread]
	if entries == nil then
		entries = {}
		Timer._entries[thread] = entries
	end

	table.insert(entries, {
		Label = label,
		Profiled = profiled,
		Start = if enabled then clock() else nil,
	})
end

function Timer._stop()
	local thread = coroutine.running()
	local entries = Timer._entries[thread]
	if entries == nil then
		error("[Rojo+ Timer] Timer.stop has no label to stop.", 2)
	end

	local entry = table.remove(entries)
	if entry.Profiled then
		Timer._profiler.End()
	end
	if #entries == 0 then
		Timer._entries[thread] = nil
	end

	-- A setting change while a scope is active must not unbalance it or change
	-- whether that scope logs its elapsed time.
	if entry.Start == nil then
		return
	end

	local duration = clock() - entry.Start
	local label = entry.Label
	if #entries > 0 then
		local priorLabels = {}
		for _, priorEntry in ipairs(entries) do
			table.insert(priorLabels, priorEntry.Label)
		end
		label = table.concat(priorLabels, "/") .. "/" .. label
	end

	print(string.format("[Rojo+ Timer] %s took %.3f ms", label, duration * 1000))
end

function Timer.measure(label, mode, callback, ...)
	if type(callback) ~= "function" then
		error("[Rojo+ Timer] Timer.measure requires a callback.", 2)
	end
	Timer._start(label, mode)
	local results = table.pack(pcall(callback, ...))
	Timer._stop()
	if not results[1] then
		error(results[2], 0)
	end
	return table.unpack(results, 2, results.n)
end

Timer.start = Timer._start
Timer.stop = Timer._stop

return Timer
