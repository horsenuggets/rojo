return function()
	local yieldBudget = require(script.Parent.yieldBudget)

	-- A clock the test moves by hand, and a yield that only counts.
	local function harness(seconds)
		local state = { now = 0, yields = 0 }
		local step = yieldBudget(seconds, function()
			return state.now
		end, function()
			state.yields += 1
		end)
		return state, step
	end

	it("should not yield while the budget lasts", function()
		local state, step = harness(1)
		for _ = 1, 100 do
			state.now += 0.009
			expect(step()).to.equal(false)
		end
		expect(state.yields).to.equal(0)
	end)

	it("should yield once the budget runs out", function()
		local state, step = harness(1)
		state.now = 1
		expect(step()).to.equal(true)
		expect(state.yields).to.equal(1)
	end)

	it("should start a fresh budget after each yield", function()
		local state, step = harness(1)
		state.now = 5
		expect(step()).to.equal(true)
		-- Still inside the budget that began at 5.
		state.now = 5.5
		expect(step()).to.equal(false)
		state.now = 6
		expect(step()).to.equal(true)
		expect(state.yields).to.equal(2)
	end)

	it("should not charge time spent yielding to the next budget", function()
		local now = 0
		local yields = 0
		local step = yieldBudget(1, function()
			return now
		end, function()
			yields += 1
			-- The yield itself takes a long time.
			now += 10
		end)
		now = 1
		expect(step()).to.equal(true)
		now += 0.5
		expect(step()).to.equal(false)
		expect(yields).to.equal(1)
	end)

	-- However the work is sliced, no stretch between yields outlasts the budget
	-- by more than one step.
	it("should bound every stretch of work between yields", function()
		for _, stepSeconds in { 0.001, 0.03, 0.25, 0.9 } do
			local state, step = harness(1)
			local stretchStart = 0
			local longest = 0
			for _ = 1, 2000 do
				state.now += stepSeconds
				local before = state.now
				if step() then
					longest = math.max(longest, before - stretchStart)
					stretchStart = state.now
				end
			end
			expect(state.yields > 0).to.equal(true)
			expect(longest <= 1 + stepSeconds + 1e-9).to.equal(true)
		end
	end)
end
