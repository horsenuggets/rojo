local Settings = require(script.Parent.Settings)

local clock = os.clock

local Timer = {
	-- Hydration can yield, so another coroutine must not consume its active scopes.
	_entries = setmetatable({}, { __mode = "k" }),
}

function Timer._start(label)
	if type(label) ~= "string" then
		error("[Rojo+ Timer] Timer.start requires a label.", 2)
	end

	local thread = coroutine.running()
	local entries = Timer._entries[thread]
	if entries == nil then
		entries = {}
		Timer._entries[thread] = entries
	end

	local enabled = Settings:get("timingLogsEnabled")
	debug.profilebegin("Rojo." .. label)
	table.insert(entries, {
		Label = label,
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
	debug.profileend()
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

Timer.start = Timer._start
Timer.stop = Timer._stop

return Timer
