--[[
	Lets long-running work give control back to Studio every so often.

	Studio stops a script that runs for longer than its script timeout without
	yielding, and it stops it without raising an error the script could catch.
	Walking a large place takes longer than that, so the walk calls the function
	this returns once per step, and it yields whenever the budget has run out.

	The clock and the yield are parameters so tests can drive both.
]]

local DEFAULT_BUDGET_SECONDS = 0.1

local function yieldBudget(seconds, clock, yield)
	seconds = seconds or DEFAULT_BUDGET_SECONDS
	clock = clock or os.clock
	yield = yield or task.wait

	local deadline = clock() + seconds

	-- Returns whether it yielded.
	return function()
		if clock() < deadline then
			return false
		end

		yield()
		-- Measured after the yield, so time spent waiting isn't charged to the
		-- next stretch of work.
		deadline = clock() + seconds
		return true
	end
end

return yieldBudget
